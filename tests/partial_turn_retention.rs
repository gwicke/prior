//! A partial turn survives a mid-stream failure, end to end.
//!
//! `doc/261102_partial_turn_retention.md`: text the user watched arrive is
//! written to the session as an ordinary assistant message, a synthetic
//! *continuation* follows it (a pi `custom_message`, identifiable by entry type
//! rather than by matching text), and the retried request carries both — so the
//! model continues from what it said instead of restarting, and a reload shows
//! the opening the user actually saw.
//!
//! Driven through the real binary so the assertions are about the session file
//! and the wire, not about a mocked-out layer.

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
const BASE_MS: u64 = 100;

/// The text request 1 streams before the stream breaks.
const PARTIAL: &str = "I will start by reading the file";

struct MockLlm {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    /// Hold the first request's connection open after streaming the partial,
    /// instead of breaking it. That is how the abort path is reached: the turn
    /// has to still be in flight when `Esc` arrives, or the failure path gets
    /// there first and writes a continuation for a retry that never happens.
    // Read only by the server thread, which the compiler cannot see through
    // the closure — hence the allow rather than an unused-field warning.
    #[allow(dead_code)]
    hang: bool,
}

impl MockLlm {
    fn start() -> Self {
        Self::with_hang(false)
    }

    fn hanging() -> Self {
        Self::with_hang(true)
    }

    fn with_hang(hang: bool) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_c = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let requests = requests_c.clone();
                thread::spawn(move || {
                    let _ = serve(&mut stream, &requests, hang);
                });
            }
        });
        MockLlm {
            port,
            requests,
            hang,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Bodies of the requests after the first, i.e. the retried attempt.
    fn retried_bodies(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .skip(1)
            .cloned()
            .collect()
    }
}

fn serve(
    stream: &mut std::net::TcpStream,
    requests: &Arc<Mutex<Vec<String>>>,
    hang: bool,
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
        // Stream some text, then either break the connection mid-flight (a
        // transport failure after content has been rendered — the case the
        // retention design exists for) or hold it open, which is how the abort
        // path is reached while the turn is still in flight.
        let first = format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": "test",
                "choices": [{
                    "index": 0,
                    "delta": { "content": PARTIAL },
                    "finish_reason": serde_json::Value::Null,
                }],
            })
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            first.len() + 4096
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(first.as_bytes())?;
        stream.flush()?;
        if hang {
            // Keep the body incomplete and the connection open: the turn stays
            // in flight until the user aborts it.
            thread::sleep(Duration::from_secs(20));
            return Ok(());
        }
        // Declared length not delivered: the client sees a broken body.
        return Ok(());
    }

    // The retried attempt completes.
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":" and now I have finished."},"finish_reason":null}]}"#,
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

/// Every entry of the newest session file, in append order.
fn session_entries(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Messages of the active branch, in root→leaf order, excluding pointer rows.
fn branch_messages(entries: &[serde_json::Value]) -> Vec<(String, serde_json::Value)> {
    entries
        .iter()
        .filter(|e| e["type"] == "message" || e["type"] == "custom_message")
        .map(|e| {
            let kind = e["type"].as_str().unwrap_or_default().to_string();
            (kind, e.clone())
        })
        .collect()
}

#[test]
fn a_failed_partial_turn_is_persisted_and_replayed() {
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

    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        llm.count() >= 2,
        "the broken stream was not retried ({} requests)",
        llm.count()
    );
    thread::sleep(Duration::from_millis(800));
    let _ = child.kill();
    let _ = child.wait();

    // The partial the user watched reached the session, as an ordinary
    // assistant message — not as an annotation, and not dropped.
    let sessions = config_tempdir.path().join("prior").join("sessions");
    let files = session_files(&sessions);
    assert!(!files.is_empty(), "no session file was written");
    let entries: Vec<serde_json::Value> = files.iter().flat_map(|f| session_entries(f)).collect();
    let branch = branch_messages(&entries);

    let partial_index = branch
        .iter()
        .position(|(kind, e)| {
            kind == "message"
                && e["message"]["role"] == "assistant"
                && e["message"]["content"].to_string().contains(PARTIAL)
        })
        .unwrap_or_else(|| {
            panic!(
                "the partial text never reached the session; branch was: {:#?}",
                branch
                    .iter()
                    .map(|(k, e)| (k.clone(), e.to_string()))
                    .collect::<Vec<_>>()
            )
        });

    // And it is followed by exactly one synthetic continuation, identifiable by
    // entry type rather than by its text.
    let after = &branch[partial_index + 1..];
    let continuations = after
        .iter()
        .filter(|(kind, e)| kind == "custom_message" && e["customType"] == "prior_continue")
        .count();
    assert_eq!(
        continuations, 1,
        "exactly one synthetic continuation follows the partial"
    );

    // The retried request carried both, in order: the model continues from its
    // own text instead of restarting, and the nudge is the last thing it sees.
    let retried = llm.retried_bodies();
    let body = retried.first().expect("a retried request");
    let partial_at = body
        .find(PARTIAL)
        .unwrap_or_else(|| panic!("the retried request lost the partial text: {body}"));
    let continue_at = body
        .find("continue")
        .unwrap_or_else(|| panic!("the retried request lost the continuation: {body}"));
    assert!(
        partial_at < continue_at,
        "the continuation must follow the partial"
    );
}

