//! The interactive session: the REPL's main loop, and the agent-rebuild gate
//! it consults each cycle.
//!
//! This is the loop's own layer, deliberately outside [`crate::agent`]. It
//! *drives* the agent — reads input, converges the applied config against the
//! menu-visible target, rebuilds the agent when that target changes, runs a
//! turn — where `agent.rs` is what the rebuild produces. Keeping the
//! dependency one-way (`interactive` → `agent`) makes that layering visible in
//! the import block rather than in a doc comment.
//!
//! Nothing else depends on this module; [`run_interactive`] is the entry point
//! `main` awaits, and it returns when the user quits.

use crate::agent::build::{create_agent, effective_params, AgentConfig};
use crate::agent::errors::{retry_budget, retry_delay_ms};
use crate::agent::pumps::{cancellable_sleep_pumped, turn_backoff_pump};
use crate::agent::status::resolved_context_limit;
use crate::agent::{build_session_ctx, ensure_session, record_reasoning_change, TurnPersistence};
use crate::app_state::AppState;
use crate::confirm::ConfirmBridge;
use crate::model_catalog;
use crate::params;
use crate::repl::{History, InputAction, ModelFilter, PromptSession, Repl, ReplCtx};
use crate::session::repo::JsonlSessionRepo;
use crate::settings::SettingsStore;
use crate::termio::InputEvent;
use crate::tools::set_sandbox;
use crate::Result;

