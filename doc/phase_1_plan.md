# Phase 1 Implementation Plan — Interactive TUI Foundation

This plan covers the four feature groups in `doc/requirements.md`, grounded in the
current architecture described in `doc/picocode_rendering.md`. It is organized in two
parts:

- **Part A — Technical Design**: the *what* and *why* (constraints, dependencies,
  the `Repl` architecture, signals, input, screen model).
- **Part B — Implementation Plan**: the *how* (file changes, a staged checklist
  with concrete items + tests, risks, and a decisions/corrections log).

All open questions from earlier drafts are now **resolved and folded into their
sections**; the audit trail lives in Part B §7.

---

# Part A — Technical Design

## 1. Current-state constraints (why this is a re-architecture)

From `doc/picocode_rendering.md` and a read of `src/`:

- **Cooked mode only.** All input flows through `std::io::stdin().read_line()`
  (`output.rs:327`, `:72`; `QuietOutput::confirm`). No `termios`, `tcsetattr`,
  `AlternateScreen`, `crossterm`/`ratatui`, or keystroke handling exists anywhere
  in `src/`.
- **No keystroke capture** → no arrow navigation, no incremental autocomplete,
  no `<shift-enter>`, no soft-cancel bound to a key.
- **`Ctrl-C`** is the kernel default `SIGINT` (terminates the process), not a
  soft cancel. There are **no signal handlers** installed.
- **Output is line-oriented `println!` + ANSI + glyphs** (`ConsoleOutput`), not a
  managed screen buffer. "Native terminal scrolling" is the *default* today
  because nothing redraws the screen — but there is also no "container" concept
  or resize redraw.

**Conclusion:** three of the four requirements (slash/fuzzy autocomplete,
multi-line input, native-scroll container with resize redraw) require a
**raw-mode, alternate-screen, event-loop input layer**. This is a foundational
change, not an additive one. The plan isolates that layer behind a clean module
so the existing `Output` rendering stays intact for the non-interactive modes.

## 2. Dependencies

**Decision (Q1, resolved):** accept `crossterm` + `fuzzy-matcher`. The zero-dep
`libc`/`nix` termios path was rejected — `crossterm` is safe, mature, and
battle-tested.

Proposed additions to `Cargo.toml` (prefer existing/lightweight, no unsafe):

| Need | Crate | Notes |
|---|---|---|
| Raw mode + alt screen + key events + resize | **`crossterm`** | Mature, safe, no `termios`. Replaces need for `console::Term` sizing. |
| Fuzzy matching (fzf-like) | **`fuzzy-matcher`** (`skim` matcher) | Small, well-tested; gives subsequence scoring + indices. |
| Word-boundary wrapping | **`unicode-segmentation`** | Grapheme/word aware; safer than manual `char_indices` for CJK. |
| Model metadata fetch | **`reqwest`** (promote to **direct** dep) | Today it is only transitive via `rig`; `/model` needs it directly (§3.2). |
| Slash command + model list state | **none** (std collections) | Keep it minimal. |

- **Do NOT** introduce `ratatui` for Phase 1. Requirements explicitly say "leverage
  native terminal scrolling" and "do not re-implement scrolling" — a full TUI
  framework would fight that. We use `crossterm` raw mode + alt buffer only for the
  *input region*; the session history is plain scrolled stdout.
- Keep `console`, `termimad`, `indicatif` for rendering; migrate sizing calls to
  `crossterm::terminal::size()`.

## 3. Architecture: the `Repl` input/event layer

Create `src/repl.rs` (and split rendering where needed). The interactive REPL in
`agent.rs::run_interactive` currently calls `self.output.get_user_input()` in a
loop. We replace that with a `Repl` that owns:

- raw-mode + alternate-screen lifecycle (enter on start, restore on exit / panic /
  signal),
- a keystroke event loop (`crossterm::event::read` with `poll` for non-blocking
  + resize),
- a line-editor buffer (multi-line, cursor, history),
- the slash-command / fuzzy filtering state machine,
- a `/model` provider-aware model fetcher + 5-minute cache.

### 3.1 Public surface changes

- Add a `fuzzy` module: `fn fuzzy_filter(items: &[Item], query: &str) -> Vec<Ranked>`.
- Add `slash` module: a static registry of `SlashCommand { name, help, kind }`
  where `kind` ∈ `{Help, Thinking, Model}`. Extensible for future commands.
- `ConsoleOutput::get_user_input()` stays as-is for `confirm()` and non-TTY use.
  The new interactive path does **not** go through `get_user_input`; it goes
  through `Repl::read_input(&mut self) -> Option<String>`.

### 3.2 Slash commands (`/model`, `/thinking`, `/help`) — req group 1

- Typing `/` at the start of the (empty) input opens the command list in the alt
  buffer. Further chars filter with `fuzzy-matcher` (subsequence + score), same
  algorithm reused for `/model` and any future autocomplete.
