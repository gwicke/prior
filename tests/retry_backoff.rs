//! Probe the retry back-off behavior directly against the real `picocode`
//! binary and a mock OpenAI-compatible SSE server.
//!
//! Focus: does a "successful model response" (streamed text / tool work)
//! reset the exponential back-off, so a subsequent error retries at the
//! base delay again? We drive the binary in a pty, time each request that
//! reaches the mock, and reconstruct the observed back-off sequence from
//! the inter-request gaps.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 40;
const COLS: u16 = 120;

/// The back-off base, in ms, that `run_one` hands the child process.
///
/// 30ms keeps each gap comfortably above the ~50ms polling granularity and the
/// noise of a pty round-trip, while making a 1+2+4+8+16 ms schedule cost well
/// under a second instead of 31 seconds. Growth assertions are written against
/// multiples of this, so raising it does not mean rewriting them.
const BASE_MS: u64 = 30;

/// One expected back-off gap: the base scaled by its `2^(n-1)` step.
///
/// Wide on the low side (never *shorter* than the schedule says) and generous
/// on the high side (a loaded runner may be slow), because these measure that a
/// delay happened and roughly how big it was — not that the scheduler hit an
/// exact millisecond.
fn gap_window(step: u32) -> std::ops::RangeInclusive<u64> {
    let nominal = BASE_MS * (1u64 << step);
    (nominal / 2)..=(nominal * 4 + 500)
}

/// Tracks the (elapsed, request-index, had_tool_result) of every chat request.
struct TrackedMockLlm {
    port: u16,
    request_times: Arc<Mutex<Vec<(Duration, usize, bool)>>>,
    served: Arc<AtomicUsize>,
}

impl TrackedMockLlm {
    /// `pattern` is a string of 'E' (error, 429) and 'T' (successful turn)
    /// characters, one per request in order. Any request past the pattern
    /// length is treated as a successful final turn.
    fn start(pattern: &str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let request_times = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        let base = Instant::now();
        let pattern = pattern.to_string();
        let times = request_times.clone();
        let served_c = served.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let times = times.clone();
                let served_c = served_c.clone();
                let pattern = pattern.clone();
                let base = base;
                let sentinel = Arc::new(AtomicBool::new(false));
                thread::spawn(move || {
                    let _ = serve(&mut stream, pattern, base, times, served_c, sentinel);
                });
            }
        });
        TrackedMockLlm {
            port,
            request_times,
            served,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn count(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// Inter-request gaps in milliseconds (elapsed[1..] - elapsed[..-1]).
    fn gaps_ms(&self) -> Vec<u64> {
        let t = self.request_times.lock().unwrap().clone();
        let mut out = Vec::new();
        for w in t.windows(2) {
            out.push((w[1].0 - w[0].0).as_millis() as u64);
        }
        out
    }

    fn log(&self) -> Vec<(u64, usize, bool)> {
        self.request_times
            .lock()
            .unwrap()
            .iter()
            .map(|(d, i, t)| (d.as_millis() as u64, *i, *t))
            .collect()
    }
}

fn serve(
    stream: &mut std::net::TcpStream,
    pattern: String,
    base: Instant,
    times: Arc<Mutex<Vec<(Duration, usize, bool)>>>,
    served: Arc<AtomicUsize>,
    _sentinel: Arc<AtomicBool>,
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

    let n = served.fetch_add(1, Ordering::SeqCst); // 0-based index
    let had_tool_result = body_str.contains("\"role\":\"tool\"");
    let elapsed = base.elapsed();
    times.lock().unwrap().push((elapsed, n, had_tool_result));

    let kind = pattern.chars().nth(n).unwrap_or('T');
    if kind == 'E' {
        let json = r#"{"error":{"message":"Rate limit exceeded: mock","code":429}}"#;
        return write_response(
            stream,
            "429 Too Many Requests",
            "application/json",
            json.as_bytes(),
        );
    }

    // Successful turn. If this request carries a tool result (a continuation
    // after the first text+tool call), end cleanly; otherwise emit text+tool.
    let sse = if had_tool_result {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        )
    } else {
        format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"Let me run a command.\n"},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\": \"echo '2. sleep 30 — done'\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        )
    };
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
fn run_one(pattern: &str, label: &str) -> (Vec<u64>, Vec<(u64, usize, bool)>) {
    let llm = TrackedMockLlm::start(pattern);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
    let args: Vec<String> = vec![
        "-p".into(),
        "llamacpp".into(),
        "-m".into(),
        "test".into(),
        "--yolo".into(),
        "true".into(),
    ];
    cmd.args(args);
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    // Run the back-off schedule in milliseconds instead of seconds. These tests
    // spawn the real binary, so `tokio::time::pause` is not available to them;
    // without this they sleep out real 1+2+4+8+16 s of back-off per run, which
    // made `retry_backoff` the slowest binary in the suite. The growth *shape*
    // is what these tests are about and it is unchanged — only its time base is.
    // See `PICOCODE_TEST_RETRY_BASE_MS` in src/agent.rs.
    cmd.env("PICOCODE_TEST_RETRY_BASE_MS", BASE_MS.to_string());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait for startup banner.
    let _started = wait_for(&bytes, "picocode -", Duration::from_secs(10));
    // Answer cursor-position query.
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"run the command\r");
    let _ = writer.flush();

    // Wait until the mock has served the whole pattern.
    //
    // The old `want = pattern.len() + 1` assumed an extra request would always
    // arrive. It does not: "TEET" makes 4 requests and "EEETEETT" makes 7, so
    // both waited out the entire 26s deadline and then asserted on a complete
    // set of gaps anyway. That is where two of these tests spent their time —
    // the loop was timing out, not working. With the back-off now in
    // milliseconds the whole turn is over in well under a second, so an
    // unmet `want` is unambiguously a bug rather than slowness.
    //
    // `serve` stops one short of `pattern.len()`: the trailing T of a pattern is
    // the request that completes the turn, and the client is killed once it is
    // in flight rather than waiting for its (unused) response. The assertion
    // below is what pins that, so a change to `serve` cannot silently shift it.
    let want = pattern.len() - 1;
    let deadline = Instant::now() + Duration::from_secs(10);
    while llm.count() < want && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let served = llm.count();
    eprintln!(
        "[{label}] served={served} (want {want}){} gaps={:?}",
        if served < want { "  UNDERSERVED" } else { "" },
        llm.gaps_ms()
    );
    assert!(
        served >= want,
        "[{label}] mock served {served} of {want} requests for pattern {pattern:?} — \
         the gaps below are then asserted on an incomplete set"
    );
    thread::sleep(Duration::from_millis(400));
    eprintln!("[{label}] log={:?}", llm.log());

    let _ = child.kill();
    let _ = child.wait();
    (llm.gaps_ms(), llm.log())
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

