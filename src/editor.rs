//! The single external-editor entry point (doc/editor_cleanup.md §2).
//!
//! Every `$EDITOR` interaction goes through [`run_editor`], which owns the
//! whole terminal turnover: the in-flight reader's stdin gate, raw mode off
//! for the child's lifetime, raw mode back on, and the DSR re-anchor after a
//! foreign screen writer. No call site keeps local guard/DSR logic.
//!
//! [`edit_in_editor`] is this module's private mechanism (tempfile + spawn +
//! read-back); everything outside this module must use [`run_editor`].

use crate::output::Output;
use crate::repl::TermGuard;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the caller intends to do with the editor session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorMode {
    /// The text is editable: the tempfile is writable (mode 0600) and the
    /// edited content is read back and returned for the caller to persist.
    Edit,
    /// View-only: the tempfile is created read-only (mode 0400 — the editor
    /// sees a file it cannot save; `vim` behaves like `:view`) and no result
    /// is read back; [`run_editor`] returns the input unchanged. The
    /// permission bit is the portable signal — no per-editor `-R`/`-v` flags —
    /// and a view-only caller cannot persist even a force-write.
    View,
}

/// Raise the in-flight reader's stdin gate for the duration of an operation
/// that owns stdin, restoring the *previous* value on drop (nesting-safe: an
/// outer pause survives an inner one).
struct ReaderHold(Option<Arc<AtomicBool>>, bool);

impl ReaderHold {
    fn raise(out: &dyn Output) -> Self {
        let gate = out.confirm_gate();
        let prev = gate
            .as_ref()
            .map(|g| g.swap(true, Ordering::SeqCst))
            .unwrap_or(false);
        Self(gate, prev)
    }
}

impl Drop for ReaderHold {
    fn drop(&mut self) {
        if let Some(g) = &self.0 {
            g.store(self.1, Ordering::SeqCst);
        }
    }
}

/// Hand the terminal to an external editor and take it back — the only place
/// that sequence exists (doc/editor_cleanup.md §2):
///
/// 1. Raise the reader gate for the whole window — including the DSR below:
///    its reply arrives on stdin, so an unguarded reader would swallow it.
/// 2. Drop raw mode, run `$EDITOR` on a tempfile (foreground child with
///    inherited stdio), re-enter raw mode.
/// 3. Re-anchor the draw cache (DSR): the editor was a foreign screen writer,
///    so the frame region can no longer be trusted (plan §4.2). Unconditional
///    — even a failed launch may have touched the screen.
///
/// Blocking by design: while the editor owns the tty nothing else may write
/// to it, so there is nothing for a concurrent task to do (§7 of the plan).
pub(crate) fn run_editor(
    guard: &TermGuard,
    out: &dyn Output,
    current: &str,
    mode: EditorMode,
) -> io::Result<String> {
    let hold = ReaderHold::raise(out);
    let _ = guard.stop();
    let edited = edit_in_editor(current, mode);
    let _ = guard.start();
    let _ = out.cursor_row();
    drop(hold); // gate down only after the DSR has consumed its reply
    edited
}

/// Resolve the external editor command from environment variables.
/// Priority: `$VISUAL` > `$EDITOR` > `"vim"`.
/// Returns `(command, args)` where args may include flags e.g. `("emacs", ["-nw"])`.
fn resolve_editor() -> (String, Vec<String>) {
    let raw = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vim".into());
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    let cmd = parts.first().cloned().unwrap_or_else(|| "vim".into());
    let args: Vec<String> = parts.into_iter().skip(1).collect();
    (cmd, args)
}