- **`/model`**: fetch models across *all* configured providers and normalize to
  `{provider}:{modelname}`, presenting token limits + cost per row.

  **Model metadata source (verified against `rig-core` 0.28.0 + live endpoints):**
  `rig` does **not** expose a model-listing API for the vast majority of providers.
  Only `mira` has `list_models()`, and it returns `Vec<String>` (names only — no
  token limits or cost). The `VERIFY_PATH: "/models"` on `openai`/`openrouter`/
  `deepseek` is **API-key validation**, not a listing. Provider-native APIs such as
  DeepSeek (`api.deepseek.com/models`, OpenAI-compatible), OpenAI (`/v1/models`),
  and Anthropic (`/v1/models`) return **only model IDs** (`id`/`object`/
  `owned_by`) — no `context_length` and no `pricing`. **Verified:** even with a
  valid `DEEPSEEK_API_KEY`, DeepSeek's `/models` returns just
  `{id,object,owned_by}` (2 models: `deepseek-v4-flash`, `deepseek-v4-pro`); the
  key only authorizes *requests*, it does not unlock metadata.

  **Decision (OQ1 / Q2, resolved):** primary source = OpenRouter's public,
  unauthenticated `GET https://openrouter.ai/api/v1/models`, which aggregates
  ~300 providers and returns `context_length` + `pricing.prompt` /
  `pricing.completion` per model — a direct fit for the `{provider}:{modelname}` +
  limits + cost list. **Fallback** = a small curated `provider → model →
  context_window → cost` table (~20-40 rows), used when offline or for providers
  OpenRouter does not surface. Cache 5 min via `OnceLock<Mutex<Cache>>` +
  `Instant` expiry. `reqwest` becomes a *direct* dependency (see §2, §4).

  **Bonus:** OpenRouter's per-model `supported_parameters` includes `reasoning` /
  `reasoning_effort` — reuse it to **gate the `/thinking` mapping per model**
  (skip injecting thinking params when the model lacks support).

  **Reference — how pi does it (corroboration):** pi uses the *generated-static*
  variant of this design. `pi/packages/ai/scripts/generate-models.ts` fetches
  `https://openrouter.ai/api/v1/models` at **build time** (filtering to
  `supported_parameters.includes("tools")`, mapping `pricing.*` → $/million,
  `top_provider.context_length` → context window, capturing `reasoning`), then
  writes a committed catalog (`src/models.generated.ts` + `models.json`). At runtime
  `ModelRegistry` (`pi/.../src/core/model-registry.ts`, exported as `ln`) loads that
  **static** catalog via `FileModelsStore` — **no live fetch**. This confirms
  OpenRouter as the canonical metadata source and validates our OpenRouter-primary
  choice; the only deviation is build-time generation vs our runtime live fetch.
  pi also gates reasoning on the per-model `supported_parameters` flag — exactly
  the gating we propose.

  - **Provider availability via env-var scan (prerequisite):** picocode today
    requires an explicit `--provider` (flag/recipe/default `anthropic`,
    `main.rs:110`) and only *then* validates that provider's key in `create_agent`
    via `check_env!` (hard `MissingApiKey` error if absent, `agent.rs:145-216`).
    There is no availability discovery. **Invert this:** add
    `provider_available(p) -> bool` that scans the environment for each provider's
    required key(s) — reusing the exact mapping `create_agent` already encodes:
    `anthropic`→`ANTHROPIC_API_KEY`, `openai`→`OPENAI_API_KEY`,
    `deepseek`→`DEEPSEEK_API_KEY`, `gemini`/`google`→`GOOGLE_API_KEY`,
    `azure`→`AZURE_OPENAI_API_KEY`+`AZURE_OPENAI_ENDPOINT`, `ollama`→optional
    (`OLLAMA_API_BASE_URL`, defaults to localhost), `cohere`→`COHERE_API_KEY`,
    `galadriel`→`GALADRIEL_API_KEY`, `groq`→`GROQ_API_KEY`, `huggingface`→
    `HF_TOKEN`, `hyperbolic`→`HYPERBOLIC_API_KEY`, `mira`→`MIRA_API_KEY`,
    `mistral`→`MISTRAL_API_KEY`, `moonshot`→`MOONSHOT_API_KEY`, `openrouter`→
    `OPENROUTER_API_KEY`, `perplexity`→`PERPLEXITY_API_KEY`, `together`→
    `TOGETHER_API_KEY`, `xai`→`XAI_API_KEY`. This single scan (a) **gates the
    extrapolation heuristic** below (only extrapolate to providers that are
    available), and (b) lets `main.rs` pick a **default provider when `--provider`
    is omitted** (first available, instead of blindly defaulting to `anthropic` and
    erroring). Distinguish two levels: *available* = env key present (cheap; may be
    stale/expired), vs *verified* = native `/models` reachable (the liveness query
    below upgrades available→verified). **Special case:** `OPENROUTER_API_KEY`
    present unlocks every `openrouter/*` model even without per-provider keys, so
    OpenRouter counts as "available" whenever its key exists.

  - **Provider extrapolation heuristic (avoid staleness + correct direct pricing):**
    OpenRouter IDs are namespaced `{provider}/{model}` (e.g.
    `deepseek/deepseek-v4-flash`). Parse into `(provider, model)` and, **for each
    provider that is configured/available** (env key present), extrapolate a
    **direct-provider** entry: `id` = `model` with the `provider/` prefix stripped,
    `provider` = `provider`, `context_window` + `reasoning` taken from the
    OpenRouter record (intrinsic to the model), and **pricing from a curated
    `DIRECT_PRICING` map keyed by `(provider, model)` — NOT OpenRouter's**, because
    direct-access rates differ from OpenRouter's markup (e.g. DeepSeek-direct vs
    DeepSeek-via-OpenRouter; user observation). When direct pricing is unknown, fall
    back to OpenRouter's value but tag it `estimate-via-openrouter` so the UI labels
    it. **Liveness (kills the `deepseek-chat` staleness):** for configured
    providers that expose a `/models` endpoint returning IDs (DeepSeek, OpenAI, …),
    also query it with the user's key and (a) prune the extrapolated list to models
    the key can actually call, and (b) surface brand-new models OpenRouter/our
    defaults don't yet list (e.g. DeepSeek `v4-flash`/`v4-pro`). Provider-native
    `/models` returns only IDs, but combined with OpenRouter's context/reasoning
    metadata this yields a live, non-stale, correctly-priced direct-provider list.
    **Unconfigured providers still appear under the `openrouter` namespace** (OR can
    route to them) using OpenRouter pricing, so a model is selectable even without a
    direct key. Each underlying model may thus appear twice:
    `openrouter/{provider}/{model}` (OR pricing) and `{provider}/{model}` (direct
    pricing, only if that provider is configured).

  - **Cache**: `std::sync::OnceLock<Mutex<Cache>>` (or `moka` if async TTL wanted)
    with a 5-minute TTL keyed by provider. Phase 1 uses a simple
    `Instant`-based expiry to avoid a new dep.

  - **Prioritization (later phase)**: config field `model_priorities: Vec<String>`
    in `picocode.yaml` that pins rows to the top before filtering. Defer to Phase 2
    but leave the hook (`sort_with_priority`).

- **`/thinking {off|low|med|high}`**: sets a thinking level. **Decision (OQ4,
  resolved): map to provider thinking params** (user goal), not store/display only.

  **Investigation of `rig` 0.28 (done):** no provider exposes a *typed* thinking-
  level request API — `anthropic` only *parses* thinking in responses; `openai`
  `reasoning_effort` exists only on the **Responses API**
  (`with_reasoning(Reasoning{effort})`), not picocode's **Completions** path (which
  panics on reasoning); `openrouter` surfaces `reasoning` in its request struct.
  The universal, provider-agnostic path is core `CompletionRequest::additional_params`
  (a `serde_json::Value` **merged into the request body**). Note
  `PromptRequestBuilder` (from `agent.prompt()`) does **not** expose
  `additional_params`, but `AgentBuilder` does — so implement `/thinking` by
  **rebuilding the rig `Agent`** (via `AgentBuilder::additional_params(think_json)`)
  whenever the level changes.

  **Design:** new `src/thinking.rs` maps `ThinkingLevel {Off,Low,Med,High}` →
  `Option<Value>` per provider:
  - `anthropic`: `Off`→`None`; else `{"thinking":{"type":"enabled","budget_tokens":N}}`
    (requires `temperature:0` + a thinking-capable model — gate by model).
  - `openai` (completions): `Off`→`None`; else `{"reasoning_effort":"low"|"medium"|"high"}`
    (o1/o3-class models only).
  - `openrouter`: `Off`→`None`; else `{"reasoning":{"effort":"low"|"medium"|"high"}}`.
  - other providers (ollama, groq, cohere, …): `None` (store + display only).
  `Off`/unsupported → no param. Treat mapping as **best-effort**: if the
  provider/model doesn't support thinking, silently fall back to store+display.
  `AgentConfig` gains `thinking_level: Option<String>`; `CodeAgent` holds it and
  rebuilds the agent on change.

- **`/help`**: prints the command list + key bindings.

### 3.3 Native terminal scrolling container — req group 2

The requirement: show full session history, *don't* re-implement scrolling,
break lines at word boundaries, redraw on resize/resume, and avoid force-scrolling
to bottom when the user is reading earlier content.

**Model decision (informed by `doc/pi_tui_screen_handling.md`, Appendix A):**
session history is **plain stdout on the primary screen, app-appended** — the same
trick pi uses so history survives quit/suspend for free. We deliberately do *not*
use an alternate buffer for history, and we do *not* emit `\x1b[3J` on resize (the
pi resize-scrollback-wipe trap). Only the input/list region is "managed."

Approach (no full TUI, no re-implemented scrolling):
- Session history is **emitted as plain stdout to the primary screen** (already the
  case) and **also retained in an in-memory buffer**. On normal output we append
  (native scroll, no re-render). On resize we re-render the in-memory buffer at the
  new width into the visible frame (clearing only the current frame, never
  `\x1b[3J`). This satisfies "leverage native scrolling" for the common case while
  giving correct word-boundary wrapping after a resize, at the cost of re-rendering
  history on resize rather than leaving it fully untouched.
- The **input area (+ autocomplete popup)** is the only thing redrawn on resize.
  An alt buffer is permitted for the popup and "it is okay to stop scrolling" there;
  we may use either an inline primary-screen render (pi-style, single scrollback) or
  a small alt buffer for the list (decision: pi-style inline, see §7.2 Q7).
- **Word-boundary wrapping**: add a `wrap(text, width)` helper using
  `unicode-segmentation` so long assistant/tool lines break at spaces, not mid-word.
  Replace the fixed `truncate(line, 100)` in `output.rs:313` with width-aware
  wrapping using `crossterm::terminal::size()`.
  **Decision (OQ5, resolved): wrap at *render*/resize time, NOT pre-wrapped at
  emit.** Pre-wrapping at emit would leave history at a stale width and cause ugly
  double-wrapping after a resize. Instead, keep the full session in an **in-memory
  history buffer**; wrap at the *current* width when emitting and **re-wrap the
  whole buffer at the new width on resize**. This avoids double-wrapping at the cost
  of re-rendering history on resize (see §3.6). Termimad's `print_inline` should be
  replaced by our `wrap()` for session lines so wrapping is width-correct.
- **History buffer + resize redraw**: keep an in-memory `Vec<RenderedLine>` of the
  session. On **new output**, append wrapped lines to the primary-screen
  scrollback (native append). On **resize** (`crossterm::event::Event::Resize`),
  move the cursor to the top of our managed frame, clear the visible region only
  (`\x1b[0J` — clear from cursor to end of screen, **not** `\x1b[3J`), and
  re-emit the in-memory history wrapped at the new width, then repaint the
  input/list region (see §3.6).
