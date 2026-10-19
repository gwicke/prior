//! The compaction window's key pump, end to end: an `Esc` typed while the
//! LLM summarization runs (requests *and* retry back-offs) must cancel it
//! promptly, and keys the window cannot act on must be parked and delivered
//! to `read_input` afterwards — not dropped, not leaked into `$EDITOR`.
//!
//! Flow: a text-only turn gives the session a leaf → `/compact` redirects to
//! `/tree <leaf> compact ` → `prompt` picks the LLM mode → `prepare()`
//! generates → the mock's second request answers 429 + `Retry-After: 30` →
//! a 30s back-off opens. During it we type `zzq` (must park) and press `Esc`
//! (must cancel within seconds — without the pump nothing reads stdin and
//! the 30s floor always runs). After the cancel, `read_input` must drain the
//! parked key: `zzq` appears in the input row.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 40;
const COLS: u16 = 120;
const BACKOFF_SECS: u64 = 30;

/// Mock OpenAI-compatible SSE server. Request 1: the opening turn
/// (text-only, so the turn ends and the session gains a leaf). Request 2:
/// 429 + `Retry-After` (the back-off arm). Later requests: a final answer,
/// reached only on the unpumped (red) path after the floor elapses.
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
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"All clear."},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        );
        return write_response(stream, "200 OK", "text/event-stream", "", sse.as_bytes());
    }
    if n == 2 {
        let json = r#"{"error":{"message":"Rate limit exceeded: mock","code":429}}"#;
        return write_response(
            stream,
            "429 Too Many Requests",
            "application/json",
            &format!("Retry-After: {BACKOFF_SECS}\r\n"),
            json.as_bytes(),
        );
    }
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

fn contains(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str) -> bool {
    bytes
        .lock()
        .unwrap()
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn esc_cancels_compaction_backoff_and_parked_keys_reach_read_input() {
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
    cmd.args(["-p", "llamacpp", "-m", "test", "--yolo", "true"]);
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    // The unpumped (red) path reaches $EDITOR after the floor; keep it inert.
    cmd.env("EDITOR", "/bin/true");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    let started = wait_for(&bytes, "prior -", Duration::from_secs(10));
    assert!(started, "banner never appeared:\n{}", tail(&bytes));
    let _ = writer.write_all(format!("\x1b[{ROWS};1R").as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));

    // Turn 1: a text-only answer — ends the turn and gives the session a
    // leaf for `/compact` to resolve.
    let _ = writer.write_all(b"go\r");
    let _ = writer.flush();
    let answered = wait_for(&bytes, "All clear.", Duration::from_secs(15));
    assert!(
        answered,
        "the opening turn never answered:\n{}",
        tail(&bytes)
    );
    // Let the loop return to `read_input` with the new leaf in the cycle view.
    thread::sleep(Duration::from_millis(1200));

    // /compact → redirect to /tree <leaf> compact → the modes listing.
    let _ = writer.write_all(b"/compact\r");
    let _ = writer.flush();
    let modes = wait_for(&bytes, "text-only", Duration::from_secs(5));
    assert!(
        modes,
        "the compact modes listing never appeared:\n{}",
        tail(&bytes)
    );

    // Pick the LLM mode: the path names the node, and the node shows its
    // confirm header ("Enter generates + opens $EDITOR...") — the second
    // Enter runs prepare() → generate() → request 2 → 429 → the back-off.
    let _ = writer.write_all(b"prompt\r");
    let _ = writer.flush();
    let header = wait_for(&bytes, "Enter generates", Duration::from_secs(5));
    assert!(
        header,
        "the compact confirm header never appeared:\n{}",
        tail(&bytes)
    );
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    let deadline = Instant::now() + Duration::from_secs(15);
    while llm.count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        llm.count() >= 2,
        "the summarization request never went out (nav failed?):\n{}",
        tail(&bytes)
    );
    let armed = wait_for(
        &bytes,
        &format!("Retrying in {BACKOFF_SECS}s"),
        Duration::from_secs(5),
    );
    assert!(
        armed,
        "the compaction back-off never announced:\n{}",
        tail(&bytes)
    );

    // A key typed during the window: nobody can dispatch it now (the REPL
    // is inside `settle`), so the pump parks it for `read_input`.
    let _ = writer.write_all(b"zzq");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(300));
    assert!(
        !contains(&bytes, "zzq"),
        "a parked key must not paint during the window"
    );

    // The Esc: cancel the in-flight window now, not after the 30s floor.
    let esc_at = Instant::now();
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    let cancelled = wait_for(&bytes, "Summary failed", Duration::from_secs(6));
    assert!(
        cancelled,
        "Esc did not cancel the compaction back-off within 6s \
         (elapsed {:?}):\n{}",
        esc_at.elapsed(),
        tail(&bytes)
    );

    // The parked key lands through `read_input`'s normal dispatch — after
    // the window, in the input row.
    let drained = wait_for(&bytes, "zzq", Duration::from_secs(5));
    assert!(
        drained,
        "the parked key never reached read_input:\n{}",
        tail(&bytes)
    );

    let _ = child.kill();
    let _ = child.wait();
}