/// The abort half of §7: an `<Esc>` mid-stream persists the text the user
/// watched, and stops there.
///
/// No continuation is written, because there is no retry to continue into — the
/// turn is over. The partial simply ends the session, and the next typed prompt
/// sees `[assistant] [user]`, which every provider accepts without a prefill.
#[test]
fn an_aborted_partial_turn_is_persisted_without_a_continuation() {
    let llm = MockLlm::hanging();
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

    // Wait until the partial text has been painted, then abort.
    // Wait until the request is in flight (the tool-less turn as server sees
    // it), then abort. Painting may lag the wire; what must hold is that the
    // turn was cut short mid-flight.
    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    thread::sleep(Duration::from_millis(600));
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    assert!(
        wait_for(&bytes, "Aborted.", Duration::from_secs(10)),
        "Esc did not abort the turn"
    );
    thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();

    let sessions = config_tempdir.path().join("prior").join("sessions");
    let files = session_files(&sessions);
    assert!(!files.is_empty(), "no session file was written");
    let entries: Vec<serde_json::Value> = files.iter().flat_map(|f| session_entries(f)).collect();
    let branch = branch_messages(&entries);

    assert!(
        branch.iter().any(|(kind, e)| kind == "message"
            && e["message"]["role"] == "assistant"
            && e["message"]["content"].to_string().contains(PARTIAL)),
        "the partial text the user watched must be persisted: {branch:#?}"
    );
    assert!(
        !branch
            .iter()
            .any(|(kind, e)| kind == "custom_message" && e["customType"] == "prior_continue"),
        "an aborted turn writes no continuation — nothing will ask the model to continue"
    );
}

/// §7's REPL behaviour change: returning with nothing typed resumes an
/// unfinished turn, along the same path a retry takes.
///
/// The point of the assertion is that the resumed request *continues* — it
/// carries the partial text and the synthetic continuation — rather than
/// re-sending an empty prompt or restarting from scratch.
#[test]
fn an_empty_submit_resumes_the_unfinished_turn() {
    let llm = MockLlm::hanging();
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

    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    thread::sleep(Duration::from_millis(600));
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    assert!(
        wait_for(&bytes, "Aborted.", Duration::from_secs(10)),
        "Esc did not abort the turn"
    );
    thread::sleep(Duration::from_millis(300));

    // Return with nothing typed: a resume, not an empty prompt.
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    let deadline = Instant::now() + Duration::from_secs(20);
    while llm.count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let served = llm.count();
    thread::sleep(Duration::from_millis(400));
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        served >= 2,
        "the empty submit did not resume the turn ({served} requests)"
    );
    let body = llm
        .retried_bodies()
        .into_iter()
        .next()
        .expect("a resumed request");
    assert!(
        body.contains(PARTIAL),
        "the resumed request must carry the partial text: {body}"
    );
    assert!(
        body.contains("continue"),
        "the resumed request must end on the continuation: {body}"
    );
}
