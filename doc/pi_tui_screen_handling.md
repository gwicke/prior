# pi TUI: Screen & Terminal-Mode Handling

Notes on how pi's interactive TUI manages raw vs. cooked terminal mode, how it
renders session output vs. the input area and type-ahead pop-ups, and why the
chat history survives quit/suspend. All paths are relative to the repo root.

## 1. Raw vs. cooked terminal mode

**The TUI only runs in raw mode while active; "cooked" is the restored/exit
state.**

- `ProcessTerminal.start()` (`packages/tui/src/terminal.ts:155`) enters raw
  mode: `process.stdin.setRawMode(true)`, `setEncoding("utf8")`, `resume()`. It
  saves `wasRaw` so it can be restored later (`terminal.ts:160`, `:341`).
- Enables bracketed paste (`\x1b[?2004h`) and, on Windows,
  `ENABLE_VIRTUAL_TERMINAL_INPUT` so modified keys arrive as VT sequences.
- Queries the Kitty keyboard protocol (flags 1+2+4) and falls back to
  `modifyOtherKeys` otherwise (`terminal.ts:204`,
  `queryAndEnableKittyProtocol`). Raw mode is required for per-key events,
  press/release event types, and alternate keys.
- All stdin passes through `StdinBuffer` (`terminal.ts:setupStdinBuffer`), which
  splits batched bytes into individual sequences so key-release events and
  escape sequences are dispatched one at a time.
- Output is fully app-rendered via differential rendering (escape sequences,
  no terminal line-discipline echo).

Restoring cooked mode is `ProcessTerminal.stop()` (`terminal.ts:300`): disables
bracketed paste, Kitty (`\x1b[<u`) and modifyOtherKeys, shows the cursor, pauses
stdin, and `setRawMode(this.wasRaw)`.

Deliberate hand-offs to cooked mode (so a child program gets the real terminal):

- **External editor launch**
  (`packages/coding-agent/src/modes/interactive/interactive-mode.ts:3791`):
  `ui.stop()` -> `spawn(editor, …, { stdio: "inherit" })` -> `ui.start()` in a
  `finally`.
- **Ctrl+Z suspend** (`interactive-mode.ts:3639`): `ui.stop()` then
  `process.kill(0, "SIGTSTP")`; on `SIGCONT`, `ui.start()`.

Shutdown drains first: `terminal.drainInput(1000)`
(`interactive-mode.ts:3501`, `:3512`) consumes late Kitty key-release events so
escape sequences don't leak into the parent shell over SSH, *then* `ui.stop()`
restores cooked. `uncaughtCrash` (`interactive-mode.ts:3542`) also calls
`ui.stop()` so a thrown exception never leaves the terminal stuck in raw mode
with a hidden cursor.

## 2. Session output vs. input area vs. type-ahead pop-ups

All three are drawn through **one** differential renderer. `TUI` extends
`Container`; `Container.render()` concatenates every child's `render(width)`
lines into a single `string[]`, and `doRender()` (`packages/tui/src/tui.ts`)
only repaints changed lines, scrolling the terminal to show the bottom `height`
rows (viewport = `prevViewportTop..prevViewportTop+height`).

**Composition order** (`interactive-mode.ts:702-713`):
`headerContainer` -> `loadedResourcesContainer` -> `chatContainer` ->
`pendingMessagesContainer` -> `statusContainer` -> `widgetContainerAbove` ->
`editorContainer` -> `widgetContainerBelow` -> `footer`.

- **Session output** = message components (assistant-message, user-message,
  tool-execution, etc.) added to `chatContainer` via `addChild`, with streaming
  text fed in via `appendOutput` (e.g. `interactive-mode.ts:3175`, `:5946`).
  They are just lines in the concatenated buffer; the renderer handles viewport
  scrolling. Rich/terminal-sensitive content (Kitty images, OSC) is reconciled
  specially (`expandChangedRangeForKittyImages`, `deleteChangedKittyImages` in
  tui.ts).
- **Input area** = the `Editor` component (focusable). It emits `CURSOR_MARKER`
  (a zero-width APC) at the cursor so `TUI.extractCursorPosition` /
  `positionHardwareCursor` (tui.ts) place the hardware cursor for IME candidate
  windows. It renders a bordered, word-wrapped box with scroll indicators and a
  reverse-video cursor grapheme.
- **Type-ahead pop-ups** (autocomplete) are **not** a separate overlay. The
  editor renders the suggestion `SelectList` as extra lines appended **below its
  own bottom border**, inside `Editor.render()` (the
  `if (this.autocompleteState && this.autocompleteList)` block in
  `packages/tui/src/components/editor.ts`). So the popup:
  - is part of the input component's render output, drawn just above the footer,
    and scrolls/positions with the editor;
  - shares the editor's input focus — `Editor.handleInput` intercepts
    up/down/tab/enter/esc while `autocompleteState` is active and routes them to
    `autocompleteList` before normal editing;
  - is triggered inline (slash commands at line start, `@`/`#` symbol contexts,
    debounced attachment suggestions) and is cancelled/updated as you type
    (`tryTriggerAutocomplete` / `updateAutocomplete` in editor.ts).

Real overlays (`showOverlay` -> `compositeOverlays` in tui.ts) are a *different*
mechanism used for dialogs, extension widgets, and slash-command resolvers
(`interactive-mode.ts:2482`). Those are composited on top of the base content
and grab focus independently — distinct from the always-inline autocomplete
pop-up.

Because autocomplete is inline, the editor still emits `CURSOR_MARKER` while the
popup is visible (editor.ts `emitCursorMarker = this.focused`) so IME
positioning keeps working.

## 3. Why the chat history survives quit/suspend

- **`chatContainer` holds the full history.** It is a `Container` of one
  component per message and has **no length cap**. It is only `clear()`-ed on
  session switches/rollbacks (`interactive-mode.ts:1648`, `:1734`, `:3059`,
  `:3424`, `:3454`, `:3762`) — never trimmed as it grows. `Container.render()`
  concatenates *every* child's lines, so `newLines` always represents the entire
  conversation; the viewport window is used only for cursor/scroll math and for
  choosing which changed lines to repaint.
- **pi renders to the primary screen, not the alternate buffer.** There is no
  `\x1b[?1049h` / `smcup` anywhere in the TUI (grep matches only comments noting
  the *external editor* uses the alternate screen — `interactive-mode.ts:3825`,
  `extension-editor.ts:163`). On the first frame, `doRender()` takes the
  `fullRender(false)` path (`tui.ts:1343`), which writes every line at the
  current cursor **without** clearing. As the session grows, new lines are
  appended at the bottom and the terminal scrolls, so the whole conversation
  accumulates in the terminal's normal scrollback. `stop()` (`tui.ts:735`) moves
  the cursor to the end of the content and restores the cursor — it does **not**
  clear the screen or scrollback.

Consequences:

- **After quit**: `shutdown()` -> `drainInput()` -> `ui.stop()` leaves all
  rendered chat in the primary scrollback, above the restored shell prompt.
- **After suspend** (`Ctrl+Z`): `ui.stop()` drops raw mode and backgrounds the
  process, but the last frame is already in the primary scrollback, so `fg`
  returns to your chat in place.

**Caveat:** a width or height change forces `fullRender(true)`, which emits
`\x1b[2J\x1b[H\x1b[3J` (`tui.ts:1276`) — clearing both screen *and* scrollback.
So persistence is best-effort during a stable-size session; any resize
mid-session wipes the scrollback history (Termux is exempted from the
height-triggered wipe, `tui.ts:1358`). The deliberate non-use of the alternate
screen is what makes "quit and your conversation is still there" possible.
