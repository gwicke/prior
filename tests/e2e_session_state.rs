//! E2E: session-state regression + interactive smokes.
//!
//! Drives the REAL `prior` binary in a real pty with an isolated
//! `XDG_CONFIG_HOME` (so sessions/settings/history never touch the
//! developer's home), a temp working directory (the session-dir encoding
//! keys off the child's cwd), and pre-seeded session files on disk —
//! no network and no LLM turn required.
//!
//! Test 1 is THE regression for the historical empty-session bug: menu
//! commits (thinking switch → convergence → `record_reasoning_change`, and
//! a `/session` delete) used to leak through `ensure_persisted` and create
//! header+marker "empty" session files. The session file count must be
//! exactly what we seeded — nothing more — after arbitrary menu traffic.
//!
//! Test 2 smokes resume: replay renders the seeded history and the model
//! decision lands in the input line as a `/model …` prefill.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use prior::session::types::{encode_cwd, session_filename};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tempfile::TempDir;
use vt100::Parser;

const ROWS: u16 = 40;
const COLS: u16 = 120;

// ---------------------------------------------------------------------------
// Harness (same patterns as mock_turn.rs / confirm_raw_mode.rs)
// ---------------------------------------------------------------------------

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

fn assert_wait(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str, what: &str) {
    assert!(
        wait_for(bytes, needle, Duration::from_secs(10)),
        "timed out waiting for {what}: {needle:?}\n--- screen tail ---\n{}",
        screen_tail(bytes)
    );
}

/// Render the captured bytes through vt100 for failure messages.
fn screen_tail(bytes: &Arc<Mutex<Vec<u8>>>) -> String {
    let data = bytes.lock().unwrap().clone();
    let mut p = Parser::new(ROWS, COLS, 1000);
    p.process(&data);
    let screen = p.screen().contents();
    let len = screen.chars().count();
    screen.chars().skip(len.saturating_sub(1500)).collect()
}

struct Fixture {
    _xdg: TempDir,
    work: TempDir,
    sessions: PathBuf,
}

/// Isolated XDG_CONFIG_HOME + a temp child cwd; returns the fixture.
fn setup() -> Fixture {
    let xdg = tempfile::tempdir().expect("xdg tempdir");
    let work = tempfile::tempdir().expect("work tempdir");
    let sessions = xdg.path().join("prior").join("sessions");
    Fixture {
        _xdg: xdg,
        work,
        sessions,
    }
}

/// Seed one session file under the child-cwd's encoded dir. Returns the
/// session id. `model` (provider, model_id) is stamped into the header so
/// resume computes a `/model …` prefill for it.
fn seed_session(fx: &Fixture, model: (&str, &str), text: &str) -> String {
    use prior::session::SessionHeader;
    let work_path = Path::new(fx.work.path());
    let dir = fx.sessions.join(encode_cwd(work_path));
    std::fs::create_dir_all(&dir).expect("session dir");
    let header = SessionHeader::with_model_context(
        work_path,
        Some(model.0.to_string()),
        Some(model.1.to_string()),
        None,
    );
    let id = header.id.clone();
    let path = dir.join(session_filename(&chrono::Utc::now()));
    let user_entry = serde_json::json!({
        "id": "e1",
        "timestamp": "2025-01-01T00:00:00.000Z",
        "type": "message",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    });
    let leaf = serde_json::json!({
        "id": "leaf1",
        "timestamp": "2025-01-01T00:00:01.000Z",
        "type": "leaf",
        "targetId": "e1",
    });
    let contents = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&user_entry).unwrap(),
        serde_json::to_string(&leaf).unwrap(),
    );
    std::fs::write(&path, contents).unwrap();
    id
}

/// Count every `.jsonl` under the sessions dir (recursive).
fn count_sessions(sessions: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(sessions, &mut n);
    n
}

/// Poll until the session-file count equals `want` (delete/creation are
/// async-ish on disk).
fn wait_session_count(sessions: &Path, want: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let got = count_sessions(sessions);
        if got == want {
            return;
        }
        if Instant::now() > deadline {
            panic!("expected {want} session file(s) {what}, found {got}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn prior: llamacpp provider (no API key; base url points at a
/// dead local port — client construction never pings it), model `test`,
/// isolated XDG, child cwd = the fixture work dir.
fn spawn_prior(
    fx: &Fixture,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Arc<Mutex<Vec<u8>>>,
    Box<dyn Write + Send>,
) {
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
    cmd.args(["-p", "llamacpp", "-m", "test"]);
    cmd.cwd(fx.work.path());
    cmd.env("XDG_CONFIG_HOME", fx._xdg.path());
    cmd.env("LLAMACPP_API_BASE_URL", "http://127.0.0.1:1");

    let child = pair.slave.spawn_command(cmd).expect("spawn prior");
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, _stop) = spawn_reader(reader);
    let writer = pair.master.take_writer().expect("take writer");

    assert_wait(&bytes, "prior -", "the startup banner");
    // Answer the DSR cursor query so later `cursor_row()` calls don't eat
    // typed bytes while scanning stdin for a reply (mock_turn handshake).
    let mut writer = writer;
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(200));

    // Keep the child alive; tests kill it if quit doesn't land.
    std::mem::forget(pair.slave); // (slave handle kept by child)
    (child, bytes, writer)
}

/// Satisfy any in-flight DSR cursor scan so the next payload bytes are not
/// swallowed by `cursor_row()`'s 60ms non-blocking stdin window. Every
/// `emit_commit` issues a DSR query (see `Repl::emit_commit`), and a scan
/// that overlaps a typed line eats those bytes — the classic "half a
/// command vanished" E2E flake. A stray reply with no pending scan shows
/// up as a cursor-position event, which the REPL ignores (mock_turn
/// precedent).
fn poke_dsr(writer: &mut Box<dyn Write + Send>) {
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(80));
}