use rig_core::message::{AssistantContent, Message, Text};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Load the list of recently-used model IDs from `recent_models.txt`.
/// Returns an empty vec if the file doesn't exist or can't be read.
fn load_recent_models() -> Vec<String> {
    let path = match crate::platform::recent_models_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Atomically write the list of recently-used model IDs to `recent_models.txt`.
/// Writes to a temp file in the same directory, then renames (atomic on POSIX).
fn save_recent_models(models: &[String]) {
    let path = match crate::platform::recent_models_path() {
        Some(p) => p,
        None => return,
    };
    let dir = match path.parent() {
        Some(d) => d,
        None => return,
    };
    // Build the content as one model id per line.
    let content = models.join("\n") + "\n";
    // Write to a temporary file in the same directory, then atomically rename.
    let tmp_name = format!(".recent_models.tmp.{}", std::process::id());
    let tmp_path = dir.join(&tmp_name);
    match std::fs::write(&tmp_path, &content) {
        Ok(()) => {
            let _ = std::fs::rename(&tmp_path, &path);
        }
        Err(_) => {
            // Best-effort cleanup on write failure.
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// Interactive chat loop. Owns the agent and the `AgentConfig` so slash
/// commands that change model/provider/thinking can rebuild the agent via
/// `create_agent`.
///
/// (The parenthetical this used to carry — "required because `CodeAgent<M>` is
/// generic over the provider's completion-model type" — died with the rig 0.42
/// migration that made `CodeAgent` concrete. The rebuild mechanism outlived its
/// original reason, which is usually a sign the comment should go rather than
/// the code.)
/// Fingerprint of the target state the REPL has *attempted* to build an agent
/// for. Equality is the whole point: the loop rebuilds once per distinct
/// fingerprint, so a failed `create_agent` is reported once instead of every
/// cycle while the user navigates back to the menu.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RebuildFingerprint {
    pub model: String,
    pub params: params::ModelParams,
    pub persona: Option<String>,
    /// Part of the identity, not just the comparison: a Ctrl-E that changes the
    /// context window is a new target, so a *previous* failed attempt must not
    /// dedupe it away — the user edited something, which is exactly the signal
    /// the dedupe is meant to wait for.
    pub context_limit: Option<u64>,
}

/// Decide whether the convergence step should rebuild the agent this cycle.
///
/// Returns the fingerprint to record on a rebuild, or `None` to leave the
/// running agent alone. Two invariants, both easy to break and awkward to
/// test through the REPL loop, so they live here:
///
/// 1. **Only a real change rebuilds.** The applied config and the runtime
///    target are compared field by field — never by re-deriving the model id
///    from segments, which would make every commit look like a model change.
/// 2. **One attempt per distinct target.** Re-arming requires a *different*
///    fingerprint, so a target that already failed is not retried until
///    something actually changes. Recovery is "change the target", because
///    `create_agent` errors (missing key, bad model id) are final for a given
///    target.
///
/// Any param field counts as a change: temperature and top_p are baked into
/// the agent's request builder, so there is no in-place path for them.
pub(crate) fn plan_rebuild(
    applied: &AgentConfig,
    target: &AgentConfig,
    last_attempt: Option<&RebuildFingerprint>,
) -> Option<RebuildFingerprint> {
    let changed = applied.qualified_model_id() != target.qualified_model_id()
        || applied.params != target.params
        || applied.persona_name != target.persona_name
        // The window is compared because it reaches the mid-turn status bar
        // only through a rebuild, and a per-model `context_length` override
        // moves nothing else here.
        || applied.context_limit != target.context_limit;
    if !changed {
        return None;
    }
    let attempt = RebuildFingerprint {
        model: target.qualified_model_id(),
        params: target.params,
        persona: target.persona_name.clone(),
        context_limit: target.context_limit,
    };
    if last_attempt == Some(&attempt) {
        return None;
    }
    Some(attempt)
}

/// The state of a live interactive session, carried across REPL turns.
///
/// Everything that must survive one input cycle is a field here: the applied
/// config, the REPL handle, the carried prompt session, the cancel flags, the
/// session repo, and the rebuild gate's fingerprint.
/// [`run_interactive`] builds one of these and drives [`Interactive::run`].
struct Interactive {
    config: AgentConfig,
    prompt_str: String,
    confirm_bridge: ConfirmBridge,
    repl: Option<Repl>,
    repo: Option<JsonlSessionRepo>,
    line_history: History,
    cancel: Arc<AtomicBool>,
    app_state: Arc<AppState>,
    last_attempted_target: Option<RebuildFingerprint>,
    prompt_session: Option<PromptSession>,
    /// The persistent reader's input queue — the one receiver of the
    /// `TermIO::handles()` distribution this struct keeps (design §3:
    /// distributed once at setup). `read_input`
    /// consumes it at the prompt, each turn borrows it as `InTurnInput::keys`
    /// — one channel for the process, so a key typed at the end of a turn
    /// lands in the next prompt instead of dying with a per-turn queue.
    /// `caret` went to the manager (the event's only consumer), `size` was
    /// a spare — content and the manager took theirs at construction.
    input: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
}

/// Whether the input loop keeps going after an arm runs.
enum LoopCtl {
    /// Run another cycle.
    Continue,
    /// Exit the loop.
    Break,
}

pub async fn run_interactive(
    config: AgentConfig,
    settings: SettingsStore,
    auto_resume: bool,
) -> Result<()> {
    let mut interactive = Interactive::new(config, settings, auto_resume).await;
    let result = interactive.run().await;
    // Teardown: the loop's exits (`/quit`, an error mid-turn) leave raw mode
    // on, and the `TermGuard` drop backstop can never fire — the persistent
    // stdin reader holds an `Arc` clone for the process lifetime. The front
    // door is idempotent (Plain's `Unsupported` is a no-op here), so this
    // is unconditional.
    let _ = interactive.config.content.manager.lock().unwrap().suspend();
    result
}

impl Interactive {
    /// Build the session state: publish the sandbox config, open the REPL and
    /// session repo, seed the initial session and model params, and arm the
    /// cancel flags. Runs once, before the input loop.
    async fn new(config: AgentConfig, settings: SettingsStore, auto_resume: bool) -> Self {
        let mut config = config;

        // Store the original (non-merged) sandbox config so the remove overlay
        // can list config-provided bindings and allow temporary exclusion.
        crate::sandbox_state::set_base(config.sandbox.clone());
        // Publish the resolved sandbox config immediately so that user-initiated
        // `!`/`!!` shell commands use the same bwrap/timeout policy as the agent's
        // `bash` tool — even before the agent is lazily created on the first
        // normal prompt. `set_sandbox` is otherwise only called inside
        // `build_rig_agent`, so without this the first `!`/`!!` (issued before any
        // LLM turn) would run UNSANDBOXED. The sandbox config is constant for the
        // session (`apply_to_config` never touches it), so publishing once here is
        // sufficient.
        set_sandbox(config.sandbox.clone());
        // The agent lives in the AppState slot: created lazily on first use
        // (or on /model, /thinking — commands like /help must work without a
        // network round-trip), always **loop-side from the applied config**,
        // published via `activate_agent`; every reader clones it out.

        // Empty prompt: the REPL input is left-aligned on every row (a `> ` prefix
        // would only indent the first line and leave wrapped/continuation lines
        // ragged). Mirrors pi's left-aligned input region.
        let prompt_str = String::new();
        let use_repl = std::io::stdin().is_terminal() && !config.quiet;
        // The confirm port, built here and *before* any agent exists, because the
        // guards capture the sender half at build time and a `/model` rebuild
        // re-captures this same one — so a session approval survives the rebuild.
        // The receiver half is borrowed per turn below (it cannot outlive the pump
        // that answers it).
        //
        // The bridge itself is built unconditionally so the receiver always exists;
        // only the *port* is conditional. Without a REPL there is no key pump to
        // draw a dialog, so no guard holds the sender and this receiver simply
        // never yields — the same degradation `InTurnInput::keys` already uses.
        let confirm_bridge = crate::confirm::ConfirmBridge::new();
        if use_repl {
            config.confirm_port = Some(confirm_bridge.port.clone());
        }
        // The terminal backend was constructed first, in `main`:
        // Tty iff `use_repl`, already seeded into `Content` (size watch)
        // and moved under `Draw` on the way in — its caps stay inside it.
        // Nothing here names
        // `TermIO` — the manager's front door is the whole surface
        // (`resume`/`suspend`/`is_tty`/`handles`/`probe_size`).
        let repl = use_repl.then_some(Repl);
        if repl.is_some() {
            // Raw-mode entry (idempotent), where `Repl::start` used to do it.
            let _ = config.content.manager.lock().unwrap().resume();
            // First screen: a one-line hint
            let content = &config.content;
            content.display_text("Welcome to picocode - type `/` for details.");
            content.display_separator();
        }

        // Persistent session storage.
        // Session storage: create repo eagerly (just a path, no I/O) for /session
        // overlay, but defer actual session file creation to the first turn
        // to avoid empty sessions.
        let sessions_dir = config
            .sessions_dir
            .clone()
            .or_else(|| crate::platform::app_config_dir().map(|d| d.join("sessions")));
        let cwd = std::env::current_dir().ok();
        let repo: Option<crate::session::repo::JsonlSessionRepo> =
            if let (Some(dir), Some(_)) = (&sessions_dir, &cwd) {
                let repo = crate::session::repo::JsonlSessionRepo::new(dir.clone());
                // Repo is created eagerly (just a path); the session object is also
                // created unconditionally here so the `/session` overlay can mark the
                // current session from the start. `repo.create()` only opens the
                // storage — the actual `.jsonl` file is written lazily on the first
                // `append_message()` (Session::ensure_persisted) — so no empty session
                // file is created if the user never takes a turn. Creating up front
                // removes the mid-loop `ensure_session` calls (in the first-turn and
                // `!`/`!!` paths).
                Some(repo)
            } else {
                None
            };
        let line_history = crate::repl::History::load();
        // Shared cancel flag: set on a root `Esc` by the turn's key branch (the
        // in-flight reader used to set it, but it cannot see a menu and so could
        // not honour menu-first); the `LoggingHook` bridge turns it into a rig
        // `CancelSignal` that aborts the current request.
        let cancel = Arc::new(AtomicBool::new(false));
        // So `/model` etc. embed the same flag into the rebuilt agent.
        config.user_cancel = cancel.clone();
        // Event-based cancel signal — the streaming loop selects on this instead
        // of polling.  Paired with `user_cancel` above.
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        config.cancel_notify = cancel_notify.clone();
        // Single source of truth for menu-visible state. The target config is
        // seeded from the startup config (same Arc handles: output, cancel
        // flags); the loop's local `config` remains the applied layer that
        // converges to it. Settings move in as the one durable copy.
        let app_state = AppState::new(config.clone(), settings, repo.clone());
        // Load-or-seed the initial model's live params from its stored record, then
        // pull them back into the applied layer. Seeding before `ensure_session`
        // below is what puts the right level in the session header, and copying
        // them into `config` means the first agent is built once with the final
        // values instead of being rebuilt on the first cycle to catch up.
        let initial_model = config.qualified_model_id();
        app_state.activate_model_params(&initial_model);
        config.params = app_state.params();
        // The resolved context window, read once here and carried on the config so
        // the status bar can divide by it without a store lookup per paint.
        config.context_limit = resolved_context_limit(&app_state);
        // Onto the target too, or the two sides disagree at startup and the
        // convergence step rebuilds the agent once for nothing.
        let startup_limit = config.context_limit;
        app_state.update_config(|c| c.context_limit = startup_limit);
        // First session — created up front so the `/session` overlay can mark
        // it from the start (the file itself is written lazily on the first
        // `append_message`, so no empty session is created for a user who
        // never takes a turn).
        let first = ensure_session(
            app_state.session(),
            &repo,
            Some(config.provider.clone()),
            Some(config.model.clone()),
            config.params.reasoning_str(),
        )
        .await;
        let first_id = first
            .as_ref()
            .map(|s| s.get_session_id().to_string())
            .unwrap_or_default();
        app_state.activate_session(first, first_id);
        app_state.set_recent_models(load_recent_models());
        // Seed the model switch from the configured default model so it appears
        // in the recent list on first run (if present and available).
        app_state.seed_recent_model(&config.qualified_model_id());
        // Rebuild gate state: fingerprint of the last target we ATTEMPTED to
        // apply. A failed attempt (bad id, missing key) stays silent and
        // un-retried until the target changes — one error per distinct target,
        // not one per cycle while the user navigates back to the menu.
        let last_attempted_target: Option<RebuildFingerprint> = None;
        // The persistent stdin reader: one thread, spawned
        // once by `TermIO::handles` — reached through the manager, which has
        // owned the backend since construction (raw mode is already on),
        // owning stdin for the process lifetime. It forwards keys/paste on
        // this channel; the prompt loop and every turn consume the receiver
        // half. The reader backs off when the main task takes stdin over for
        // `$EDITOR` — via the pause gate (`termio::PAUSE_GATE`, raised by
        // `TermIO::suspend`). It never stands down for an LLM window: the
        // compaction pump consumes the reader's *divert* instead.
        // DSR needs no gate: from here on the reader answers those itself.
        //
        // The entry DSR runs *before* `handles()` spawns the reader, so the
        // main task is then the one and only stdin reader — the reply cannot
        // be swallowed and no pause is needed (the old raise-before-spawn
        // dance, gone). Ordering with raw mode is still resume → banner →
        // DSR: the reply is not echoed and the terminal can answer.
        let handles = if use_repl {
            crate::repl::run_entry_dsr(config.content.as_ref());
            config.content.manager.lock().unwrap().handles()
        } else {
            // Plain (piped/quiet): no reader, and `handles()`' input channel
            // arrives closed — the shape piped mode had when it dropped the
            // sender half, so nothing ever reads it in that mode.
            config.content.manager.lock().unwrap().handles()
        };
        // Distribute the hand-out (design §3): the caret half's only
        // consumer is the render manager, which takes it here — the reader
        // sends its `Invalidated` events from the first poll onward, so the
        // receiver must be installed before the prompt loop runs.
        let crate::termio::TermHandles {
            input,
            size: _size,
            caret,
        } = handles;
        config
            .content
            .manager
            .lock()
            .unwrap()
            .set_caret_receiver(caret);
        // Warm the model catalog cache in the background so the `/model`
        // overlay is populated within a few seconds *without* blocking the
        // first prompt. The overlay falls back to the cached/static list
        // until the fetch completes.
        tokio::task::spawn(async {
            let _ = model_catalog::catalog().await;
        });
        // One-shot input buffer preload. A `/session <id> resume` may compute a
        // `/model <provider>:<model_id>` prefill to surface the resumed session's
        // stored model to the user (so they can press Enter to accept, Esc to
        // dismiss, or type to choose a different model). It is set by the
        // `Switched` arm of the session-command dispatch and consumed by the next
        // `read_input` call; cleared afterwards so subsequent prompts start empty.
        //
        // The CLI `--resume` flag populates this BEFORE the loop so the very
        // first prompt already has the slash command queued — the user presses
        // Enter once and goes through the exact same code path as a manual
        // `/session`, Tab, Enter: the REPL parses `/session <id> resume`, the
        // session node's `submit` opens + replays the session, and the
        // usual `/model …` prefill flows back into the slot's prefill
        // second prompt. This keeps the history and UX identical to a typed
        // resume (including the model-overlay catalog fetch that needs the
        // inter-keystroke time to land).
        if auto_resume {
            // Pre-fill with the session listing for the current directory.
            // The user presses Enter to resume the top (most recent) session.
            // This is simpler than resolving the session ID upfront and avoids
            // any race conditions with the session repository listing.
            app_state.set_prefill("/session .path:.".to_string());
        }
        // One prompt session, carried across turns (ui_draw_v2 §4.5 U4).
        //
        // Declared outside the loop so its life spans `read_input` → the whole
        // turn → the *next* `read_input`, which is the only shape in which typing
        // during a turn has anywhere to land. `None` until the interactive path
        // builds it: piped/batch input has no prompt to carry.
        let prompt_session: Option<crate::repl::PromptSession> = None;

        Self {
            config,
            prompt_str,
            confirm_bridge,
            repl,
            repo,
            line_history,
            cancel,
            app_state,
            last_attempted_target,
            prompt_session,
            input,
        }
    }

    /// Drive the input loop: read one input, converge the applied config
    /// against the target, then dispatch. Runs until the user quits.
    /// Convergence: fold the runtime-target config (AppState) into the
    /// applied layer, then record session metadata and drain control
    /// targets — this loop is the targets' sole consumer. Runs every
    /// cycle: run-to-completion means at most one menu submit per cycle,
    /// and a missed application point only delays convergence.
    async fn converge(&mut self) -> bool {
        let target = self.app_state.config_snapshot();
        let control = self.app_state.take_control();
        if control.quit {
            return false;
        }

        // Rebuild gate: canonical whole-string compares against the
        // RUNNING model (config), never a segment split — model ids may
        // themselves contain colons (openrouter `…:free`), and a
        // `nth(1)`-style truncation reads a different value than what is
        // configured, making every menu commit look like a model change.
        // Narrower flag for the journal: only a *level* change is worth a
        // `ThinkingLevelChange` entry, and recording it for a temperature
        // tweak would be noise.
        let reasoning_changed = self.config.params.reasoning != target.params.reasoning;
        let target_reasoning = target.params.reasoning_str();
        // What the loop's rebuild gate should do this cycle. Extracted so the
        // once-per-target invariant is testable without running the REPL.
        let rebuild = plan_rebuild(&self.config, &target, self.last_attempted_target.as_ref());
        let model_changed = self.config.qualified_model_id() != target.qualified_model_id();
        let params_changed = self.config.params != target.params;
        let persona_changed = self.config.persona_name != target.persona_name;

        // One rebuild attempt per distinct target: re-running a failed
        // attempt would reprint the same error every cycle while the user
        // digs back to the menu to fix it. Changing the target — or an
        // explicit `/model` re-commit of the same model — arms the next
        // attempt. (`create_agent` is local: key/config errors are final
        // for a given target, so recovery really is "change the target".)
        if let Some(attempt) = rebuild {
            self.last_attempted_target = Some(attempt);
            let mut new_cfg = self.config.clone();
            if model_changed {
                new_cfg.provider = target.provider.clone();
                new_cfg.model = target.model.clone();
            }
            if params_changed {
                new_cfg.params = target.params;
            }
            // Re-read on every rebuild: the window moves when the model does,
            // and also when the user Ctrl-E's a `context_length` override and
            // then switches away and back.
            new_cfg.context_limit = resolved_context_limit(&self.app_state);
            if persona_changed {
                new_cfg.persona_name = target.persona_name.clone();
                new_cfg.persona_prompt = target
                    .persona_name
                    .as_ref()
                    .and_then(|p| crate::persona::get_persona(p));
            }
            match create_agent(new_cfg.clone()).await {
                Ok(new_agent) => {
                    self.app_state.activate_agent(Arc::from(new_agent));
                    if model_changed {
                        let model_id = new_cfg.qualified_model_id();
                        let updated = self.app_state.note_recent_model(&model_id);
                        save_recent_models(&updated);
                    }
                    self.config = new_cfg;
                }
                Err(e) => {
                    let kind = if model_changed {
                        "switch model"
                    } else if params_changed {
                        "set model params"
                    } else {
                        "set persona"
                    };
                    self.config
                        .content
                        .display_text(&format!("Cannot {}: {}", kind, e));
                }
            }
        }

        // Thinking metadata recording (the model entry moved inline: the
        // /model node journals its explicit commit at submit time — its
        // record helper skips values the session already holds and folds
        // metadata into the in-memory header while the session is still
        // unpersisted). Runs AFTER the rebuild so `config` matches the
        // target when we record.
        let recording_session = self.app_state.session();
        if reasoning_changed && self.config.params.reasoning_str() == target_reasoning {
            if let Some(ref s) = recording_session {
                record_reasoning_change(s, target_reasoning.as_deref().unwrap_or("off")).await;
            }
        }
        // Control appliers — each field's sole consumer. `take_control()`
        // above already GC'd the one-shots; run-to-completion means at
        // most one menu submit set them this cycle.

        if let Some(op) = control.sandbox {
            // Relocated v2.0 /sandbox handler: mutate the state
            // store, then re-publish the merged config — BOTH
            // `tools::set_sandbox` (live) AND `config.sandbox`
            // (so the merge survives a lazy agent rebuild, which
            // does `set_sandbox(config.sandbox.clone())`).
            use crate::app_state::SandboxOp;
            let result = match &op {
                SandboxOp::Add {
                    host_path,
                    sandbox_path,
                    mode,
                } => crate::sandbox_state::add(host_path, sandbox_path, *mode),
                SandboxOp::Remove { sandbox_path } => crate::sandbox_state::remove(sandbox_path),
                SandboxOp::Exclude { sandbox_path } => crate::sandbox_state::exclude(sandbox_path),
                SandboxOp::Restore { sandbox_path } => crate::sandbox_state::restore(sandbox_path),
            };
            match result {
                Ok(()) => {
                    let mut merged = self.config.sandbox.clone().unwrap_or_default();
                    crate::sandbox_state::merge_into(&mut merged);
                    crate::tools::set_sandbox(Some(merged.clone()));
                    self.config.sandbox = Some(merged.clone());
                    // Keep the target snapshot in sync — this applier is the
                    // one that *applies* sandbox state.
                    self.app_state.update_config(|c| c.sandbox = Some(merged));
                    let msg = match &op {
                        SandboxOp::Add {
                            host_path,
                            sandbox_path,
                            mode,
                        } => {
                            let m = if *mode == crate::config::BindMode::Rw {
                                "add_rw"
                            } else {
                                "add_ro"
                            };
                            if sandbox_path.is_empty() {
                                format!("sandbox: {} {} (ok)", m, host_path)
                            } else {
                                format!("sandbox: {} {} → {} (ok)", m, host_path, sandbox_path)
                            }
                        }
                        SandboxOp::Remove { sandbox_path } => {
                            format!("sandbox: removed {} (ok)", sandbox_path)
                        }
                        SandboxOp::Exclude { sandbox_path } => {
                            format!("sandbox: excluded {} (ok)", sandbox_path)
                        }
                        SandboxOp::Restore { sandbox_path } => {
                            format!("sandbox: restored {} (ok)", sandbox_path)
                        }
                    };
                    self.config.content.display_text(&msg);
                }
                Err(e) => {
                    self.config
                        .content
                        .display_text(&format!("sandbox error: {}", e));
                }
            }
        }
        true
    }

    /// Run one submitted user line: record it in history, drop the shell
    /// `!`/`!!` path, split per-turn overrides, and run the prompt turn.
    /// Returns [`LoopCtl::Break`] only for `/q` / `exit`.
    async fn handle_submit(&mut self, input: String) -> LoopCtl {
        if input.is_empty() {
            return LoopCtl::Continue;
        }
        self.line_history.add(&input);
        if input == "/q" || input == "exit" {
            return LoopCtl::Break;
        }
        // Per-turn overrides (doc/model_settings_v2.md A.10).
        //
        // `line_history` above took the RAW line, so recall shows the
        // user what they typed including the overrides; from here on
        // `input` is the text with the override lines removed, which is
        // what is persisted, sent, and counted for context. Nothing is
        // mutated: the overrides travel as a value into `prompt()` for
        // this turn only, so there is no restore step and no way one
        // reaches the live params or the next turn.
        let split = crate::turn_overrides::split(&input);
        for w in &split.warnings {
            self.config.content.display_text(w);
        }
        // An overrides-only prompt has nothing to send. Checked *before*
        // `ensure_session` below, or a turn that never runs would still
        // create a session.
        if split.text.trim().is_empty() {
            self.config
                .content
                .display_text("nothing to send — the prompt was only overrides");
            return LoopCtl::Continue;
        }
        let input = split.text;
        let (effective_params, notes) = effective_params(&self.app_state, &split.overrides);
        for n in &notes {
            self.config.content.display_text(n);
        }
        // Safety net: any code path that drops the in-memory session
        // (notably `/new` and `/clear`) must leave us with a fresh one
        // before the first post-drop prompt — otherwise
        // `prompt(input, None, ...)` would build an empty prior context
        // and skip every `append_message` (gated on `session.is_some()`),
        // losing turn-to-turn memory and never writing the new session
        // to disk. Cheap when the session is already populated.
        let guarded = ensure_session(
            self.app_state.session(),
            &self.repo,
            Some(self.config.provider.clone()),
            Some(self.config.model.clone()),
            self.config.params.reasoning_str(),
        )
        .await;
        let guarded_id = guarded
            .as_ref()
            .map(|s| s.get_session_id().to_string())
            .unwrap_or_default();
        self.app_state.activate_session(guarded, guarded_id);
        // User-initiated shell command. `!cmd` runs `cmd` in the
        // sandbox and captures its output into the session context (so
        // the LLM sees it on the next turn); `!!cmd` runs and displays
        // it but does NOT add it to context. Both reuse the same
        // `run_sandboxed` helper (and therefore the same bwrap sandbox
        // + timeout policy) as the LLM's `bash` tool, so user- and
        // model-initiated shelling are identical. No confirmation is
        // required: the user typed it explicitly, unlike the
        // LLM-initiated tool call. This skips the LLM prompt entirely.
        if input.starts_with("!!") || input.starts_with('!') {
            self.run_bang_command(&input).await;
            return LoopCtl::Continue;
        }
        // `/stat` is handled by the REPL overlay (typing it opens the
        // stats panel); it is never submitted here, so there's nothing
        // to do for that keyword.
        // The bar's label for an unnamed session on the very first prompt: the
        // cycle view is written before this message is persisted, so the branch
        // is still empty then. Record the text being sent as the stand-in (the
        // `first_user_text(prior).or(input)` fallback, relocated here from the
        // turn); the next cycle's branch write supersedes it.
        self.app_state.set_first_message_if_unset(&input);
        // Reprint the submitted prompt via the `Output` impl so the
        // session (and cursor tracking) stays correct; `read_input`
        // cleared the input line on submit. Bracketed — a rule above
        // *and* below, which the second call used to add by hand — and
        // shared with a directly preceding commit instead of stacked
        // (ui_draw_v2 R4).
        self.run_prompt_turn(input, effective_params).await;
        LoopCtl::Continue
    }

    /// Run a user `!cmd` (capture its output into the session context) or
    /// `!!cmd` (display only) command. Reuses `run_sandboxed`, so it runs under
    /// the same bwrap + timeout policy as the agent's `bash` tool; no LLM turn.
    async fn run_bang_command(&mut self, input: &str) {
        let (capture, cmd) = if let Some(rest) = input.strip_prefix("!!") {
            (false, rest)
        } else if let Some(rest) = input.strip_prefix('!') {
            (true, rest)
        } else {
            unreachable!()
        };
        let cmd = cmd.trim();
        // Reprint the submitted command (cleared by `read_input`),
        // bracketed like every other line the user committed with
        // Enter (ui_draw_v2 R4).
        self.config.content.display_committed_prompt(input);
        if cmd.is_empty() {
            self.config.content.display_plain(
                "Usage: !<command> (capture output into context), or !!<command> (display only)",
            );
        } else {
            // Re-publish the resolved sandbox config immediately
            // before running the command. The config is held in a
            // process-wide global that other tests (and other
            // sessions) may mutate concurrently; pinning it here —
            // with no await between this and `run_sandboxed`'s read
            // of the global — keeps the `!`/`!!` path immune to
            // that race while preserving the "publish once" intent.
            set_sandbox(self.config.sandbox.clone());
            match crate::tools::run_sandboxed(cmd, None).await {
                Ok(out) => {
                    let out = out.trim();
                    if out.is_empty() {
                        self.config.content.display_plain("(no output)");
                    } else {
                        self.config.content.display_plain(out);
                    }
                    if capture {
                        // Mirror a normal turn so the model sees
                        // both the command and its result: a user
                        // message naming the command, plus an
                        // assistant message carrying the captured
                        // output.
                        let msg = Message::user(format!("Ran shell command: {cmd}"));
                        if let Some(s) = self.app_state.session() {
                            let _ = s.append_message(&msg).await;
                            let reply = Message::Assistant {
                                id: None,
                                content: vec![AssistantContent::Text(Text::new(out.to_string()))],
                            };
                            let _ = s.append_message(&reply).await;
                        }
                    } else {
                        self.config
                            .content
                            .display_plain("(output was displayed but not added to context)");
                    }
                }
                Err(e) => {
                    self.config
                        .content
                        .display_text(&format!("shell error: {}", e));
                }
            }
        }
    }

    /// Create the agent lazily on first use, then run the prompt turn: spawn
    /// the in-flight reader, drive the single-attempt + auto-retry back-off
    /// loop (handing the reader's key queue into `prompt()`), and surface the
    /// outcome. `effective_params` are this turn's live params + overrides.
    async fn run_prompt_turn(
        &mut self,
        input: String,
        effective_params: crate::params::ModelParams,
    ) {
        self.config.content.display_committed_prompt(&input);
        // Normal prompt — create the agent lazily on first use
        // (loop-side, applied config; published to the slot).
        if self.app_state.agent().is_none() {
            match create_agent(self.config.clone()).await {
                Ok(a) => self.app_state.activate_agent(Arc::from(a)),
                Err(e) => {
                    self.config
                        .content
                        .display_text(&format!("Cannot start agent: {}", e));
                    return;
                }
            }
        }
        // Clone-out for this turn (lock rule 3: no guard across await).
        let agent = self.app_state.agent();
        let session = self.app_state.session();
        // Single attempt + auto-retry, with the persistent reader spanning the
        // whole window. `Esc` is dispatched by the turn's one key dispatcher
        // wherever the turn is: inside `prompt()` while an attempt streams, by
        // the back-off pump while a retry delay runs (`cancellable_sleep_pumped`)
        // — either way it fires the cancel signal, and the wait (or the stream)
        // ends immediately, returning `Cancelled`.
        //
        // U4.2/U4.3: edit keys typed while the turn runs come from the **one
        // process-lifetime channel** the persistent reader feeds
        // (`self.input`,
        // — a process-lifetime channel, no per-turn reader, no per-turn queue. The
        // receiver is borrowed for the whole turn and handed into `prompt()` as
        // `InTurnInput::keys`; because the channel is never dropped between
        // turns, a key typed at the very end of a turn lands in the next
        // prompt rather than being discarded (a deliberate change from the
        // per-turn queue, which died with the turn).
        let response: std::result::Result<String, crate::PicocodeError> = {
            let mut consecutive_failures: u8 = 0;
            let mut next_input: String = input.clone();
            // The params this turn sends. They are the live ones with
            // the typed overrides applied, and they are *carried*, not
            // applied to the live config — so a retry of the same
            // dispatch re-uses them automatically (the stripped text is
            // already in `next_input`), while a synthetic re-prompt
            // below resets them, since an override belongs to the
            // user's dispatch and not to the model's own feedback.
            let mut turn_params = effective_params;
            // Per-turn persistence flag. Reset once per user prompt
            // (NOT per retry): the retry loop below reuses it across
            // `prompt()` attempts so a pre-persisted `user(input)` is
            // not duplicated, while the NEXT turn starts fresh (so
            // two identical inputs in a row, e.g. "continue" twice,
            // never collide). See `TurnPersistence`.
            let turn = TurnPersistence::new();
            // The user's original prompt is this turn's first NEW
            // dispatch.
            turn.mark_new_dispatch();
            // The full steering message (synthetic feedback / "continue")
            // is delivered once per logical turn; `prompt()` persists it
            // with the partial transcript when a retry makes progress and
            // then fails again. Re-sending the same full steer on that
            // later interruption would repeat it in the session once the
            // retry succeeds, so later interruptions get a short
            // continuation instead.
            let mut steer_delivered = false;
            let outcome: std::result::Result<String, crate::PicocodeError> = loop {
                let prompt_result = agent
                    .as_ref()
                    .unwrap()
                    .prompt(
                        &next_input,
                        session.as_deref(),
                        &turn,
                        &turn_params,
                        // The key branch exists only where there is a session
                        // to receive it — built by the REPL arm alone, i.e.
                        // interactive tty (the same fact the old `guard` zip
                        // re-checked). Piped input gets no key branch: the
                        // persistent reader's queue is simply not read there.
                        // `keys` is the one process-lifetime channel the
                        // reader feeds.
                        self.prompt_session
                            .as_mut()
                            .map(|sess| crate::repl::InTurnInput {
                                session: sess,
                                keys: &mut self.input,
                                confirms: &mut self.confirm_bridge.requests,
                                label: &self.prompt_str,
                                history: &mut self.line_history,
                            }),
                    )
                    .await;
                match prompt_result {
                    Ok(text) => break Ok(text),
                    Err(crate::PicocodeError::Cancelled(_)) => {
                        break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                    }
                    // Mid-turn failure that persisted progress: the
                    // underlying run produced content/tool calls
                    // before erroring. This is NOT a "consecutive
                    // failure" for back-off purposes — there was
                    // successful work in between — so reset the
                    // counter and treat the upcoming retry as a
                    // fresh first attempt (delay = 2^0 = 1 s, or
                    // the provider's Retry-After floor). The budget
                    // is still consumed (one retry used), but the
                    // exponential delay does not compound.
                    Err(crate::PicocodeError::LlmInterrupted { cause, .. })
                        if consecutive_failures < retry_budget(&cause) =>
                    {
                        consecutive_failures = 0;
                        // `prompt()` persisted the user prompt + the
                        // finalized partial transcript to the session
                        // and returned `LlmInterrupted`. Continue from
                        // that transcript with a steering message — the
                        // original `input` is already in the session,
                        // so re-sending it would duplicate the user's
                        // request.
                        let ms = retry_delay_ms(&cause, 1);
                        self.config.content.display_text(&format!(
                            "Error: {}. Retrying in {}s...",
                            cause,
                            ms / 1000
                        ));
                        let slept = cancellable_sleep_pumped(
                            &self.config.user_cancel,
                            &self.config.cancel_notify,
                            ms,
                            turn_backoff_pump(
                                &mut self.prompt_session,
                                &mut self.input,
                                &mut self.line_history,
                                &self.prompt_str,
                                &self.config.content,
                            ),
                        )
                        .await;
                        if !slept {
                            break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                        }
                        // Steering prompt: the synthetic feedback for
                        // e.g. malformed tool calls (carries the parse
                        // error the model needs to self-correct), else a
                        // short "continue" pointing at the persisted
                        // work. NOT appended to the session — it becomes
                        // the retry's prompt (input + partial are already
                        // persisted). Delivered in full (the synthetic
                        // feedback) only once per logical turn; after the
                        // model has already been steered once and made
                        // further (persisted) progress, a plain "continue"
                        // suffices and avoids repeating the steer text in
                        // the session.
                        //
                        // When there is no session, nothing was actually
                        // persisted, so a "continue where you left off"
                        // steer would point at context that doesn't exist
                        // and drop the user's original request. In that
                        // case re-send the original `input` instead (same
                        // as the no-progress path) — the back-off reset
                        // above still applies, but the user's turn is not
                        // lost.
                        next_input = if session.is_none() {
                            input.clone()
                        } else if steer_delivered {
                            "continue".to_string()
                        } else {
                            steer_delivered = true;
                            match cause.synthetic_retry_feedback() {
                                Some(fb) => fb,
                                None => "continue".to_string(),
                            }
                        };
                        // The steering message is a NEW user dispatch
                        // for this turn (distinct from the original
                        // prompt), so it counts toward `turns_sent` and
                        // will be persisted/deduped as its own message.
                        turn.mark_new_dispatch();
                        if self.cancel.load(Ordering::SeqCst) {
                            break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                        }
                        continue;
                    }
                    // No-progress retryable failure: `prompt()` returned
                    // the plain classified error. `prompt()`'s error
                    // branch already de-duped and persisted `user(input)`
                    // (or skipped it because the session already ended
                    // on it), so we do NOT re-append here. The retry
                    // re-runs from `input` (the turn never got past its
                    // first model call). For malformed tool calls the
                    // model needs the parse error to self-correct; in
                    // that case the synthetic feedback becomes the
                    // retry's prompt instead of `input`.
                    Err(e) if e.is_auto_retryable() && consecutive_failures < retry_budget(&e) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let ms = retry_delay_ms(&e, consecutive_failures);
                        self.config.content.display_text(&format!(
                            "Error: {}. Retrying in {}s...",
                            e,
                            ms / 1000
                        ));
                        let slept = cancellable_sleep_pumped(
                            &self.config.user_cancel,
                            &self.config.cancel_notify,
                            ms,
                            turn_backoff_pump(
                                &mut self.prompt_session,
                                &mut self.input,
                                &mut self.line_history,
                                &self.prompt_str,
                                &self.config.content,
                            ),
                        )
                        .await;
                        if !slept {
                            break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                        }
                        if let Some(feedback) = e.synthetic_retry_feedback() {
                            // `prompt()` already persisted `user(input)`
                            // (de-duped). Re-prompt with the synthetic
                            // feedback so the model can self-correct.
                            next_input = feedback;
                            // Drop the override: it was the user's
                            // instruction for *their* dispatch, and
                            // this is the model's own corrective
                            // re-prompt. Carrying a `/temperature 0.2`
                            // into it would silently keep applying a
                            // sampling the user never asked for on this
                            // exchange.
                            turn_params = self.app_state.params();
                            // The feedback replaces the original prompt
                            // as a NEW user dispatch for this turn.
                            turn.mark_new_dispatch();
                        }
                        // Race window: cancel could fire after
                        // `cancellable_sleep_pumped` returned but
                        // before `prompt()` clears its flag. Bail
                        // out here so the retry respects `Esc`.
                        if self.cancel.load(Ordering::SeqCst) {
                            break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                        }
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            };
            outcome
        };
        // No reader teardown here: the stdin reader is process-lifetime now
        // — it keeps feeding the one channel across turns
        // and into the next prompt.
        match response {
            Ok(_) => {}
            Err(crate::PicocodeError::Cancelled(msg)) => {
                // Turn aborted via `<Esc>`; `prompt()` already persisted
                // the in-flight history, so show a clear notice instead
                // of a generic "Error:".
                self.config.content.display_text(&msg);
            }
            Err(e) => {
                self.config.content.display_text(&format!("Error: {}", e));
            }
        }
    }

    async fn run(&mut self) -> Result<()> {
        loop {
            let action = match self.repl.as_mut() {
                Some(r) => {
                    // No separator here any more: the prompt-side status bar occupies
                    // the divider's row, above the input (it is a `Lines` element of
                    // the input frame). The bar separates the last turn from the
                    // prompt *and* says what is about to answer it, which a dim rule
                    // could not do — and it is the same bar the turn paints while it
                    // streams, so the dot is the only thing that changes between the
                    // two states.
                    //
                    // The spacing the rule provided is not lost: a response ends on
                    // a blank (`end_response`), and the greeter ends on one of its
                    // own, so the frame's first row still lands one row clear.

                    // Build session context data for REPL overlays.
                    let (_sessions, branch_messages, tree, leaf_id, context_tokens) =
                        build_session_ctx(&self.app_state.session()).await;
                    // The session's own name and opening message, for the prompt-side
                    // status bar. Off the same `branch_messages` the overlays read,
                    // so the two bars cannot disagree about which session they are
                    // naming — and off the *index* for the name, which is where the
                    // mid-turn bar reads it too.
                    let session_name = match self.app_state.session() {
                        Some(session) => session.index.lock().await.header.name.clone(),
                        None => None,
                    };
                    let session_first_message =
                        branch_messages.first().map(|(_, preview)| preview.clone());

                    let ctx = ReplCtx {
                        recent: self.app_state.recent_models(),
                        persona: self.config.persona_name.clone(),
                        repo: self.repo.clone(),
                        current_session_id: self.app_state.current_session_id(),
                        branch_messages,
                        tree,
                        leaf_id,
                        context_tokens,
                        model_filter: ModelFilter::default_for_coding(),
                        // The bar names the session off `AppState`, which this
                        // cycle writes below: the name off the session index,
                        // with the opening message as the fallback. The model,
                        // thinking level and context window are read straight
                        // off `AppState` by the bar at render.
                        session_name,
                        session_first_message,
                    };
                    // Pop the pending prefill (if any). It is the one thing that
                    // may override the carried buffer, and it is once-only: a
                    // `/session <id> resume` sets it, this consumes it, and the
                    // next iteration sees `None`.
                    let prefill = self.app_state.take_prefill();
                    // Built on first use and then carried — never reconstructed.
                    let session = self.prompt_session.get_or_insert_with(|| {
                        crate::repl::PromptSession::new(None, &self.app_state)
                    });
                    if let Some(text) = prefill.as_deref() {
                        session.apply_prefill(text);
                    }
                    // `read_input` re-anchors the draw cache itself on entry
                    // (DSR); afterwards the frame follows the cursor, which the
                    // cache tracks (ui_draw_v2 §5.2).
                    r.read_input(
                        &self.prompt_str,
                        session,
                        &mut self.line_history,
                        &ctx,
                        &self.app_state,
                        self.config.content.as_ref(),
                        &mut self.input,
                    )
                    .await
                }
                None => {
                    // Non-REPL input (quiet / piped stdin): `/`-prefixed lines
                    // go through the v2.5 engine as a batch (passive derive +
                    // virtual Enter — `Repl::batch_slash`), with no overlay
                    // drawing; everything else is raw `Submit` text. This runs
                    // BEFORE the convergence step below, so control/target
                    // writes land the same cycle as an interactive `MenuDone`
                    // (doc/editor_cleanup.md §10.9).
                    let input = self.config.content.get_user_input();
                    match Repl::batch_slash(
                        &self.app_state,
                        &input,
                        self.config.content.as_ref(),
                        &mut self.line_history,
                    )
                    .await
                    {
                        Some(action) => Some(action),
                        None => Some(InputAction::Submit(input)),
                    }
                }
            };
            let action = match action {
                Some(a) => a,
                None => continue,
            };
            if !self.converge().await {
                break;
            }
            match action {
                InputAction::MenuDone => {
                    // v2.5 menu committed: the node already wrote to `AppState`
                    // (target config / control fields); the convergence step
                    // above applied them. No-op arm.
                }
                InputAction::Submit(input) => match self.handle_submit(input).await {
                    LoopCtl::Continue => {}
                    LoopCtl::Break => break,
                },
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::build::AgentConfig;
    use serial_test::serial;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Minimal `AgentConfig` for gate decisions — the plan only reads the
    /// model id, params, and persona, so the rest is filler.
    fn gate_cfg(provider: &str, model: &str, params: crate::params::ModelParams) -> AgentConfig {
        let content = Arc::new(crate::content::Content::quiet());
        AgentConfig {
            provider: provider.into(),
            model: model.into(),
            content,
            yolo: false,
            confirm_port: None,
            quiet: true,
            params,
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: None,
        }
    }

    /// Nothing changed: no rebuild, whatever the last attempt was.
    #[test]
    fn plan_rebuild_is_a_noop_when_target_matches_applied() {
        let p = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::Med),
            temperature: Some(0.5),
            top_p: None,
        };
        let cfg = gate_cfg("openai", "gpt-4o", p);
        assert!(plan_rebuild(&cfg, &cfg, None).is_none());
        assert!(plan_rebuild(
            &cfg,
            &cfg,
            Some(&super::RebuildFingerprint {
                model: "openai:gpt-4o".into(),
                params: p,
                persona: None,
                context_limit: None,
            })
        )
        .is_none());
    }

    /// A temperature-only change rebuilds: it is baked into the agent's request
    /// builder, so there is no in-place path. Same for top_p and the level.
    #[test]
    fn plan_rebuild_fires_for_each_param_field() {
        let base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        for next in [
            crate::params::ModelParams {
                temperature: Some(0.7),
                ..Default::default()
            },
            crate::params::ModelParams {
                top_p: Some(0.9),
                ..Default::default()
            },
            crate::params::ModelParams {
                reasoning: Some(crate::params::ReasoningLevel::High),
                ..Default::default()
            },
        ] {
            let target = gate_cfg("openai", "gpt-4o", next);
            assert!(
                plan_rebuild(&base, &target, None).is_some(),
                "{next:?} must rebuild"
            );
        }
    }

    /// The once-per-target invariant: the same target that already failed is
    /// not retried, so a bad model id reports once instead of every cycle.
    /// Changing anything about the target re-arms it.
    #[test]
    fn plan_rebuild_attempts_once_per_distinct_target() {
        let applied = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        let target = gate_cfg(
            "openai",
            "gpt-4o",
            crate::params::ModelParams {
                temperature: Some(0.7),
                ..Default::default()
            },
        );
        let first = plan_rebuild(&applied, &target, None).expect("first attempt");
        // Same target, already attempted: silence.
        assert!(plan_rebuild(&applied, &target, Some(&first)).is_none());
        // A different param value is a different target: re-armed.
        let other = gate_cfg(
            "openai",
            "gpt-4o",
            crate::params::ModelParams {
                temperature: Some(0.9),
                ..Default::default()
            },
        );
        assert!(plan_rebuild(&applied, &other, Some(&first)).is_some());
    }

    /// The model id is compared as one canonical string. A model id that
    /// itself contains a colon (`openrouter:tencent/hy3:free`) must not read
    /// as a change on every cycle, which a segment-wise compare would cause.
    #[test]
    fn plan_rebuild_compares_the_whole_qualified_id() {
        let applied = gate_cfg(
            "openrouter",
            "tencent/hy3:free",
            crate::params::ModelParams::default(),
        );
        let same = gate_cfg(
            "openrouter",
            "tencent/hy3:free",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &same, None).is_none());
        let different = gate_cfg(
            "openrouter",
            "z-ai/glm-5.3",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &different, None).is_some());
    }

    /// Switching model with identical params still rebuilds — the agent is
    /// generic over its model, so this is a real change.
    #[test]
    fn plan_rebuild_fires_on_a_model_switch_with_unchanged_params() {
        let applied = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        let target = gate_cfg(
            "anthropic",
            "claude-3-5-sonnet-20241022",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &target, None).is_some());
    }

    /// A Ctrl-E of `context_length` moves nothing else `plan_rebuild` looks at,
    /// but the mid-turn status bar reads the window off the running agent, so
    /// without this term the edit would not reach it until an unrelated rebuild.
    #[test]
    fn a_context_window_change_rebuilds() {
        let mut base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        base.context_limit = Some(200_000);
        let mut next = base.clone();
        next.context_limit = Some(1_048_576);
        assert!(
            plan_rebuild(&base, &next, None).is_some(),
            "a Ctrl-E'd context_length must arm a rebuild"
        );
        // ...and is not a loop: once applied, the two sides agree again.
        assert!(plan_rebuild(&next, &next, None).is_none());
    }

    /// The dedupe waits for a *distinct* target, so a window edit is a fresh
    /// attempt even when a previous one failed — otherwise the user edits
    /// something, the fix lands, and the gate still refuses to retry.
    #[test]
    fn a_window_change_is_not_deduped_against_a_failed_attempt() {
        let mut base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        base.context_limit = Some(200_000);
        let mut next = base.clone();
        next.context_limit = Some(1_048_576);
        let prior = plan_rebuild(&base, &next, None).expect("first attempt is armed");
        assert!(
            plan_rebuild(&base, &next, Some(&prior)).is_none(),
            "the same target must not re-arm"
        );
        // Same model, params and persona; only the window differs.
        let after_fix = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        assert!(
            plan_rebuild(&base, &after_fix, Some(&prior)).is_some(),
            "a new window is a new target and must re-arm"
        );
    }

    /// The other half of option A: a settings write has to move the *target*
    /// config's window, or `plan_rebuild` has nothing to notice.
    ///
    /// Goes through `update_settings` because that is the path the Ctrl-E
    /// accept arm takes, and reads the target back with `config_snapshot`,
    /// because that is what the convergence step compares against.
    #[test]
    fn a_record_write_moves_the_target_configs_window() {
        let mut store =
            crate::settings::SettingsStore::from_config(&crate::Config::default(), None);
        store.default_model = "openai:gpt-4o".into();
        let app = crate::app_state::AppState::new(
            gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default()),
            store,
            None,
        );
        let before = app.config_snapshot().context_limit;

        app.update_settings(|s| {
            s.apply_model_settings(
                "openai:gpt-4o",
                &crate::model_settings::ModelSettings {
                    context_length: Some(123_456),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();

        let after = app.config_snapshot().context_limit;
        assert_eq!(
            after,
            Some(123_456),
            "the target must carry the window so the rebuild gate can see it"
        );
        assert_ne!(after, before, "so a rebuild is armed, not suppressed");
    }

    // Regression test for the `!`/`!!` sandbox bug: user-initiated shell
    // commands must run inside the same bwrap sandbox as the agent's `bash`
    // tool, *even when the agent has not been created yet*. The sandbox config
    // is published by `set_sandbox`, which used to be called only inside
    // `build_rig_agent` — i.e. lazily on the first normal prompt. So a `!cmd`
    // issued as the very first input (before any LLM turn) ran UNSANDBOXED.
    //
    // This test drives `run_interactive` with a scripted in-memory `Output`
    // that feeds `!<cmd>` then `/q`, with the sandbox remapping a host file to
    // a path that does not exist on the host. If the command runs sandboxed,
    // the remap is visible and `cat` prints the file's content; if it runs
    // unsandboxed, the path is absent and `cat` errors with "No such file".
    #[cfg(unix)]
    // `#[serial]`: this test runs a real sandboxed command, so it holds a
    // `TOOL_CANCEL_SENDERS` registration. The Esc-pump tests above
    // broadcast to every registration in the process, so without this the
    // two classes of test kill each other's children at random.
    #[tokio::test]
    #[serial]
    async fn bang_command_uses_sandbox_before_agent_created() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use crate::config::{BindPath, SandboxConfig};
        use crate::AgentConfig;
        use std::sync::atomic::AtomicBool;

        // bwrap is required for the sandbox to engage at all.
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        // Reset the process-wide sandbox slot so the test is deterministic. The
        // bug was precisely that `!`/`!!` ran with this still at `None`
        // (unsandboxed) until the agent lazily created itself. With the fix,
        // `run_interactive` republishes the resolved config before reading any
        // input, overwriting this.
        crate::tools::set_sandbox(None);

        // Host file we remap into the sandbox at a path absent on the host.
        let sentinel = "SANDBOX_REGRESSION_MARKER_12345";
        let _tmp = tempfile::tempdir().unwrap();
        let host_path = _tmp.path().join("picocode_sandbox_regr.txt");
        std::fs::write(&host_path, sentinel).unwrap();
        let probe = "/sandbox_probe_xyz".to_string();

        let sandbox = SandboxConfig {
            timeout_secs: crate::config::SandboxTimeoutSecs(Some(30)),
            network: Some(false),
            memory_mb: crate::config::SandboxMemoryMb(None),
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: crate::config::SandboxTmpSizeMb(Some(16)),
            ro: vec![],
            rw: vec![BindPath::Remap {
                from: host_path.display().to_string(),
                to: probe.clone(),
            }],
            env: None,
            max_output_bytes: crate::config::SandboxMaxOutputBytes(None),
        };

        // A scripted input line replays the `!` command; captured bytes record
        // what content shows.
        let inputs = Arc::new(Mutex::new(VecDeque::from([
            format!("!cat {}", probe),
            "/q".to_string(),
        ])));
        let content = Arc::new(crate::content::Content::for_test());
        content.set_input_script(inputs);

        // Use a temp directory for session storage to avoid littering the user's config.
        // tempfile::TempDir cleans up automatically on drop.
        let sessions_tempdir = tempfile::tempdir().unwrap();
        let sessions_dir = sessions_tempdir.path().to_path_buf();

        let config = AgentConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            content: content.clone(),
            yolo: true,
            confirm_port: None,
            quiet: true,
            params: Default::default(),
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: Some(sandbox),
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: Some(sessions_dir.clone()),
        };
        let settings = SettingsStore::from_agent_config(&config, &Default::default(), None);

        // No normal prompt is ever submitted, so the agent is never created and
        // `create_agent` (and its `set_sandbox`) is never called — exactly the
        // pre-fix condition where `!` would otherwise run unsandboxed.
        content.begin_capture();
        super::run_interactive(config, settings, false)
            .await
            .unwrap();

        let shown = String::from_utf8_lossy(&content.end_capture()).into_owned();
        let _ = std::fs::remove_file(&host_path);
        // sessions_tempdir is automatically cleaned up when dropped here

        assert!(
            !shown.contains("No such file"),
            "bang command ran UNSANDBOXED (probe path {probe} absent); output:\n{shown}"
        );
        assert!(
            shown.contains(sentinel),
            "expected sandboxed `!` command to print the remapped host file content; output:\n{shown}"
        );
    }

    /// The `repl == None` input arm (in-process host / non-tty) routes
    /// `/`-prefixed lines through `Repl::batch_slash` — passive derive +
    /// virtual Enter, no overlay drawing — and returns `MenuDone`, so the
    /// node's control writes (`/quit` → `request_quit`) are drained by the
    /// same cycle's convergence step. Pins the loop wiring the `batch_slash`
    /// unit tests cannot see: input-position eval → `MenuDone` → quit break.
    #[tokio::test]
    async fn non_repl_slash_lines_run_through_batch_engine() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use crate::AgentConfig;
        use std::sync::atomic::AtomicBool;

        // `/thinking high` commits through the engine (echoed line proves
        // `batch_slash` ran); `/quit` must then end the loop — if the eval
        // sat AFTER convergence, quit would be stranded until another line
        // arrived and the `unwrap_or_else("/q")` fallback would mask it.
        // So: exactly two lines, and the queue must be empty afterwards.
        let inputs = Arc::new(Mutex::new(VecDeque::from([
            "/reasoning high".to_string(),
            "/quit".to_string(),
        ])));
        let sessions_tempdir = tempfile::tempdir().unwrap();
        let content = Arc::new(crate::content::Content::for_test());
        content.set_input_script(inputs.clone());
        let config = AgentConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            content: content.clone(),
            yolo: true,
            confirm_port: None,
            quiet: true,
            params: Default::default(),
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: Some(sessions_tempdir.path().to_path_buf()),
        };
        let settings = SettingsStore::from_agent_config(&config, &Default::default(), None);

        content.begin_capture();
        super::run_interactive(config, settings, false)
            .await
            .unwrap();

        let shown = String::from_utf8_lossy(&content.end_capture()).into_owned();
        assert!(
            shown.contains("reasoning high"),
            "expected the batch commit echo for `/thinking high`, got:\n{shown}"
        );
        assert!(
            inputs.lock().unwrap().is_empty(),
            "both scripted lines must have been consumed (quit must land on its own cycle)"
        );
    }
}