- **"Don't force scroll to end"**: when new output arrives, emit no
  cursor-down/scroll command; rely on the terminal's natural scroll so a user
  reading earlier content stays put. (This is the default with native history and
  is why we avoid pi's `fullRender(true)` viewport reset.)
- **Resume redraw**: on `SIGCONT` after `Ctrl-Z` (§3.4), repaint only the input
  region — history is already safe in primary scrollback, so `fg` returns to the
  session in place (no container-wide redraw needed; see Q4 resolution, §7.2).

### 3.4 Signals: Ctrl-C / Esc / Ctrl-Z / SIGCONT — req group 3

**No unsafe code.** `Ctrl-C`/`Ctrl-Z` are handled as **key events** in the REPL
(see §3.4.1 — raw mode clears `ISIG`, so no POSIX signal handler is involved for
them). `SIGCONT` (resume) uses a real signal handler via `libc::signal`/
`tokio::signal` (safe FFI wrappers). `killpg(getpgrp(), SIGTSTP)` is a safe FFI
call.

- **`Ctrl-C` (process exit):** process exit — but see §3.4.1: in raw mode this is a
  **key event**, not a kernel `SIGINT` (raw mode clears `ISIG`). Do **not** install
  a soft-cancel handler; instead, `Esc` (a raw-mode key event) performs the
  hard-abort of an in-flight request via the §3.4.3 cancel bridge. Terminal mode is
  still restored on exit by the `Drop`/`TermGuard::stop()` path (`main.rs:56` panic
  hook + REPL `Drop`), so the terminal is never left broken.
- **`Ctrl-Z` (suspend process group):** mirror pi (`doc/pi_tui_screen_handling.md`
  §1/§3) but extend to the **process group**. Triggered by the `Ctrl-Z` **key
  event** (see §3.4.1 — not a `SIGTSTP` signal handler in raw mode): on suspend,
  **restore cooked mode + disable bracketed paste/Kitty** and send `SIGTSTP` to the
  process group via `killpg(getpgrp(), SIGTSTP)` so wrapper scripts suspend too; on
  `SIGCONT`, **re-enter raw** and repaint only the input region. History is already
  safe in primary scrollback, so `fg` returns to the session in place. Do NOT clear
  the screen/scrollback.
- Must **restore cooked mode on exit, panic, and signal** to avoid leaving the
  terminal broken. Wrap the REPL in a guard (`Drop` + panic hook already exists in
  `main.rs:56`). See §3.4.2 for the exact handler→lifecycle wiring, and §3.4.3 for
  *who* reads those key events while an LLM request is in flight.

#### 3.4.1 Raw-mode signal reality (correction)

`crossterm::terminal::enable_raw_mode()` enters non-canonical mode and **clears
`ISIG`**, so while the REPL owns the terminal the line discipline no longer turns
`Ctrl-C`/`Ctrl-Z` into `SIGINT`/`SIGTSTP`. They arrive as ordinary key events
(`KeyCode::Char('c')`/`KeyCode::Char('z')` with `CTRL`):

1. **`Ctrl-C` does *not* raise `SIGINT`** in raw mode. "Keep kernel default →
   process exit" is impossible while raw. To preserve "Ctrl-C = exit", the event
   loop must detect the `Ctrl-C` key event and call `guard.stop()` then
   `std::process::exit(1)`. There is no signal to rely on.
2. **`Ctrl-Z` does *not* raise `SIGTSTP`** in raw mode, so a `SIGTSTP` *signal
   handler* will not fire from the key. Suspend must be triggered from the **key
   event**: `guard.stop()` (restore cooked + disable paste/Kitty) then
   `killpg(getpgrp(), SIGTSTP)` to background the whole group. The
   reset-to-`SIG_DFL` + re-raise dance is only needed in cooked mode; in raw mode
   just call `killpg` directly after restoring cooked.
3. **`SIGCONT` is still a real signal** delivered by the kernel on `fg`
   (job-control signals are not suppressed by terminal modes), so the `SIGCONT`
   resume handler remains valid and is the correct resume trigger.

**Net:** `Ctrl-C`/`Ctrl-Z` → **key-event handlers** in the REPL; `SIGCONT` →
**signal handler**.

#### 3.4.2 Signal-handler wiring into the `start()`/`stop()` lifecycle

The REPL owns a `TermGuard` whose `start()` (§3.5.2) and `stop()` (§3.5.2) are the
single source of truth for terminal mode. Signals route *through* that lifecycle so
the terminal is always left consistent — mirroring pi's `ui.stop()` before
`SIGTSTP` and `ui.start()` on `SIGCONT`.

State shared across handlers:
```
guard:      Arc<TermGuard>          # start()/stop() from §3.5.2
cancel:     Arc<AtomicBool>         # Esc hard-abort (rig CancelSignal source)
suspended:  Arc<AtomicBool>         # true between SIGTSTP and SIGCONT
input_tx:   channel for injecting synthetic key events into the event loop
```

`Ctrl-C` — **process exit (key-event driven), NOT a soft cancel**:
```
# In raw mode Ctrl-C is a KEY EVENT, not a signal. There is no kernel
# SIGINT to rely on while the REPL owns the terminal.
on_ctrl_c_key():
  guard.stop()                             # restore cooked + disable paste/Kitty
  std::process::exit(1)                    # true process exit (§3.4.1)
```
- No `unsafe`, and **no `SIGINT` handler** needed in interactive mode. The exit is
  performed explicitly from the key-event handler. `Drop`/`TermGuard::stop()` still
  run first so the terminal is never left broken.

`Esc` — **hard-abort of in-flight request** (replaces the old Ctrl-C role, OQ2
resolved):
```
# Esc is a *key event* in raw mode (disambiguated by the Kitty layer, §3.5.2),
# not a signal. The event loop routes it to the agent instead of the tty handler.
on_esc_key():
  cancel.store(true, SeqCst)            # rig prompt await observes this
  # rig's CancelSignal (PromptHook, agent.rs:249,262) aborts the current
  # completion/tool loop; partial output is discarded and we return to a
  # fresh prompt.
```

`Ctrl-Z` — suspend **process group** to shell, triggered from the **key event**
(OQ6 resolved):
```
on_ctrl_z_key():                           # NOTE: key event, NOT a SIGTSTP handler
  if suspended.load() { return }
  guard.stop()                             # §3.5.2 stop(): drain, pop kitty,
                                            # disable paste, restore cooked
  suspended.store(true, SeqCst)
  killpg(getpgrp(), SIGTSTP)               # background the WHOLE process group
```
- The suspend is performed by `killpg(getpgrp(), SIGTSTP)` called from the key-
  event handler after `guard.stop()`. We do **not** rely on a `SIGTSTP` signal
  handler firing — in raw mode it would not (§3.4.1). This avoids re-implementing
  job control.

`Ctrl-Z` resume (`SIGCONT`) — re-enter raw, repaint input only:
```
on_sigcont():
  suspended.store(false, SeqCst)
  guard.start()                            # §3.5.2 start(): re-enable raw,
                                           # paste, kitty; re-snapshot was_raw
  repl.repaint_input_region_only()         # NOT a fullRender(true) — history is
                                           # already in primary scrollback
```
- History needs no repaint: the last frame lived in the primary scrollback, so
  `fg` returns to the chat in place. Only the input/list region is redrawn.

Idempotency / safety:
- `guard.stop()` is idempotent so a panic hook + `Drop` + signal handler calling it
  twice is harmless.
- Register handlers **after** `guard.start()` and **before** entering the event
  loop; deregister/restore on `guard.stop()` for non-interactive exits.
- On `std::io::IsTerminal() == false` (piped/quiet/`LogOutput`/`NoOutput`), **skip
  all of this** — those paths keep using cooked `read_line` (see §6).

#### 3.4.3 In-flight key capture & the cancel bridge

`crossterm::event::read()` is **blocking**, and while a request runs the main Tokio
task is busy awaiting `prompt().await` (rig streams the completion). So the single
REPL loop cannot also read keys during the await. Two things must be made explicit:

1. **A concurrent stdin reader.** Run `crossterm::event::read`/`poll` on a
   dedicated OS thread (`std::thread::spawn`) or `tokio::task::spawn_blocking`,
   feeding key events into an `mpsc` channel. It stays alive during the LLM await
   so `Esc`/`Ctrl-C`/`Ctrl-Z` are captured even while "busy":
   - `Esc`     → set shared `Arc<AtomicBool>` `user_cancel`;
   - `Ctrl-C` key → `guard.stop()` + `std::process::exit(1)`;
   - `Ctrl-Z` key → `guard.stop()` + `killpg(getpgrp(), SIGTSTP)`.
   These actions are safe to perform from the reader thread (`guard` is `Arc` and
   `stop()` is idempotent; `process::exit`/`killpg` are process-wide). The same
   channel can later carry **queued user prompts** submitted while a turn is
   in-flight, enabling follow-up input without interrupting the running request.
2. **Cancel bridge to rig.** `rig`'s `CancelSignal` is `CancelSignal(Arc<AtomicBool>)`
   with a **private** `new()` but a **public** `cancel()`. rig constructs one per
   request and checks `is_cancelled()` *after each* `PromptHook` callback
   (`streaming.rs:233,278,304,327,360,379`). We cannot hold rig's signal from
   outside, so bridge via our own flag: our `PromptHook` impl carries
   `Arc<AtomicBool>` `user_cancel`, and in every callback does
   `if user_cancel.load(SeqCst) { cancel_sig.cancel(); }`. The `Esc` key event
   sets `user_cancel`, and rig aborts at the next poll. This is the only hook
   point rig gives us for cancellation.

### 3.5 Multi-line text input — req group 4

Implemented inside `Repl`'s line editor (managed input region; per §3.3 this is
either inline primary-screen render lines or a small alt buffer for the popup).

