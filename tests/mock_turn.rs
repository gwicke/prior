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
        return write_response(
            stream,
            "429 Too Many Requests",
            "application/json",
            json.as_bytes(),
        );
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

    // Scenario 4: a fenced code block, and the reason the status's gap exists.
    //
    // A fence is emitted line by line and its last byte is a single newline, so
    // the last line of text sits *directly* above where the bar goes. A
    // paragraph cannot show that: the blank-line rule swallows its second
    // newline, so a paragraph's block already ends on a blank line and the
    // status reuses it. Only the fence leaves the bar with nothing in between,
    // which is why this scenario exists and why the screen assertion below needs
    // it — against a paragraph the assertion passes with the gap switched off.
    if scenario == 4 {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{"content":"```rust\nfn main() {}\n```\n"},"finish_reason":null}]}"#,
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
                r#"{"id":"chatcmpl-2","object":"chat.completion.chunk","created":2,"model":"test","choices":[{"index":0,"delta":{"content":"2. sleep 30 — done\n"},"finish_reason":null}]}"#,
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
/// runs with `default_reasoning_level = "high"` (answer-hold / reasoning path).
fn run_turn(scenario: u32, label: &str, thinking: bool) -> Vec<u8> {
    run_turn_typing(scenario, label, thinking, "run the command", ROWS)
}

/// As [`run_turn`], but the `DSR` reply puts the input row at `dsr_row` instead
/// of the last screen row, so the area starts **mid-screen** with rows below it
/// free.
///
/// **That is the only regime that distinguishes the two designs** (`ui_draw_v2`
/// §4): a reserved area sits at the screen's last row from the first prompt,
/// while this one starts under the greeter and is pushed down by content. In
/// the steady state — a turn long enough to fill the screen — the two converge
/// and every screen looks identical, which is why `end_to_end_input_mode_layout`
/// could not arbitrate the placement arithmetic.
fn run_turn_at(scenario: u32, label: &str, dsr_row: u16) -> Vec<u8> {
    run_turn_typing(scenario, label, false, "run the command", dsr_row)
}