/// All consecutive 429s with no progress: back-off should double each time.
/// Pattern "EEET": E,E,E then T. Expect gaps roughly 1,2,4 s (then final T).
#[test]
fn consecutive_429s_grow_exponentially() {
    let (gaps, _log) = run_one("EEET", "grow");
    // First three requests are 429 (no progress): base=1s, then 2s, then 4s.
    assert!(gaps.len() >= 3, "expected >=3 gaps, got {:?}", gaps);
    let g1 = gaps[0];
    let g2 = gaps[1];
    let g3 = gaps[2];
    eprintln!("gaps={gaps:?}");
    assert!(gap_window(0).contains(&g1), "gap0 ~1 base, got {g1}ms");
    assert!(gap_window(1).contains(&g2), "gap1 ~2 base, got {g2}ms");
    assert!(gap_window(2).contains(&g3), "gap2 ~4 base, got {g3}ms");
    // The point of the test is that each gap is LONGER than the one before.
    assert!((g2 > g1) && (g3 > g2), "gaps must grow: {gaps:?}");
}

/// Progress then mid-run error: within ONE prompt(), req0 streams text + a tool
/// call (progress committed), the tool runs, and the continuation request req1
/// returns 429. This error occurs AFTER progress, so the retry loop must reset
/// the back-off to the base delay (~1s) for the NEXT attempt (req2).
/// Pattern: req0=T (progress), req1=E (mid-run 429), req2=T, req3=T.
#[test]
fn progress_then_error_resets_to_base() {
    let (gaps, log) = run_one("TEET", "progress-then-error");
    eprintln!("gaps={gaps:?} log={log:?}");
    // req0 (T) -> req1 (E) are part of the SAME prompt() run (tool runs then
    // continuation errors), so req0->req1 is the tool execution time, not a
    // backoff. The backoff appears between req1 (E) and req2 (next attempt).
    assert!(gaps.len() >= 2, "need >=2 gps, got {gaps:?}");
    // After a progress-then-mid-run-error the retry must back off by the BASE
    // delay (1s), NOT grow. So the req1->req2 gap should be ~1s.
    let g1 = gaps[1];
    eprintln!("req1->req2 gap = {g1}ms");
    assert!(
        gap_window(0).contains(&g1),
        "expected base ~1 base after progress+error, got {g1}ms"
    );
}

