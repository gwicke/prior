# picocode Rendering & I/O Notes

This document summarizes how `picocode` handles terminal input modes and
output formatting. Sources: `src/output.rs`, `src/main.rs`, `src/agent.rs`,
and `Cargo.toml`.

---

## 1. Input Mode: Cooked Only

The application uses **exclusively cooked (line-buffered) mode**. There is no
raw / non-canonical terminal handling anywhere in the codebase.

### Evidence

- All user input flows through `std::io::stdin().read_line()` — the canonical
  cooked-mode API. Occurrences in `src/output.rs`:
  - `output.rs:76` — `QuietOutput::confirm()`
  - `output.rs:333` — `RichOutput::get_user_input()` (main REPL prompt)
  - `output.rs:356` — `RichOutput::confirm()` (delegates to `get_user_input()`)
- **No terminal-mode manipulation exists.** Searches for `termios`,
  `tcsetattr`/`tcgetattr`, `ICANON`, `crossterm`/`termion` raw-screen, and
  `AlternateScreen` return zero hits in `src/`.
- The only terminal-related dependencies are `console` (for `style`/`Term`
  rendering) and `indicatif` (spinners) — neither enters raw mode. `console`
  is used purely for ANSI styling and terminal sizing.
- The REPL loop (`agent.rs:38-51`) is line-oriented: it reads a whole line,
  trims it, checks for empty/exit, then processes.

### Implications

- The kernel line discipline buffers input, handles backspace/line editing,
  and delivers a line only after Enter.
- No byte-at-a-time / keystroke capture: no arrow-key navigation, incremental
  autocomplete, or key-driven menus.
- Input is read **synchronously/blocking** in the foreground `Output` impls.
  No `select`/`poll` on stdin.
- Non-interactive variants (`NoOutput`, and the empty-returning
  `get_user_input` in `QuietOutput`/`NoOutput`) bypass input — used for
  piped/automated runs where stdin may be a pipe.
- `Ctrl-C` is delivered in cooked mode as the default `SIGINT`, terminating
  the process rather than being caught for a soft cancel.

| Concern                  | Status                                          |
| ------------------------ | ----------------------------------------------- |
| Raw mode setup           | None                                            |
| Cooked mode              | Default; used everywhere via `read_line`        |
| Input dependency         | `std::io::stdin()` only (`console` for styling) |
| Async/non-blocking input | No                                              |
| Keystroke-level handling | No                                              |

---

## 2. Output Formatting: Pluggable `Output` Trait

Formatting is fully abstracted behind a single trait (`src/output.rs:15-34`):

```rust
fn display_text(&self, text: &str);
fn display_tool_call(&self, name: &str, args: &Value);
fn display_tool_result(&self, result: &str);
fn get_user_input(&self) -> String;
fn display_error(&self, error: &str);
fn display_system(&self, text: &str);
fn confirm(&self, message: &str) -> Confirmation;
fn display_separator(&self);
fn display_thinking(&self, message: &str);
fn stop_thinking(&self);
fn display_header(&self, provider, model, yolo, limit, persona);
```

The consumer (`CodeAgent`) holds it as `Arc<dyn Output>` (`agent.rs:82`), so
**formatting is decoupled from logic**. Four implementations, selected at
startup (`main.rs:126-130`) based on the `--quiet` flag / recipe:

- **Impl**: `ConsoleOutput`
  - **Use case**: Default interactive TTY
  - **Behavior**: Full pretty ANSI rendering
- **Impl**: `QuietOutput`
  - **Use case**: `--quiet` / recipe `quiet`
  - **Behavior**: Minimal: only errors to stderr; spinner for "thinking"; input
    via `read_line`
- **Impl**: `LogOutput`
  - **Use case**: Logging mode
  - **Behavior**: No stdout rendering — everything via `tracing`
    (`info!`/`error!`/`debug!`)
- **Impl**: `NoOutput`
  - **Use case**: Fully silent / non-interactive
  - **Behavior**: All display methods are no-ops; `confirm` returns `No`

### `ConsoleOutput` — the real formatting layer

1. **ANSI styling via `console::style`** (`output.rs:1`): `.cyan()`, `.green()`,
   `.bold()`, `.dim()`, `.red()`, `.blue()`, `.magenta()`, `.yellow()`.
2. **Unicode box-drawing glyphs** for a TUI-like look:
   - `─` separators (`output.rs:227`), repeated to terminal width via
     `Term::stdout().size().1`.
   - `⏺` bullets for text/tool-call/error lines (`output.rs:247,262,339`).
   - `⚠` for confirmations (`output.rs:349`), `❯` for the input prompt
     (`output.rs:330`).
   - `│` / `└` tree connectors for tool-result lines (`output.rs:304-323`).
3. **Markdown rendering**: `display_text` uses `termimad::print_inline(text)`
   (`output.rs:248`) for inline markdown (default width, no custom skin).
4. **Tool-call formatting** (`output.rs:252-266`): capitalizes the tool name's
   first letter; shows a 50-char single-value preview via `get_preview`
   (`output.rs:195-208`, collapses newlines, takes first JSON value), dim.
5. **Tool-result formatting** (`output.rs:268-325`):
   - Tries `serde_json` parse to unwrap a JSON string or join an array.
   - Strips `Toolset error:` / `ToolCallError:` prefixes; detects error
     status.
   - Renders lines with a tree prefix; **errors shown in full red**, normal
     results **truncated to 4 lines / 100 chars per line** with a
     `... +N lines` footer.
6. **Spinner / "thinking" state** (`output.rs:230-240`):
   `indicatif::ProgressBar` (120ms steady tick, blue spinner) in a
   `Mutex<Option<…>>`. Every display method calls `stop_thinking()` first to
   clear it before output.
7. **Header** (`output.rs:382-438`): centered ASCII avatar + status line built
   by a local `status()` closure that dims inactive flags and colors `yolo`
   red. Width-aware centering via `Term::stdout().size()`.

### Cross-cutting observations

- **Partial width awareness.** Separators and header read
  `Term::stdout().size()`, but tool-result truncation uses fixed constants
  (4 lines, 100 chars) — not terminal-width aware.
- **`Term::stdout()` re-queried per render** (not cached).
- **`truncate` (`output.rs:187`) byte-indexes UTF-8** (`&s[..max_len]`) and can
  **panic on multi-byte chars** straddling the boundary. Should use
  `char_indices()` or `chars().take()`.
- **No raw/alternate-screen TUI** — formatting is plain `println!` + ANSI +
  glyphs, not a managed screen buffer.
- **Logging path bypasses formatting** — `LogOutput` sends raw text to
  `tracing`; the pretty console layer is irrelevant in log mode.

### Flow summary

```text
main.rs selects Output impl (quiet flag)
   └─ agent loop calls output.display_* / get_user_input / confirm
        ├─ ConsoleOutput → ANSI + glyphs + termimad + indicatif spinner
        ├─ QuietOutput   → spinner + stderr errors only
        ├─ LogOutput     → tracing events
        └─ NoOutput      → no-ops
```

---

## 3. Potential Improvements

- **Fix `truncate` UTF-8 panic risk** — use `char_indices()` /
  `chars().take()` instead of byte slicing.
- **Make tool-result truncation width-aware** using the existing
  `Term::stdout().size()` query.
- **Optional raw-mode input path** (feature-flagged) for line editing, history
  navigation, and `Ctrl-C` soft-cancel, while keeping cooked mode as default.
