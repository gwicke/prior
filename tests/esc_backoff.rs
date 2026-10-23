//! Esc must cut a retry back-off short — the turn-scoped key pump.
//!
//! Scenario: the first request succeeds (text + tool call), the second
//! answers 429 with `Retry-After: 30`, so the turn enters a 30-second
//! back-off while the in-flight reader keeps feeding the turn's key queue.
//! Two things must happen *during* that window, both of which are impossible
//! without the pump (the queue was only read inside `prompt()`, which has
//! returned between attempts — so an Esc sat unread and the wait ran its
//! full course: the reported "the session only aborts once the timeout has
//! elapsed"):
//!
//!   1. a typed key dispatches — it settles into the input row;
//!   2. a root `Esc` fires the cancel signal — `Aborted.` lands within
//!      seconds, not after the 30s floor.
//!
//! Both assertions are deadline-based, and the back-off floor (30s) sits
//! far above them, so the test is red without the pump and green with it.

mod child_env;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use child_env::clear_proxy_vars;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 40;
const COLS: u16 = 120;
/// The `Retry-After` floor the mock reports. The pump's deadlines must sit
/// far below it (so an unpumped wait is unmistakably late) and far above
/// normal test latency (so the assertions aren't racy).
const BACKOFF_SECS: u64 = 30;

/// Tiny HTTP/1.1 server speaking OpenAI-compatible SSE chat completions.
///
/// Request 1: text + a `bash` tool call (so the turn continues and makes a
/// second request). Request 2: 429 + `Retry-After` (back-off arm). Request
/// 3+: a final text stop (only reached if the test fails to cancel).
struct MockLlm {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockLlm {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_c = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let requests = requests_c.clone();
                thread::spawn(move || {
                    let _ = serve(&mut stream, &requests);
                });
            }
        });
        MockLlm { port, requests }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn serve(
    stream: &mut std::net::TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (head, body) = read_http_request(stream)?;
    let body_str = String::from_utf8_lossy(&body).to_string();
    let is_chat = head.starts_with("POST /v1/chat/completions");

    if head.starts_with("GET /v1/models") || head.starts_with("GET /health") {
        let json = r#"{"object":"list","data":[{"id":"test","object":"model","owned_by":"mock"}]}"#;
        return write_response(stream, "200 OK", "application/json", "", json.as_bytes());
    }
    if !is_chat {
        return write_response(stream, "404 Not Found", "text/plain", "", b"nope");
    }

    let n = {
        let mut v = requests.lock().unwrap();
        v.push(body_str.clone());
        v.len()
    };

    if n == 1 {
        // Text + a bash tool call: the turn continues and issues request 2.
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Let me run a command.\n"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\": \"echo hi\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        );
        return write_response(stream, "200 OK", "text/event-stream", "", sse.as_bytes());
    }
    if n == 2 {
        // The back-off arm: 429 with the provider hint the delay floors on.
        let json = r#"{"error":{"message":"Rate limit exceeded: mock","code":429}}"#;
        return write_response(
            stream,
            "429 Too Many Requests",
            "application/json",
            &format!("Retry-After: {BACKOFF_SECS}\r\n"),
            json.as_bytes(),
        );
    }
    // Reached only when the test fails to cancel: serve a final answer so
    // the process has something to do after the 30s floor.
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    );
    write_response(stream, "200 OK", "text/event-stream", "", sse.as_bytes())
}

fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 1024 * 1024 {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut body = buf[header_end + 4..].to_vec();
    let clen: usize = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length:")
                .or_else(|| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    while body.len() < clen {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Ok((head, body))
}

fn write_response(
    stream: &mut std::net::TcpStream,
    status: &str,
    ctype: &str,
    extra_headers: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> (Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let bytes_c = bytes.clone();
    let stop_c = stop.clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            if stop_c.load(Ordering::SeqCst) || Instant::now() > deadline {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes_c.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });
    (bytes, stop)
}

fn wait_for(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if bytes
            .lock()
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
        {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn tail(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
    let data = bytes.lock().unwrap();
    String::from_utf8_lossy(&data[data.len().saturating_sub(3000)..]).to_string()
}

#[test]
fn esc_cuts_retry_backoff_and_keys_dispatch_during_it() {
    let llm = MockLlm::start();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    clear_proxy_vars(&mut cmd);
    cmd.args(["-p", "llamacpp", "-m", "test", "--yolo", "true"]);
    // Keep the test off the developer's real config (and its sessions).
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    let started = wait_for(&bytes, "prior -", Duration::from_secs(10));
    assert!(started, "banner never appeared:\n{}", tail(&bytes));
    // Answer the DSR cursor-position query, same handshake as mock_turn.
    let _ = writer.write_all(format!("\x1b[{ROWS};1R").as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));

    // Submit a prompt: text + tool call → second request → 429 + back-off.
    let _ = writer.write_all(b"go\r");
    let _ = writer.flush();

    let streamed = wait_for(&bytes, "Let me run a command", Duration::from_secs(15));
    assert!(streamed, "first response never streamed:\n{}", tail(&bytes));

    let deadline = Instant::now() + Duration::from_secs(15);
    while llm.count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        llm.count() >= 2,
        "the 429 was never served:\n{}",
        tail(&bytes)
    );

    // The back-off message pins the Retry-After floor: everything below
    // happens inside a 30s wait that must NOT be waited out.
    let armed = wait_for(
        &bytes,
        &format!("Retrying in {BACKOFF_SECS}s"),
        Duration::from_secs(5),
    );
    assert!(armed, "back-off never announced:\n{}", tail(&bytes));

    // 1. A plain key typed during the back-off must dispatch (settle into
    //    the input row). Without the pump it sits in the queue until the
    //    next attempt at t+30s and never appears within the deadline.
    let _ = writer.write_all(b"zzq");
    let _ = writer.flush();
    let dispatched = wait_for(&bytes, "zzq", Duration::from_secs(4));
    assert!(
        dispatched,
        "a key typed during the back-off was not dispatched within 4s \
         (the queue is unread between attempts):\n{}",
        tail(&bytes)
    );

    // 2. A root Esc must fire the cancel signal now, not at t+30s.
    let esc_at = Instant::now();
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    let aborted = wait_for(&bytes, "Aborted.", Duration::from_secs(6));
    assert!(
        aborted,
        "Esc did not cut the {BACKOFF_SECS}s back-off short within 6s \
         (elapsed {:?}):\n{}",
        esc_at.elapsed(),
        tail(&bytes)
    );

    let _ = child.kill();
    let _ = child.wait();
}