/// Build up the back-off with several no-progress 429s, then make a
/// progress-then-mid-run-error (resets to base), then immediately hit another
/// no-progress error. The no-progress error that follows the reset must be at
/// the BASE delay (~1s), not grown.
/// Pattern: req0..req2 = E (no-progress 429s), req3 = T(progress),
/// req4 = E(429 on tool continuation => LlmInterrupted => reset),
/// req5 = E(next attempt, no-progress => counter 1 => base), req6=T req7=T.
#[test]
fn backoff_resets_after_progress_even_with_prior_growth() {
    let (gaps, log) = run_one("EEETEETT", "grow-then-progress");
    eprintln!("gaps={gaps:?} log={log:?}");
    // gaps indices:
    //   [0] req0->req1 (1s after first 429)
    //   [1] req1->req2 (2s after second 429)
    //   [2] req2->req3 (4s after third 429)
    //   [3] req3->req4 (tool exec ~0ms, same run)
    //   [4] req4->req5 (base 1s after reset)
    assert!(gaps.len() >= 5, "need >=5 gaps, got {gaps:?}");
    assert!(
        gap_window(0).contains(&gaps[0]),
        "gap0 ~1 base, got {}",
        gaps[0]
    );
    assert!(
        gap_window(1).contains(&gaps[1]),
        "gap1 ~2 base, got {}",
        gaps[1]
    );
    assert!(
        gap_window(2).contains(&gaps[2]),
        "gap2 ~4 base, got {}",
        gaps[2]
    );
    // Growth before the reset is the whole premise of this test.
    assert!(
        gaps[1] > gaps[0] && gaps[2] > gaps[1],
        "the pre-reset gaps must grow: {gaps:?}"
    );
    // After the progress+midrun-error the next no-progress error must be base.
    let g4 = gaps[4];
    eprintln!("post-reset no-progress gap = {g4}ms");
    assert!(
        gap_window(0).contains(&g4),
        "expected base ~1 base after reset, got {g4}ms"
    );
    // The reset is the claim: this gap is BASE again, not grown like gaps 1-2.
    // Without this, a run that never grew at all would also satisfy the windows
    // above and the test would pass without testing a reset.
    assert!(
        g4 < gaps[2],
        "post-reset gap {g4} should be shorter than the pre-reset peak {}: {gaps:?}",
        gaps[2]
    );
}

/// Regression test: the consecutive-failure back-off must reset after the
/// model makes progress even WITHOUT a session (HOME/XDG/APPDATA unset).
/// Previously the reset was gated on session persistence, so without a
/// session the counter kept building and a post-progress error retried at a
/// large delay instead of the base ~1s.
#[test]
fn no_session_still_resets_backoff_after_progress() {
    let llm = TrackedMockLlm::start("EEETEETT");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
    let args: Vec<String> = vec![
        "-p".into(),
        "llamacpp".into(),
        "-m".into(),
        "test".into(),
        "--yolo".into(),
        "true".into(),
    ];
    cmd.args(args);
    cmd.env("LLAMACPP_API_BASE_URL", llm.base_url());
    // Remove all home/config vars so app_config_dir() -> None => no session.
    cmd.env_remove("HOME");
    cmd.env_remove("XDG_CONFIG_HOME");
    // Same scaled back-off base as `run_one` — see `BASE_MS`.
    cmd.env("PICOCODE_TEST_RETRY_BASE_MS", BASE_MS.to_string());
    cmd.env_remove("APPDATA");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");
    let _started = wait_for(&bytes, "picocode -", Duration::from_secs(10));
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"run the command\r");
    let _ = writer.flush();
    let want = 6usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while llm.count() < want && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(400));
    let gaps = llm.gaps_ms();
    let log = llm.log();
    eprintln!(
        "[no-session] served={} gaps={gaps:?} log={log:?}",
        llm.count()
    );
    let _ = child.kill();
    let _ = child.wait();
    assert!(gaps.len() >= 5, "need >=5 gaps, got {gaps:?}");
    // Gaps: [0]=1s, [1]=2s, [2]=4s grow; [3] tool-exec (~0); [4] post-progress.
    // After progress+mid-run-error the back-off must reset to base (~1s),
    // even with no session. A value >= 2s means the counter kept building.
    let g4 = gaps[4];
    eprintln!("post-progress gap under no-session = {g4}ms gaps={gaps:?}");
    assert!(
        gap_window(0).contains(&g4),
        "expected base ~1 base after progress with no session, got {g4}ms (gaps {gaps:?})"
    );
    // The regression: without a session the counter used to keep building, so
    // this gap grew instead of resetting. It must be shorter than the grown
    // gaps before it, i.e. back near base.
    assert!(
        g4 < gaps[2],
        "post-progress gap {g4}ms should be shorter than the pre-reset peak {} (gaps {gaps:?})",
        gaps[2]
    );
}