#### 3.5.1 Terminal capability negotiation (prerequisite)

Reliable paste detection and `Shift-Enter` / modified-key handling require the
terminal to emit unambiguous sequences. Mirror pi's `ProcessTerminal.start()`
(`doc/pi_tui_screen_handling.md` §1):

- **Enable bracketed paste** (`\x1b[?2004h`) on `Repl` start; disable
  (`\x1b[?2004l`) on stop/restore. `crossterm` surfaces paste as
  `Event::Paste(String)` — the robust path (Q6 resolved: **bracketed paste, no
  timing heuristic**). On `DisableBracketedPaste`/unsupported terminals, fall back
  to the "newline within X ms = paste" heuristic.
- **Query the Kitty keyboard protocol** (CSI `?u` flags 1+2+4) and, if
  unsupported, enable `modifyOtherKeys` (CSI `>1;1u` / `>4;1u`). This is what
  makes `Shift-Enter`, `Alt-<key>`, and key *release* events arrive as distinct
  VT sequences. `crossterm` supports the Kitty protocol on its
  `KeyboardEnhancement` feature (Q8 resolved: **requiring Kitty is okay**; keep
  `modifyOtherKeys` as fallback); otherwise we read raw escape sequences.
- **Save `was_raw` / prior mode flags** at start and restore them exactly on stop
  (incl. panic/signal), so we never leave the terminal in raw mode with a hidden
  cursor or stray paste mode.
- **Drain late input on shutdown**: before restoring cooked mode, consume any
  pending stdin (Kitty key-release / escape sequences) so they don't leak into the
  parent shell over SSH.

#### 3.5.2 Exact setup/shutdown ordering (CSI sequence order)

`start()` runs **once** when the REPL takes over the terminal; `stop()` runs on
normal exit, panic, and signal (via the `Drop` guard + `main.rs:56` panic hook).
Order matters: query capabilities *before* relying on them; tear down in reverse
and only after draining late input.

```
start():
  # 1. snapshot prior state so stop() can restore exactly
  saved_cooked   = is_cooked_mode()          # crossterm::terminal::is_raw_mode() inverse
  saved_paste    = query_mode(2004)          # ?2004; remember if paste was already on
  saved_kitty    = query_mode_kitty()        # ?u current flags, if supported

  # 2. enter raw mode (no line discipline echo / canonical buffering)
  enable_raw_mode()                          # \x1b[?1049h NOT used — see Appendix A

  # 3. enable bracketed paste so multi-line paste is one Event::Paste
  write("\x1b[?2004h")

  # 4. negotiate keyboard enhancement (Kitty protocol flags 1+2+4):
  if SupportsKeyboardEnhancement:            # crossterm probe
      PushKeyboardEnhancementFlags(REPORT_EVENT_TYPES
                                   | REPORT_ALTERNATE_KEYS
                                   | REPORT_ALL_KEY_EVENTS)
  else:
      write("\x1b[>1;1u")                    # modifyOtherKeys: on
      write("\x1b[>4;1u")                    # modifyOtherKeys: report as CSI u

  # 5. stdin buffer: split batched bytes into discrete sequences
  setup_stdin_reader()                       # crossterm::event::read with poll()

stop():                                      # idempotent; safe to call twice
  # 6. drain late input so release/escape sequences don't leak to parent shell
  drain_stdin(timeout=1000ms)

  # 7. pop keyboard enhancement in reverse order of push
  if pushed_kitty: PopKeyboardEnhancementFlags()
  else:              write("\x1b[>1;0u")           # modifyOtherKeys: off

  # 8. disable bracketed paste (restore prior state if it was off)
  write(saved_paste ? "\x1b[?2004h" : "\x1b[?2004l")

  # 9. restore cursor visibility + cooked mode exactly as snapshot
  show_cursor()
  disable_raw_mode() if saved_cooked         # \x1b[?1049l NOT used
```

Notes:
- Steps 2/9 deliberately avoid `smcup`/`rmcup` (`\x1b[?1049h`/`l`) — history stays
  in the **primary** scrollback (Appendix A), so quit/suspend leaves the
  conversation intact. The alt buffer (if used for the popup) is entered/left
  *locally* and never with a scrollback clear.
- Steps 4/7 bracket the whole session; `PopKeyboardEnhancementFlags` must run
  **before** `disable_raw_mode` so the terminal is fully reset while still in our
  control.
- `drain_stdin` (step 6) must precede mode teardown (7-9) so stray bytes consumed
  during drain aren't interpreted after we've already told the terminal we're
  leaving raw mode.

#### 3.5.3 Editing behavior

- **Enter** submits; **pasted newlines do not** submit (paste arrives via the
  bracketed-paste event as a single `Event::Paste`, inserted as literal newlines).
  Pasted multi-line text is inserted verbatim.
- **`<Shift-Enter>`** (disambiguated by the Kitty/`modifyOtherKeys` layer) inserts
  a newline explicitly.
- **Arrow keys** move within and across lines; `Home`/`End`, `Backspace`/`Delete`,
  word-left/right (`Alt-B`/`Alt-F`) for edit commands.
- **Word-boundary wrap** of the input as it grows; the input box **expands**
  (multiple rows), pushing session content upward via terminal scroll.
- History (`Up`/`Down`) over previous prompts, persisted to
  `~/.config/picocode/history` (Phase 1 in-memory + file append).

### 3.6 Resize redraw

On `crossterm::event::Event::Resize`, redraw only the input/list region and the
visible frame; **never** clear scrollback:
- Move the cursor to the top of our managed frame, clear the visible region only
  (`\x1b[0J` — clear from cursor to end of screen, **not** `\x1b[3J`).
- Re-emit the in-memory history (§3.3) **wrapped at the new width** using `wrap()`,
  then repaint the input/list region.
- This trades "never touch history" simplicity for correct wrapping after resize
  (the OQ5 user preference — avoids double-wrapping) while preserving the
  no-scrollback-wipe property (Appendix A invariant).

---

# Part B — Implementation Plan

## 4. Proposed file changes