/// Open `current` in `$EDITOR` (foreground child process) and return the
/// text per `mode`. The editor inherits the terminal directly (stdin/stdout/
/// stderr) — callers must not reach here; go through [`run_editor`], which
/// owns the raw-mode turnover around this call.
///
/// In `EditorMode::Edit`, a single trailing `\n` is stripped on read-back
/// because most editors add one; internal newlines and intentional trailing
/// blank lines are preserved. In `EditorMode::View` nothing is read back and
/// `current` is returned unchanged. The tempfile is cleaned up on every
/// return path.
fn edit_in_editor(current: &str, mode: EditorMode) -> io::Result<String> {
    use std::os::unix::fs::PermissionsExt;

    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let mut path = PathBuf::from(tmpdir);
    path.push(format!("picocode_prompt_{}", std::process::id()));

    // Write current content. 0600 keeps other users from reading the prompt
    // mid-edit; View uses 0400 so the editor sees a file it cannot save.
    let mode_bits = match mode {
        EditorMode::Edit => 0o600,
        EditorMode::View => 0o400,
    };
    {
        let mut f = std::fs::File::create(&path)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode_bits))?;
        f.write_all(current.as_bytes())?;
        f.flush()?;
    }

    // Launch editor.
    let (cmd, mut args) = resolve_editor();
    args.push(path.to_string_lossy().to_string());

    let status = std::process::Command::new(&cmd)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&path);
        })?;

    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(io::Error::other(format!(
            "editor exited with status {:?}",
            status.code()
        )));
    }

    // View-only: nothing to persist, so nothing to read back.
    if mode == EditorMode::View {
        let _ = std::fs::remove_file(&path);
        return Ok(current.to_string());
    }

    // Read back.
    let mut result = std::fs::read_to_string(&path).inspect_err(|_| {
        let _ = std::fs::remove_file(&path);
    })?;

    // Strip exactly one trailing newline (most editors add one).
    if result.ends_with('\n') {
        result.pop();
    }

    let _ = std::fs::remove_file(&path);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ===== resolve_editor / edit_in_editor tests =====
    //
    // These tests modify environment variables (VISUAL / EDITOR) and must
    // run serially to avoid races.  `ENV_LOCK` serialises them.

    use std::os::unix::fs::PermissionsExt;

    /// Global mutex that serialises all env-var-modifying tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Save and restore environment variables around a test closure.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => unsafe {
                    std::env::set_var(k, val);
                },
                None => unsafe {
                    std::env::remove_var(k);
                },
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(val) => unsafe {
                    std::env::set_var(&k, &val);
                },
                None => unsafe {
                    std::env::remove_var(&k);
                },
            }
        }
    }

    #[test]
    fn resolve_editor_prefers_visual() {
        with_env(
            &[("VISUAL", Some("myeditor")), ("EDITOR", Some("other"))],
            || {
                let (cmd, args) = super::resolve_editor();
                assert_eq!(cmd, "myeditor", "VISUAL must take priority");
                assert!(args.is_empty(), "no flags: {:?}", args);
            },
        );
    }

    #[test]
    fn resolve_editor_falls_back_to_editor() {
        with_env(&[("VISUAL", None), ("EDITOR", Some("nano"))], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "nano", "EDITOR fallback");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_defaults_to_vim() {
        with_env(&[("VISUAL", None), ("EDITOR", None)], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "vim", "default to vim");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_splits_flags() {
        with_env(
            &[
                ("VISUAL", Some("emacs -nw --no-site-file")),
                ("EDITOR", None),
            ],
            || {
                let (cmd, args) = super::resolve_editor();
                assert_eq!(cmd, "emacs");
                assert_eq!(args, vec!["-nw", "--no-site-file"]);
            },
        );
    }

    /// Write a mock editor script into `dir` and make it executable.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let script = dir.join(name);
        {
            let mut f = std::fs::File::create(&script).expect("create script");
            f.write_all(b"#!/bin/sh\n").unwrap();
            f.write_all(body.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        script
    }

    /// Helper: run `edit_in_editor(Edit)` with `current` content by setting
    /// `EDITOR` to a shell script that writes `expected_output` to the file.
    fn mock_edit(current: &str, editor_body: &str, expected_output: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(dir.path(), "mock_editor.sh", editor_body);

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor(current, EditorMode::Edit);
                assert!(result.is_ok(), "edit_in_editor failed: {:?}", result.err());
                assert_eq!(result.unwrap(), expected_output);
            },
        );
    }

    #[test]
    fn edit_in_editor_roundtrip() {
        mock_edit(
            "hello world",
            "printf '%s' 'edited content' > \"$1\"\n",
            "edited content",
        );
    }

    #[test]
    fn edit_in_editor_empty_buffer() {
        mock_edit("", "printf '%s' 'new content' > \"$1\"\n", "new content");
    }

    #[test]
    fn edit_in_editor_trailing_newline_stripped() {
        mock_edit(
            "hello",
            "printf '%s\\n' 'hello world' > \"$1\"\n",
            "hello world",
        );
    }

    #[test]
    fn edit_in_editor_preserves_internal_newlines() {
        mock_edit(
            "line1\nline2\nline3",
            "printf '%s\\n%s\\n%s' 'aaa' 'bbb' 'ccc' > \"$1\"\n",
            "aaa\nbbb\nccc",
        );
    }

    #[test]
    fn edit_in_editor_keeps_trailing_blank_lines() {
        // If the file ends with "a\n\n" (a, plus one blank line), stripping
        // one trailing \n must leave "a\n" — the blank line is preserved.
        mock_edit("a\n\n", "printf '%s\\n\\n' 'a' > \"$1\"\n", "a\n");
    }

    #[test]
    fn edit_in_editor_nonzero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(dir.path(), "failing_editor.sh", "exit 1\n");

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor("original", EditorMode::Edit);
                assert!(result.is_err(), "non-zero exit must return error");
            },
        );
    }

    #[test]
    fn edit_in_editor_nonexistent_command() {
        with_env(
            &[
                ("VISUAL", Some("/nonexistent/editor_binary")),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor("hello", EditorMode::Edit);
                assert!(result.is_err(), "nonexistent command must return error");
            },
        );
    }

    #[test]
    fn edit_in_editor_view_hands_out_a_readonly_file() {
        // The mock editor records whether the file was writable and changes
        // nothing; View must hand it a 0400 file and return the input.
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = dir.path().join("meta");
        let script = write_script(
            dir.path(),
            "view_editor.sh",
            &format!(
                "if [ -w \"$1\" ]; then printf 'writable' > '{m}'; else printf 'readonly' > '{m}'; fi\n",
                m = meta.display()
            ),
        );

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor("original", EditorMode::View);
                assert_eq!(result.expect("view edit"), "original");
            },
        );
        assert_eq!(
            std::fs::read_to_string(&meta).expect("meta"),
            "readonly",
            "View must create the tempfile read-only (0400)"
        );
    }

    /// Output with a live reader gate; `cursor_row()` records whether the
    /// gate was still raised when the DSR ran.
    struct GatedOutput {
        gate: Arc<AtomicBool>,
        dsr_saw_gate: AtomicBool,
    }

    impl Output for GatedOutput {
        fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
            Some(self.gate.clone())
        }
        fn cursor_row(&self) -> usize {
            self.dsr_saw_gate
                .store(self.gate.load(Ordering::SeqCst), Ordering::SeqCst);
            1
        }
        fn display_text(&self, _text: &str) {}
        fn display_tool_call(&self, _name: &str, _args: &serde_json::Value) {}
        fn display_tool_result(&self, _result: &str) {}
        fn get_user_input(&self) -> String {
            String::new()
        }
        fn display_error(&self, _error: &str) {}
        fn display_system(&self, _text: &str) {}
        fn confirm(&self, _message: &str) -> crate::output::Confirmation {
            crate::output::Confirmation::No
        }
        fn display_separator(&self) {}
        fn display_thinking(&self, _message: &str) {}
        fn stop_thinking(&self) {}
        fn display_header(
            &self,
            _provider: &str,
            _model: &str,
            _yolo: bool,
            _limit: usize,
            _persona: Option<&str>,
        ) {
        }
    }

    /// Run `run_editor` with a no-op mock editor against a gated output.
    /// Returns `(gate seen raised during DSR, gate value after the call)`.
    fn gated_run(pre_raised: bool) -> (bool, bool) {
        let gate = Arc::new(AtomicBool::new(pre_raised));
        let out = GatedOutput {
            gate: gate.clone(),
            dsr_saw_gate: AtomicBool::new(false),
        };
        let guard = TermGuard::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(dir.path(), "noop_editor.sh", "exit 0\n");

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let r = run_editor(&guard, &out, "text", EditorMode::View);
                assert!(r.is_ok(), "run_editor failed: {:?}", r.err());
            },
        );
        let _ = guard.stop(); // undo a possible raw-mode entry on a real tty
        (
            out.dsr_saw_gate.load(Ordering::SeqCst),
            gate.load(Ordering::SeqCst),
        )
    }

    /// Same root cause as `run_editor_edit_roundtrip`: `run_editor`'s DSR
    /// round-trip has no terminal to answer it under `cargo test`.
    #[test]
    #[ignore = "blocks waiting for a DSR reply that never arrives without a tty"]
    fn run_editor_holds_the_gate_across_the_dsr() {
        let (saw_dsr, after) = gated_run(false);
        assert!(
            saw_dsr,
            "the DSR reply arrives on stdin — the reader gate must still be up"
        );
        assert!(!after, "gate restored to its pre-call value");
    }

    /// Same root cause as `run_editor_edit_roundtrip`: `run_editor`'s DSR
    /// round-trip has no terminal to answer it under `cargo test`.
    #[test]
    #[ignore = "blocks waiting for a DSR reply that never arrives without a tty"]
    fn run_editor_restores_an_outer_pause() {
        let (saw_dsr, after) = gated_run(true);
        assert!(saw_dsr);
        assert!(after, "an outer pause must survive the editor's hold");
    }

    /// Disabled: it blocks forever instead of failing.
    ///
    /// `run_editor` finishes by querying the cursor position (DSR, `ESC[6n`) and
    /// reading the reply from stdin. Under `cargo test` stdin is not a terminal, so
    /// nothing ever replies and the test hangs until the harness is killed — which
    /// silently truncated the tail of the suite, hiding whatever came after.
    ///
    /// Run it interactively (on a real tty) with
    /// `cargo test --lib -- --ignored run_editor`. The proper fix is a pty:
    /// `portable-pty` and `vt100` are already dev-deps, so the roundtrip can be
    /// driven against a terminal that answers the DSR.
    #[test]
    #[ignore = "blocks waiting for a DSR reply that never arrives without a tty"]
    fn run_editor_edit_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(
            dir.path(),
            "mock_editor.sh",
            "printf '%s' 'edited content' > \"$1\"\n",
        );
        let out = NoGateOutput;
        let guard = TermGuard::new();

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let r = run_editor(&guard, &out, "hello world", EditorMode::Edit);
                let _ = guard.stop(); // undo a possible raw-mode entry on a real tty
                assert_eq!(r.expect("run_editor"), "edited content");
            },
        );
    }

    /// Minimal output without a reader gate (quiet/pipe-style).
    struct NoGateOutput;

    impl Output for NoGateOutput {
        fn display_text(&self, _text: &str) {}
        fn display_tool_call(&self, _name: &str, _args: &serde_json::Value) {}
        fn display_tool_result(&self, _result: &str) {}
        fn get_user_input(&self) -> String {
            String::new()
        }
        fn display_error(&self, _error: &str) {}
        fn display_system(&self, _text: &str) {}
        fn confirm(&self, _message: &str) -> crate::output::Confirmation {
            crate::output::Confirmation::No
        }
        fn display_separator(&self) {}
        fn display_thinking(&self, _message: &str) {}
        fn stop_thinking(&self) {}
        fn display_header(
            &self,
            _provider: &str,
            _model: &str,
            _yolo: bool,
            _limit: usize,
            _persona: Option<&str>,
        ) {
        }
    }
}
