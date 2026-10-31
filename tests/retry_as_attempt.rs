//! A mid-turn retry is a re-issued attempt, not a new dispatch.
//!
//! This is the whole point of driving `AgentRun` directly
//! (`doc/261030_rig_direct_drive.md` §5). Before it, a retryable failure after
//! progress re-prompted the agent with a synthetic `"continue"` (or the
//! corrective feedback text) as a *new user message*. Two things followed, both
//! bad and both visible to the model on every later turn:
//!
//!   1. the session grew a `user` message the human never typed;
//!   2. the model read that message as a request and replied to it conversationally,
//!      instead of continuing the work.
//!
//! Now the failed attempt never reached `streamed_turn`, so the run is still
//! awaiting the model and `next_step` re-emits the identical `CallModel`. The
//! retry consumes no turn and appends no message.
//!
//! The assertion is therefore about the **session file**: after a turn whose
//! second request 429s and then succeeds, exactly one user message exists (the
//! human's), and no `"continue"` appears anywhere in the transcript.

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

/// The back-off base, in ms. The retry path is what is under test, not the
/// schedule, so this keeps the wait short (`PRIOR_TEST_RETRY_BASE_MS`).
const BASE_MS: u64 = 100;

/// Request 1: text + a tool call (so the turn continues).
/// Request 2: 429 (a *mid-turn* failure — the retry case).
/// Request 3+: a final text stop.
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

    /// Whether any request body carried a `"continue"` user message. This is
    /// the litter the old retry loop injected; it must never appear on the wire.
    fn saw_continue(&self) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.contains("\"content\":\"continue\""))
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
        // Text + a tool call, so the batch runs and the turn continues.
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Let me check.\n"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\": \"a.rs\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        );
        return write_response(stream, "200 OK", "text/event-stream", "", sse.as_bytes());
    }
    if n == 2 {
        // The mid-turn failure: progress has been committed, so the old code
        // injected a synthetic "continue" here. The new code re-issues the same
        // attempt.
        let json = r#"{"error":{"message":"Rate limit exceeded: mock","code":429}}"#;
        return write_response(
            stream,
            "429 Too Many Requests",
            "application/json",
            "",
            json.as_bytes(),
        );
    }
    // The retried attempt succeeds and ends the turn.
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":"\n\nAll done.\n"},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
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
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
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
    let deadline = Instant::now() + Duration::from_secs(40);
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

/// Every `.jsonl` under the sessions dir, newest first.
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

/// Count `role: "user"` message entries in a session file.
fn user_message_texts(path: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "message" && v["message"]["role"] == "user")
        .map(|v| v["message"]["content"].to_string())
        .collect()
}

/// The regression this whole phase exists for: a retry after progress must not
/// put a synthetic user message in the transcript, and must not tell the model
/// to "continue" — it re-issues the attempt instead.
#[test]
fn a_mid_turn_retry_adds_no_user_message() {
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
    cmd.env("PRIOR_TEST_RETRY_BASE_MS", BASE_MS.to_string());

    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(10)),
        "banner never appeared"
    );
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"read the file\r");
    let _ = writer.flush();

    // Three requests: the tool turn, the 429, and the retried attempt.
    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let served = llm.count();
    assert!(
        served >= 3,
        "expected the 429 to be retried in place (3 requests), got {served}"
    );
    // Give the session write a moment to land.
    thread::sleep(Duration::from_millis(600));
    let _ = child.kill();
    let _ = child.wait();

    // The wire never carried a synthetic "continue".
    assert!(
        !llm.saw_continue(),
        "a synthetic \"continue\" message reached the provider — the retry \
         re-dispatched the prompt instead of re-issuing the attempt"
    );

    // And the session holds exactly the human's prompt, once.
    let sessions = config_tempdir.path().join("prior").join("sessions");
    let files = session_files(&sessions);
    let mut all_users: Vec<String> = Vec::new();
    for f in &files {
        all_users.extend(user_message_texts(f));
    }
    assert_eq!(
        all_users.len(),
        1,
        "expected exactly one user message in the session, got: {all_users:?}"
    );
    assert!(
        all_users[0].contains("read the file"),
        "the one user message must be the human's prompt: {all_users:?}"
    );
}
