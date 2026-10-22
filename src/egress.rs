//! The network egress bridge (R3b/R3c).
//!
//! `--unshare-net` is unconditional, so a sandbox has no route out at all. When
//! `network.enabled` is set, prior grants egress through a SOCKS5 bridge whose
//! listening socket is created **inside the sandbox's network namespace** and
//! handed out to prior over a `SCM_RIGHTS` bootstrap socketpair. That is
//! `nono`'s NEP-0004 mechanism, and it is chosen over a bind-mounted socket
//! file for one reason: no socket path exists in the sandbox filesystem, so no
//! `ro`/`rw` binding can ever re-expose the channel. See
//! `doc/261021_sandbox_env_x11_audit.md` — R3a (contract), R3b (mechanism),
//! R3c (policy engine).
//!
//! Two processes are involved and they share this module:
//!
//! - **The shim** (`src/bin/prior-egress.rs`, run as bwrap's payload). It binds
//!   `127.0.0.1:0` inside the new namespace, sends the listening fd out, points
//!   the proxy variables at itself, and `exec`s the real command.
//! - **The supervisor** (this module, in prior). It receives the fd, wraps it in
//!   a listener, and runs the SOCKS5 accept loop for the command's lifetime.
//!
//! The port is chosen by the shim, not passed in, so there is no ordering
//! constraint between "bind" and "build the argv" — the shim owns both the bind
//! and the `exec`, and it sets the proxy environment itself right before the
//! `exec`. That is simpler than the fixed-port arrangement NEP-0004 needs when
//! the supervisor builds the child environment.

use std::io;

/// Environment variable naming the inherited bootstrap fd for the shim.
pub const EGRESS_FD_ENV: &str = "PRIOR_EGRESS_FD";

/// The shim binary name. Kept in one place so the resolver and the argv agree.
pub const EGRESS_BIN: &str = "prior-egress";

/// The proxy environment a sandbox gets, as `NAME=template` entries for the
/// shim. `%p` is the bound `127.0.0.1:<port>`.
///
/// # Why the uppercase variables are `http://` and `ALL_PROXY` is not
///
/// The bridge speaks **two** protocols on one port (SOCKS5 and HTTP proxy; see
/// [`serve_conversation`]), so either scheme reaches it. The split is about
/// clients, not capability:
///
/// - `ALL_PROXY` is the variable SOCKS-aware clients read (curl, git, python
///   `requests`) and where `socks5h://` carries real meaning: the `h` resolves
///   the name at the proxy, so the sandbox never learns an address it cannot
///   route to. Keep it `socks5h://`.
/// - `HTTP_PROXY`/`HTTPS_PROXY` are the variables *everything* reads, including
///   clients with no SOCKS support at all — `w3m` has no SOCKS code (verified),
///   and `wget` here honours only the lowercase spelling. A client that cannot
///   parse the scheme **fails rather than falling through** (measured: w3m given
///   `HTTP_PROXY=socks5h://…` dies, it does not try `http_proxy`). So these must
///   hold a value every client can parse: `http://`.
///
/// Each name is also set in lower case, because clients disagree on spelling and
/// several (wget) read only the lower-case form. Where both are set, the
/// upper-case one wins in every client measured, so they must agree — which they
/// do, being generated from the same template.
///
/// # Why there is no `NO_PROXY`
///
/// Deliberately unset. Inside the netns `127.0.0.1` is the sandbox's own
/// loopback, and the only thing listening there is the bridge — so bypassing the
/// proxy for it would route a client to nothing, not to a local server. Worse,
/// excluding it would remove the one way to reach a *host* loopback service
/// through the bridge when the allowlist permits it, which is a use `network =
/// true` is documented to support.
pub fn proxy_var_specs() -> Vec<String> {
    // Every variable is published in both the upper- and lower-case spelling.
    // That is not redundancy: no client reads all of them (wget here reads only
    // `http_proxy`; w3m reads the upper-case form first and does not fall
    // through), so a missing spelling is a client with no egress.
    let mut v = Vec::with_capacity(6);
    for &(upper, scheme) in &[
        ("ALL_PROXY", "socks5h"),
        ("HTTP_PROXY", "http"),
        ("HTTPS_PROXY", "http"),
    ] {
        let url = format!("{scheme}://%p");
        v.push(format!("{upper}={url}"));
        v.push(format!("{}={url}", upper.to_lowercase()));
    }
    v
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS fd passing
// ---------------------------------------------------------------------------

/// Send `fd` (plus an optional `payload` of ordinary bytes) over `sock`.
///
/// Used by the shim to hand the in-namespace listening socket to prior. The
/// receiving side is [`recv_fd`]. Both live here so the wire format is defined
/// once, next to the code that depends on it.
pub fn send_fd(sock: std::os::fd::RawFd, fd: std::os::fd::RawFd, payload: &[u8]) -> io::Result<()> {
    use std::os::fd::RawFd;

    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    // Room for exactly one `cmsghdr` carrying one `RawFd`.
    // SAFETY: pure size computation; the argument is a compile-time constant.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    // SAFETY: `control` is `CMSG_SPACE(sizeof RawFd)` bytes, and `CMSG_FIRSTHDR`
    // returns a header inside that buffer. The single `RawFd` copied into the
    // data area fits by construction of that size.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as libc::c_uint) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const RawFd as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<RawFd>(),
        );
    }

    // SAFETY: `msg` and its iovec/control buffers are live for the call.
    let n = unsafe { libc::sendmsg(sock, &msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive one fd sent by [`send_fd`], together with whatever ordinary bytes
/// accompanied it. The returned fd has `CLOEXEC` set (via `MSG_CMSG_CLOEXEC`),
/// so it will not leak into processes prior spawns later.
///
/// Blocks up to the socket's `SO_RCVTIMEO`; the caller sets that so a shim that
/// dies before sending cannot wedge the launch.
pub fn recv_fd(
    sock: std::os::fd::RawFd,
    buf: &mut [u8],
) -> io::Result<(usize, std::os::fd::RawFd)> {
    use std::os::fd::RawFd;

    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    // SAFETY: pure size computation; the argument is a compile-time constant.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    // SAFETY: buffers are sized as above and outlive the call.
    let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::other(
            "egress shim closed the bootstrap socket without sending a listener",
        ));
    }

    // SAFETY: walking the control buffer the kernel just filled. `CMSG_*`
    // accessors are libc functions that range-check against `msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let mut fd: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    &mut fd as *mut RawFd as *mut u8,
                    std::mem::size_of::<RawFd>(),
                );
                if fd >= 0 {
                    return Ok((n as usize, fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Err(io::Error::other(
        "egress shim sent a message with no file descriptor",
    ))
}

// ---------------------------------------------------------------------------
// Allowlist matching
// ---------------------------------------------------------------------------

/// Decides whether a `CONNECT` to `host:port` is permitted.
///
/// An empty allowlist permits any destination (`network = true`); that is the
/// documented "open but host-safe" default, and the safety comes from the
/// namespace, not from this check. Entries are `host` or `host:port`, and a
/// leading `*.` matches any subdomain (`*.crates.io` matches `static.crates.io`
/// but not `crates.io` itself).
pub fn host_allowed(allow: &[String], host: &str, port: u16) -> bool {
    if allow.is_empty() {
        return true;
    }
    let host_lc = host.to_ascii_lowercase();
    allow
        .iter()
        .any(|entry| entry_matches(entry.trim(), &host_lc, port))
}

fn entry_matches(entry: &str, host: &str, port: u16) -> bool {
    if entry.is_empty() {
        return false;
    }
    // Split an optional `:port`. IPv6 literals arrive in `host` without
    // brackets, so an entry that is a bare IPv6 address has no port suffix; a
    // trailing `:<digits>` which also parses as a port is treated as one.
    let (ehost, eport) = match entry.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            match p.parse::<u16>() {
                Ok(p) => (h, Some(p)),
                Err(_) => (entry, None),
            }
        }
        _ => (entry, None),
    };
    if let Some(p) = eport {
        if p != port {
            return false;
        }
    }
    let ehost = ehost.to_ascii_lowercase();
    match ehost.strip_prefix("*.") {
        Some(suffix) => {
            host.len() > suffix.len() + 1
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        }
        None => ehost == host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_permits_anything() {
        assert!(host_allowed(&[], "example.com", 80));
        assert!(host_allowed(&[], "127.0.0.1", 1));
    }

    #[test]
    fn exact_host_and_port() {
        let a = vec!["github.com:443".to_string()];
        assert!(host_allowed(&a, "github.com", 443));
        assert!(!host_allowed(&a, "github.com", 80));
        assert!(!host_allowed(&a, "evil.com", 443));
    }

    #[test]
    fn bare_host_matches_any_port() {
        let a = vec!["example.com".to_string()];
        assert!(host_allowed(&a, "example.com", 80));
        assert!(host_allowed(&a, "example.com", 8443));
    }

    #[test]
    fn case_insensitive() {
        let a = vec!["GitHub.com:443".to_string()];
        assert!(host_allowed(&a, "github.com", 443));
    }

    #[test]
    fn wildcard_matches_subdomains_only() {
        let a = vec!["*.crates.io:443".to_string()];
        assert!(host_allowed(&a, "static.crates.io", 443));
        assert!(host_allowed(&a, "a.b.crates.io", 443));
        assert!(!host_allowed(&a, "crates.io", 443));
        assert!(!host_allowed(&a, "notcrates.io", 443));
        assert!(!host_allowed(&a, "evil.com.crates.io.attacker.net", 443));
    }

    #[test]
    fn wildcard_respects_boundary() {
        // `a.wild.com.evil` must not match `*.wild.com`.
        let a = vec!["*.wild.com".to_string()];
        assert!(!host_allowed(&a, "a.wild.com.evil", 80));
        assert!(host_allowed(&a, "a.wild.com", 80));
    }
}

// ---------------------------------------------------------------------------
// SOCKS5 CONNECT engine (R3c)
// ---------------------------------------------------------------------------

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};

