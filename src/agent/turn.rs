//! The execution half of a turn: `CodeAgent::prompt` and its companions.
//!
//! `prompt` runs one user input through the rig agent and reconciles the
//! session: it pumps the stream (assistant turns, tool results, reasoning)
//! into a [`TurnTranscript`], drives the auto-retry/back-off/Esc-cancel loop on
//! a classifiable provider error, and persists incrementally — each completion
//! turn reaches disk by the start of the next call, the prompt at dispatch, and
//! only the in-flight response waits for a turn exit
//! (doc/261022_incremental_session_write.md). `run_once` is the one-shot variant
//! for piped/batch input; `summarize` and `summarize_with_session_context` are
//! the plain and session-scoped summarization requests.
//!
//! The turn-pump primitives (`dispatch_in_turn_event`, the back-off sleep) live
//! in `pumps`; the transcript, its `prepare`/`commit` and the message shaping
//! (`maybe_strip_reasoning`, the edit-call splits) in `transcript`; error
//! classification in `errors`. This module is where they come together against
//! a live `CodeAgent`.

use super::errors::{classify_prompt_error, classify_stream_error, retry_budget, retry_delay_ms};
use super::pumps::dispatch_in_turn_event;
use crate::Result;
use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::completion::PromptError;
use rig_agent::streaming::{Item, StreamEvent};
use rig_core::message::Message;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::agent::compaction::session_context_messages;

use crate::agent::estimate_ctx_with;
use crate::agent::CodeAgent;
use crate::session::Session;

impl CodeAgent {
    /// Record the turn's phase on `AppState` — the status bar's dot and the
    /// input row's "is a turn running" fact.
    ///
    /// A no-op without an `AppState` (piped/batch): there is no bar to read it
    /// there, and the phase is UI state, not something the run cares about.
    ///
    /// Called at the three transitions that define it: an attempt going out
    /// (`Streaming`), a retryable failure before the back-off
    /// (`WaitingRetry`), and the turn ending (`Idle`).
    fn set_turn_phase(&self, phase: crate::turn_phase::TurnPhase) {
        if let Some(app_state) = &self.app_state {
            app_state.set_turn_phase(phase);
        }
    }

    /// `--log-messages`: a live-pump lifecycle marker — the points where the
    /// content boundary state changes (`closed` flips). Logged beside the raw
    /// deltas so a spacing investigation can reconstruct exactly what
    /// `boundary_spacer` saw from one file. A no-op without the flag.
    async fn log_pump_event(&self, event: &str) {
        if let Some(logger) = &self.message_logger {
            logger.log_event(event, logger.current_turn_id()).await;
        }
    }

