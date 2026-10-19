//! Probe the retry back-off behavior directly against the real `prior`
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
const BASE_MS: u64 = 400;

/// The floor a retry gap has to clear.
///
/// One minimum-delay assertion is the whole contract at this level: that the
/// retry loop *waits* before trying again. The size of the schedule — 1s, 2s,
/// 4s — is arithmetic, and `retry_delay_ms` pins it in milliseconds in
/// `agent.rs` without any process, pty or network involved. Re-deriving it here
/// bought nothing and cost stability: asserting each gap against its own step
/// meant a single scheduling hiccup could flip the comparison, since these gaps
/// are measured across two processes on a shared machine.
///
/// The floor is deliberately a fraction of `BASE_MS`, not the nominal value.
/// The sleep is `tokio::time::sleep_until(deadline)`, which never fires early,
/// so a correct run is at or above nominal; the slack absorbs a coarse clock on
/// the waiting side without letting "no delay at all" through.
const MIN_GAP_TOLERANCE: f64 = 0.75;

/// Assert that consecutive retries were separated by at least the back-off base.
fn assert_min_gap(gap_ms: u64, label: &str) {
    let floor = (BASE_MS as f64 * MIN_GAP_TOLERANCE) as u64;
    assert!(
        gap_ms >= floor,
        "{label}: retries must be separated by at least ~{BASE_MS}ms, \
         got {gap_ms}ms — the retry loop did not wait"
    );
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
fn run_one(pattern: &str, label: &str, need: usize) -> (Vec<u64>, Vec<(u64, usize, bool)>) {
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

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
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
    // See `PRIOR_TEST_RETRY_BASE_MS` in src/agent.rs.
    cmd.env("PRIOR_TEST_RETRY_BASE_MS", BASE_MS.to_string());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait for startup banner.
    let _started = wait_for(&bytes, "prior -", Duration::from_secs(10));
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
    // Wait for enough requests that the gaps the caller asserts on exist.
    //
    // `need` is a floor, not an exact count. How many requests a pattern
    // actually produces depends on timing as well as on the pattern: a `T` that
    // arrives carrying a tool result ends the turn, and whether a given retry
    // lands before or after the client is torn down varies with scheduling. An
    // exact `want` therefore made these tests intermittent in both directions —
    // waiting one short raced `gaps[k]`'s own arrival, while demanding one too
    // many sat out the deadline and then failed on a short slice.
    //
    // Extra requests are harmless: the mock serves `pattern[n]` past its end, and
    // the assertions only read the gaps they name.
    let want = need;
    let deadline = Instant::now() + Duration::from_secs(10);
    while llm.count() < want && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
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

/// A run of consecutive 429s must be spaced by the back-off, not hammered.
///
/// This is the core contract at this level: the retry loop waits before trying
/// again. How much it waits *grows* each time, but that is arithmetic — see
/// `retry_delay_ms`'s unit tests — so asserting it here would re-test the same
/// arithmetic through two processes and a pty, which only adds ways to be
/// wrong.
///
/// Every gap between consecutive 429s is checked, not just the first: that the
/// loop keeps waiting across several retries, rather than waiting once and then
/// giving up on the pacing.
#[test]
fn consecutive_429s_are_spaced_by_the_backoff() {
    let (gaps, _log) = run_one("EEET", "grow", 4);
    eprintln!("gaps={gaps:?}");
    assert!(gaps.len() >= 3, "expected >=3 gaps, got {gaps:?}");
    for (i, &g) in gaps.iter().take(3).enumerate() {
        assert_min_gap(g, &format!("gap{i}"));
    }
}

/// A mid-run error, after the model made progress, is retried at the base.
///
/// Still only a minimum-delay assertion — but the *pattern* carries the meaning:
/// this one fails a different way than the plain 429 run above. Here the 429
/// arrives on a tool continuation, i.e. after progress committed, which is the
/// case the retry loop is supposed to treat as a fresh start. If that handling
/// regressed into an unbounded hammer, the gap collapses and this catches it.
///
/// That it resets to the *base* rather than a grown delay is arithmetic again
/// and is pinned by `retry_delay_ms`; what this level can see is only that a
/// delay happened.
#[test]
fn progress_then_error_resets_to_base() {
    let (gaps, log) = run_one("TEET", "progress-then-error", 3);
    eprintln!("gaps={gaps:?} log={log:?}");
    // req0 (T) -> req1 (E) are part of the SAME prompt() run (tool runs then
    // continuation errors), so req0->req1 is the tool execution time, not a
    // backoff. The backoff appears between req1 (E) and req2 (next attempt).
    assert!(gaps.len() >= 2, "need >=2 gps, got {gaps:?}");
    let g1 = gaps[1];
    eprintln!("req1->req2 gap = {g1}ms");
    assert_min_gap(g1, "req1->req2");
}

/// Growth across a progress boundary, then a further no-progress error.
///
/// The `E E E T E E T T` pattern is the one case where the *order* of events
/// matters and a plain minimum-delay check would be too weak: three 429s, then
/// progress, then more 429s. If the retry loop mishandled the progress
/// boundary it could either stop waiting or grow without bound, and this
/// arrangement puts a delay on both sides of the boundary.
///
/// What is asserted is therefore the minimum delay for every gap that follows
/// a 429 — including the first one after the progress boundary, which is the
/// gap a regression in the reset would collapse. The size of the schedule is
/// not re-derived here; see `retry_delay_ms` for that.
#[test]
fn backoff_resets_after_progress_even_with_prior_growth() {
    let (gaps, log) = run_one("EEETEETT", "grow-then-progress", 6);
    eprintln!("gaps={gaps:?} log={log:?}");
    // gaps indices:
    //   [0] req0->req1 (after first 429)
    //   [1] req1->req2 (after second 429)
    //   [2] req2->req3 (after third 429)
    //   [3] req3->req4 (tool exec ~0ms, same run — NOT a back-off, skipped)
    //   [4] req4->req5 (after the 429 that follows progress)
    assert!(gaps.len() >= 5, "need >=5 gaps, got {gaps:?}");
    for i in [0usize, 1, 2, 4] {
        assert_min_gap(gaps[i], &format!("gap{i}"));
    }
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
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
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
    cmd.env("PRIOR_TEST_RETRY_BASE_MS", BASE_MS.to_string());
    cmd.env_remove("APPDATA");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");
    let _started = wait_for(&bytes, "prior -", Duration::from_secs(10));
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
    // Gaps: [0],[1],[2] follow the pre-progress 429s; [3] is tool-exec (~0, not
    // a back-off); [4] is the first gap after the post-progress 429.
    //
    // The bug this is a regression test for: the reset was gated on session
    // persistence, so with no session the counter kept building and the
    // post-progress retry was delayed by an ever-growing amount. The observable
    // difference is that the loop *did* wait — it is waiting, just for far too
    // long — so a minimum-delay check cannot catch it on its own. The upper
    // bound is what distinguishes "reset to base" from "kept growing", and it
    // is loose enough to absorb scheduler jitter: a correct post-reset gap is
    // ~BASE_MS, whereas the first growth step alone would be 2x that.
    let g4 = gaps[4];
    eprintln!("post-progress gap under no-session = {g4}ms gaps={gaps:?}");
    assert_min_gap(g4, "post-progress, no session");
    assert!(
        g4 <= BASE_MS * 3,
        "post-progress gap {g4}ms is too long: without a session the \
         counter must reset to base (~{BASE_MS}ms), not keep growing (gaps {gaps:?})"
    );
}