/// SOCKS5 reply codes we emit. Only the handful that carry meaning.
const REP_SUCCEEDED: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_COMMAND_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;

/// What a `CONNECT` asked for, after parsing.
struct Request {
    host: String,
    port: u16,
}

/// Run one proxy conversation on `client`: decide which proxy protocol the
/// client is speaking and serve it. Blocks until the conversation is over.
///
/// # One port, two protocols
///
/// The listener speaks both SOCKS5 and HTTP-proxy on the same port, told apart
/// by the first byte the client sends (peeked, not consumed):
///
/// - `0x05` — the SOCKS5 greeting version, so [`serve_socks5`].
/// - anything else — an HTTP request (`C`/`G`/`P`/…), so [`serve_http_proxy`].
///
/// A single port is why the proxy variables can name one endpoint and satisfy
/// every client: SOCKS-aware tools get `socks5h://`, while `w3m`, `wget` and
/// anything else that speaks only HTTP-proxy get the same port over `http://`.
/// The two never collide, because a SOCKS5 greeting version byte is not an ASCII
/// letter.
fn serve_conversation(
    client: TcpStream,
    allow: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    // Peek so the byte is still there for whichever protocol takes over.
    let mut first = [0u8; 1];
    match client.peek(&mut first) {
        // Zero bytes means the client already hung up; nothing to do.
        Ok(0) => Ok(()),
        Ok(_) if first[0] == 0x05 => serve_socks5(client, allow, log),
        Ok(_) => serve_http_proxy(client, allow, log),
        Err(_) => Ok(()),
    }
}

/// Run one SOCKS5 conversation on `client`, splicing it to the destination the
/// allowlist permits. Blocks until both directions are done.
///
/// Denials are written to `log`, which is how a refusal becomes actionable: the
/// model and the operator both need to know *what* was refused, not just that
/// the connection failed. No auth is offered (`0x00` only) — prior runs one
/// bridge per tool call, so the call itself is the attribution unit.
fn serve_socks5(
    mut client: TcpStream,
    allow: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    client.set_nodelay(true).ok();

    // Greeting: version, n methods, then the methods.
    let mut head = [0u8; 2];
    client.read_exact(&mut head)?;
    if head[0] != 0x05 {
        return Ok(()); // not SOCKS5; nothing useful to say
    }
    let mut methods = vec![0u8; head[1] as usize];
    client.read_exact(&mut methods)?;
    // Reply: version, chosen method. `0x00` = no auth required.
    client.write_all(&[0x05, 0x00])?;

    let request = match read_request(&mut client) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // unsupported command/atyp, reply already sent
        Err(e) => return Err(e),
    };

    if !host_allowed(allow, &request.host, request.port) {
        log(&format!("DENY {}:{}", request.host, request.port));
        let _ = send_reply(&mut client, REP_NOT_ALLOWED);
        return Ok(());
    }

    // Resolve and dial *here*, in the supervisor — never in the sandbox. This
    // is why the sandbox sets `socks5h://`: it never learns an address it has
    // no route to, and DNS is governed by the same netns as the sandbox.
    let target = match resolve(&request.host, request.port) {
        Some(a) => a,
        None => {
            log(&format!("DENY-RESOLVE {}:{}", request.host, request.port));
            let _ = send_reply(&mut client, REP_HOST_UNREACHABLE);
            return Ok(());
        }
    };
    let upstream = match TcpStream::connect(target) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("FAIL {}:{} ({e})", request.host, request.port));
            let _ = send_reply(&mut client, REP_GENERAL_FAILURE);
            return Ok(());
        }
    };
    upstream.set_nodelay(true).ok();
    log(&format!("ALLOW {}:{}", request.host, request.port));
    send_reply(&mut client, REP_SUCCEEDED)?;
    splice(client, upstream);
    Ok(())
}