| File | Change |
|---|---|
| `Cargo.toml` | Add `crossterm`, `fuzzy-matcher`, `unicode-segmentation`; promote `reqwest` to a **direct** dep (was transitive via `rig`) for the OpenRouter model fetch (§3.2). |
| `src/repl.rs` | **New.** Raw-mode REPL: **capability negotiation (bracketed paste, Kitty/`modifyOtherKeys`, save/restore `was_raw`, input drain)** (§3.5.2), event loop, line editor, slash state, resize. |
| `src/fuzzy.rs` | **New.** `fuzzy_filter` over string items. |
| `src/slash.rs` | **New.** Command registry (`/model`, `/thinking`, `/help`). |
| `src/thinking.rs` | **New.** `ThinkingLevel` → per-provider `additional_params` JSON mapping (§3.2 `/thinking`). |
| `src/model_catalog.rs` | **New.** OpenRouter live fetch + parse; `provider_available()` env-var scan; `{provider}/{model}` → direct-provider extrapolation (§3.2 heuristic); `DIRECT_PRICING` table; native `/models` liveness prune. |
| `src/output.rs` | Width-aware `wrap()` (word-boundary via `unicode-segmentation`), fix `truncate` UTF-8 panic (`char_indices`), migrate `Term::stdout().size()` → `crossterm::terminal::size()`; route session lines through `wrap()` instead of termimad `print_inline`. |
| `src/agent.rs` | `run_interactive` uses `Repl`; thread `thinking_level`; share `Arc<AtomicBool>` cancel signal with hooks; `AgentConfig` gains fields; rebuild rig `Agent` on `/thinking` change. |
| `src/main.rs` | Wire `ctrlc`/signal handlers; construct `Repl`; pass cancel signal; pick default provider from `provider_available()` scan when `--provider` omitted. |
| `src/lib.rs` | Re-export new modules/types as needed. |
| `src/config.rs` | Add `model_priorities` (later), `thinking` defaults (optional). |

## 5. Implementation checklist (staged, each stage compiles & is testable)

> Guideline (per project AGENTS.md): add tests for tricky code; prefer integration
> tests where possible, unit tests to isolate. Always investigate build warnings.

### Stage 0 — Foundations (no behavior change)
### Stage 0 — Foundations (no behavior change) — ✅ DONE
> Implemented 2026-07-16. `cargo build` + `cargo test --lib output::` green,
> no warnings. `Term` import dropped (migrated to `crossterm::terminal::size()`).
> Follow-ups (out of Stage 0 scope): CJK wrap measures in chars, not display
> columns (CJK = 2 cols/char) — could add `unicode-width` later;
> `display_text`'s `termimad::print_inline` migration to `wrap()` is deferred
> to §3.3 / Stage 3-6.
- [x] `Cargo.toml`: add `crossterm` (0.29), `fuzzy-matcher` (0.3),
      `unicode-segmentation` (1.12); promote `reqwest` to direct dep (0.12,
      `json` feature).
- [x] `src/output.rs`: add `wrap(text, width)` (Unicode word-boundary, CJK-safe,
      hard-split for over-long runs) and `term_width()`; replace `truncate(line, 100)`
      in `display_tool_result` with width-aware wrap.
- [x] Fix `truncate` UTF-8 panic: switched to `chars().count()` /
      `chars().take()` (was byte-index slice `&s[..max_len]`).
- [x] Migrate both `console::Term::stdout().size().1` calls (a latent bug — `.1`
      is *height*) to `term_width()` (`.0` = width).
- [x] **Unit tests** (`output.rs`): word-boundary, long no-space, CJK width +
      content-preservation, blank line, multiple-space collapse, `truncate` multibyte
      + ascii (9 tests, all passing).
- [x] **Verify** non-TTY paths (`QuietOutput`/`NoOutput`/`LogOutput`) untouched and
      compiling.

### Stage 1 — Fuzzy matching + slash registry (pure, TTY-free) — ✅ DONE
> Implemented 2026-07-16. `cargo build` green, no warnings.
> `pub mod fuzzy;` + `pub mod slash;` added to `lib.rs`. 16 new tests
> (fuzzy: 4; slash: 5) + 9 output tests = 25 lib tests passing.
- [x] `src/fuzzy.rs`: `fuzzy_filter(items, query, to_text) -> Vec<Ranked>`
      using `fuzzy-matcher` (Skim). Generic over `T` via a `to_text` closure
      (matches `&str` and `SlashCommand`); empty query returns all in order.
- [x] **Unit tests** (`fuzzy.rs`): subsequence matches; scoring order; empty query
      returns all in stable order; no false-positive on disjoint query.
- [x] `src/slash.rs`: `SlashCommand { name, help, kind }` (`kind ∈
      {Help, Thinking, Model}`); static registry + `filter_commands` resolver.
- [x] **Unit tests** (`slash.rs`): registry lookup by name; fuzzy filtering of
      command list; unknown command handling.

### Stage 2 — `Repl` skeleton + capability negotiation
> **Stage 2 — ✅ DONE (2026-07-16).** `cargo build` green, **no warnings**;
> 28 lib tests pass (incl. 3 new `repl::tests::render_line`). Pty smoke
> test (Python `pty`, `deepseek` provider, key in env): child exits 0;
> captured output shows `display_header` (cooked) → start CSI
> `\x1b[?2004h\x1b[>1;1u\x1b[>4;1u` → prompt `> ` in raw mode;
> cursor CSI `\x1b[1D`/`\x1b[0D` correct; empty `Enter` re-prompts;
> non-empty enters the turn (separator printed); `/q` quits and `Drop`
> restores cooked mode. `Ctrl-C`/`Esc`/panic restore paths are wired
> (handler code present) but only the normal-exit Drop path is verified here.
- [x] `src/repl.rs`: `TermGuard` with `start()`/`stop()` implementing the §3.5.2
      CSI ordering (snapshot → `enable_raw_mode` (no `smcup`) → `?2004h` →
      `modifyOtherKeys` `>1;1u`/`>4;1u`; idempotent `stop()`).
      `pub mod repl;` added to `lib.rs`. (Kitty `PushKeyboardEnhancementFlags`
      push deferred to Stage 4 — `modifyOtherKeys` is the sanctioned fallback
      per §3.5.1/§3.5.2.)
- [x] `Drop` for `TermGuard` (restores terminal); `main.rs:56` panic hook kept.
- [x] `Repl` activation gated behind `std::io::IsTerminal(stdin)` **and**
      `!quiet` (new `quiet: bool` field on `CodeAgent`/`AgentConfig`, threaded
      from `main.rs`). Non-TTY / `--quiet` still use `get_user_input`.
- [x] Event loop with `crossterm::event::read` (`read_input`); **single-line
      input only** for this stage (insert/backspace/delete/left/right/home/end,
      `Enter` submit, `Esc` cancels line, `Ctrl-C` → `guard.stop()` + `exit(1)`).
- [x] `agent.rs::run_interactive` wired to `Repl::read_input` when active;
      `ConsoleOutput::get_user_input` retained for `confirm()`/non-TTY.
- [x] **Unit tests** (`repl.rs`): pure `render_line(prompt, buf, cursor)`
      (cursor-at-end / mid-buffer / empty); 3 tests passing.
- [~] **Manual TTY test** (Ctrl-C + forced panic restore): code paths wired
      and normal-exit restore verified via pty; interactive Ctrl-C/panic check
      still recommended for a final human sign-off.

### Stage 3 — Slash commands + `/model` (fetch, cache, extrapolation) + `/thinking` — ✅ DONE
> Implemented 2026-07-16. Build green, **no warnings**; 43 lib tests pass
> (incl. new `thinking` ×7, `model_catalog` ×7, `slash` ×2; 1 `#[ignore]`
> live-network test). Pty smoke test verified `/help`, `/thinking low` →
> "thinking level -> low", `/model` opens the live selector, clean `/q`
> exit (status 0). Outside the sandbox the `/model` list shows updated
> models from OpenRouter; **alignment/wrapping of that list is a known
> separate issue** (see note at end of this block) — belongs with Stage 6
> redraw / a dedicated formatting pass, not Stage 3.
- [x] `src/model_catalog.rs`: `fetch_openrouter()` via `reqwest` (unauthenticated
      `GET /api/v1/models`, **time-boxed 8s** so a slow endpoint can't hang the
      REPL); `parse_openrouter` maps `context_length` + `pricing.{prompt,completion}`
      (per-token → per-million) + `supported_parameters` (`reasoning`).
- [x] `provider_available(p)` env-var scan (mapping from `agent.rs`); gates
      extrapolation + defaults unavailable providers out of the direct list.