    /// `params` are the params to send *for this request*: the live ones, or
    /// the live ones with this turn's per-turn overrides applied. Passing them
    /// in rather than reading them off `AppState` is what makes an override
    /// per-request — nothing here mutates the live params, so there is nothing
    /// to restore and no path by which one leaks into the next turn.
    ///
    /// **This future is `!Send`; the summarizers below are not.** Since U4.3
    /// the turn carries the REPL's prompt session in, and that session is
    /// single-threaded by construction — `Arc` line windows, `Arc` menu views, a
    /// sync `mpsc` completer — and it never leaves the task `run_interactive`
    /// runs on. Requiring `Send` would mean rewriting that chain to `Arc` for a
    /// property nothing uses: `main` awaits `run_interactive`, every test
    /// awaits it, and no request is ever spawned. (`run_once` inherits it — it
    /// awaits this.)
    ///
    /// It is recorded here as a property of the future rather than as a bound
    /// on an interface, which is what the trait this replaces declared: an
    /// interface-wide `?Send` would cost every method its `Send`-ness to buy
    /// one of them, and would need updating by hand if the session ever became
    /// `Send`. A caller who wants to `tokio::spawn` a request makes the session
    /// chain `Send` first, and this is where that becomes visible.
    pub async fn prompt(
        &self,
        input: &str,
        session: Option<Arc<Session>>,
        params: &crate::params::ModelParams,
        overrides: crate::turn_overrides::TurnOverrides,
        in_turn: Option<crate::repl::InTurnInput<'_>>,
    ) -> Result<String> {
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this prompt. The turn's key
        // dispatcher — this select's key branch, or the back-off pump between
        // attempts — is what fires `cancel` on `<Esc>`; the in-flight reader
        // only routes keys (U4.3), and it is always joined (after `done` is
        // set) before the next `prompt()` begins. A leftover flag can therefore
        // only be one fired in the gap after the previous attempt ended, which
        // is exactly what this clear is for. See
        // doc/turn_cancellation_persistence_bugs.md §3.
        self.user_cancel.store(false, Ordering::SeqCst);

        // An empty prompt on a branch that ends mid-turn *is* a resume: the
        // signal is the session's own tail, not a keyword
        // (`doc/261102_partial_turn_retention.md` §7).
        //
        // The continuation is written here, at resume time, and not at abort
        // time — an aborted turn has nothing to continue *into*, so its branch
        // ends on the partial. Resuming is what needs the nudge: without one the
        // rebuilt run would take the assistant partial as its prompt, i.e. a
        // prefill, which most providers refuse (§4). `ensure` keeps this a no-op
        // when a retry already wrote one, which is what makes returning with
        // nothing typed exactly a retry.
        let resume = match (input.trim().is_empty(), session.as_deref()) {
            (false, _) | (true, None) => false,
            (true, Some(s)) => s.ends_mid_turn().await,
        };
        if resume {
            if let Some(s) = session.as_deref() {
                if let Err(e) = s
                    .ensure_custom_message(
                        crate::agent::continuation::CUSTOM_TYPE,
                        crate::agent::continuation::TEXT,
                    )
                    .await
                {
                    // A persist failure is a real error the user should see.
                    // Not `eprintln!`: under raw mode the live frame owns the
                    // screen, and a stray stderr line corrupts it. The error
                    // channel routes through the draw pipeline like everything
                    // else, so the row accounting stays honest too.
                    self.content
                        .display_error(&format!("failed to persist the continuation: {e}"));
                }
            }
        }

        // Build prior context from session, or use empty vec as fallback.
        // After the write above, so a resume's context is the branch as the
        // model will see it — partial, then continuation.
        let prior: Vec<Message> = match session.as_deref() {
            Some(s) => match session_context_messages(
                s,
                self.reasoning_in_context,
                &self.provider,
                &self.model,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    // Session index may be stale; report and fall back to
                    // empty. Rides the error kind (§Content: no warning verb
                    // of its own).
                    self.content
                        .display_error(&format!("session build_context failed: {}", e));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // Persist the user prompt at dispatch, after `prior` is read (so it is
        // not duplicated into context) and before the first request goes out,
        // so grepping the session finds the prompt the moment it is submitted.
        //
        // Nothing to de-duplicate against: `prompt` is called exactly once per
        // user dispatch, and a retry re-issues an attempt inside that turn. A
        // *resume* appends nothing at all — the prompt is the continuation that
        // already ends the branch.
        if !resume {
            if let Some(s) = session.as_deref() {
                if let Err(e) = s.append_message(&Message::user(input)).await {
                    self.content
                        .display_error(&format!("failed to persist the prompt: {e}"));
                }
            }
        }

        // The first turn has no reported usage at all (the first completion
        // call is still in flight), so the status line would have no context
        // number for exactly the turn a new session is most likely to be
        // watched. The estimate `/tree compact` already uses is computable from
        // the messages in hand — `prior` plus this turn's prompt — so record it
        // as an *estimate*; the provider's own count replaces it
        // through `context_figure` from the second turn on, with nothing
        // impersonating a report.
        let estimated_ctx = estimate_ctx_with(&prior, input);
        {
            let mut usage = self.usage.lock().unwrap();
            usage.estimated_input_tokens = estimated_ctx;
        }

        // `stream_prompt(...).await` yields the item stream (rig 0.40's
        // `IntoFuture` returns the stream itself, not a collected response).
        //
        // Per-request params, set unconditionally from `params`.
        //
        // `replace_additional_params`, not `merge_`: a `/reasoning` override
        // must not leave a stale key behind, and anthropic's `thinking` block is
        // the case that matters — merging would carry the old `budget_tokens`
        // alongside a new `type`. Recomposing through `request_additional_params`
        // rather than patching the baked value keeps an override turn
        // byte-identical to a normal one, and the `without_*` arms mean an
        // explicit `None` clears instead of silently keeping the built value.
        //
        // With no overrides `params` *is* `live_params`, so this recomputes what
        // was baked at build time and the turn is unchanged.
        // The loop's persistence, kept out here because the exit has to close it
        // after the stream is gone: an `<Esc>` mid-batch must still write the
        // files that batch already edited.
        //
        // The run is built once and outlives a failed attempt. A retry polls the
        // same loop again rather than re-dispatching the prompt: the failed
        // attempt never reached `streamed_turn`, so the run is still awaiting
        // the model and `next_step` re-emits the identical `CallModel`. Nothing
        // is rolled back, no turn is consumed, and no user message is appended
        // — which is why there is no `"continue"` in the transcript any more.
        let (mut drive, persist) = match self.build_drive(
            input,
            prior,
            params,
            overrides,
            session.clone(),
            resume,
            in_turn.as_ref().map(|i| i.session.editor_handle()),
        ) {
            Ok(pair) => pair,
            Err(error) => {
                // No run to drive, so no attempt loop either: the prompt is on
                // disk (dispatch) and the error is the whole outcome.
                self.log_pump_event("response_end").await;
                self.content.end_response();
                return Err(classify_stream_error(&error));
            }
        };

        // `output` is the returned assistant text (what `prompt()` yields).
        let mut output = String::new();
        let mut aborted = false;
        // The transient status line (doc/row_accounting.md zone 2) is set before
        // the turn's first content write, because that is where the output
        // paints it — after each block, for the whole turn. The thinking level
        // is not in it: the agent carries none (it went with the hold-back in
        // B3b) and it only changes on an explicit commit, so it belongs to the
        // prompt-side carrier.
        self.log_pump_event("begin_response").await;
        self.set_turn_phase(crate::turn_phase::TurnPhase::Streaming);
        self.content.begin_response();
        // U4.3: the turn's second pump, split out of `in_turn` into bindings the
        // two new futures below can borrow *disjointly* — one future per
        // binding, so neither is borrowed twice at once. `tokio::select!`
        // scopes its futures to their own block ("a scope to separate polling
        // from handling the output"), so by the time a handler runs these are
        // free again for the handler to take.
        let (mut it_keys, mut it_confirms, mut it_session, it_label, mut it_history) = match in_turn
        {
            Some(crate::repl::InTurnInput {
                session,
                keys,
                confirms,
                label,
                history,
            }) => (
                Some(keys),
                Some(confirms),
                Some(session),
                label,
                Some(history),
            ),
            None => (None, None, None, "", None),
        };
        // The area has to say the turn started *now*. The phase was set before
        // `begin_response` above, and the frame on screen was composed while it
        // was idle — so until some keystroke happened to compose again, the
        // input row stayed drawn and the status dot stayed grey for the whole
        // turn.
        //
        // One `settle` at the switch, and it is the same paint path both pumps
        // use rather than a second way to compose: the buffer was just emptied
        // by the submit, so its `eval` is a passive derive with nothing to
        // dispatch and the action it returns is empty by construction.
        if let (Some(sess), Some(hist)) = (it_session.as_deref_mut(), it_history.as_deref_mut()) {
            let _ = sess.settle(self.content.as_ref(), it_label, hist).await;
        }
        // One attempt at a time over the single loop. A retry that needs no
        // rebuild — the failure produced no text — polls the same `drive` again,
        // which re-issues the identical `CallModel` without consuming a turn.
        //
        // A failure that *did* produce text rebuilds instead
        // (`doc/261102_partial_turn_retention.md` §3): the text the user watched
        // cannot enter the run (it is not a turn rig accepts, and rig strips a
        // failed turn from the wire), so the turn is reconstructed around the
        // session's history, which now holds it.
        let mut consecutive_failures: u8 = 0;
        let mut outcome: std::result::Result<(), crate::PriorError> = Ok(());
        'attempts: loop {
            // Reset per attempt: this is the verdict the attempt hands back.
            let mut attempt_failed: Option<(crate::PriorError, bool)> = None;
            // The *returned* text starts clean per attempt: a failed attempt's
            // text is not part of the answer, and concatenating it would return
            // a spliced reply nobody asked for. It is not lost, though — the
            // loop keeps it, and the retry path writes it to the session and
            // rebuilds the turn around it (§3).
            output.clear();
            // The hint mailbox is a clone of the loop's own `Arc`, taken
            // before the stream borrows the loop: the borrow lasts the whole
            // attempt, and the hint is written after it ends.
            let retry_hint = drive.retry_hint().clone();
            let mut stream = Box::pin(crate::agent::drive::run_stream(&mut drive));
            loop {
                // Prose's wake-up for this iteration, resolved *before* the select
                // rather than inside the timer's async block: `Option<Duration>` is
                // `Copy`, so the future below needs nothing from the session and
                // borrows nothing — one fewer thing held across every await in
                // here. Computed per iteration, which is as fresh as computing it
                // in the block would be, since the block is rebuilt each pass.
                let prose_deadline = it_session
                    .as_deref()
                    .and_then(crate::repl::PromptSession::prose_timeout);
                tokio::select! {
                    item = stream.next() => {
                        match item {

                            Some(Ok(MultiTurnStreamItem::StreamAssistantItem(item))) => {
                                match item {
                                    // rig 0.45 streams text and reasoning as
                                    // *fragments*; the part's `End` carries the
                                    // finalized block.
                                    Item::Event(StreamEvent::Text { text, .. }) => {
                                        if let Some(logger) = &self.message_logger {
                                            logger
                                                .log_delta("text", &text, logger.current_turn_id())
                                                .await;
                                        }
                                        output.push_str(&text);
                                        self.content.stream_text(&text);
                                    }
                                    Item::Event(StreamEvent::Reasoning { text, .. })
                                        if !text.is_empty() =>
                                    {
                                        if let Some(logger) = &self.message_logger {
                                            logger
                                                .log_delta("reasoning", &text, logger.current_turn_id())
                                                .await;
                                        }
                                        self.content.stream_thinking(&text);
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(MultiTurnStreamItem::ToolResult { .. })) => {
                                // The run committed this batch's results, so the
                                // persister writes it at the next boundary. Nothing
                                // to accumulate: the run's log *is* the record.
                            }
                            Some(Ok(MultiTurnStreamItem::CompletionCall(call))) => {
                                // Plumb per-request usage out of the stream (compaction's
                                // prerequisite — doc/compaction.md §2.7). The last call's
                                // `input_tokens` is the current context size; the sum across
                                // calls approximates session cost. Zero-valued usage is rig's
                                // sentinel for "provider reported nothing" (e.g. ollama), so
                                // skip it to keep the telemetry honest.
                                let u = call.usage;
                                if u.is_reported() {
                                    // rig 0.45 reports each counter as `Option`:
                                    // an unreported counter adds nothing, and the
                                    // `last_*` mirror keeps prior's `0 == "no
                                    // report"` convention.
                                    let counter = |value: Option<u64>| value.unwrap_or(0);
                                    if let Ok(mut g) = self.usage.lock() {
                                        // A fresh report replaces whatever stood
                                        // before — including a compaction's
                                        // invalidation — so this write is the
                                        // switch back from estimate to the
                                        // provider's own count.
                                        g.last_input_tokens = u.input_tokens;
                                        g.last_output_tokens = counter(u.output_tokens);
                                        g.last_cached_input_tokens = counter(u.cached_input_tokens);
                                        g.last_cache_creation_input_tokens =
                                            counter(u.cache_creation_input_tokens);
                                        g.session_input_tokens += counter(u.input_tokens);
                                        g.session_output_tokens += counter(u.output_tokens);
                                        g.session_cached_input_tokens += counter(u.cached_input_tokens);
                                        g.session_cache_creation_input_tokens +=
                                            counter(u.cache_creation_input_tokens);
                                        g.session_reasoning_tokens += counter(u.reasoning_tokens);
                                    }
                                }
                                // Commit the text so far before the tool call / result line.
                                self.log_pump_event("segment_end").await;
                                self.content.end_response_segment();
                            }
                            Some(Ok(MultiTurnStreamItem::FinalResponse(_resp))) => {
                                // Nothing to do: the loop already wrote the final
                                // turn at its `Done` step, where the run commits
                                // it. This arm is named rather than swallowed so
                                // the omission is deliberate — the response's
                                // `usage` is ignored here, and after a rebuild it
                                // covers only the rebuilt segment. Per-call
                                // figures reach the status line via
                                // `CompletionCall`, which is why that costs
                                // nothing today (doc §6a).
                            }
                            Some(Ok(MultiTurnStreamItem::ToolExecutionCommitted { .. })) => {}
                            Some(Ok(MultiTurnStreamItem::ModelTurnRetried { .. })) => {}
                            Some(Err(e)) => {
                                // Every exit persists through one path: the closed
                                // persister, which writes the committed tail and then
                                // a batch whose results never all landed. rig's error
                                // may carry the in-flight full history, but prior does
                                // not need it — the run *is* the record.
                                if let PromptError::Cancelled { .. } = &e {
                                    // Cancelled mid-run (Esc or a hook Stop): the
                                    // turn is aborted. Persistence is uniform — the
                                    // exit below closes the persister, which writes
                                    // the committed tail plus a batch whose results
                                    // never all landed.
                                    aborted = true;
                                    break;
                                }
                                // A non-cancellation error (provider mid-stream,
                                // tool-dispatch failure). Classify it and hand the
                                // verdict to the attempt loop rather than returning:
                                // the back-off, the budget and the corrective hint
                                // are decision the *turn* makes, not the stream.
                                //
                                // Nothing is closed here. The run is still alive and
                                // resumable, and a retry re-issues the same attempt —
                                // closing the persister would write a "did not
                                // execute" batch for a batch that is about to be
                                // re-attempted. The exit below closes once, on
                                // whichever path ends the turn.
                                let classified = classify_stream_error(&e);
                                let retryable = classified.is_auto_retryable();
                                // Progress resets the back-off: there was successful
                                // work inside the run, so the next attempt is not
                                // punished for the earlier growth. It is read from
                                // the persister because that is what the committed
                                // log and the held answers both report.
                                let made_progress = persist.made_progress().await;
                                attempt_failed = Some((classified, retryable));
                                let _ = made_progress;
                                break;
                            }
                            // `MultiTurnStreamItem` is non-exhaustive: an item this
                            // loop does not interpret is ignored.
                            Some(Ok(_)) => {}
                            None => break,
                        }
                    }
                    _ = self.cancel_notify.notified() => {
                        if self.user_cancel.load(Ordering::SeqCst) {
                            // User pressed `<Esc>` — stop rendering and return
                            // control to the REPL immediately. The stream is
                            // dropped, closing the HTTP connection; any running
                            // tool was already killed by the watchdog. Persistence
                            // is the uniform exit path: the prompt is already on
                            // disk (dispatch), and the exit `commit()` below writes
                            // the transcript delta plus an `Ended` synthesis.
                            aborted = true;
                            break;
                        }
                    }

                    // A gated tool call parked on the port. The tool future is
                    // inside the stream arm above, so this request is what makes
                    // the arm go `Pending` while the loop stays free to serve keys
                    // (doc/confirm_as_menu_261002.md). The session holds the
                    // dialog as an overlay element; the tool waits on the reply.
                    //
                    // Only here, not in the back-off pump: a confirm blocks its
                    // own tool, so by the time a retry back-off runs the dialog is
                    // resolved and the tool has returned.
                    Some(req) = async {
                        match it_confirms.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(sess) = it_session.as_deref_mut() {
                            sess.open_confirm(req, self.confirm_approvals.clone());
                            sess.sync_stack(self.content.as_ref(), it_label);
                            crate::repl::paint_prompt(self.content.as_ref());
                        }
                    }

                    // U4.3: a key typed during the turn, evaluated as it arrives —
                    // no queue, no holding back (ui_draw_v2 §4.5). This is
                    // `read_input`'s loop minus its wait: dispatch, then settle.
                    // `route_event` takes `Ctrl-C`/`Ctrl-Z` inline before anything
                    // reaches this channel, so what lands here is ordinary editing:
                    // a submit (blocked, silently) and an `Esc` whose menu-first
                    // call the shared dispatcher makes. One dispatcher, two pumps:
                    // the back-off between attempts (`cancellable_sleep_pumped`)
                    // runs the same body over the same queue, so a key means the
                    // same thing while the turn streams and while it waits out a
                    // retry delay.
                    ev = async {
                        match it_keys.as_mut() {
                            Some(rx) => rx.recv().await,
                            // No in-flight reader (piped input): nothing can ever
                            // arrive, so the branch never completes instead of
                            // completing with `None` every time round.
                            None => std::future::pending().await,
                        }
                    } => {
                        let _ = dispatch_in_turn_event(
                            ev,
                            it_session.as_deref_mut(),
                            it_history.as_deref_mut(),
                            it_label,
                            self.content.as_ref(),
                            &self.user_cancel,
                            &self.cancel_notify,
                            &self.steer,
                        )
                        .await;
                    }

                    // Prose needs a deadline even while the stream runs: its
                    // debounce, its in-flight slices and its dialog all fire on it.
                    // Without this arm prose would go inert for the whole turn —
                    // the stream branch alone never yields for them — which is the
                    // same reason `read_input`'s wait is a *poll* while prose is
                    // armed and a block otherwise.
                    _ = async {
                        match prose_deadline {
                            Some(d) => tokio::time::sleep(d).await,
                            // `None`: nothing pending, and no timer to wake for.
                            None => std::future::pending().await,
                        }
                    } => {
                        if let (Some(sess), Some(history)) =
                            (it_session.as_deref_mut(), it_history.as_deref_mut())
                        {
                            let _ = sess
                                .settle(self.content.as_ref(), it_label, history)
                                .await;
                        }
                    }
                }
            }
            // The attempt ended (stream exhausted, aborted, or failed). Drop the
            // stream first: it borrows the loop, and everything below needs the
            // loop back — to read what the attempt streamed, and to rebuild the
            // run around it.
            drop(stream);
            // Read before anything replaces the persister: a rebuild installs a
            // fresh one, and this is the *turn's* progress, which is what
            // decides whether the back-off restarts.
            //
            // Progress resets the growth, not the budget: a stream that made
            // work in between gets the base delay again, and the budget is
            // consumed either way. Gated on progress, not on persistence, so a
            // sessionless run resets too.
            let made_progress = persist.made_progress().await;
            // Write what the attempt rendered, whatever happens next: the user
            // watched it arrive, so a session that dropped it would disagree
            // with the screen, and a reload would show a turn missing its
            // opening.
            //
            // On abort that is *all* there is — the turn ends, so nothing asks
            // the model to carry on, and the partial simply ends the session (a
            // later turn then sees `[assistant] [user]`, which every model
            // accepts). The continuation is written by the retry path below,
            // and only once a retry is certain: a nudge appended for an attempt
            // that never runs would be litter, and the session is append-only.
            //
            // The persister is deliberately not closed. A stream failure leaves
            // the run `AwaitingModel` with no pending batch — the previous one
            // was answered and committed before this request went out — so the
            // only thing at risk is the streamed text handled here.
            let wrote_partial = self.persist_abandoned(&mut drive, session.as_deref()).await;
            if aborted {
                break 'attempts;
            }
            let Some((error, retryable)) = attempt_failed else {
                // Stream exhausted with no error: the run is done and its final
                // response has been seen.
                break 'attempts;
            };
            if !retryable {
                outcome = Err(error);
                break 'attempts;
            }
            consecutive_failures = if made_progress {
                0
            } else {
                consecutive_failures.saturating_add(1)
            };
            // Read once, then cleared: the flag spans attempts because the one
            // that makes progress is often not the one that fails, so it is
            // consumed here rather than at dispatch.
            persist.reset_progress().await;
            if consecutive_failures > retry_budget(&error) {
                outcome = Err(error);
                break 'attempts;
            }
            // A retry is now certain. Rebuild the turn around the session, which
            // holds the abandoned text: the retried request then carries it and
            // the model continues from it instead of restarting (§3). A fresh
            // persister comes with the fresh run.
            //
            // Only when there is something to continue *from*. A failure with
            // nothing streamed leaves the history exactly as the run has it, so
            // re-issuing in place is both cheaper and correct — and it appends
            // no continuation, which is what keeps a retry of an unproductive
            // attempt from growing the transcript (§3, "Consecutive retries
            // append nothing").
            if wrote_partial {
                // The persister is rebased, not replaced: the handle the pump
                // holds stays this turn's, which is what lets a rebuild happen
                // with nobody told (`doc/261030_rig_direct_drive.md` §5a).
                self.rebuild_after_partial(&mut drive, session.as_deref())
                    .await;
            }
            let delay = retry_delay_ms(&error, consecutive_failures.max(1));
            // The attempt's text is closed, but the *turn* is not: the phase
            // becomes `WaitingRetry`, so the bar's dot stays lit across the
            // back-off — waiting out a retry is the turn still working, and
            // that is the state the user was already watching. Ending the turn
            // here would drop the dot to grey for the length of the delay and
            // light it again below, which reads as "done, then started" for one
            // turn.
            //
            // The text flush is separate from the phase on purpose: the
            // attempts' streams are separate documents, the turn is one.
            self.log_pump_event("segment_end").await;
            self.set_turn_phase(crate::turn_phase::TurnPhase::WaitingRetry);
            self.content.end_response();
            self.content.display_text(&format!(
                "Error: {}. Retrying in {}s...",
                error,
                delay / 1000
            ));
            let slept = crate::agent::pumps::cancellable_sleep_pumped(
                &self.user_cancel,
                &self.cancel_notify,
                delay,
                crate::agent::pumps::turn_backoff_pump(
                    it_session.as_deref_mut(),
                    it_keys.as_deref_mut(),
                    it_history.as_deref_mut(),
                    it_label,
                    self.content.as_ref(),
                    &self.steer,
                ),
            )
            .await;
            if !slept {
                aborted = true;
                break 'attempts;
            }
            // The corrective hint for the retried request. It is delivered as a
            // context document, never as prompt text: the model needs to see why
            // its call failed, and the transcript must not keep the reminder.
            if let Some(feedback) = error.synthetic_retry_feedback() {
                *retry_hint
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(crate::agent::drive::context_document(feedback));
            }
            self.log_pump_event("begin_response").await;
            self.set_turn_phase(crate::turn_phase::TurnPhase::Streaming);
            self.content.begin_response();
        }

        // A steer that never reached a boundary — typed after the turn's last
        // request, or during the `Esc` that ended it — has nowhere to go.
        //
        // The row still holds it, because scheduling no longer clears the input:
        // that is the whole point of the editable-until-sent rule, and it means
        // there is nothing to hand back. Discarding the *slot* is all that is
        // left, so a stale steer cannot be delivered by the next turn's first
        // boundary.
        //
        // The one case worth restoring: the row was emptied after the steer was
        // scheduled (the user submitted, then cleared). Then the text exists
        // only here, and losing it would swallow words the model never heard.
        if let Some(pending) = self.steer.take() {
            if let Some(sess) = it_session {
                if sess.input_is_empty() {
                    sess.apply_prefill(&pending.steer.raw);
                    sess.sync_stack(self.content.as_ref(), it_label);
                    crate::repl::paint_prompt(self.content.as_ref());
                }
            }
        }

        self.log_pump_event("response_end").await;
        self.set_turn_phase(crate::turn_phase::TurnPhase::Idle);
        self.content.end_response();

        // The uniform exit persistence: the committed tail, then a batch whose
        // results never all landed, with "did not execute" for the calls that
        // never ran. The prompt is already on disk (dispatch).
        persist.close().await;

        if aborted {
            return Err(crate::PriorError::Cancelled("Aborted.".into()));
        }
        outcome?;
        Ok(output)
    }

    /// Build this turn's drive loop: prior owns the `AgentRun`.
    ///
    /// The loop is built once per user prompt and outlives a failed attempt —
    /// that is the whole of the retry story (design §5). `prompt` polls
    /// [`crate::agent::drive::run_stream`] over it, and when an attempt fails it
    /// waits and polls again: `next_step` from the run's untouched
    /// `AwaitingModel` re-emits the identical `CallModel`, so the retry *is* the
    /// same attempt — no new user message, no turn consumed, nothing to roll
    /// back.
    ///
    /// `Err` when the handler table is miswired: there is no run to drive, so
    /// the caller reports it as the turn's error rather than receiving a stream
    /// that can only ever fail.
    // `PromptError` carries the whole conversation in its error variants, so the
    // `Err` half is inherently large; boxing it would only move the cost to every
    // `?` site on a cold path. Same rationale as `drive.rs`'s module-level allow.
    #[allow(clippy::result_large_err)]
    fn build_drive(
        &self,
        input: &str,
        prior: Vec<Message>,
        params: &crate::params::ModelParams,
        overrides: crate::turn_overrides::TurnOverrides,
        session: Option<Arc<Session>>,
        resume: bool,
        // The REPL's input row, for the steer delivery to clear once the steer is
        // consumed. Taken here rather than from `in_turn` below because
        // `build_drive` runs before that is destructured, and the handle is a
        // cheap `Arc` clone of state the session keeps anyway. `None` without a
        // REPL, where there is no row.
        input_row: Option<std::sync::Arc<std::sync::Mutex<crate::repl::LineEditor>>>,
    ) -> std::result::Result<
        (
            crate::agent::drive::DriveLoop,
            crate::agent::drive::PersistHandle,
        ),
        PromptError,
    > {
        use crate::agent::drive::{Drive, DriveLoop, RetryHint};
        use rig_agent::run::AgentRun;

        // The spec is the agent's own (preamble, budget). Sampling is *not*
        // written here: it is recomposed at every call boundary by
        // `Drive::sampling`, so a knob changed while the turn runs — or trailed
        // on a steering message — reaches the next request of this same turn.
        let mut spec = self.agent.run_spec();
        spec.max_turns = Some(self.tool_call_limit);
        spec.max_invalid_tool_call_retries = 1;

        // A resume has no new prompt: the branch already ends on the synthetic
        // continuation, which is what `call_model_step` should take as the
        // prompt. So the split is the same one the rebuild does — last message
        // is the prompt, everything before it is history — and the model sees
        // `[user] [asst: partial] [user: continue]` unchanged across the resume.
        let run = if resume {
            let prior = prior;
            match prior.split_last() {
                Some((prompt, core)) => AgentRun::from_spec(
                    &spec,
                    prompt.clone(),
                    Some(core.iter().cloned().map(Arc::new).collect()),
                ),
                // Nothing to resume from (the branch lost its tail between the
                // check and here). Fall back to a plain empty prompt, which is
                // what a non-resuming dispatch would have sent.
                None => AgentRun::from_spec(&spec, Message::user(input), None),
            }
        } else {
            AgentRun::from_spec(
                &spec,
                Message::user(input),
                Some(prior.into_iter().map(Arc::new).collect()),
            )
        };

        let model = match self.agent.handlers().bind(self.agent.model_key()) {
            Ok(model) => model,
            // A miswired handler table: no run to drive, so the turn cannot
            // start. The prompt is already on disk (dispatch), so there is
            // nothing for a persister to close.
            Err(report) => return Err(PromptError::Report(report)),
        };

        let observer = crate::agent::observer::TurnObserver {
            content: self.content.clone(),
            user_cancel: self.user_cancel.clone(),
            message_logger: self.message_logger.clone(),
            reasoning_in_context: self.reasoning_in_context,
            provider: self.provider.clone(),
            model: self.model.clone(),
        };
        let tools = Arc::new(self.agent.tool_server_handle().snapshot());
        let drive = Drive {
            model,
            tools,
            spec,
            observer: Arc::new(observer),
            session,
            content: self.content.clone(),
            retry_hint: RetryHint::default(),
            // The slot is the agent's, not the loop's: `build_drive` runs inside
            // `prompt()`, so a slot here would be invisible to the key pump that
            // fills it.
            steer: self.steer_slot(),
            input: input_row,
            // `params` is this turn's composition, already carrying the prompt's
            // own override lines. The loop varies it from here.
            sampling: crate::agent::sampling::Sampling::new(
                self.app_state.clone(),
                *params,
                // The baked map is the fallback for a caller with no app state
                // (piped, batch, a summarizer): the same catalog composition
                // `AppState::reasoning_map` reads through.
                crate::params::reasoning_map(
                    &self.provider,
                    &crate::model_catalog::reasoning_support(&self.provider, &self.model),
                ),
                self.provider.clone(),
                self.routing.clone(),
                overrides,
            ),
            reasoning_in_context: self.reasoning_in_context,
            provider: self.provider.clone(),
            model_name: self.model.clone(),
        };
        Ok(DriveLoop::new(drive, run, self.tool_call_limit))
    }

    pub async fn run_once(&self, input: String) -> Result<String> {
        self.content.display_header(
            &self.provider,
            &self.model,
            self.yolo,
            self.tool_call_limit,
            self.persona_name.as_deref(),
        );
        self.content.display_separator();
        // `prompt` now renders the response itself via the streaming Output
        // methods, so there is no separate `display_markdown` here.
        // No override surface on this path, so the built live params.
        let response = self
            .prompt(
                &input,
                None,
                &self.live_params,
                crate::turn_overrides::TurnOverrides::default(),
                None,
            )
            .await?;
        Ok(response)
    }

    /// Generate a concise summary of the given text using the agent's model.
    /// Used by `/tree --summarize` to summarise abandoned branches.
    pub async fn summarize(&self, prompt: &str) -> Result<String> {
        self.agent
            .prompt(Message::user(prompt))
            .max_turns(SUMMARIZATION_MAX_TURNS)
            .await
            .map(|response| response.output())
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }

    /// Run a one-shot summarization whose **request context is the live
    /// conversation prefix** at the session's current leaf, plus
    /// `instruction` as the final user message.
    ///
    /// This is the cache-preserving compaction request shape: the leading
    /// history is built by exactly the same `Session::build_context()` (plus
    /// the same `maybe_strip_reasoning` pass) that a normal user prompt at
    /// that position would use, so the provider's prefix cache — warmed by
    /// the live turns — hits for the whole conversation span. Only the
    /// summarization instruction itself is a fresh (cache-write) tail.
    ///
    /// The session's leaf is READ but never MODIFIED here: callers that need
    /// to summarize a *prefix* of the conversation move the leaf to the
    /// desired position first (the `/tree <id> compact llm` leaf dance) and
    /// restore it on every return path afterwards.
    pub async fn summarize_with_session_context(
        &self,
        session: &Session,
        instruction: &str,
    ) -> Result<String> {
        // Leading history = the live conversation prefix at the current leaf,
        // built by the SAME helper a normal user prompt uses
        // (`session_context_messages` — `Session::build_context()` +
        // `maybe_strip_reasoning`), so this request shares the provider's
        // prefix cache (warmed by live turns) for the whole span.
        // `Session::build_context()` applies the compaction transforms, so a
        // prior compaction active at this position emits its summary as the
        // leading `<summary>` wrapper — implicit incremental-compaction
        // semantics from the tree structure alone.
        //
        // The leaf is READ but never MODIFIED here: callers that need to
        // summarize a *prefix* of the conversation move the leaf to the
        // desired position first (the `/tree <id> compact llm` leaf dance)
        // and restore it on every return path afterwards.
        let prior = session_context_messages(
            session,
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        )
        .await?;

        self.agent
            .prompt(Message::user(instruction))
            .history(prior)
            .max_turns(SUMMARIZATION_MAX_TURNS)
            .await
            .map(|response| response.output())
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }

    /// Write what a failed or aborted attempt had already rendered.
    ///
    /// The first half of the retention doc's §3: the text the user watched goes
    /// to the session as an ordinary assistant message, because everything
    /// rendered must be persisted — on the retry path *and* on an abort, where
    /// it is the turn's last word.
    ///
    /// The message is hand-built by
    /// [`assistant_message`](crate::agent::transcript::assistant_message), which
    /// leaves `stop: None`. That is load-bearing: rig drops a turn whose stop
    /// reason marks it failed, so a partial carrying `Aborted`/`Error` would be
    /// silently stripped from every later request (§2).
    ///
    /// A write failure is a warning rather than an error: the turn still has a
    /// retry to run, and the persister retries its own unsaved batches.
    ///
    /// Returns whether there was new text to write. That is the signal the
    /// caller needs, because a failure with nothing streamed (a 429 on the very
    /// first delta) leaves the history untouched — and then the retry re-issues
    /// the attempt in place, appending nothing at all.
    async fn persist_abandoned(
        &self,
        drive: &mut crate::agent::drive::DriveLoop,
        session: Option<&Session>,
    ) -> bool {
        let Some(session) = session else {
            return false;
        };
        // An `<Esc>` abort drops the stream before the loop's failure path
        // captures anything, so this is the one place the two paths meet.
        drive.capture_abandoned();
        let Some(content) = drive.take_abandoned() else {
            return false;
        };
        let partial = crate::agent::transcript::assistant_message(None, content);
        match session.append_message(&partial).await {
            Ok(_) => true,
            Err(error) => {
                self.content
                    .display_error(&format!("failed to persist the interrupted turn: {error}"));
                false
            }
        }
    }

    /// Rebuild the turn around what was just persisted, so the retry continues
    /// from the model's own text instead of restarting.
    ///
    /// The retention doc's §3, in one place:
    ///
    /// 1. the text the user watched goes to the session as an ordinary
    ///    assistant message, because everything rendered must be persisted;
    /// 2. a synthetic continuation is appended **unless the branch already ends
    ///    with one**, which is what makes a consecutive retry append nothing
    ///    (§3 "Consecutive retries append nothing") — the session is
    ///    append-only, so a wrong second one could never be undone;
    /// 3. the turn is rebuilt from the session's exact history, its last message
    ///    becoming the rebuilt run's prompt.
    ///
    /// Returns whether the run was replaced. The persister is shared and rebased
    /// in place, so there is no handle to hand back (design §5a). Write failures
    /// are warnings: the turn still has a retry to run.
    async fn rebuild_after_partial(
        &self,
        drive: &mut crate::agent::drive::DriveLoop,
        session: Option<&Session>,
    ) -> bool {
        let Some(session) = session else {
            return false;
        };
        // The nudge that makes the rebuilt history end on a user message, so
        // the model continues the partial instead of being handed a request that
        // ends in an assistant turn — a *prefill*, which most providers refuse
        // (§4). Written once per incomplete turn: on a consecutive retry the
        // branch already ends with one, and appending a second would grow the
        // history by a message per attempt with no way to undo it.
        if let Err(error) = session
            .ensure_custom_message(
                crate::agent::continuation::CUSTOM_TYPE,
                crate::agent::continuation::TEXT,
            )
            .await
        {
            self.content
                .display_error(&format!("failed to persist the continuation: {error}"));
        }
        // Rebuild from the session, which now holds what the model said and the
        // nudge that follows it. Reading the session rather than splicing the
        // partial into a patch is deliberate: a patch is a one-shot that the
        // next `CallModel` would have to re-place, reordering the history as the
        // turn progresses (retention doc §3 "Why not splice").
        let history = match session_context_messages(
            session,
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        )
        .await
        {
            Ok(history) => history,
            Err(error) => {
                self.content
                    .display_error(&format!("failed to rebuild the turn: {error}"));
                return false;
            }
        };
        drive.rebuild_from(history).await
    }
}

/// Model-call budget for a summarization / compaction request.
///
/// A summarizer is not a single-shot request in practice: the agent
/// carries the full tool set, so a summary that has to check a file,
/// re-read a span, or recover from a hallucinated call costs more than
/// one model call — and one call is all rig allows implicitly (an
/// unconfigured agent's budget), which fails the request outright on the
/// first tool call. Ten leaves room for that lookup-and-answer shape (a
/// tool call plus the model-authored summary) without letting a
/// summarizer wander.
///
/// Deliberately *not* `self.tool_call_limit`: that is the interactive
/// turn budget, and a summary is a background, user-initiated operation
/// whose cost is bounded by what the user asked for, not by the budget
/// they set for a coding turn.
const SUMMARIZATION_MAX_TURNS: usize = 10;
