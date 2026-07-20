# External-Editor Integration (`Ctrl-E`)

## 1. Summary

Typing `Ctrl-E` in the REPL prompt drops raw mode, opens the current prompt
buffer in `$EDITOR` (a foreground child process that takes over the whole
terminal), waits for the editor to exit, re-enters raw mode, and replaces the
prompt buffer with the saved file content.  This gives users access to the full
editing power of vim/emacs/nano for composing complex multi-line prompts,
shell snippets, or structured text.

## 2. Critical evaluation of the rough sketch

### What works

- **Foreground child + terminal takeover** is the standard Unix pattern (`sudo`,
  `git commit`, `visudo`).  The kernel handles job control: the child inherits
  the pty, reads keystrokes, and paints the screen while picocode is `waitpid`-ing.
  Re-entering raw mode after the child exits is the Ctrl-Z pattern, already
  proven in the codebase.

- **Tempfile round-trip** (`editor <tmpfile>` → `inotify`/`waitpid` → read back)
  is the defacto standard for `$EDITOR` integration (git, crontab, visudo).  No
  IPC complexity, no pty-sharing race, no signal forwarding.

- **No new sandbox hole.**  The editor runs **outside** the sandbox (plain
  `std::process::Command`, same as Ctrl-Z would resume into a shell).  The
  user already has direct access to a shell via Ctrl-Z; adding `$EDITOR` access
  adds no new privilege-escalation surface.

### What needs careful design