- [x] Extrapolation: `{provider}/{model}` → direct entries gated on availability;
      `DIRECT_PRICING` table (per-million, direct rates); OR price used as
      `OpenRouterEstimate` fallback (tagged `(est)`, shown when local lacks a
      price — user-confirmed desired behavior); native `/models` liveness prune
      (DeepSeek/OpenAI, **time-boxed 5s**) prunes stale + surfaces new models.
- [x] `/thinking` → `src/thinking.rs` mapping (anthropic `thinking` block /
      openai `reasoning_effort` / openrouter `reasoning.effort`; other→`None`);
      applied by **rebuilding the rig `Agent`** via `AgentBuilder::additional_params`
      whenever the level (or model) changes. `AgentConfig::thinking_level` +
      `CodeAgent` track it; the interactive loop is a free `run_interactive`
      that owns `Box<dyn PicoAgent>` + `AgentConfig` and rebuilds on slash changes
      (required: `CodeAgent<M>` is generic over the provider's model type).
- [x] `/model` UI: `repl.select` list picker (raw TTY) or numbered list
      (non-TTY); `format_option` shows `{provider}:{model}` + ctx + cost +
      `(est)` tag + `[reasoning]`. Selection rebuilds the agent with the chosen
      provider/model. (Per-model `supported_parameters` gating of thinking
      injection: deferred — `/thinking` currently maps by provider.)
- [x] `/help` command (lists commands + key bindings).
- [x] 5-min cache (`OnceLock<Mutex<Cache>>` + `Instant` expiry), whole-catalog key.
- [x] **Unit/integration tests** (`model_catalog.rs`): `parse_openrouter` fixture;
      `extrapolate` (direct entries + availability gating + Direct/Estimate
      pricing); `format_option` (est tag when local price missing); `provider_available`;
      `thinking` mapping per provider. `#[ignore]` live `catalog()` test for manual runs.
- [~] **Known follow-up (separate issue, not Stage 3):** the `/model` list rows
      are not yet column-aligned or width-wrapped; long lines overflow. Fix with
      a padded/aligned layout + `wrap()` (Stage 0) and redraw on resize (Stage 6).

### Stage 4 — Multi-line editing — ✅ DONE
> Implemented 2026-07-16. Build green, **no warnings**; 54 lib tests pass
> (incl. 12 new `repl` tests: insert/backspace/delete boundaries, cross-line
> move, word boundaries, `Ctrl-U`/`Ctrl-K`, `layout` word-wrap + prompt offset +
> hard-newline split, `render_input` multi-row cursor positioning, `History`
> navigation with draft restore). Pty smoke (deepseek): basic editing + `Esc`
> cancel + `/q` exits 0; `<Shift-Enter>` (`\x1b[13;2u`) inserts a newline
> (stays in editor, no LLM submit) — confirmed via Kitty `>1u`/`>4u` flags.
- [x] Arrows, `Home`/`End`, `Backspace`/`Delete`, word-left/right (`Alt-B`/`Alt-F`):
      pure `LineEditor` (flat `Vec<char>` + char-index cursor) in `src/repl.rs`.
- [x] `<Shift-Enter>` inserts newline via Kitty layer (`TermGuard` now emits the
      **correct** via crossterm `PushKeyboardEnhancementFlags(
      DISAMBIGUATE_ESCAPE_CODES | REPORT_ALL_KEYS_AS_ESCAPE_CODES)` (emits `[>9u`) — this is
      what actually makes wezterm/alacritty/kitty send `Shift+Enter` as `[13;2u`
      (Enter+SHIFT). `Ctrl-J` (LF) is a terminal-agnostic newline fallback; both are
      documented in `/help`. `<Enter>` submits; bracketed paste (`Event::Paste`)

      **Empirical (2026-07-16, wezterm):** wezterm's `enable_kitty_keyboard`
      defaults to **false**, and the app's `[>9u` push does NOT override that
      config — `Shift+Enter` only disambiguates once the user sets the config true.
      So `Ctrl-J` is the reliable newline for users who won't change terminal settings.
      inserts verbatim via `insert_str`; input box expands (multi-row) and the
      terminal's natural scroll pushes session content up.
- [x] History `Up`/`Down` (`src/repl.rs::History`): in-memory + appended to
      `~/.config/picocode/history` (`$HOME/.config/picocode/history`); `Up`
      navigates when already on the first/last logical line, else moves the
      cursor across lines; in-progress line restored when paging past newest.
- [x] **Unit tests** (`repl.rs`): insert; cross-line cursor move; boundary
      backspace/delete; word-boundary `layout` wrap + prompt offset; paste
      insertion; `render_input` multi-row cursor positioning; `History` nav.
- [~] Rendering uses `layout()` word-wrap (prompt offset on row 0, CJK
      char-wrap) + a save/restore renderer (`[s`/`[u`) so each redraw is
      idempotent and reclaims the `/model` picker's screen cleanly. Visual
      alignment of wrapped rows is still basic (ties to the Stage 3 `/model`
      alignment follow-up + Stage 6 resize redraw).

### Stage 5 — Signals (Ctrl-C / Esc / Ctrl-Z / SIGCONT)
- [ ] Concurrent stdin reader thread → `mpsc`: routes `Esc` (cancel), `Ctrl-C`
      (exit), `Ctrl-Z` (suspend).
- [ ] §3.4.2 handlers: `Esc` → `user_cancel` (cancel bridge); `Ctrl-C` key →
      `guard.stop()` + `process::exit(1)`; `Ctrl-Z` key → `guard.stop()` +
      `killpg(getpgrp(), SIGTSTP)`.
- [ ] `SIGCONT` handler (`libc::signal`/`tokio::signal`): `guard.start()` +
      `repaint_input_region_only()`.
- [ ] Cancel bridge: `PromptHook` impl carrying `Arc<AtomicBool>`, calls
      `CancelSignal::cancel()` when set (§3.4.3).
- [ ] Wire `ctrlc`/tokio handlers + panic-hook `Drop` guard in `main.rs`/`agent.rs`.
- [ ] **Integration test / manual**: `Esc` aborts an in-flight request and returns
      to a fresh prompt; `Ctrl-C` exits cleanly (terminal restored); `Ctrl-Z`
      backgrounds the group and `fg` resumes with input repainted.

### Stage 6 — Resize redraw
- [ ] `Event::Resize` handler: re-wrap in-memory history at new width; clear
      visible frame (`\x1b[0J`) not `\x1b[3J`; repaint input/list region only.
- [ ] **Manual test**: resize mid-session → no scrollback wipe, no double-wrap;
      reading earlier content stays put on new output.

## 6. Risks / open items

- **`/model` primary source is OpenRouter's live `/api/v1/models`** (rig 0.28 lists
  only `mira`); direct-provider entries are extrapolated from OpenRouter's
  `{provider}/{model}` IDs + a curated `DIRECT_PRICING` table, with native `/models`
  liveness pruning. Small fallback table only for offline / OR-uncovered providers.
- **Cancel semantics:** `Esc` → `Arc<AtomicBool>` → `PromptHook` calls
  `CancelSignal::cancel()` (§3.4.3). Confirmed rig exposes `cancel()` publicly; the
  signal itself cannot be constructed outside rig.
- Keeping `ConsoleOutput` (cooked) and `Repl` (raw) in sync for styling — the
  `Output` trait stays the rendering side; `Repl` only owns input + framing.
- Non-TTY / `--quiet` / `LogOutput` / `NoOutput` paths must remain **untouched**
  (they rely on cooked `read_line`). Guard `Repl` activation behind
  `std::io::IsTerminal` on stdin + not `--quiet`.
- **`DIRECT_PRICING` staleness:** the direct-rate table is hand-authored (provider
  APIs expose no pricing). It is smaller/more stable than a full model catalog and
  only needed for configured providers. Follow-up: optionally generate it from each
  provider's published price page at build time (mirroring pi's generator).
- **Raw-mode output needs `OPOST` re-enabled (root cause of `/help` stagger).**
  `crossterm::enable_raw_mode` calls `cfmakeraw`, which clears `OPOST` (output
  post-processing). With `OPOST` off the terminal does **not** translate `\n` →
  `\r\n`, so every bare `\n` only moves down a row and leaves the cursor in the
  same column — producing staggered "indentation" for any multi-line REPL output
  (help, separators, **and termimad agent responses**). Fixed in `TermGuard::start()`
  by re-enabling `OPOST | ONLCR` on `/dev/tty` (via `libc`) immediately after
  `enable_raw_mode`; keeps raw *input* semantics. `disable_raw_mode` restores the
  original cooked mode, so nothing leaks. (`print_block` and friends just emit `\n`
  and rely on this translation.)