/// As [`run_turn`], but types `typed` instead of a prompt.
///
/// **A slash command is a trap here.** It never reaches the mock, so the
/// "wait until the mock has served the expected turns" loop below is satisfied
/// immediately (`want == 0`) and the only thing standing between the capture and
/// the program is a fixed sleep. A capture taken in that window describes a
/// screen the program never drew — mid-typing, before Enter was even read.
///
/// So a slash-command scenario must not use this: it has to wait for something
/// the *commit* writes (the typed line itself is no use as a marker, since it
/// appears in the input row while it is still being keyed). Asserting on a
/// slash commit through a pty also means driving the v2.5 menu's key routing,
/// which `/thinking` does not accept as typed text — the level comes from a
/// drilled row, not from the buffer. `emit_commit`'s bracketing is therefore
/// pinned by a unit test in `repl.rs` instead, at the level the contract lives
/// at.
fn run_turn_typing(
    scenario: u32,
    label: &str,
    thinking: bool,
    typed: &str,
    dsr_row: u16,
) -> Vec<u8> {
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
    let mut args: Vec<String> = vec![
        "-p".into(),
        "llamacpp".into(),
        "-m".into(),
        "test".into(),
        "--yolo".into(),
        "true".into(),
    ];
    let _cfg_keep;
    let _config_tempdir;
    if thinking {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("picocode.toml");
        std::fs::write(&cfg, "default_reasoning_level = \"high\"\n").unwrap();
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
    let mut child = pair.slave.spawn_command(cmd).expect("spawn picocode");
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
    let _ = writer.write_all(format!("\x1b[{dsr_row};1R").as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    // Submit a prompt (Enter submits the typed line in the REPL).
    let _ = writer.write_all(format!("{typed}\r").as_bytes());
    let _ = writer.flush();

    // Wait until the mock has served the expected turns, then a beat for the
    // separator + next prompt redraw.
    let want = if typed.starts_with('/') {
        0
    } else if scenario == 2 {
        1
    } else {
        2
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while llm.count() < want && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!("[{label}] requests served: {}", llm.count());
    thread::sleep(Duration::from_millis(1200));

    let data = bytes.lock().unwrap().clone();
    std::fs::write(format!("/tmp/mock_turn_{label}.bin"), &data).unwrap();
    let tail: String =
        String::from_utf8_lossy(&data[data.len().saturating_sub(3000)..]).to_string();
    eprintln!("[{label}] raw tail = {:?}", tail);
    let mut p = Parser::new(ROWS, COLS, 1000);
    p.process(&data);
    let screen = p.screen().contents();
    eprintln!("=== [{label}] screen ===\n{screen}\n=== end [{label}] ===");

    let _ = child.kill();
    let _ = child.wait();
    data
}

// (Removed: `blanks_before_separator` counted the blank rows above the divider
// that opened each turn, and the `screen_rows` doc comment that explained the
// screen-based counting went with it — the helper those notes belonged to is
// still here, under its own doc comment below.)

/// The final screen, as trimmed rows.
///
/// Counted on the **screen**, not in the byte stream, and that is a correction
/// rather than a style choice. The transient status paints a gap row of its own
/// (`row_accounting` §3.5) and erases it again before the next write, so a model
/// that splits the stream on newlines counts one blank per content block — three
/// for a text/tool/text turn — none of which ever existed on screen. The screen
/// is what these assertions are about, and it is immune to transient rows by
/// construction.
/// Blank screen rows between the last content row and the **status bar**.
///
/// The bar is the row that now separates a finished turn from the input area, so
/// this is where "one empty line at the end of a turn" is now decided. It
/// replaces `blanks_before_separator`, which counted the same thing against the
/// divider that opened each turn — a divider the bar has since taken the row of.
fn blanks_before_status(data: &[u8]) -> usize {
    let rows = screen_rows(data);
    let Some(bar) = rows.iter().position(|r| is_status_row(r)) else {
        return 0;
    };
    let mut blanks = 0;
    let mut i = bar;
    while i > 0 {
        i -= 1;
        if rows[i].trim().is_empty() {
            blanks += 1;
        } else {
            break;
        }
    }
    if blanks > 0 {
        for (i, r) in rows.iter().enumerate() {
            let t = r.trim();
            let shown = if t.is_empty() {
                "<blank>".to_string()
            } else {
                t.chars().take(50).collect()
            };
            eprintln!("ROW {i:02}: {shown}");
        }
    }
    blanks
}

/// Is this row the status bar? It always carries the qualified model id, which is
/// what the mock runs on (`-p llamacpp -m test`) — and the prompt-side bar and the
/// transient one are the same text, so this matches either.
fn is_status_row(row: &str) -> bool {
    row.contains("llamacpp:test")
}

/// The final screen, as trimmed rows.
///
/// Counted on the **screen**, not in the byte stream, and that is a correction
/// rather than a style choice. The transient status paints a gap row of its own
/// (`row_accounting` §3.5) and erases it again before the next write, so a model
/// that splits the stream on newlines counts one blank per content block — three
/// for a text/tool/text turn — none of which ever existed on screen. The screen
/// is what these assertions are about, and it is immune to transient rows by
/// construction.
fn screen_rows(data: &[u8]) -> Vec<String> {
    let mut p = Parser::new(ROWS, COLS, 1000);
    p.process(data);
    let screen = p.screen();
    (0..ROWS)
        .map(|r| {
            (0..COLS)
                .map(|c| {
                    screen
                        .cell(r, c)
                        .and_then(|x| x.contents().chars().next())
                        .unwrap_or(' ')
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Count the empty screen rows directly above the first `⏺ Error:` line.
///
/// Screen-based for the same reason as [`blanks_before_separator`]: the
/// transient status's gap row is blank in the stream and absent from the screen,
/// and this is a spacing assertion about what the reader sees.
fn blanks_above_error(data: &[u8]) -> usize {
    let rows = screen_rows(data);
    let Some(err) = rows.iter().position(|l| l.contains("Error:")) else {
        return usize::MAX;
    };
    let mut blanks = 0;
    let mut i = err;
    while i > 0 {
        i -= 1;
        if rows[i].trim().is_empty() {
            blanks += 1;
        } else {
            break;
        }
    }
    blanks
}

// (Removed: `ansi_lines` split the byte stream on newlines to model the screen.
// The status's gap row is blank in the stream and absent from the screen, so
// that model counted one blank per content block that never existed on screen —
// three for a text/tool/text turn. The spacing assertions are screen-based now,
// via `screen_rows`.)

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

/// Every `CSI … H` (CUP) in a string: the parameters between the `ESC [` and
/// the `H`. Scans for the real sequence rather than the letter, so the status
/// text itself could contain an `H` without passing for an address.
fn cups(s: &str) -> Vec<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\x1b' && b.get(i + 1) == Some(&'[') {
            let start = i + 2;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == ';') {
                j += 1;
            }
            if b.get(j) == Some(&'H') {
                out.push(b[start..j].iter().collect());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The bar keeps a blank row between itself and the last line of content, and
/// exactly one — measured on the screen, mid-turn, with the bar up.
///
/// This is the requirement the whole gap is for, so it is asserted on the screen
/// rather than on the byte stream. The lib tests count newlines, which is a
/// proxy: a stream that splits on newlines cannot tell a row that was painted
/// and erased from one that survived, and the gap is precisely a row that does
/// not survive. So the transcript is replayed up to a paint and the rows are
/// read off the screen.
#[test]
fn the_status_line_keeps_one_blank_row_above_it() {
    let bytes = run_turn(4, "fenced-code", false);
    // The offsets index the **raw transcript**, so the search is a byte search:
    // `\r\n` collapsing shifts every position after the first line, and
    // `from_utf8_lossy` shifts them again wherever a byte is not valid UTF-8.
    // Either one and the slice below is cut in the wrong place.
    // Streaming bars only — see the note in the transient test above. The
    // prompt-side bar is a frame row, so the *first* bar in the transcript is no
    // longer the one this is about.
    let all = |needle: &[u8]| {
        bytes
            .windows(needle.len())
            .enumerate()
            .filter(|(_, w)| *w == needle)
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    let green = b"\x1b[48;5;237m \x1b[38;5;41m";
    // The **first** paint after content — and with a fence, the first paint is
    // the one that reproduces the bug, which is why this scenario exists.
    //
    // A fence is emitted line by line, so after its last line the stream ends on
    // a single newline and the next row is the one the bar wants. Nothing has
    // left a blank there: the trailing blank that makes the *later* paints look
    // fine is written by the segment flush at the end of the turn, which is two
    // paints too late. So a test that samples a later paint passes with the gap
    // switched off — which is exactly what mine did before this scenario was
    // driven through every paint.
    let paints = all(green);
    let bar_start = *paints
        .first()
        .unwrap_or_else(|| panic!("the bar should be painted at all"));
    // Stop just after that bar's own trailing reset: mid-turn, bar up, nothing
    // painted after it yet. The search is *after* the bar's `SGR` — the
    // transcript is full of resets from earlier styling, and the first one in the
    // whole stream belongs to something else.
    let stop = bar_start
        + bytes[bar_start..]
            .windows(4)
            .position(|w| w == b"\x1b[0m")
            .expect("the bar should be reset")
        + 4;
    let mut p = Parser::new(ROWS, COLS, 1000);
    p.process(&bytes[..stop]);
    let screen = p.screen();
    let rows: Vec<String> = (0..ROWS)
        .map(|r| {
            (0..COLS)
                .map(|c| {
                    screen
                        .cell(r, c)
                        .and_then(|x| x.contents().chars().next())
                        .unwrap_or(' ')
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    let bar_row = rows
        .iter()
        .position(|r| is_status_row(r))
        .unwrap_or_else(|| panic!("the bar should be on screen: {rows:?}"));
    // Exactly one blank row above the bar, and the row above *that* is content.
    // Zero would be the bar jammed against the text; two or more is the gap
    // stacking on a blank the content path had already written.
    assert!(
        rows[bar_row - 1].trim().is_empty(),
        "one blank row between the last content and the bar: {rows:?}"
    );
    assert!(
        is_status_row(&rows[bar_row - 1]) || !rows[bar_row - 2].trim().is_empty(),
        "and only one — the row above the gap is content, not another blank: {rows:?}"
    );
    // **Blank is not enough — the gap has to be unpainted.** A tool panel paints
    // its background across every row it spans, so a row that is empty but still
    // carries the panel's grey is part of the panel as far as the eye is
    // concerned, and the bar still reads as jammed against it. This is the whole
    // point of the gap, so it is the part worth asserting: a row is only a gap if
    // its cells are back to the terminal's default background.
    let gap_bg = screen.cell((bar_row - 1) as u16, 4).map(|c| c.bgcolor());
    assert_eq!(
        gap_bg,
        Some(vt100::Color::Default),
        "the gap row must be unpainted, not an empty row inside the panel: {rows:?}"
    );
    // And the bar's own row is a solid block, which is what makes it read as a
    // separate surface rather than another line of the transcript.
    assert_eq!(
        screen.cell(bar_row as u16, 4).map(|c| c.bgcolor()),
        Some(vt100::Color::Idx(237)),
        "the bar's background should be the dark grey: {rows:?}"
    );
}

/// One blank row between the last content of a turn and the status bar that now
/// sits where the next turn's separator used to.
///
/// This is what `end_to_end_separator_layout` measured, against a different row:
/// the divider that opened each turn is gone, and the bar took its place. The
/// invariant is the same — a turn ends one blank line before the input area —
/// and it is worth keeping in the same shape (one screen-based count per
/// scenario) because it is the thing that regresses when a block's trailing blank
/// goes missing.

/// **The area follows the content; it is not parked at the screen's bottom.**
///
/// Every other layout test here drives a turn that fills the screen, which is
/// the *steady state* — and `ui_draw_v2` §4 notes that a reserved area and an
/// unreserved one look identical there. So none of them can tell the two designs
/// apart, which is exactly why `end_to_end_input_mode_layout` settled the LF
/// arithmetic in the wrong direction instead of catching it.
///
/// This one starts the input row mid-screen (`dsr_row`) and emits a turn short
/// enough that the area never reaches the bottom, so the two designs diverge and
/// the assertions below are falsifiable.
///
/// The gap *count* is deliberately not asserted. One blank row before the bar
/// is a spacing decision that §5.2 has still to make explicit; what is settled
/// is that the area is **immediately** below the content rather than at the
/// screen's edge. Asserting adjacency (and not the gap) keeps this test honest
/// while that decision is open.
#[test]
fn the_area_follows_the_content_rather_than_the_screen_bottom() {
    // Mid-screen, with `ROWS / 2` rows left beneath the area — more than the
    // turn below will ever push it down.
    let dsr = ROWS / 2;

    // **Control.** The same turn with the input row at the *last* screen row.
    // Under a reserved area that is where it belongs; under this one it belongs
    // wherever the content left it. Both runs emit identical content, so any
    // difference in where the area lands is the design, not the data — and if
    // this control ever stopped landing at the bottom, the `dsr` parameter would
    // have gone dead and every assertion below would be vacuous.
    let pinned = screen_rows(&run_turn_at(0, "control-bottom", ROWS));
    let pinned_bar = pinned
        .iter()
        .position(|r| is_status_row(r))
        .expect("the status bar should be on screen");
    assert_eq!(
        pinned_bar as isize,
        pinned.len() as isize - 2,
        "control: with the input row at the last screen row the area should land \
         there, filling the screen; bar={pinned_bar} Screen:\n{pinned:?}"
    );

    let data = run_turn_at(0, "mid-screen-area", dsr);
    let rows = screen_rows(&data);

    // The area is `[status bar, input]`, so the bar is its first row and the
    // input is the one below it — which is blank here, since the prompt was
    // submitted. Finding "the last non-empty row" would find the bar itself.
    let bar = rows
        .iter()
        .position(|r| is_status_row(r))
        .expect("the status bar should be on screen");
    let area_below = bar + 2;

    // The load-bearing assertion: **rows beneath the area are ordinary empty
    // screen.** A reserved area would be flush at the last row from the first
    // prompt, so this fails immediately under that design and passes under this
    // one — which is what makes it the fence the other tests could not be.
    assert!(
        area_below + 1 < rows.len(),
        "the area should NOT be flush at the screen bottom — that is the reserved \
         design, which this test exists to falsify; bar={bar} Screen:\n{rows:?}"
    );
    assert!(
        rows[area_below..].iter().all(|r| r.trim().is_empty()),
        "every row below the area should be ordinary empty screen, not more of the \
         area; bar={bar} Screen:\n{rows:?}"
    );

    // Adjacency: the transcript's last row is the last non-empty one *above* the
    // bar. A reserved area would show the whole remainder of the screen blank
    // here, growing with every chunk of content.
    let transcript_last = rows[..bar]
        .iter()
        .rposition(|r| !r.trim().is_empty())
        .expect("some content above the area");
    let gap = bar as isize - transcript_last as isize - 1;
    assert!(
        (1..=2).contains(&gap),
        "the area should sit just below the content (gap 1–2 rows), not at the \
         screen's edge; gap={gap} bar={bar} transcript_last={transcript_last} \
         Screen:\n{rows:?}"
    );
}

#[test]
fn end_to_end_input_mode_layout() {
    // Scenario 2: single text-only turn.
    let b = run_turn(2, "text-only", false);
    let blanks = blanks_before_status(&b);
    assert_eq!(
        blanks, 1,
        "text-only turn must end with exactly ONE empty line before the bar (got {blanks})"
    );

    // Scenario 0: text + tool + final text answer (the user's example).
    let a = run_turn(0, "text-tool-finaltext", false);
    let blanks = blanks_before_status(&a);
    assert_eq!(
        blanks, 1,
        "text+tool+final-answer turn must end with exactly ONE empty line before the bar (got {blanks})"
    );

    // Scenario 1: turn ends right after the tool result.
    let c = run_turn(1, "tool-result-end", false);
    let blanks = blanks_before_status(&c);
    assert_eq!(
        blanks, 1,
        "tool-result-ended turn must end with exactly ONE empty line before the bar (got {blanks})"
    );

    // Scenario 0 + thinking on: answer-hold path (reasoning model).
    let h = run_turn(0, "held-answer", true);
    let blanks = blanks_before_status(&h);
    assert_eq!(
        blanks, 1,
        "answer-held (reasoning) turn must end with exactly ONE empty line before the bar (got {blanks})"
    );

    // Scenario 3: second request 429 -> display_text("Error: ...").
    let d = run_turn(3, "error-429", false);
    let above = blanks_above_error(&d);
    assert_eq!(
        above, 1,
        "error message must sit exactly ONE empty line below the prior block (got {above})"
    );
    // The error gets the same one-blank gap as every other block. It used to be
    // the exception — the error line sat *directly* above the divider, because
    // the loop-top rule was written into the blank the error path left behind.
    // With the bar in the frame there is nothing to consume that blank, so the
    // error is spaced like the rest instead of being the one block that butts
    // against the input area.
    let before = blanks_before_status(&d);
    assert_eq!(
        before, 1,
        "error line must get the same ONE blank line as any other block (got {before} blanks)"
    );
}

/// The bar is on screen in input mode, between turns, with a **grey** dot — and
/// mid-turn the same bar is painted at the live edge with a **green** one.
///
/// This is the point of the whole exercise: the bar is permanent, so the dot is
/// what says whether a turn is in flight. Before the prompt-side bar existed the
/// green state had nowhere to be seen, because the bar only existed mid-turn.
#[test]
fn the_status_bar_is_permanent_and_the_dot_says_which_state() {
    let bytes = run_turn(2, "text-only", false);
    let text = String::from_utf8_lossy(&bytes);
    let grey = "\x1b[48;5;237m \x1b[38;5;245m●";
    let green = "\x1b[48;5;237m \x1b[38;5;41m●";
    assert!(
        text.contains(grey),
        "the idle bar should be on screen with a grey dot"
    );
    assert!(
        text.contains(green),
        "and the streaming bar with a green one"
    );
    // The idle one is *last*: the turn is over, so the bar the reader is left
    // looking at is the idle one. This is what "permanent" means in practice —
    // the bar the session ends on is the one that stays.
    assert!(
        text.rfind(grey) > text.rfind(green),
        "the final bar should be the idle one: grey at rest, green while working"
    );
}

/// The prompt-side bar is a row of the input frame, so the frame's own clear
/// takes it when the input is submitted — and it is *not* a transient line that a
/// content write has to erase.
///
/// A transient bar would need a relative erase at submit, and by then the cursor
/// has moved into the frame region below it, so that erase would land on the
/// wrong rows. Being in the frame is what makes it safe, and the proof is that
/// the next turn's content starts clean: the input echo, immediately below where
/// the bar was, with no bar left stranded above it.
#[test]
fn the_idle_bar_is_frame_owned_so_submit_leaves_nothing_behind() {
    let bytes = run_turn(2, "text-only", false);
    let rows = screen_rows(&bytes);
    let bar = rows
        .iter()
        .position(|r| is_status_row(r))
        .expect("the idle bar should be on the final screen");
    // Nothing above it but the turn's content and one blank — no *second* bar
    // stranded where a transient one would have been left.
    let bars: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| is_status_row(r))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        bars,
        vec![bar],
        "exactly one bar on the final screen: {rows:?}"
    );
    // And the input echo is on screen below the bar's former row, i.e. the
    // submit wrote content where the frame's caret was, not over the bar.
    let echo = rows
        .iter()
        .position(|r| r.contains("run the command") && !is_status_row(r))
        .expect("the input echo should be on screen");
    assert!(
        echo < bar,
        "the echo should sit above the bar, not be overwritten by it: {rows:?}"
    );
}
