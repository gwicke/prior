//! End-to-end blank-line layout probe.
//!
//! Drives the REAL `picocode` binary against a local mock OpenAI-compatible
//! SSE server (via the `llamacpp` provider, which needs no API key and honors
//! `LLAMACPP_API_BASE_URL`), inside a real pty, then renders the captured byte
//! stream through `vt100` and prints the visible screen. Used to observe the
//! exact number of empty lines between the last message of a turn and the
//! horizontal separator — including the error path (429 on the second request).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use vt100::Parser;

const ROWS: u16 = 40;
const COLS: u16 = 120;

/// Tiny HTTP/1.1 server speaking OpenAI-compatible SSE chat completions.
/// Scenario: 0 = text→tool→final-text, 1 = text→tool→empty-stop,
/// 2 = text-only (single turn), 3 = 429 on the second request.
struct MockLlm {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockLlm {
    fn start(scenario: u32) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_c = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let requests = requests_c.clone();
                let scenario = scenario;
                thread::spawn(move || {
                    let _ = serve(&mut stream, &requests, scenario);
                });
            }
        });
        MockLlm {
            port,
            requests,
        }
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
    scenario: u32,
) -> std::io::Result<()> {
    let (head, body) = read_http_request(stream)?;
    let body_str = String::from_utf8_lossy(&body).to_string();
    let is_chat = head.starts_with("POST /v1/chat/completions");

    if head.starts_with("GET /v1/models") || head.starts_with("GET /health") {
        let json = r#"{"object":"list","data":[{"id":"test","object":"model","owned_by":"mock"}]}"#;
        return write_response(stream, "200 OK", "application/json", json.as_bytes());
    }
    if !is_chat {
        return write_response(stream, "404 Not Found", "text/plain", b"nope");
    }

    let n = {
        let mut v = requests.lock().unwrap();
        v.push(body_str.clone());
        v.len()
    };
    let has_tool_result = body_str.contains("\"role\":\"tool\"");

    if scenario == 3 && n >= 2 {
        let json = r#"{"error":{"message":"Rate limit exceeded: mock","code":429}}"#;
        return write_response(stream, "429 Too Many Requests", "application/json", json.as_bytes());
    }

    // Scenario 2: single text-only turn, no tool call.
    if scenario == 2 {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Just a plain text answer"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        );
        return write_response(stream, "200 OK", "text/event-stream", sse.as_bytes());
    }

    let sse = if has_tool_result {
        if scenario == 1 {
            // Empty stop: turn ends right after the tool result.
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            )
        } else {
            // Final plain-text answer, NO trailing newline.
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":"2. sleep 30 — done"},"finish_reason":null}]}"#,
                r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            )
        }
    } else {
        // First turn: some text + a bash tool call.
        format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Let me run a command.\n"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\": \"echo '2. sleep 30 — done'\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        )
    };
    write_response(stream, "200 OK", "text/event-stream", sse.as_bytes())
}