/// Parse the `CONNECT` request line and the address that follows it. Returns
/// `Ok(None)` when a well-formed reply has already been sent on a request we
/// cannot serve (a non-`CONNECT` command or an unknown address type).
fn read_request(client: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut head = [0u8; 4];
    client.read_exact(&mut head)?;
    // head = [VER, CMD, RSV, ATYP]
    let cmd = head[1];
    let atyp = head[3];

    // We must consume the address even for a bad command, or the reply is
    // written while unread bytes are still in flight.
    let host = match atyp {
        0x01 => {
            let mut o = [0u8; 4];
            client.read_exact(&mut o)?;
            Ipv4Addr::from(o).to_string()
        }
        0x04 => {
            let mut o = [0u8; 16];
            client.read_exact(&mut o)?;
            Ipv6Addr::from(o).to_string()
        }
        0x03 => {
            let mut l = [0u8; 1];
            client.read_exact(&mut l)?;
            let mut name = vec![0u8; l[0] as usize];
            client.read_exact(&mut name)?;
            String::from_utf8_lossy(&name).to_string()
        }
        _ => {
            let _ = send_reply(client, REP_ATYP_UNSUPPORTED);
            return Ok(None);
        }
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port)?;
    let port = u16::from_be_bytes(port);

    if cmd != 0x01 {
        // Only CONNECT. `UDP ASSOCIATE` (0x03) is deliberately unsupported: its
        // client support is near-nil, so implementing it would be a spec nobody
        // calls. See R3h in the audit doc.
        let _ = send_reply(client, REP_COMMAND_UNSUPPORTED);
        return Ok(None);
    }
    Ok(Some(Request { host, port }))
}

fn send_reply(client: &mut TcpStream, rep: u8) -> io::Result<()> {
    // BND.ADDR is all-zero: we do not bind a meaningful local endpoint.
    client.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
}

fn resolve(host: &str, port: u16) -> Option<SocketAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().ok()?.next()
}

/// Relay a `Connection: close` request/response pair. Unlike [`splice`] this does
/// not wait for the client to half-close: an HTTP client sends its request (and
/// any body) and then *waits*, keeping its socket open for the response, so
/// joining the client→upstream pump would deadlock. The client→upstream
/// direction is pumped on a detached thread that ends when `client` is dropped,
/// and the caller blocks only on the response direction, which terminates on the
/// upstream's EOF — the delimiter `Connection: close` guarantees.
fn splice_response(client: TcpStream, upstream: TcpStream) {
    if let (Ok(mut c_read), Ok(mut u_write)) = (client.try_clone(), upstream.try_clone()) {
        let _ = std::thread::Builder::new()
            .name("egress-req".into())
            .spawn(move || {
                let _ = io::copy(&mut c_read, &mut u_write);
                let _ = u_write.shutdown(std::net::Shutdown::Write);
            });
    }
    let mut c_write = client;
    let mut u_read = upstream;
    let _ = io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(std::net::Shutdown::Write);
}

/// Copy bytes in both directions until EOF. Runs the client→upstream direction
/// on a thread (both sides are blocking `std` sockets) and returns when both are
/// done, so a half-closed connection does not leak a thread per request.
fn splice(client: TcpStream, upstream: TcpStream) {
    let Ok(mut c_read) = client.try_clone() else {
        return;
    };
    let Ok(mut u_write) = upstream.try_clone() else {
        return;
    };
    let pump = std::thread::Builder::new()
        .name("egress-pump".into())
        .spawn(move || {
            let _ = io::copy(&mut c_read, &mut u_write);
            // Signal EOF to the destination without tearing down the other
            // direction, so a request/response protocol gets its full reply.
            let _ = u_write.shutdown(std::net::Shutdown::Write);
        });
    let mut c_write = client;
    let mut u_read = upstream;
    let _ = io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(std::net::Shutdown::Write);
    if let Ok(h) = pump {
        let _ = h.join();
    }
}

// ---------------------------------------------------------------------------
// HTTP-proxy engine (R3i)
// ---------------------------------------------------------------------------

/// Largest request head we will parse. A proxy request is a request line plus
/// headers; anything larger is abuse, not a request. 16 KiB is well above any
/// real head (browsers stay under 8 KiB) and small enough to bound memory per
/// connection.
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// Serve one HTTP-proxy conversation on `client`.
///
/// Two request forms matter, and they are handled differently:
///
/// - `CONNECT host:port HTTP/1.x` — a tunnel request, used by every client for
///   `https://`. We reply `200 Connection established` and then [`splice`]
///   raw bytes: no TLS involvement, no header rewriting, and **keep-alive is
///   preserved inside the tunnel** because we never look at what flows through.
///   This is the path `git`, `cargo`, `curl` and `wget` all take for HTTPS.
/// - `GET http://host/path HTTP/1.x` (absolute-form) — plain HTTP, the form
///   `w3m`/`wget`/old clients use for `http://`. We dial, rewrite the request
///   line to origin-form and force `Connection: close`, then splice to EOF.
///
/// ## Why absolute-form forces `Connection: close`
///
/// Splitting a response from the next one on a kept-alive connection needs the
/// body framing (`Content-Length` or chunked). Rather than implement an HTTP/1.1
/// message parser — a large, security-sensitive surface for no benefit — we ask
/// for one request per connection and use EOF as the delimiter, which is what
/// every CLI client here already does for plain HTTP. Measured cost: one extra
/// RTT per `http://` resource. HTTPS, the case that matters, is unaffected: it
/// goes through `CONNECT`, which stays keep-alive.
///
/// ## Routing decision is the request line, and only the request line
///
/// For absolute-form the authority in the request line decides the allowlist
/// answer, and a disagreeing `Host:` header is *ignored*, not honoured. Trusting
/// the header while dialling one destination and forwarding the other is a
/// request-smuggling vector: the decision and the dial must be the same value.
fn serve_http_proxy(
    mut client: TcpStream,
    allow: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    client.set_nodelay(true).ok();

    let head = match read_request_head(&mut client) {
        Ok(h) => h,
        Err(_) => {
            let _ = write_http_error(&mut client, 400, "Bad Request");
            return Ok(());
        }
    };
    let Some(line_end) = head.find("\r\n") else {
        let _ = write_http_error(&mut client, 400, "Bad Request");
        return Ok(());
    };
    let request_line = &head[..line_end];
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        let _ = write_http_error(&mut client, 400, "Bad Request");
        return Ok(());
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        return serve_http_connect(client, target, allow, log);
    }
    serve_http_forward(client, method, target, &head[line_end + 2..], allow, log)
}

