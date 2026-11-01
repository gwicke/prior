//! A mid-turn submit is a steer: it becomes a real user message at the next
//! call boundary.
//!
//! The design is `doc/261030_rig_direct_drive.md` §5. The properties this pins,
//! each of which was a wrong answer at some point:
//!
//!   1. the steer reaches the **wire** as an ordinary user message, not as a
//!      context document and not as a `RequestPatch::history` — so a reload
//!      still sees it and compaction keeps it;
//!   2. it is delivered at a **boundary**, i.e. after the tool batch's results,
//!      never between an assistant turn and the results that answer it;
//!   3. it reaches the **session** as a `type: "message"` entry with
//!      `role: "user"` — the same shape a typed prompt has, so nothing
//!      downstream has to learn that steering exists.
//!
//! The timing is deterministic rather than racy: the first response asks for a
//! slow `bash` command, so the steer is typed while a tool runs and the only
//! request it can enter is the one after the batch.

mod child_env;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use child_env::clear_proxy_vars;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 40;
const COLS: u16 = 120;

/// What the user types mid-turn. Distinctive enough that a substring match in a
/// request body is unambiguous.
const STEER: &str = "also check the readme";

/// How long the first response's `bash` call sleeps, in seconds. Long enough to
/// type into, short enough not to slow the suite.
const TOOL_SLEEP: u64 = 4;

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

    /// Bodies of every chat request, in arrival order.
    fn bodies(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(
    stream: &mut std::net::TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (head, body) = read_http_request(stream)?;
    let body_str = String::from_utf8_lossy(&body).to_string();

    if head.starts_with("GET /v1/models") || head.starts_with("GET /health") {
        let json = r#"{"object":"list","data":[{"id":"test","object":"model","owned_by":"mock"}]}"#;
        return write_response(stream, "200 OK", "application/json", json.as_bytes());
    }
    if !head.starts_with("POST /v1/chat/completions") {
        return write_response(stream, "404 Not Found", "text/plain", b"nope");
    }

    let n = {
        let mut v = requests.lock().unwrap();
        v.push(body_str.clone());
        v.len()
    };

    if n == 1 {
        // A slow tool, so the steer has a window to be typed in.
        let tool_call = format!(
            r#"{{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_1","type":"function","function":{{"name":"bash","arguments":"{{\"command\": \"sleep {TOOL_SLEEP}\"}}"}}}}]}},"finish_reason":null}}]}}"#
        );
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Checking.\n"},"finish_reason":null}]}"#,
            tool_call,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        );
        return write_response(stream, "200 OK", "text/event-stream", sse.as_bytes());
    }

    // Later requests: answer and stop. Request 2 is the one the steer should
    // have entered — the batch's results are followed by exactly one boundary.
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":"Done.\n"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    );
    write_response(stream, "200 OK", "text/event-stream", sse.as_bytes())
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
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

fn session_files(sessions: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(sessions, &mut out);
    out
}

#[test]
fn a_mid_turn_submit_is_delivered_at_the_next_boundary() {
    let llm = MockLlm::start();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    clear_proxy_vars(&mut cmd);
    cmd.args(["-p", "llamacpp", "-m", "test", "--yolo", "true"]);
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());

    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(10)),
        "banner never appeared"
    );
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"sleep, then tell me\r");
    let _ = writer.flush();

    // The tool call arrived and its command is running: this is the window a
    // user would type into. One request has been served and the second has not
    // gone out, because the batch is still executing.
    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(llm.count(), 1, "the first request never reached the mock");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        llm.count(),
        1,
        "the slow tool should hold the turn open; the next request must not \
         have gone out yet"
    );

    // The steer. Typed one byte at a time so it goes through the same edit path
    // a user's keystrokes do, then submitted with `\r`.
    for b in STEER.as_bytes() {
        let _ = writer.write_all(&[*b]);
        let _ = writer.flush();
    }
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();

    // Request 2 is the boundary the steer belongs to.
    let deadline = Instant::now() + Duration::from_secs(25);
    while llm.count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let served = llm.count();
    thread::sleep(Duration::from_millis(400));
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, Ordering::SeqCst);

    assert!(
        served >= 2,
        "expected the turn to continue past the tool batch, got {served} requests"
    );

    let bodies = llm.bodies();
    let second = &bodies[1];
    assert!(
        second.contains(STEER),
        "the steer never reached the wire; request 2 was:\n{second}"
    );
    // Delivered *after* the batch, which on the wire means the tool result
    // precedes it in the same request.
    let tool_result = second.find("\"role\":\"tool\"");
    let steer_at = second.find(STEER);
    assert!(
        matches!((tool_result, steer_at), (Some(t), Some(s)) if t < s),
        "the steer must follow the batch's results, not precede them: {second}"
    );
    // It is a user message, not a document or a synthetic marker.
    assert!(
        second.contains("\"role\":\"user\""),
        "the steer must arrive as a user message: {second}"
    );

    // And it is in the session as an ordinary user `message` entry — the shape a
    // reload and a compaction both understand, with nothing marking it as a
    // steer.
    let sessions = config_tempdir.path().join("prior").join("sessions");
    let files = session_files(&sessions);
    assert!(!files.is_empty(), "no session file was written");
    let mut steers = 0;
    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v["type"] == "message" && v["message"]["role"] == "user" {
                let text = v["message"]["content"].to_string();
                if text.contains(STEER) {
                    steers += 1;
                }
            }
        }
    }
    assert_eq!(
        steers, 1,
        "the steer must be written to the session exactly once, as a user message"
    );
}