fn read_http_request(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<(String, Vec<u8>)> {
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
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(buf.len());
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
    let deadline = Instant::now() + Duration::from_secs(30);
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

/// Run one full turn against the mock and print the vt100 screen.
/// Returns the raw byte stream for analysis. When `thinking` is set, the agent
/// runs with `default_thinking_level = "high"` (answer-hold / reasoning path).
fn run_turn(scenario: u32, label: &str, thinking: bool) -> Vec<u8> {
    let llm = MockLlm::start(scenario);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
    let mut args: Vec<String> = vec!["-p".into(), "llamacpp".into(), "-m".into(), "test".into(), "--yolo".into(), "true".into()];
    let _cfg_keep;
    let _config_tempdir;
    if thinking {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("picocode.toml");
        std::fs::write(&cfg, "default_thinking_level = \"high\"\n").unwrap();
        args.extend(["-c".into(), cfg.display().to_string()]);
        // Keep the tempdir alive for the duration of the run.
        _cfg_keep = Some(tmp);
    } else {
        _cfg_keep = None;
    }
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    cmd.args(args);
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn picocode");
    // Keep the tempdir alive for the duration of the run.
    _config_tempdir = Some(config_tempdir);

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    let started = wait_for(&bytes, "picocode -", Duration::from_secs(10));
    eprintln!("[{label}] started={started}");
    // Answer picocode's cursor-position query (DSR `ESC[6n`) so `cursor_row()`
    // does not swallow our typed bytes while scanning stdin for the reply.
    // Same handshake as tests/render_scroll.rs.
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    // Submit a prompt (Enter submits the typed line in the REPL).
    let _ = writer.write_all(b"run the command\r");
    let _ = writer.flush();

    // Wait until the mock has served the expected turns, then a beat for the
    // separator + next prompt redraw.
    let want = if scenario == 2 { 1 } else { 2 };
    let deadline = Instant::now() + Duration::from_secs(30);
    while llm.count() < want && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!("[{label}] requests served: {}", llm.count());
    thread::sleep(Duration::from_millis(1200));

    let data = bytes.lock().unwrap().clone();
    std::fs::write(format!("/tmp/mock_turn_{label}.bin"), &data).unwrap();
    let tail: String = String::from_utf8_lossy(&data[data.len().saturating_sub(3000)..]).to_string();
    eprintln!("[{label}] raw tail = {:?}", tail);
    let mut p = Parser::new(ROWS, COLS, 1000);
    p.process(&data);
    let screen = p.screen().contents();
    eprintln!("=== [{label}] screen ===\n{screen}\n=== end [{label}] ===");

    let _ = child.kill();
    let _ = child.wait();
    data
}

/// Count the empty lines between the last non-empty content line and the
/// horizontal separator (the line of `─` drawn at the top of the next loop).
fn blanks_before_separator(data: &[u8]) -> usize {
    let lines = ansi_lines(data);
    // Find the last separator line.
    let mut sep = None;
    for (i, t) in lines.iter().enumerate() {
        if t.chars().all(|c| c == '─') && t.len() >= 10 {
            sep = Some(i);
        }
    }
    let Some(sep) = sep else { return 0 };
    // Walk up from the separator, counting empty lines until the first
    // non-empty content line.
    let mut blanks = 0;
    let mut i = sep;
    while i > 0 {
        i -= 1;
        if lines[i].trim().is_empty() {
            blanks += 1;
        } else {
            break;
        }
    }
    blanks
}

/// Count the empty lines directly above the first `⏺ Error:` line.
fn blanks_above_error(data: &[u8]) -> usize {
    let lines = ansi_lines(data);
    let Some(err) = lines.iter().position(|l| l.contains("Error:")) else {
        return usize::MAX;
    };
    let mut blanks = 0;
    let mut i = err;
    while i > 0 {
        i -= 1;
        if lines[i].trim().is_empty() {
            blanks += 1;
        } else {
            break;
        }
    }
    blanks
}

fn ansi_lines(data: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(data)
        .split("\r\n")
        .map(strip_ansi)
        .collect()
}

/// Remove ANSI escape sequences (CSI `ESC [ params final`) from a line.
fn strip_ansi(l: &str) -> String {
    let mut out = String::new();
    let mut chars = l.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() || c2 == '~' {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn end_to_end_separator_layout() {
    // Scenario 0: text + tool + final text answer (the user's example).
    let a = run_turn(0, "text-tool-finaltext", false);
    let blanks = blanks_before_separator(&a);
    assert_eq!(
        blanks, 1,
        "text+tool+final-answer turn must end with exactly ONE empty line before the separator (got {blanks})"
    );

    // Scenario 2: single text-only turn.
    let b = run_turn(2, "text-only", false);
    let blanks = blanks_before_separator(&b);
    assert_eq!(
        blanks, 1,
        "text-only turn must end with exactly ONE empty line before the separator (got {blanks})"
    );

    // Scenario 1: turn ends right after the tool result.
    let c = run_turn(1, "tool-result-end", false);
    let blanks = blanks_before_separator(&c);
    assert_eq!(
        blanks, 1,
        "tool-result-ended turn must end with exactly ONE empty line before the separator (got {blanks})"
    );

    // Scenario 0 + thinking on: answer-hold path (reasoning model).
    let h = run_turn(0, "held-answer", true);
    let blanks = blanks_before_separator(&h);
    assert_eq!(
        blanks, 1,
        "answer-held (reasoning) turn must end with exactly ONE empty line before the separator (got {blanks})"
    );

    // Scenario 3: second request 429 -> display_text("Error: ...").
    let d = run_turn(3, "error-429", false);
    let above = blanks_above_error(&d);
    assert_eq!(
        above, 1,
        "error message must sit exactly ONE empty line below the prior block (got {above})"
    );
    // And the error line itself sits directly above the separator (0 blanks).
    let before = blanks_before_separator(&d);
    assert_eq!(
        before, 0,
        "error line must sit directly above the separator (got {before} blanks)"
    );
}