/// The `CONNECT host:port` half: dial, acknowledge, splice. The client keeps
/// its own TLS handshake, so this is opaque to the bridge.
fn serve_http_connect(
    mut client: TcpStream,
    authority: &str,
    allow: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let Some((host, port)) = split_authority(authority, 443) else {
        let _ = write_http_error(&mut client, 400, "Bad Request");
        return Ok(());
    };
    if !host_allowed(allow, &host, port) {
        log(&format!("DENY {host}:{port}"));
        let _ = write_http_error(&mut client, 403, "Forbidden");
        return Ok(());
    }
    let Some(target) = resolve(&host, port) else {
        log(&format!("DENY-RESOLVE {host}:{port}"));
        let _ = write_http_error(&mut client, 502, "Bad Gateway");
        return Ok(());
    };
    let upstream = match TcpStream::connect(target) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("FAIL {host}:{port} ({e})"));
            let _ = write_http_error(&mut client, 502, "Bad Gateway");
            return Ok(());
        }
    };
    upstream.set_nodelay(true).ok();
    log(&format!("ALLOW {host}:{port}"));
    // The canonical CONNECT success line. The client treats everything after it
    // as tunnel bytes, so this must be flushed before the splice begins.
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    splice(client, upstream);
    Ok(())
}

/// The absolute-form `GET http://host/path` half: dial the request-line
/// authority, rewrite to origin-form, force `Connection: close`, splice.
fn serve_http_forward(
    mut client: TcpStream,
    method: &str,
    target: &str,
    rest_of_head: &str,
    allow: &[String],
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let Some(host_and_path) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))
    else {
        // https:// never arrives here (clients use CONNECT), and a bare
        // origin-form request means the client is not using us as a proxy.
        let _ = write_http_error(&mut client, 400, "Bad Request");
        return Ok(());
    };
    // Split authority from path at the first '/'. `w3m` sends a bare authority
    // for `http://host` (no trailing slash at all); normalise to `/`.
    let (authority, path) = match host_and_path.find('/') {
        Some(i) => (&host_and_path[..i], &host_and_path[i..]),
        None => (host_and_path, "/"),
    };
    let Some((host, port)) = split_authority(authority, 80) else {
        let _ = write_http_error(&mut client, 400, "Bad Request");
        return Ok(());
    };
    if !host_allowed(allow, &host, port) {
        log(&format!("DENY {host}:{port}"));
        let _ = write_http_error(&mut client, 403, "Forbidden");
        return Ok(());
    }
    let Some(addr) = resolve(&host, port) else {
        log(&format!("DENY-RESOLVE {host}:{port}"));
        let _ = write_http_error(&mut client, 502, "Bad Gateway");
        return Ok(());
    };
    let mut upstream = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("FAIL {host}:{port} ({e})"));
            let _ = write_http_error(&mut client, 502, "Bad Gateway");
            return Ok(());
        }
    };
    upstream.set_nodelay(true).ok();
    log(&format!("ALLOW {host}:{port}"));

    let rewritten = rewrite_forward_request(method, path, rest_of_head, &host, port);
    upstream.write_all(rewritten.as_bytes())?;
    splice_response(client, upstream);
    Ok(())
}

/// Rewrite an absolute-form request head into origin-form for the origin server.
///
/// - the request line loses the scheme and authority (`GET http://h/p` →
///   `GET /p`),
/// - `Connection:` is forced to `close` (see [`serve_http_forward`]),
/// - `Proxy-Connection:` is dropped: it is a client→proxy hop header, and
///   forwarding it to the origin would make the origin think it is the proxy.
fn rewrite_forward_request(
    method: &str,
    path: &str,
    rest_of_head: &str,
    host: &str,
    port: u16,
) -> String {
    let mut out = String::with_capacity(rest_of_head.len() + 64);
    out.push_str(method);
    out.push(' ');
    out.push_str(path);
    out.push_str(" HTTP/1.1\r\n");

    let mut saw_host = false;
    for line in rest_of_head.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (name, _) = match line.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let lname = name.trim();
        if lname.eq_ignore_ascii_case("connection")
            || lname.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        if lname.eq_ignore_ascii_case("host") {
            saw_host = true;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !saw_host {
        // The origin needs a `Host`, and the request-line authority is the
        // truthful one, so synthesise it from there rather than from a header we
        // deliberately do not trust.
        if port == 80 {
            out.push_str(&format!("Host: {host}\r\n"));
        } else {
            out.push_str(&format!("Host: {host}:{port}\r\n"));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

/// Split `host:port` / `[v6]:port` / bare `host`, applying `default_port`.
/// Returns `None` on an empty host or a malformed port.
fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.is_empty() {
        return None;
    }
    // `[::1]:8080` — bracketed IPv6.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_string(), port));
    }
    // A bare IPv6 literal without brackets cannot carry a port unambiguously;
    // treat the whole thing as the host.
    if authority.matches(':').count() > 1 {
        return Some((authority.to_string(), default_port));
    }
    match authority.split_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), port.parse().ok()?))
        }
        None => Some((authority.to_string(), default_port)),
    }
}