fn type_line(writer: &mut Box<dyn Write + Send>, text: &str) {
    poke_dsr(writer);
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
}

fn press_enter(writer: &mut Box<dyn Write + Send>) {
    poke_dsr(writer);
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(100));
}

/// Clear the input line with backspaces (works for any residual prefill —
/// resume leaves a `/model …` in the buffer, and appended text would turn
/// `/quit ` into `/model …/quit `).
fn clear_input(writer: &mut Box<dyn Write + Send>) {
    poke_dsr(writer);
    let del = vec![0x7fu8; 64];
    let _ = writer.write_all(&del);
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
}

/// `/quit ` → committed line → the process exits on its own.
fn quit_and_wait(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    bytes: &Arc<Mutex<Vec<u8>>>,
) {
    // (caller has already typed "/quit " + Enter; we just await the exit —
    // kept as a helper for symmetry.)
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            assert!(status.success(), "prior must exit cleanly: {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("prior did not exit after /quit\n{}", screen_tail(bytes));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// THE regression: menu traffic (delete + thinking switch → convergence →
/// journal record against an unpersisted current session) must never mint
/// empty session files. Also smokes `/session` listing + delete + quit.
#[test]
fn menu_traffic_never_creates_empty_session_files() {
    let fx = setup();
    let uuid1 = seed_session(&fx, ("llamacpp", "seed-a"), "seed-one");
    let _uuid2 = seed_session(&fx, ("llamacpp", "seed-b"), "seed-two");
    assert_eq!(count_sessions(&fx.sessions), 2, "seeded");

    let (mut child, bytes, mut writer) = spawn_prior(&fx);

    // --- /session listing smoke: the overlay shows the seeded session.
    type_line(&mut writer, "/session ");
    assert_wait(
        &bytes,
        "seed-one",
        "/session listing shows the seeded session",
    );
    thread::sleep(Duration::from_millis(300));

    // --- /session delete (two-phase: prepare → confirm → submit).
    type_line(&mut writer, &format!("/session {uuid1} delete "));
    thread::sleep(Duration::from_millis(200));
    press_enter(&mut writer);
    assert_wait(
        &bytes,
        "Permanently delete",
        "the delete confirm frame (prepare armed)",
    );
    press_enter(&mut writer);
    assert_wait(
        &bytes,
        &format!("/session {uuid1} delete"),
        "the committed delete line",
    );
    wait_session_count(&fx.sessions, 1, "after menu delete");

    // --- Thinking switch → convergence rebuild + journal record against
    // the UNPERSISTED auto-created current session. Historically this is
    // where `ensure_persisted` leaked a header+marker "empty" file.
    type_line(&mut writer, "/reasoning high ");
    thread::sleep(Duration::from_millis(200));
    press_enter(&mut writer);
    assert_wait(&bytes, "/reasoning high", "the committed thinking line");
    // Give convergence + recording a beat, then demand: no new files.
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        count_sessions(&fx.sessions),
        1,
        "a thinking commit must not create an empty session file \
         (regression: ensure_persisted header+marker leak)\n{}",
        screen_tail(&bytes)
    );

    // --- quit smoke: the menu quit exits the process cleanly.
    type_line(&mut writer, "/quit ");
    thread::sleep(Duration::from_millis(200));
    press_enter(&mut writer);
    assert_wait(&bytes, "/quit", "the committed quit line");
    quit_and_wait(&mut child, &bytes);

    assert_eq!(count_sessions(&fx.sessions), 1, "no files after quit");
}

/// Resume smoke: replay renders the seeded history into the terminal, and
/// the session's recorded model (≠ the running one) becomes a `/model …`
/// prefill on the next prompt.
#[test]
fn resume_replays_history_and_seeds_model_prefill() {
    let fx = setup();
    // Recorded model differs from the running `-m test` → prefill armed.
    let uuid = seed_session(&fx, ("llamacpp", "old-e2e-model"), "hello-e2e");
    assert_eq!(count_sessions(&fx.sessions), 1, "seeded");

    let (mut child, bytes, mut writer) = spawn_prior(&fx);

    // Resume with an explicit replay count (deterministic regardless of
    // the settings default): history must render on screen.
    type_line(&mut writer, &format!("/session {uuid} resume 3 "));
    thread::sleep(Duration::from_millis(200));
    press_enter(&mut writer);

    assert_wait(&bytes, "hello-e2e", "the replayed history text");
    assert_wait(
        &bytes,
        "/model llamacpp:old-e2e-model",
        "the model prefill on the next prompt",
    );

    // Resume must not mint empty files either.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        count_sessions(&fx.sessions),
        1,
        "no empty files after resume"
    );

    // quit smoke (second path). The resume prefill occupies the input
    // line — clear it first or "/quit " appends to the `/model …`.
    clear_input(&mut writer);
    type_line(&mut writer, "/quit ");
    thread::sleep(Duration::from_millis(200));
    press_enter(&mut writer);
    quit_and_wait(&mut child, &bytes);
}