| Concern | Mitigation |
|---------|------------|
| **Editor never starts / crashes** | Detect non-zero exit, retry or abort.  Also handle `$EDITOR` unset → fallback chain. |
| **Tempfile race (editor saves + picocode reads while editor still writing)** | `waitpid` guarantees the file is fully written before picocode reads it.  No `inotify` needed. |
| **Tempfile contains stale content from a previous invocation** | Unlink before `waitpid` return read?  No — we read, then unlink.  The OS retains the inode until the last fd (ours) is closed (`open/unlink` trick). |
| **Prompt is multi-line (includes `\n`)** | The tempfile stores the raw prompt text as-is (newlines preserved).  The editor shows them as line breaks naturally.  No escaping needed. |
| **User deletes all content (empty buffer)** | Trivial: read back an empty string, set the prompt to `""`.  The user can then type fresh content or press Esc to cancel. |
| **Editor writes a trailing newline** | Most editors add a trailing `\n`.  Trim exactly one trailing `\n` when reading back (but not more — preserve the user's intentional blank lines). |
| **`$EDITOR` contains flags (e.g. `vim -p`)** | `Command::new` splits on first word only.  Use `sh -c "$EDITOR $tmpfile"` or parse `EDITOR` with `shlex`.  Shell-parse is more compatible. |
| **Tempfile path predictability / security** | Write to a random path under `/tmp` (or `$TMPDIR`).  Use `std::os::unix::fs::PermissionsExt` to set mode `0600` so other users cannot read the prompt mid-edit.  Clean up with `unlink` after read-back. |

### Not in scope (v1)

- **Cursor / scrollback position after editor close:** The user sees the editor
  screen, then exits, and the REPL redraws the prompt from scratch (the
  `redraw` closure handles this — it clears from `input_row` down).  No attempt
  to restore the previous scrollback state; the session transcript above the
  prompt is preserved because `redraw` only clears from `input_row` downward.
  The greeter/status/divider/session content above the input is untouched.

- **Pty-sharing live editor:** Running picocode in raw mode while hosting an
  editor *inside* the same pty (like Emacs `M-x term`) is a different feature
  and would require full terminal multiplexer logic.  Out of scope.

- **Windows:** `$EDITOR` integration works on Unix-likes only (the Ctrl-Z
  suspend pattern is also Unix-only).  A future `start`-based fallback for
  Windows is possible but not planned here.

## 3. Implementation plan

### 3.1 Changes by file

#### 3.1.1 `src/repl.rs` — the Ctrl-E handler

**A. Add an `editor_edit` method to `Repl`** (or a free function) that:

1. Resolves the editor command:
   - `$VISUAL` (first priority, per POSIX convention)
   - `$EDITOR` (fallback)
   - Hard-coded fallback chain: `["vim", "nano", "ed"]`
2. Writes `editor.text()` to a tempfile.
3. Calls `self.guard.stop()` to leave raw mode (same as Ctrl-Z).
4. Runs the editor as a foreground child via `std::process::Command`,
   passing the tempfile path as the sole positional argument after shell
   expansion of `$VISUAL`/`$EDITOR`.
5. `waitpid` for the child to finish.
6. Reads the tempfile back.
7. Strips exactly one trailing `\n` (but preserves all other content,
   including internal newlines and trailing blank lines beyond the first).
8. Calls `self.guard.start()` to re-enter raw mode.
9. Returns the new text (or `None` on abort/error).

**B. Hook Ctrl-E in the main key-dispatch loop** (`read_input`, around
line 759 in the current file, alongside the Ctrl-C/Ctrl-Z handlers):

```
if let KeyCode::Char('e') = key.code {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        // Must not be inside an overlay (kind != 0).
        // If the overlay is a slash-command being composed, we could
        // still open the editor on the whole buffer — the user can
        // delete the slash content in the editor.  Simpler v1: only
        // allow Ctrl-E when kind == 0 (no overlay).
        if kind == 0 {
            if let Some(new_text) = self.editor_edit(&editor) {
                editor.set(&new_text);
            }
        }
        continue;
    }
}
```

**C. Shell-expansion helper for `$EDITOR`**

`$EDITOR` commonly contains flags: `"vim -p"`, `"emacs -nw"`, `"code -w"`.
A simple `split_whitespace().next()` would lose the flags.  Instead:

```
use std::process::Command;

fn resolve_editor() -> (String, Vec<String>) {
    let raw = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vim".into());

    // Simple shell-style word split (respects single quotes).
    // For full correctness we could use the `shlex` crate, but a
    // simple split suffices for the common cases.
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    let cmd = parts.first().cloned().unwrap_or_else(|| "vim".into());
    let args: Vec<String> = parts.into_iter().skip(1).collect();
    (cmd, args)
}
```

**D. Tempfile management**

```rust
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

fn edit_in_editor(current: &str) -> io::Result<String> {
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let mut path = std::path::PathBuf::from(tmpdir);
    path.push(format!("picocode_prompt_{}.txt", std::process::id()));

    // Write current content, mode 0600
    {
        let mut f = std::fs::File::create(&path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(current.as_bytes())?;
        f.flush()?;
    }

    // Launch editor
    let (cmd, mut args) = resolve_editor();
    args.push(path.to_string_lossy().to_string());
    let status = std::process::Command::new(&cmd)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(io::Error::new(io::ErrorKind::Other,
            format!("editor exited with status {:?}", status.code())));
    }

    // Read back, strip one trailing \n
    let mut result = std::fs::read_to_string(&path)?;
    if result.ends_with('\n') {
        result.pop();
    }
    let _ = std::fs::remove_file(&path);
    Ok(result)
}
```

**E. Integration into the redraw lifecycle**

After the editor exits and `self.guard.start()` re-enters raw mode, the
next iteration of the `read_input` loop will redraw the prompt via the
existing `redraw` closure.  The `input_row` tracking still works because:

- The `RESUMED` atomic is not involved (we didn't suspend via SIGTSTP).
- But the terminal may have scrolled during the editor session.  To be
  safe, re-anchor `input_row` by calling `output.cursor_row()` after
  re-entering raw mode (the same way the Ctrl-Z handler does it
  implicitly at the top of the loop via the `cur_h != prev_term_h` check
  on line 458).  We can just set a flag or jump to the top of the loop
  via `continue`.

Simplest approach: after `self.guard.start()`, set `input_row = output.cursor_row();`
before falling through to the next redraw.  This ensures the next
`redraw` writes at the correct screen position.

#### 3.1.2 `src/settings.rs` — optional `editor_cmd` setting (deferred)

A `SettingKey::EditorCmd` would let users override `$EDITOR` from within
picocode's `/set` menu.  This is **not required for v1** since `$VISUAL`
/ `$EDITOR` environment variables are the standard configuration channel.
Add it as a follow-up if users request it.

#### 3.1.3 `doc/editor_integration.md` (this file) — plan

### 3.2 Testing strategy

| Layer | Approach |
|-------|----------|
| **`edit_in_editor` unit** | Test with `$EDITOR=cat` (writes stdin to stdout, ignores the file arg — actually `cat` doesn't modify the file.  Use `$EDITOR=cp`?  Better: `$EDITOR=tee` overwrites; or a test helper shell script).  Write current text, run editor, read back. |
| **Ctrl-E dispatch** | Unit-test the key-matching logic in isolation by calling `edit_key` (or the new handler) with a mock editor that writes a known string. |
| **Tempfile edge cases** | Empty buffer, trailing newlines, Unicode content, editor crash (non-zero exit), editor unset, pipe stall. |
| **vt100 integration** | Extend the existing vt100 tests in `repl.rs` to simulate the full cycle: `\x1b[5E` (Ctrl-E → ... well, we can't actually run a subprocess inside a pty test.  The existing `tests/render_scroll.rs` style (real pty + capturing bytes) could drive a real editor if `$EDITOR` is set to a trivial script, but that's expensive.  Prefer pure-logic unit tests for the editor round-trip and save the pty test for integration CI.) |

### 3.3 Risks and mitigations

| Risk | Mitigation |
|------|------------|
| **Editor child inherits raw-mode TTY state** | `self.guard.stop()` calls `disable_raw_mode()` which restores the original termios.  The editor then sees a cooked terminal (its preferred mode).  After the editor exits, `self.guard.start()` re-applies raw mode. |
| **Editor is a GUI app (e.g. `code`, `gvim`)** | The fork+`waitpid` pattern works fine: the GUI window opens, the terminal is NOT taken over (the child detaches from the pty in practice), and `waitpid` returns when the user closes the editor window.  The prompt reappears.  No special handling needed. |
| **Editor re-parents / daemonizes** | `Command::status` calls `waitpid` on the direct child.  If the editor spawns a background daemon and exits immediately (common with GUI launchers), picocode resumes too early — the tempfile may not yet be written.  **Mitigation:** detect editors known to daemonize (`code`, `gvim`) and sleep-wait for the file to reach a stable size, or document that users should set `$EDITOR` to a blocking CLI editor like `code -w` (the `-w` flag makes `code` block until the window closes). |
| **Large prompt (megabytes)** | Read the whole file into memory.  The prompt is bounded by user patience, not by memory.  This is fine. |
| **Tempfile left behind on crash** | `remove_file` before return.  On panic/crash, the OS cleans up `/tmp` on the next reboot or via `tmpreaper`.  Not a concern for a v1. |

### 3.4 Order of implementation

1. **Add `resolve_editor` + `edit_in_editor` free functions** in `src/repl.rs`
   (before or after the `Repl` impl block).  Write unit tests for:
   - `resolve_editor` with `$VISUAL`, `$EDITOR`, neither set.
   - `edit_in_editor` with a mock editor (shell one-liner that appends text).
   - Empty-buffer round trip.
   - Trailing-newline stripping.
   - Non-zero exit handling.

2. **Hook Ctrl-E in `read_input`** — the 4-line block shown in §3.1.1.B.
   Write unit tests that call the handler through `edit_key` (or directly).

3. **Wire `input_row` re-anchor** after editor exit (the `output.cursor_row()`
   call mentioned in §3.1.1.E).  The simplest approach: after `input_row =
   output.cursor_row()`, `continue` to the loop top so the next redraw uses
   the corrected row.  This is the same pattern used on line 459 for resize.

4. **Add one integration test** in `tests/` (optional for v1) that sets
   `$EDITOR` to a known script, types Ctrl-E via a pty, and checks the
   resulting buffer.  Model it on `tests/render_scroll.rs`.