/// Read a request head up to and including the terminating `\r\n\r\n`.
///
/// Reads byte by byte from `client`, so nothing past the head is consumed — the
/// body (for a POST) and any pipelined bytes stay in the socket for [`splice`].
fn read_request_head(client: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        client.read_exact(&mut byte)?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_HEAD {
            return Err(io::Error::other("request head too large"));
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Write a minimal HTTP error response and close. Used for every refusal so the
/// client sees a status rather than a bare connection reset — `w3m` in
/// particular surfaces the status line to the user.
fn write_http_error(client: &mut TcpStream, code: u16, reason: &str) -> io::Result<()> {
    let body = format!("{code} {reason}\n");
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(resp.as_bytes())
}

// ---------------------------------------------------------------------------
// The shim (bwrap payload)
// ---------------------------------------------------------------------------

/// The shim's main. Runs **inside** the sandbox's network namespace as bwrap's
/// payload: it binds a listener on the namespace's loopback, passes the
/// listening fd back to prior, points the proxy variables at it, and `exec`s
/// the real command.
///
/// `argv` is `<shim> <proxy-var-pack> -- <program> [args…]`, where
/// `<proxy-var-pack>` is a `;`-separated list of variable names to set (e.g.
/// `ALL_PROXY;HTTPS_PROXY;HTTP_PROXY`). Everything after `--` is exec'd
/// verbatim, so this is transparent to the command.
///
/// The listener never survives the `exec`: it is closed before then (the fd
/// prior received stays alive in prior, not here), so the command cannot
/// accept on it.
pub fn shim_main(argv: &[String]) -> i32 {
    // R3d: die with bwrap. The shim is bwrap's direct child, so if bwrap is
    // killed (prior died, `--die-with-parent` fired, or the watchdog killed
    // it) the shim must not linger holding an in-namespace listener that will
    // never be used — it would keep the accept queue open in a namespace whose
    // supervisor is gone. `PR_SET_PDEATHSIG` delivers SIGKILL on parent death.
    // There is a race if the parent died before this call, which `--die-with-
    // parent` already covers; this is the belt-and-braces half.
    // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGKILL)` is a plain process attribute
    // set; no pointers are involved.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
    }

    let Some(sep) = argv.iter().position(|a| a == "--") else {
        eprintln!("{EGRESS_BIN}: expected `<vars> -- <program> [args…]`");
        return 2;
    };
    if sep + 1 >= argv.len() {
        eprintln!("{EGRESS_BIN}: no program to run");
        return 2;
    }
    let var_pack = &argv[1..sep];
    let program = &argv[sep + 1..];
    let specs: Vec<&str> = var_pack
        .iter()
        .flat_map(|s| s.split(';'))
        .filter(|s| !s.is_empty())
        .collect();

    // The bootstrap socketpair is inherited as fd 3 (see `bwrap_spawn_env`).
    // Read the number from the environment so a future change of plumbing does
    // not have to touch this function.
    let boot_fd: std::os::fd::RawFd = match std::env::var(EGRESS_FD_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(fd) => fd,
        None => {
            eprintln!("{EGRESS_BIN}: {EGRESS_FD_ENV} is not set to a valid fd");
            return 2;
        }
    };

    let listener = match bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{EGRESS_BIN}: cannot bind the in-namespace listener: {e}");
            return 2;
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("{EGRESS_BIN}: cannot read the bound port: {e}");
            return 2;
        }
    };
    use std::os::fd::AsRawFd;
    if let Err(e) = send_fd(boot_fd, listener.as_raw_fd(), b"L") {
        eprintln!("{EGRESS_BIN}: cannot hand the listener to prior: {e}");
        return 2;
    }
    // Close both copies in this process: prior owns the listener now, and a
    // second `accept` queue here would be an unowned, unmonitored one.
    drop(listener);
    // SAFETY: fd ownership passed to prior; closing our copy is a plain close.
    unsafe { libc::close(boot_fd) };

    // Each spec is `NAME=template`; `%p` is replaced with the bound
    // `127.0.0.1:<port>`. A template (rather than a fixed `scheme://` prefix) is
    // what lets the pack carry `NO_PROXY` alongside the proxy URLs, and lets the
    // scheme differ per variable — see [`proxy_var_specs`] for why it does.
    let endpoint = format!("127.0.0.1:{port}");
    for spec in specs {
        let Some((name, template)) = spec.split_once('=') else {
            eprintln!("{EGRESS_BIN}: malformed proxy var spec {spec:?} (want NAME=template)");
            return 2;
        };
        std::env::set_var(name, template.replace("%p", &endpoint));
    }

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&program[0])
        .args(&program[1..])
        .exec();
    // `exec` only returns on failure.
    eprintln!("{EGRESS_BIN}: cannot exec {}: {err}", program[0]);
    127
}

fn bind_loopback() -> io::Result<std::net::TcpListener> {
    // `lo` is already UP inside a fresh bwrap netns (measured), so no interface
    // setup is needed — this is the ~60 lines of `zerobox`'s
    // `ensure_loopback_interface_up` that prior does not duplicate.
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
}

// ---------------------------------------------------------------------------
// The supervisor accept loop
// ---------------------------------------------------------------------------

/// How many conversations the bridge serves at once before it starts refusing.
///
/// A cap exists so a sandbox cannot turn the supervisor into an unbounded thread
/// factory: every conversation is a thread plus two blocking `io::copy`s. The
/// number is deliberately far above what any CLI client opens (`git`'s
/// `http.maxRequests` defaults to 5, `curl`'s multi interface to 10) and far
/// below anything that would exhaust a normal process — and it only bites when a
/// client is already misbehaving.
const MAX_CONVERSATIONS: usize = 64;

/// Owns the bridge for a sandbox's lifetime: holds the received listening fd and
/// pumps proxy conversations until the command exits.
pub struct EgressBridge {
    allow: Vec<String>,
    /// Stops the accept loop. Dropped (or set) when the command finishes.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    log: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    /// Conversations accepted and not yet finished. Drives [`MAX_CONVERSATIONS`].
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl EgressBridge {
    /// Wrap an fd received from the shim and start pumping. The fd becomes a
    /// `TcpListener`; the accept loop runs on its own thread so the caller can
    /// go on to read the child's output.
    pub fn start(listener_fd: std::os::fd::RawFd, allow: Vec<String>) -> io::Result<EgressBridge> {
        use std::os::fd::FromRawFd;
        // SAFETY: `listener_fd` is a fresh listening socket received over
        // `SCM_RIGHTS` and owned by nobody else.
        let listener = unsafe { std::net::TcpListener::from_raw_fd(listener_fd) };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let log = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let thread = {
            let stop = stop.clone();
            let log = log.clone();
            let live = live.clone();
            let allow = allow.clone();
            let listener = listener.try_clone()?;
            // Non-blocking accept, polled against `stop`. The obvious
            // alternative — a blocking `accept` woken by connecting to the
            // listener's address — does not work here: the socket is bound
            // inside the *sandbox's* netns, so the supervisor (in the host
            // netns) cannot reach that address to wake it, and joining the
            // thread would hang forever at shutdown.
            listener.set_nonblocking(true)?;
            std::thread::Builder::new()
                .name("egress-bridge".into())
                .spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((client, _)) => {
                                // The accepted socket inherits non-blocking on
                                // Linux; the conversation wants blocking reads,
                                // so put it back.
                                let _ = client.set_nonblocking(false);

                                // Serve each conversation on its own thread.
                                // Inline service would serialise the bridge:
                                // HTTP clients (cargo, npm, git's
                                // `http.maxRequests`, browsers) open parallel
                                // connections as a matter of course, and one
                                // slow upstream would then stall every other
                                // request behind it.
                                let n = live.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                                if n > MAX_CONVERSATIONS {
                                    live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                                    log.lock()
                                        .push(format!("DENY concurrent > {MAX_CONVERSATIONS}"));
                                    // Dropping `client` RSTs the connection,
                                    // which is the right answer to a client
                                    // holding more sockets than we serve.
                                    continue;
                                }
                                let allow = allow.clone();
                                let log = log.clone();
                                let live_conn = live.clone();
                                let spawned = std::thread::Builder::new()
                                    .name("egress-conn".into())
                                    .spawn(move || {
                                        let mut sink =
                                            |line: &str| log.lock().push(line.to_string());
                                        // Each conversation is independent; a
                                        // failure in one is not fatal to the
                                        // bridge.
                                        let _ = serve_conversation(client, &allow, &mut sink);
                                        live_conn.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                                    });
                                if spawned.is_err() {
                                    live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                            Err(_) => break,
                        }
                    }
                })?
        };

