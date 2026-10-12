//! The single external-editor entry point (doc/editor_cleanup.md §2).
//!
//! Every `$EDITOR` interaction goes through [`run_editor`], which owns the
//! whole terminal turnover: pausing the sole stdin owner (the persistent
//! reader), raw mode off for the child's lifetime, raw mode back on, and the
//! DSR re-anchor after a foreign screen writer. No call site keeps local
//! guard/DSR logic.
//!
//! [`edit_in_editor`] is this module's private mechanism (tempfile + spawn +
//! read-back); everything outside this module must use [`run_editor`].

use crate::output::Output;
use crate::repl::TermGuard;
use std::io::{self, Write};
use std::path::PathBuf;

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

/// Hand the terminal to an external editor and take it back — the only place
/// that sequence exists (doc/editor_cleanup.md §2):
///
/// 1. Pause the sole stdin owner (the persistent reader) for the whole window —
///    the editor child's keystrokes *and* the DSR reply below arrive on stdin,
///    so an unpaused reader would swallow them. Nesting-safe.
/// 2. Drop raw mode, run `$EDITOR` on a tempfile (foreground child with
///    inherited stdio), re-enter raw mode.
/// 3. Re-anchor the draw cache (DSR): the editor was a foreign screen writer,
///    so the frame region can no longer be trusted (plan §4.2). Unconditional
///    — even a failed launch may have touched the screen. The pause is still
///    held across this query (the outer guard), so its reply is safe too.
///
/// Blocking by design: while the editor owns the tty nothing else may write
/// to it, so there is nothing for a concurrent task to do (§7 of the plan).
pub(crate) fn run_editor(
    guard: &TermGuard,
    out: &dyn Output,
    current: &str,
    mode: EditorMode,
) -> io::Result<String> {
    let _pause = crate::termio::ReaderPauseGuard::raise();
    let _ = guard.stop();
    let edited = edit_in_editor(current, mode);
    let _ = guard.start();
    let _ = out.cursor_row();
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
}