- **`wrap_line` cursor placement (root cause of caret snapping to col 0).**
  In `repl.rs`, `wrap_line` only set the cursor column when the caret sat at a
  word *start* (`cursor == Some(i)`). When navigating left/backspace into the
  *middle* of a word, `cursor_pos` stayed `None`, so `Layout.cursor_col` defaulted
  to `0` and every redraw snapped the caret to column 0 (the "cursor jumps one
  character indented" / "walking back lands on the second char" reports). Fixed by
  computing the caret column for the actual position inside the word
  (`col + (cu - i)`) in both word-fit branches, and dropping a leading-space row
  start. Covered by `repl::tests::layout_cursor_tracks_mid_word`.
- **Ctrl-U did not cross newlines.** `delete_line_start` stopped at a line boundary
  (right after `\n`) and a second press did nothing, so the whole buffer could not
  be cleared. Fixed: when the caret is already at a line start, it now deletes the
  preceding `\n` *and* the whole previous line, so successive `Ctrl-U` presses
  walk backward one logical line at a time. Covered by
  `repl::tests::delete_line_start_walks_back_across_newlines` and
  `delete_line_start_keeps_partial_line`.
- **`wrap()` mangled punctuation/whitespace** (`Commands:` → `Commands :`, em-dash
  double-spaces). Rewritten to tokenize by whitespace (keeps `Commands:`/
  `(provider:name)` intact, single spaces between words, collapses alignment
  padding). `print_block` prints `/help` **verbatim** (preserving `{:<10}` column
  alignment) and only hard-wraps lines exceeding the terminal width.
- **TODO (later investigation):** `Shift+Enter` newline currently depends on the
  terminal's Kitty keyboard protocol (wezterm `enable_kitty_keyboard`) or the
  `Ctrl-J` fallback. **pi handles `Shift+Enter` as newline with *no* Kitty
  dependency** — investigate how (likely raw-byte reading / a different
  enhancement handshake / bracketed-paste or modifier inference) and adopt it so
  `Shift+Enter` works out-of-the-box on terminals that don't enable Kitty. See
  `pi/packages/coding-agent/src/...` interactive input handling.

## 7. Decisions & corrections log

Audit trail for the choices folded into Part A. Every earlier open question is now
resolved.

### 7.1 Corrections found during review (evidence-backed)
1. **`pi/` is NOT a dangling symlink.** `pi/packages/tui/src/terminal.ts` and
   `pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts` both exist
   and were cross-checked. The original "dangling symlink BLOCKER" was unfounded.
2. **`rig` 0.28 has no provider model-listing API** except `mira` (which returns
   `Vec<String>` names only). `openai`/`openrouter`/`deepseek` `/models` is
   `VERIFY_PATH` (API-key validation), not a listing (grep of
   `~/.cargo/.../rig-core-0.28.0/src/providers`). So `/model` uses a live
   OpenRouter fetch, not a static table.
3. **Raw mode clears `ISIG`** → `Ctrl-C`/`Ctrl-Z` are key events, not signals,
   while the REPL owns the terminal. The original "kernel default SIGINT" and
   "SIGTSTP signal handler" framings were incorrect; corrected in §3.4.
4. **rig `CancelSignal` cannot be constructed outside rig** (`new()` private) but
   `cancel()` is public and checked after each `PromptHook` callback. Cancellation
   bridges via our own `Arc<AtomicBool>` read inside the hook (§3.4.3).
5. **Line-number references** in the original constraints section were off: `ConsoleOutput::
   get_user_input` is `output.rs:327` (not 333); `QuietOutput::confirm` is
   `output.rs:72` (not 76). `agent.rs:249,262` (CancelSignal) and `main.rs:56`
   (panic hook) are correct.

### 7.2 Decisions (resolved open questions)
| # | Question | Resolution |
|---|---|---|
| Q1 | Accept `crossterm` + `fuzzy-matcher` deps? | **Yes** (§2). |
| Q2 | Which providers need live listing vs static fallback? | Live OpenRouter fetch primary; small curated fallback for offline/non-OR (§3.2). |
| Q3 | What does `/thinking` control? | Map to provider thinking params via `additional_params` (§3.2). |
| Q4 | Is `Ctrl-Z` a suspend-to-shell driving resume redraw? | **Yes**; suspend the **process group** via `killpg` (§3.4). Resume repaints input region only. |
| Q5 | `Ctrl-C` mid-prompt: hard-abort or stop-tools-keep-partial? | `Esc` = hard-abort (cancel bridge); `Ctrl-C` = **process exit** as a key event (§3.4.1). |
| Q6 | Paste detection: bracketed paste vs timing heuristic? | **Bracketed paste** (`Event::Paste`); no heuristic needed (§3.5.1). |
| Q7 | Input/list region: inline primary-screen vs alt buffer? | **Pi-style inline primary-screen render** (single scrollback); alt buffer permitted only for the slash-list popup if isolation is cleaner (§3.3, Appendix A). |
| Q8 | Kitty protocol required, or `modifyOtherKeys` only? | **Requiring Kitty is okay**; keep `modifyOtherKeys` as fallback (§3.5.1). |
| Q9 | `Ctrl-Z` suspend: re-raise `SIGTSTP` vs manual job control? | From the key event, `guard.stop()` + `killpg(getpgrp(), SIGTSTP)`; no re-raise dance needed in raw mode (§3.4.1). |
| OQ1 | Static-table-first for `/model`? | **No** — implement the live OpenRouter fetch directly (no catalog existed) (§3.2). |
| OQ2 | `Ctrl-C` = exit key event; `Esc` = hard abort? | **Confirmed** (§3.4). |
| OQ3 | Dedicated reader thread + `mpsc` for in-flight stdin? | **Confirmed**; channel can later carry queued prompts (§3.4.3). |
| OQ4 | `/thinking` map to provider params? | **Yes**, via `additional_params` + rebuild agent; best-effort + fall back to store/display (§3.2). |
| OQ5 | Wrap at emit (pre-wrap) or at render/resize? | **Wrap at render/resize**; in-memory buffer re-wrapped on resize; no `\x1b[3J` (§3.3, §3.6). |
| OQ6 | `Ctrl-Z` = `killpg(getpgrp(), SIGTSTP)` from key event? | **Confirmed** (§3.4). |

---

# Appendix A — Screen-handling model comparison (informed by `doc/pi_tui_screen_handling.md`)

`pi` uses a **third model** distinct from the two contrasted earlier (native-only
vs. raw + alt-buffer). Key facts from that doc:

- **Raw mode for input, but renders to the *primary* screen, NOT the alternate
  buffer** (`\x1b[?1049h`/`smcup` is deliberately absent — pi §3). The full frame
  (header → chat → input → footer) is **app-rendered via differential rendering**
  (`doRender` only repaints changed lines) — pi §2.
- **`chatContainer` holds the full, uncapped history** and is re-concatenated
  every frame; the viewport window is used only for scroll/cursor math + choosing
  which lines to repaint — pi §3. So history is app-managed, but **appended to
  real scrollback**, not an alt buffer.
- **Autocomplete pop-up is inline** — rendered as extra lines inside the editor's
  own `render()`, just above the footer — pi §2. No separate overlay.
- **Suspend (`Ctrl-Z`):** `ui.stop()` (restore cooked, disable bracketed
  paste/Kitty) → `SIGTSTP`; on `SIGCONT`, `ui.start()`. Last frame stays in
  primary scrollback, so `fg` returns to the chat in place — pi §1, §3.
- **Resize caveat (pi §3):** a width/height change forces `fullRender(true)` =
  `\x1b[2J\x1b[H\x1b[3J`, which **clears screen AND scrollback**. So history
  persistence is best-effort only during a stable-size session; any resize mid-
  session wipes scrollback (Termux exempted).

### Three models compared

| Concern | (A) Native-only | (P) pi hybrid: raw + primary-screen differential | (B) Raw + alt-buffer |
|---|---|---|---|
| Who owns scrollback | Terminal emulator | Terminal (primary); app *appends* | App (alt buffer); none after quit |
| History survives quit | Yes (stdout) | **Yes** (primary scrollback) | **No** (alt cleared on exit) |
| History survives Ctrl-Z/fg | Yes | **Yes** (last frame in scrollback) | Yes (alt preserved while suspended) |
| Resize redraw | Input region only; reflow free | Full frame repaint from in-memory model | Full frame repaint |
| Resize *wipes* scrollback? | No | **Yes, if size changes** (`\x1b[3J`) | No |
| "Don't force scroll to end" | Free (emulator keeps offset) | Must implement manually (viewport math) | Must implement manually |
| Inline autocomplete popup | Hard (no managed frame) | Trivial (more render lines) | Trivial |
| Re-implement scrolling? | No | Partially (viewport/scroll math) | Yes |
| Failure surface if SIGCONT missed | Low | Medium (corrupt frame; scrollback intact) | High (blank alt screen) |

### What this changes for the plan

pi shows the raw-managed approach **does not have to lose history** — use the
primary screen, not the alternate buffer. But two of its traits conflict with
`doc/requirements.md`:

1. pi **re-implements viewport scrolling** (§2/§3 scroll math). Our requirements
   explicitly say *"do not re-implement scrolling"* for session history.
2. pi's **resize clears scrollback** (`\x1b[3J`) — the exact failure our native
   proposal was meant to avoid.

### Decision (refined)

Adopt a **scoped pi-hybrid** that keeps pi's wins and avoids its traps:

1. **Session history = native stdout on the primary screen, app-appended** (like
   pi §3), **not** an alt buffer. Preserves quit/suspend history for free.
2. **Input area + autocomplete popup = the only managed part.** Either (a) inline
   render lines above the footer on the primary screen (pi style), or (b) a small
   alt buffer just for the input/list. `requirements.md` explicitly permits an alt
   buffer for the popup and accepts halting scroll there, so (b) is sanctioned —
   but (a) keeps everything in one scrollback. **Decision (Q7): use (a)
   pi-style inline** for history continuity, with (b) as the slash-list variant if
   isolation is cleaner.
3. **Resize: re-render history at the new width, but never `\x1b[3J`.** Keep an
   in-memory history buffer; on resize, clear only the visible frame (`\x1b[0J`)
   and re-emit the buffer wrapped at the new width, then repaint the input/list
   region. We deliberately avoid pi's `fullRender(true)` = `\x1b[2J\x1b[H\x1b[3J`
   scrollback wipe, so scrollback above the frame stays intact. This trades the
   earlier "never touch history" simplicity for correct wrapping after resize
   (avoids double-wrapping).
4. **Ctrl-Z/fg:** mirror pi — restore cooked + disable bracketed paste/Kitty before
   `SIGTSTP`; re-enter raw + repaint input region on `SIGCONT`. History is already
   safe in scrollback.

# Appendix B — Consolidated REPL lifecycle state machine

Single reference tying together §3.4.2 (signals), §3.5.2 (capability CSI order),
and Appendix A (screen model). The `TermGuard` (§3.5.2 `start()`/`stop()`) is the
only component that touches terminal modes; every transition below routes through
it.

### B.1 States

| State | Terminal mode | Scrollback | Repaint scope |
|---|---|---|---|
| `Cooked` | cooked (default) | n/a (not active) | n/a |
| `RawActive` | raw, paste on, kitty/`modifyOtherKeys` on, cursor placed | **primary** (history appended here) | input/list region only |
| `Suspended` | cooked (restored, identical to `Cooked`) | primary (last frame intact) | none (process bg'd) |

### B.2 State diagram

```
                         program start (interactive TTY)
   (start REPL) ───────────────────────────────────────────────▶ Cooked
                                                                    │
                                                    guard.start()  │
                                                    raw + ?2004h   │
                                                    + Kitty ?u     │
                                                                    ▼
                                                              RawActive ◀──────────────┐
                                                                │  │                     │
                                    ┌───────────────────────────┘  └─────────────────┐   │
                                    │ (Ctrl-Z / SIGTSTP)            (Ctrl-C / SIGINT) │   │
                                    ▼                                             ▼   │   │
                              Suspended ◀── on_sigcont() ──┐              soft-cancel   │   │
                                │  │                       │            (flip cancel)  │   │
                guard.stop()    │  │  fg (kernel SIGCONT)  │                        (loop back
                drain+pop+      │  └───────────────────────┘                          to RawActive
                restore cooked  │                                                      after await)
                                    │
                    (real bg via SIGTSTP, §3.4.2)
                                    ▼
                              [ backgrounded to shell ]
                                    │  fg
                                    └──▶ on_sigcont(): guard.start() + repaint_input_region_only()
                                         ▼
                                       RawActive

   RawActive/Suspended ──(normal exit / panic / Drop / any signal)──▶ guard.stop() ──▶ Cooked
                                    (idempotent; drain → pop kitty → ?2004l → restore was_raw)
```

### B.3 Transitions (exact CSI / actions)

| Transition | Trigger | Actions (order matters) | Ref |
|---|---|---|---|
| `Cooked → RawActive` | REPL start (TTY) | snapshot `was_raw`/paste/kitty → `enable_raw_mode()` (no `smcup`) → `\x1b[?2004h` → push Kitty `?u` flags 1+2+4 (or `\x1b[>1;1u`/`\x1b[>4;1u`) → `setup_stdin_reader()` | §3.5.2 |
| `RawActive → Suspended` | `Ctrl-Z` **key event** | `guard.stop()` (drain → pop Kitty → `\x1b[?2004l` → restore `was_raw` + show cursor) → set `suspended` → `killpg(getpgrp(), SIGTSTP)` (key event, not a SIGTSTP handler) | §3.4.1, §3.4.2 |
| `Suspended → RawActive` | `fg`/`SIGCONT` | clear `suspended` → `guard.start()` (re-enable raw/paste/kitty, re-snapshot) → `repaint_input_region_only()` (**no** `\x1b[3J`, history untouched) | §3.4.2, App. A |
| `RawActive ⇄ RawActive` | `Ctrl-C` **key event** | busy: `Esc` sets `user_cancel` (rig aborts via §3.4.3 bridge); `Ctrl-C` key → `guard.stop()`+`process::exit(1)`; idle+empty `Ctrl-C` → exit; else abort line | §3.4.1, §3.4.2, §3.4.3 |
| `RawActive → Cooked` | exit/panic/Drop/signal | `guard.stop()` idempotent (drain → pop Kitty → `\x1b[?2004l` → restore `was_raw`) | §3.5.2 |
| any → Cooked (non-TTY) | piped/quiet/`LogOutput`/`NoOutput` | **skip all of the above**; use cooked `read_line` | §6 |

### B.4 Invariants

1. **No `smcup`/`rmcup` (`\x1b[?1049h`/`l`).** History lives in the primary
   scrollback, so quit/suspend leaves the conversation intact (Appendix A). The
   alt buffer (if used for the popup) is entered/left *locally* and never with a
   scrollback clear.
2. **Never emit `\x1b[3J` / `fullRender(true)`.** Resize and resume repaint only
   the input/list region — history persistence is preserved across resize (the pi
   caveat is explicitly avoided).
3. **`guard.stop()` is idempotent** — safe to call from `Drop`, panic hook
   (`main.rs:56`), and signal handler simultaneously.
4. **Handlers registered only after `guard.start()` and only for a TTY.**
5. **`SIGTSTP` is delegated to the kernel** via `killpg(getpgrp(), SIGTSTP)` from
   the `Ctrl-Z` key-event handler after `guard.stop()`; we do not re-implement job
   control (§3.4.1/§3.4.2).
6. **Wrap at render/resize, never pre-wrap at emit** (§3.3): history is kept in an
   in-memory buffer and re-wrapped at the current width on output and on resize, so
   the terminal never double-wraps. Resize redraws only the visible frame
   (`\x1b[0J`), never `\x1b[3J` — keeping the "native scrolling" guarantee and the
   no-scrollback-wipe property.

> Note: all `pi` line/function references are sourced from the summary
> `doc/pi_tui_screen_handling.md`. The earlier "dangling symlink" claim about
> `pi/` was wrong — the source tree is present and the cited files were
> cross-checked (see §7.1).