        Ok(EgressBridge {
            allow,
            stop,
            thread: Some(thread),
            log,
            live,
        })
    }

    /// Stop accepting and join the accept thread. Safe to call more than once.
    ///
    /// The accept loop polls `stop`, so setting the flag is enough — see the
    /// comment at the loop for why a blocking `accept` cannot be woken across
    /// network namespaces.
    ///
    /// Per-connection threads are *not* joined. A conversation may be splices
    /// deep into a download, and blocking shutdown on it would turn "the command
    /// finished" into "wait for the slowest socket". Instead the accept thread is
    /// joined (so no new connections start) and then the live count is given a
    /// short grace period to drain, which is enough for the decisions log to be
    /// complete in every case that matters. Threads still running are left to
    /// the process's exit rather than to a blocking join.
    pub fn shutdown(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while self.live.load(std::sync::atomic::Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The `host:port` decisions the bridge made, in order. Used to surface
    /// denials; `allow` is `&mut` here only because the loop keeps the list.
    pub fn decisions(&self) -> Vec<String> {
        self.log.lock().clone()
    }

    /// The allowlist this bridge applies.
    pub fn allow(&self) -> &[String] {
        &self.allow
    }
}

impl Drop for EgressBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use std::net::TcpListener;

    /// A tiny upstream that answers with a fixed body, so a SOCKS conversation
    /// through the engine can be observed end to end without a network.
    fn echo_upstream() -> (SocketAddr, std::thread::JoinHandle<()>) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 4];
                let _ = s.read_exact(&mut buf);
                let _ = s.write_all(b"PONG");
            }
        });
        (addr, h)
    }

    /// A full no-auth CONNECT against a loopback upstream, with an empty
    /// allowlist: greeting, request, reply `0x00`, then spliced bytes.
    #[test]
    fn socks5_connect_splices_to_upstream() {
        let (upstream, up_thread) = echo_upstream();

        let proxy_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let allow: Vec<String> = vec![];
        let server = std::thread::spawn(move || {
            let (client, _) = proxy_listener.accept().unwrap();
            let mut log = |_s: &str| {};
            let _ = serve_socks5(client, &allow, &mut log);
        });

        let mut c = TcpStream::connect(proxy_addr).unwrap();
        // Greeting: version 5, one method, method 0x00.
        c.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut greeting = [0u8; 2];
        c.read_exact(&mut greeting).unwrap();
        assert_eq!(greeting, [0x05, 0x00]);

        // CONNECT to the upstream as an IPv4 literal.
        let IpAddr::V4(ip) = upstream.ip() else {
            unreachable!()
        };
        let mut req = vec![0x05, 0x01, 0x00, 0x01];
        req.extend_from_slice(&ip.octets());
        req.extend_from_slice(&upstream.port().to_be_bytes());
        c.write_all(&req).unwrap();

        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], REP_SUCCEEDED, "reply code");

        c.write_all(b"PING").unwrap();
        let mut body = [0u8; 4];
        c.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"PONG");

        // Close the client so the pump thread's client→upstream copy sees EOF;
        // otherwise the server side blocks joining it and the test hangs.
        drop(c);
        up_thread.join().unwrap();
        server.join().unwrap();
    }

    /// A denial must reply `0x02` and must not dial — proven by pointing at an
    /// upstream that would otherwise answer.
    #[test]
    fn socks5_denies_host_not_in_allowlist() {
        let proxy_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let allow = vec!["allowed.example:443".to_string()];
        let logs = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let server = {
            let logs = logs.clone();
            std::thread::spawn(move || {
                let (client, _) = proxy_listener.accept().unwrap();
                let mut sink = |line: &str| logs.lock().push(line.to_string());
                let _ = serve_socks5(client, &allow, &mut sink);
            })
        };

        let mut c = TcpStream::connect(proxy_addr).unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut greeting = [0u8; 2];
        c.read_exact(&mut greeting).unwrap();

        // A domain-type CONNECT to a host that is not listed.
        let host = b"evil.invalid";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&80u16.to_be_bytes());
        c.write_all(&req).unwrap();

        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], REP_NOT_ALLOWED);

        server.join().unwrap();
        assert_eq!(logs.lock().as_slice(), ["DENY evil.invalid:80"]);
    }

    /// `UDP ASSOCIATE` is refused rather than silently accepted.
    #[test]
    fn socks5_rejects_udp_associate() {
        let proxy_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (client, _) = proxy_listener.accept().unwrap();
            let mut sink = |_s: &str| {};
            let _ = serve_socks5(client, &[], &mut sink);
        });

        let mut c = TcpStream::connect(proxy_addr).unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut g = [0u8; 2];
        c.read_exact(&mut g).unwrap();
        // CMD 0x03 = UDP ASSOCIATE.
        c.write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], REP_COMMAND_UNSUPPORTED);
        server.join().unwrap();
    }

    /// Drive one full SOCKS5 conversation to `upstream` and return the bytes the
    /// far end echoed back. Used by the concurrency test below.
    fn socks5_roundtrip(proxy: SocketAddr, upstream: SocketAddr) -> String {
        let IpAddr::V4(ip) = upstream.ip() else {
            unreachable!()
        };
        let mut c = TcpStream::connect(proxy).unwrap();
        c.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut g = [0u8; 2];
        c.read_exact(&mut g).unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x01];
        req.extend_from_slice(&ip.octets());
        req.extend_from_slice(&upstream.port().to_be_bytes());
        c.write_all(&req).unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], REP_SUCCEEDED);
        c.write_all(b"PING").unwrap();
        let mut body = [0u8; 4];
        c.read_exact(&mut body).unwrap();
        String::from_utf8_lossy(&body).to_string()
    }

    /// The bridge must serve conversations **concurrently**. Serialising them
    /// would stall every HTTP client in the sandbox behind the slowest request:
    /// cargo, npm, git and browsers all open parallel connections by default.
    ///
    /// Four conversations against an origin that takes 300 ms each. Parallel
    /// finishes in about one delay; serial takes four. The bound is loose enough
    /// not to flake on a loaded machine and tight enough to fail on the old
    /// inline loop (measured: 2.46 s serial vs 0.63 s threaded).
    #[test]
    fn bridge_serves_conversations_concurrently() {
        const DELAY: std::time::Duration = std::time::Duration::from_millis(300);
        const N: usize = 4;

        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream = l.local_addr().unwrap();
        let origin = std::thread::spawn(move || {
            let mut handles = Vec::new();
            for _ in 0..N {
                let (mut s, _) = l.accept().unwrap();
                handles.push(std::thread::spawn(move || {
                    std::thread::sleep(DELAY);
                    let mut buf = [0u8; 4];
                    let _ = s.read_exact(&mut buf);
                    let _ = s.write_all(b"PONG");
                }));
            }
            for h in handles {
                let _ = h.join();
            }
        });

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        use std::os::fd::IntoRawFd;
        let mut bridge = EgressBridge::start(listener.into_raw_fd(), Vec::new()).unwrap();

        let started = std::time::Instant::now();
        let clients: Vec<_> = (0..N)
            .map(|_| std::thread::spawn(move || socks5_roundtrip(proxy, upstream)))
            .collect();
        for h in clients {
            assert_eq!(h.join().unwrap(), "PONG");
        }
        let elapsed = started.elapsed();
        bridge.shutdown();

        // Serial would be >= N * DELAY = 1.2 s. Parallel is ~1 delay; allow 2x
        // the delay for scheduling noise on a busy CI box.
        assert!(
            elapsed < DELAY * 2,
            "conversations were serialised: {N} x {DELAY:?} took {elapsed:?}"
        );
        origin.join().unwrap();
    }

    // ---- R3i: the HTTP-proxy front end ----

    /// Start a one-shot upstream that reads a request head and answers with a
    /// fixed body. Returns its address and a handle yielding the raw head it
    /// received, so a test can assert what the bridge actually forwarded.
    fn http_upstream(body: &'static str) -> (SocketAddr, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            // Read exactly the head; the client sends `Connection: close` with no
            // body, so this terminates cleanly.
            while s.read_exact(&mut b).is_ok() {
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = s.write_all(body.as_bytes());
            String::from_utf8_lossy(&head).to_string()
        });
        (addr, h)
    }

    /// Serve exactly one conversation through `serve_conversation` — the same
    /// entry point the accept loop uses, so the protocol sniff is exercised too.
    fn one_conversation(listener: TcpListener, allow: Vec<String>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (c, _) = listener.accept().unwrap();
            let mut sink = |_s: &str| {};
            let _ = serve_conversation(c, &allow, &mut sink);
        })
    }

    /// Absolute-form `GET` through the bridge: the request line is rewritten to
    /// origin-form, `Connection: close` forced, and the body relayed back.
    /// This is the `w3m`/`wget` plain-HTTP path.
    #[test]
    fn http_absolute_get_is_forwarded_and_rewritten() {
        let (upstream, up) = http_upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = one_conversation(listener, Vec::new());

        let mut c = TcpStream::connect(proxy).unwrap();
        write!(
            c,
            "GET http://127.0.0.1:{}/a/b?q=1 HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n",
            upstream.port()
        )
        .unwrap();

        let mut resp = Vec::new();
        c.read_to_end(&mut resp).unwrap();
        assert!(resp.ends_with(b"\r\n\r\nOK"), "body relayed: {resp:?}");

        server.join().unwrap();
        let head = up.join().unwrap();
        // Origin-form request line, no scheme or authority.
        assert!(
            head.starts_with("GET /a/b?q=1 HTTP/1.1\r\n"),
            "request line rewritten to origin-form: {head:?}"
        );
        // `Connection: close` forced regardless of what the client sent.
        assert!(
            head.to_lowercase().contains("connection: close"),
            "connection forced closed: {head:?}"
        );
        // The client's `HTTP/1.0` must not leave the client keep-alive guesswork
        // in place: we emit exactly one `Connection:` header.
        assert_eq!(head.to_lowercase().matches("connection:").count(), 1);
    }

    /// `CONNECT` through the bridge is an opaque tunnel: the bridge replies
    /// `200`, then relays bytes untouched. This is the `git https` path.
    #[test]
    fn http_connect_is_an_opaque_tunnel() {
        // Upstream echoes whatever it receives, standing in for a TLS peer.
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream = l.local_addr().unwrap();
        let up = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"HELO", "tunnel bytes arrive unmodified");
            let _ = s.write_all(b"BYTE!");
        });

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        let server = one_conversation(listener, Vec::new());

        let mut c = TcpStream::connect(proxy).unwrap();
        write!(
            c,
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            upstream.port()
        )
        .unwrap();

        // The success line, then everything is tunnel bytes.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while c.read_exact(&mut b).is_ok() {
            line.push(b[0]);
            if line.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let status = String::from_utf8_lossy(&line);
        assert!(
            status.starts_with("HTTP/1.1 200"),
            "CONNECT accepted: {status:?}"
        );

        c.write_all(b"HELO").unwrap();
        let mut reply = [0u8; 4];
        c.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"BYTE", "the tunnel is byte-transparent");
        drop(c);
        up.join().unwrap();
        server.join().unwrap();
    }

    /// A denied host is refused with a status line, decided on the request-line
    /// authority — and a disagreeing `Host:` header does not override it.
    #[test]
    fn http_proxy_denies_by_request_line_authority() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let proxy = listener.local_addr().unwrap();
        let allow = vec!["allowed.example:80".to_string()];
        let logs = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let server = {
            let logs = logs.clone();
            std::thread::spawn(move || {
                let (c, _) = listener.accept().unwrap();
                let mut sink = |line: &str| logs.lock().push(line.to_string());
                let _ = serve_conversation(c, &allow, &mut sink);
            })
        };

        let mut c = TcpStream::connect(proxy).unwrap();
        // Request line says evil.invalid; `Host:` lies and says allowed.example.
        // The allowlist must key on the request line.
        write!(
            c,
            "GET http://evil.invalid/ HTTP/1.0\r\nHost: allowed.example\r\n\r\n"
        )
        .unwrap();
        let mut resp = Vec::new();
        c.read_to_end(&mut resp).unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 403"),
            "must be denied by the request line, not the header: {text:?}"
        );
        server.join().unwrap();
        assert_eq!(logs.lock().as_slice(), ["DENY evil.invalid:80"]);
    }

    /// `split_authority` handles the shapes a request line can carry, including
    /// bracketed IPv6 and a default port.
    #[test]
    fn split_authority_cases() {
        assert_eq!(
            split_authority("example.com:8080", 80),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(
            split_authority("example.com", 80),
            Some(("example.com".into(), 80))
        );
        assert_eq!(split_authority("[::1]:443", 80), Some(("::1".into(), 443)));
        assert_eq!(split_authority("[::1]", 80), Some(("::1".into(), 80)));
        assert_eq!(split_authority("::1", 80), Some(("::1".into(), 80)));
        assert_eq!(split_authority("", 80), None);
        assert_eq!(split_authority(":80", 80), None);
        assert_eq!(split_authority("h:notaport", 80), None);
    }

    /// The published proxy spec list is the contract clients see. Assert the
    /// properties that make it work for both client families, not an exact
    /// string (the string is `proxy_var_specs`'s own business).
    #[test]
    fn proxy_var_specs_cover_both_client_families() {
        let specs = proxy_var_specs();
        let get = |name: &str| {
            specs
                .iter()
                .find_map(|s| s.strip_prefix(&format!("{name}=")).map(str::to_string))
        };
        // `ALL_PROXY` stays SOCKS: the only spelling that carries remote-DNS
        // (`socks5h`). Losing it would make curl/git resolve names in the sandbox.
        assert_eq!(get("ALL_PROXY").as_deref(), Some("socks5h://%p"));
        // The per-scheme variables must be `http://`: `w3m` cannot parse socks5h
        // and `wget` here honours only the lower-case spelling.
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            assert_eq!(
                get(name).as_deref(),
                Some("http://%p"),
                "{name} must be an http:// URL every client can parse"
            );
        }
        // `wget` reads only the lower case; its absence is silent egress loss.
        assert!(get("http_proxy").is_some());
        assert!(get("all_proxy").is_some());
    }
}

// ---------------------------------------------------------------------------
// Supervisor-side plumbing: the socketpair and the child fd
// ---------------------------------------------------------------------------

/// Create the bootstrap socketpair for one sandbox launch.
///
/// The returned `EgressPlumbing` owns the **parent** end (used to receive the
/// listening fd) and records the **child** end's number, which the shim reads
/// from [`EGRESS_FD_ENV`]. The child end is left inheritable (no `CLOEXEC`) so
/// it survives `exec` into bwrap; the parent end *is* `CLOEXEC`, so it does not
/// leak into later children.
///
/// The child end's fd number is the same in the child as in prior: the pair is
/// created before `duct` installs its stdio pipes, and `duct`/`std` do not
/// renumber inherited fds. That assumption is what lets the number be passed by
/// environment instead of needing an unstable `Command::pass_fds`.
pub struct EgressPlumbing {
    parent: Option<std::os::fd::RawFd>,
    child: Option<std::os::fd::RawFd>,
    allow: Vec<String>,
}

impl EgressPlumbing {
    /// Create the pair for a sandbox whose allowlist is `allow`.
    pub fn new(allow: Vec<String>) -> io::Result<EgressPlumbing> {
        let mut fds = [0 as std::os::fd::RawFd; 2];
        // SAFETY: `fds` is a valid two-element array for `socketpair`.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let (parent, child) = (fds[0], fds[1]);
        // Parent end must not leak into children; child end must survive exec.
        if let Err(e) = set_cloexec(parent, true).and_then(|()| set_cloexec(child, false)) {
            // SAFETY: we own both just-created descriptors.
            unsafe {
                libc::close(parent);
                libc::close(child);
            }
            return Err(e);
        }
        Ok(EgressPlumbing {
            parent: Some(parent),
            child: Some(child),
            allow,
        })
    }

    /// The fd number the shim must send its listener over (`EGRESS_FD_ENV`).
    pub fn child_fd(&self) -> Option<std::os::fd::RawFd> {
        self.child
    }

    /// Close the child end in the supervisor. Called after the spawn, once the
    /// child has inherited its copy — otherwise the supervisor keeps a second
    /// reference and `recv_fd` would never see EOF.
    pub fn close_child_end(&mut self) {
        if let Some(fd) = self.child.take() {
            // SAFETY: we own this fd and take it, so it is closed exactly once.
            unsafe { libc::close(fd) };
        }
    }

    /// Receive the listener from the shim and take ownership of the bridge.
    pub fn into_bridge(mut self) -> io::Result<EgressBridge> {
        let Some(parent) = self.parent else {
            return Err(io::Error::other("egress plumbing was already consumed"));
        };
        let mut buf = [0u8; 16];
        // A timeout keeps a wedged shim from hanging the whole tool call: if no
        // fd arrives in this window, the launch fails with a message rather than
        // blocking forever.
        set_rcvtimeo(parent, std::time::Duration::from_secs(30))?;
        let (_, fd) = recv_fd(parent, &mut buf)?;
        EgressBridge::start(fd, std::mem::take(&mut self.allow))
    }

    /// The raw parent fd, for tests that drive [`recv_fd`] directly.
    pub fn parent_fd(&self) -> Option<std::os::fd::RawFd> {
        self.parent
    }
}

impl Drop for EgressPlumbing {
    fn drop(&mut self) {
        // SAFETY: each descriptor is `take`n before closing, so it is closed at
        // most once even if `close_child_end` already ran.
        unsafe {
            if let Some(fd) = self.child.take() {
                libc::close(fd);
            }
            if let Some(fd) = self.parent.take() {
                libc::close(fd);
            }
        }
    }
}

fn set_cloexec(fd: std::os::fd::RawFd, on: bool) -> io::Result<()> {
    // SAFETY: `fd` is a valid descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let new = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: same descriptor, valid flags value.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, new) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_rcvtimeo(fd: std::os::fd::RawFd, d: std::time::Duration) -> io::Result<()> {
    let tv = libc::timeval {
        tv_sec: d.as_secs() as libc::time_t,
        tv_usec: d.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: `tv` outlives the call and `fd` is a valid socket.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Launch orchestration
// ---------------------------------------------------------------------------

/// Everything a launch needs to grant egress: the bootstrap plumbing and the
/// absolute path of the shim to bind in and run.
pub struct Launch {
    plumbing: EgressPlumbing,
    shim: std::path::PathBuf,
}

impl Launch {
    /// The shim binary to bind into the sandbox and run as the payload.
    pub fn shim(&self) -> &std::path::Path {
        &self.shim
    }

    /// The fd number the shim sends its listener over (for `EGRESS_FD_ENV`).
    pub fn child_fd(&self) -> Option<std::os::fd::RawFd> {
        self.plumbing.child_fd()
    }

    /// Close the supervisor's copy of the child end. Call after the child has
    /// been spawned (so it inherited its copy) and before [`Self::into_bridge`].
    pub fn close_child_end(&mut self) {
        self.plumbing.close_child_end();
    }

    /// Receive the listener and start pumping. Consumes the launch.
    pub fn into_bridge(self) -> io::Result<EgressBridge> {
        self.plumbing.into_bridge()
    }
}

/// Resolve the `prior-egress` shim to an absolute path.
///
/// Mirrors `bsed_path`: the binary is located next to the running executable
/// first (how `cargo install` and the build tree place it) and then on `PATH`.
/// It must be absolute because the sandbox has no `$PATH` and the bind is by
/// path.
pub fn resolve_shim() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(EGRESS_BIN);
            if candidate.is_file() {
                return Some(candidate);
            }
            // Under `cargo test` the running executable is
            // `target/<profile>/deps/<test-bin>`, while the binaries Cargo built
            // sit one level up in `target/<profile>`. Check there too, so the
            // end-to-end test exercises the shim the build just produced rather
            // than skipping. (Same layout note as `bsed_path`'s test plumbing.)
            if dir.ends_with("deps") {
                if let Some(profile) = dir.parent() {
                    let candidate = profile.join(EGRESS_BIN);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(EGRESS_BIN))
        .find(|c| c.is_file())
}

/// Prepare a launch for a sandbox with the given allowlist, or `None` when
/// egress is disabled.
///
/// A missing shim is a hard error rather than a silent fallback to "no egress":
/// `network = true` promises a route out, and failing to provide one while
/// claiming success would strand the command with a proxy variable pointing at
/// nothing.
pub fn prepare(enabled: bool, allow: Vec<String>) -> Result<Option<Launch>, String> {
    if !enabled {
        return Ok(None);
    }
    let shim = resolve_shim().ok_or_else(|| {
        format!(
            "network egress is enabled but the `{EGRESS_BIN}` shim was not found. \
             Install it with `cargo install --path .` (it ships alongside prior), or set \
             `network = false`."
        )
    })?;
    let plumbing = EgressPlumbing::new(allow).map_err(|e| format!("cannot set up egress: {e}"))?;
    Ok(Some(Launch { plumbing, shim }))
}
