//! Cancellation plumbing for the turn's two pumps: the retry back-off between
//! attempts, and the window wait that keeps a compaction or other long
//! operation responsive to `Esc`.
//!
//! One dispatcher ([`dispatch_in_turn_event`]) behind both, so a key means the
//! same thing wherever the turn is — `Esc` cancels, anything else edits the
//! input row or steps a menu. The two waits differ only in what they do with a
//! non-cancel key: the back-off pumps it through the dispatcher, the window
//! parks it for `read_input`.
//!
//! Both are absolute-deadline waits. A dispatched or parked key must not extend
//! the delay.

use crate::app_state::WindowKeySource;
use crate::repl::paint_prompt;
use crate::termio::InputEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Resolves once `user_cancel` is set. The notify wakes it; the load before
/// subscribing covers a notify that fired before this call existed. Loops over
/// spurious wakes — a notify with the flag still unset is somebody else's
/// signal, not the user's `Esc`.
pub(crate) async fn cancel_observed(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) {
    loop {
        if user_cancel.load(Ordering::SeqCst) {
            return;
        }
        cancel_notify.notified().await;
        if user_cancel.load(Ordering::SeqCst) {
            return;
        }
        // Spurious wakeup without cancel set — loop and re-await.
    }
}

/// What dispatching one turn-key event did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InTurnDispatch {
    /// No event, or no session to receive it: nothing happened.
    Ignored,
    /// The event was applied and the input row settled to match.
    Handled,
    /// Root `Esc`: the cancel signal fired (flag + notify + tool watchdogs).
    /// The streaming window ends on its next poll; a back-off wait ends
    /// immediately.
    CancelFired,
}

/// The turn's key dispatcher — the one body behind both of the turn's pumps:
/// `prompt()`'s select branch and the retry back-off's
/// [`cancellable_sleep_pumped`] ("one dispatcher, two pumps", ui_draw_v2 §4.5
/// U4). A key therefore means the same thing wherever the turn is.
///
/// Ordering is `read_input`'s minus the wait: dispatch, then settle.
///
/// Menu-first `Esc` (settled U4): with a v2.5 menu open the key goes to
/// `handle_key`, whose ladder steps the engine back one level. At root — a bare
/// prompt, or only unsubmitted prose that the session carries across the turn
/// anyway — there is nothing to step out of, so it stops the turn. The
/// in-flight reader used to make that call itself, and made it with no way to
/// see a menu: unconditional. It routes now (U4.3) and this dispatcher decides
/// — the place that can tell the difference.
///
/// A submit (`KeyOutcome::Action`) is blocked mid-turn, and *silently*: v0
/// offers no mid-turn action, and a message printed over the live stream would
/// be worse than none. The turn keeps running, so unlike at the prompt there
/// is a next iteration — which is why this still settles, and the screen stays
/// true to the state the key left behind.
pub(crate) async fn dispatch_in_turn_event(
    ev: Option<InputEvent>,
    sess: Option<&mut crate::repl::PromptSession>,
    history: Option<&mut crate::repl::History>,
    label: &str,
    // Content is the one handle (4f-d): the frame dims, the paint path, and —
    // through its manager — the `$EDITOR` turnover the guard/output pair used
    // to carry.
    content: &crate::content::Content,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> InTurnDispatch {
    let (Some(ev), Some(sess), Some(history)) = (ev, sess, history) else {
        return InTurnDispatch::Ignored;
    };
    match ev {
        InputEvent::Key(key) => {
            // The confirm is **modal**, and this is the top of the `Esc` ladder:
            // dialog first, menu second, turn third. The root-`Esc` cancel below
            // is unconditional, so without this precedence one `Esc` would
            // dismiss the dialog *and* kill the turn.
            if sess.confirm_open() {
                return match sess.confirm_key(key) {
                    // Resolved: paint the area back (dropping the element is the
                    // erase — no `Frame::empty()`), then record the decision
                    // where the dialog's rows were.
                    Some(decision) => {
                        let subject = sess.confirm_subject().unwrap_or_default();
                        sess.clear_confirm();
                        crate::confirm::resolve(
                            content,
                            || {
                                sess.sync_stack(content, label);
                                paint_prompt(content);
                            },
                            &subject,
                            decision,
                        );
                        InTurnDispatch::Handled
                    }
                    None => {
                        sess.sync_stack(content, label);
                        paint_prompt(content);
                        InTurnDispatch::Handled
                    }
                };
            }
            if key.code == crossterm::event::KeyCode::Esc && !sess.menu_open() {
                user_cancel.store(true, Ordering::SeqCst);
                cancel_notify.notify_one();
                // Wake in-flight tool watchdogs so a running shell dies now
                // rather than at its next check. No tool runs during a
                // back-off, so this is a no-op there — fired anyway so both
                // pumps raise the identical signal set. Unbounded mpsc, so
                // this cannot block the main task.
                for (_, tx) in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
                    let _ = tx.send(());
                }
                // Nothing to settle: the window ends on the flag.
                return InTurnDispatch::CancelFired;
            }
            let opts_len = sess.opts_len();
            // The frame's budget, from the one place a ratio resolves to a row
            // count (4h-b): `budget_now` reads the watched height — the same
            // geometry the settle and paint below compose at — so the grant
            // and the frame cannot disagree.
            let budget = content.manager.lock().unwrap().budget_now();
            match sess.handle_key(key, opts_len, budget, label, history) {
                crate::repl::KeyOutcome::RunEditor => {
                    sess.open_editor(content);
                }
                crate::repl::KeyOutcome::Continue | crate::repl::KeyOutcome::Action(_) => {}
            }
            // `settle` is where a `/`-command committed mid-turn lands exactly
            // as it does at the prompt. Its `MenuDone` is dropped: applying a
            // command needs the convergence machinery that runs *after*
            // `prompt()` returns, which is still post-turn only (U4.3+).
            let _ = sess.settle(content, label, history).await;
            InTurnDispatch::Handled
        }
        InputEvent::Paste(text) => {
            // A confirm has no text field, so a paste has nowhere to go; drop
            // it rather than pushing it into the prompt the dialog is layered
            // over, which would change the menu's meaning behind the modal.
            if sess.confirm_open() {
                return InTurnDispatch::Ignored;
            }
            sess.paste(&text);
            let _ = sess.settle(content, label, history).await;
            InTurnDispatch::Handled
        }
        _ => InTurnDispatch::Ignored,
    }
}

/// The turn's key pump, live outside `prompt()`: what a retry back-off
/// borrows so keys keep dispatching between attempts exactly as they do inside
/// `prompt()`'s select — the back-off is part of the turn, not a hole in it.
pub(crate) struct TurnPump<'a> {
    keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    session: &'a mut crate::repl::PromptSession,
    history: &'a mut crate::repl::History,
    label: &'a str,
    /// The one handle (4f-d): paints and settles go through content's manager
    /// (step 4f-b), the frame dims read content's watch (4f-c), and the
    /// `$EDITOR` turnover reaches the manager's front door through it too —
    /// the guard/output pair this pump used to carry is gone.
    content: &'a crate::content::Content,
}

impl<'a> TurnPump<'a> {
    /// Assemble a pump from the pieces a test (or any caller outside the turn)
    /// has in hand. [`turn_backoff_pump`] is the shape the loop itself uses:
    /// it `zip`s the optional pieces and yields `None` where they are absent.
    #[cfg(test)]
    pub(crate) fn new(
        keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
        session: &'a mut crate::repl::PromptSession,
        history: &'a mut crate::repl::History,
        label: &'a str,
        content: &'a crate::content::Content,
    ) -> Self {
        Self {
            keys,
            session,
            history,
            label,
            content,
        }
    }
}

/// Assemble the pump for a back-off wait, or `None` where the interactive
/// pieces don't exist — the same `zip` shape the `InTurnInput` construction
/// uses: piped/batch input has no prompt session and no reader, so there is no
/// key to pump and the wait is [`cancellable_sleep_pumped`] with `None` —
/// deadline and cancel signal only.
pub(crate) fn turn_backoff_pump<'a>(
    prompt_session: &'a mut Option<crate::repl::PromptSession>,
    keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    history: &'a mut crate::repl::History,
    label: &'a str,
    content: &'a crate::content::Content,
) -> Option<TurnPump<'a>> {
    // A prompt session exists only in the REPL (interactive tty) — the same
    // fact the old `guard` zip re-checked, since TermIO is Tty iff there is one.
    match (prompt_session.as_mut(), history) {
        (Some(session), history) => Some(TurnPump {
            keys,
            session,
            history,
            label,
            content,
        }),
        _ => None,
    }
}

/// The back-off wait with the turn's key pump running: every key in the queue
/// is dispatched through [`dispatch_in_turn_event`] for as long as it lasts. A root `Esc` fires the cancel signal immediately and
/// ends the wait (`false`); menus step their ladder; edits settle into the
/// input row — identical to the streaming window. The deadline is absolute, so
/// dispatched keys never extend the back-off.
///
/// `pump: None` (piped input) degrades to the plain wait.
pub(crate) async fn cancellable_sleep_pumped(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    ms: u64,
    mut pump: Option<TurnPump<'_>>,
) -> bool {
    if ms == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        // Prose's wake-up for this iteration, resolved *before* the select
        // rather than inside the timer's async block — the same contract
        // `prompt()`'s loop has: without this arm prose goes inert for the
        // whole back-off (its debounce, its in-flight slices and its dialog
        // all fire on it). U4.3's timer rationale, extended from the
        // streaming window to the wait between attempts.
        let prose_deadline = pump
            .as_ref()
            .and_then(|p| crate::repl::PromptSession::prose_timeout(&*p.session));
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = cancel_observed(user_cancel, cancel_notify) => return false,
            ev = async {
                match pump.as_mut() {
                    Some(p) => p.keys.recv().await,
                    // No pump (piped input): no keys can ever arrive, so the
                    // branch never completes — same trick `prompt()` uses.
                    None => std::future::pending().await,
                }
            } => {
                if let Some(p) = pump.as_mut() {
                    let outcome = dispatch_in_turn_event(
                        ev,
                        Some(&mut *p.session),
                        Some(&mut *p.history),
                        p.label,
                        p.content,
                        user_cancel,
                        cancel_notify,
                    )
                    .await;
                    if outcome == InTurnDispatch::CancelFired {
                        return false;
                    }
                }
                // Handled / Ignored: keep waiting out the rest of the
                // deadline — the loop re-selects with the same deadline.
            }
            // Prose wake: settle so the debounce/slice/dialog fires without
            // a keystroke, exactly as `prompt()`'s timer arm does.
            _ = async {
                match prose_deadline {
                    Some(d) => tokio::time::sleep(d).await,
                    // `None`: nothing pending, and no timer to wake for.
                    None => std::future::pending().await,
                }
            } => {
                if let Some(p) = pump.as_mut() {
                    let _ = p
                        .session
                        .settle(p.content, p.label, &mut *p.history)
                        .await;
                }
            }
        }
    }
}

/// Next event for a window pump, paced: polls the source without blocking
/// and yields between tries, so an idle window costs one `poll(0)` per tick
/// instead of a reader thread. `None` source, or an inactive window
/// (piped/batch — the script owns stdin): never completes, exactly like the
/// turn pump's missing-queue arm.
pub(crate) async fn next_window_key(
    src: Option<&dyn WindowKeySource>,
) -> Option<crossterm::event::Event> {
    let Some(src) = src else {
        return std::future::pending().await;
    };
    if !src.window_active() {
        return std::future::pending().await;
    }
    loop {
        if let Some(ev) = src.poll_key() {
            return Some(ev);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Route one window-pump event: a root `Esc` fires the cancel signal — the
/// same flag + notify + tool watchdogs the turn's dispatcher raises, which
/// is the whole point of the window — and reports it; anything else is
/// parked for `read_input`. Returns whether the cancel signal fired.
pub(crate) fn pump_window_key(
    ev: Option<crossterm::event::Event>,
    src: &dyn WindowKeySource,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> bool {
    let Some(ev) = ev else {
        return false;
    };
    if let crossterm::event::Event::Key(key) = &ev {
        if key.code == crossterm::event::KeyCode::Esc {
            user_cancel.store(true, Ordering::SeqCst);
            cancel_notify.notify_one();
            // Identical signal set to the turn dispatcher; no tool runs
            // during a compaction window, so the send is inert there.
            // Unbounded mpsc, so this cannot block the caller.
            for (_, tx) in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
                let _ = tx.send(());
            }
            return true;
        }
    }
    src.defer_event(ev);
    false
}

/// The back-off wait with the window pump live: the wait is part of the
/// operation, so an `Esc` typed during it ends the wait immediately
/// (`false`) and every other key is parked for `read_input`. The deadline
/// stays absolute — parked keys never extend the delay. `keys: None`, or an
/// inactive window, degrades to the plain signal-only wait.
pub(crate) async fn cancellable_sleep_windowed(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    ms: u64,
    keys: Option<&dyn WindowKeySource>,
) -> bool {
    if ms == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = cancel_observed(user_cancel, cancel_notify) => return false,
            ev = next_window_key(keys) => {
                if let Some(src) = keys {
                    if pump_window_key(ev, src, user_cancel, cancel_notify) {
                        return false;
                    }
                }
                // Parked: keep waiting out the rest of the deadline.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The window-pump tests need a long-running operation to pump during, and
    // a retried summarization is the one that exists.
    use super::super::compaction::retry_summarization;
    use crate::content::Content;
    use crate::settings::SettingsStore;
    use serial_test::serial;

    /// The content half of the same idea (step 3d): `retry_summarization`
    /// reaches content, and a `Content` tapped through `NoOutput` discards it.
    fn null_content() -> std::sync::Arc<Content> {
        std::sync::Arc::new(Content::with_tap(std::sync::Arc::new(
            crate::output::NoOutput,
        )))
    }

    fn test_settings() -> SettingsStore {
        SettingsStore {
            default_model: "p:m".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            models: Default::default(),
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::from("default prompt"),
            path: None,
        }
    }
    /// The pieces a back-off pump needs: a prompt session over the test app
    /// state and a history (4f-d: no guard — the pump no longer carries one,
    /// and the root-Esc path returns before any terminal call anyway).
    fn backoff_pump_parts(
        app_state: &std::sync::Arc<crate::app_state::AppState>,
    ) -> (crate::repl::PromptSession, crate::repl::History) {
        (
            crate::repl::PromptSession::new(None, app_state),
            crate::repl::History::load(),
        )
    }

    /// The bug this pump exists for: a root `Esc` typed during a retry
    /// back-off must cut the wait short. Before the queue was pumped between
    /// attempts the Esc sat unread until the next `prompt()` started, so the
    /// full delay always ran — the reported "the session only aborts once the
    /// timeout has elapsed".
    // `#[serial]`: an `Esc` pumped through the window reaches
    // `pump_window_key`, which broadcasts on the process-global
    // `TOOL_CANCEL_SENDERS` registry — killing the child of *every*
    // in-flight command in this process, including one another test's.
    // These must not overlap a test running a real sandboxed command.
    #[tokio::test]
    #[serial]
    async fn backoff_pump_cancels_promptly_on_queued_esc() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        let (mut session, mut history) = backoff_pump_parts(&app_state);
        let content = null_content();
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        tx.send(InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )))
        .unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let pump = TurnPump::new(&mut keys, &mut session, &mut history, "", content.as_ref());
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 60_000, Some(pump)).await;
        assert!(
            !completed,
            "a queued Esc must end the back-off, not the deadline"
        );
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited toward the 60s deadline instead of cancelling on Esc"
        );
    }

    /// A plain key dispatched during the back-off must NOT end the wait: it
    /// goes through the dispatcher (settle and all) and the delay runs out on
    /// its own schedule — the deadline is absolute.
    #[tokio::test]
    async fn backoff_pump_plain_key_does_not_cancel_the_wait() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        let (mut session, mut history) = backoff_pump_parts(&app_state);
        let content = null_content();
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        tx.send(InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        )))
        .unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let pump = TurnPump::new(&mut keys, &mut session, &mut history, "", content.as_ref());
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 1_000, Some(pump)).await;
        assert!(
            completed,
            "a plain key must not be mistaken for the cancel signal"
        );
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(900),
            "returned before the 1s deadline — the key cut the wait short"
        );
    }

    /// Piped input has no pump; the wait must still honor the plain cancel
    /// signal — the keys arm is `pending`, not `None`-completing, so the
    /// select still has a live cancel branch.
    #[tokio::test]
    async fn backoff_sleep_without_pump_still_honors_cancel_signal() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel2 = cancel.clone();
        let notify2 = notify.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel2.store(true, std::sync::atomic::Ordering::SeqCst);
            notify2.notify_one();
        });
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 60_000, None).await;
        let _ = handle.await;
        assert!(
            !completed,
            "the signal branch must still fire without a pump"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Fake window source for the compaction pump: events waiting to be
    /// read, and the park where keys the window cannot act on land.
    struct FakeWindow {
        ready: std::sync::Mutex<std::collections::VecDeque<crossterm::event::Event>>,
        parked: std::sync::Mutex<std::collections::VecDeque<crossterm::event::Event>>,
        active: bool,
    }

    impl FakeWindow {
        fn new() -> Self {
            Self {
                ready: std::sync::Mutex::new(std::collections::VecDeque::new()),
                parked: std::sync::Mutex::new(std::collections::VecDeque::new()),
                active: true,
            }
        }

        /// Piped/batch shape: stdin belongs to the script.
        fn inactive() -> Self {
            Self {
                active: false,
                ..Self::new()
            }
        }

        fn push(&self, ev: crossterm::event::Event) {
            self.ready.lock().unwrap().push_back(ev);
        }

        fn ready_len(&self) -> usize {
            self.ready.lock().unwrap().len()
        }

        fn parked_len(&self) -> usize {
            self.parked.lock().unwrap().len()
        }
    }

    impl WindowKeySource for FakeWindow {
        fn window_active(&self) -> bool {
            self.active
        }
        fn poll_key(&self) -> Option<crossterm::event::Event> {
            self.ready.lock().unwrap().pop_front()
        }
        fn defer_event(&self, ev: crossterm::event::Event) {
            self.parked.lock().unwrap().push_back(ev);
        }
    }

    fn esc_event() -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn char_event(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    /// The compaction window's core promise: an `Esc` while the
    /// summarization request is in flight cancels it. Before the pump,
    /// nothing read stdin during the window and the attempt always ran to
    /// completion.
    // `#[serial]`: an `Esc` pumped through the window reaches
    // `pump_window_key`, which broadcasts on the process-global
    // `TOOL_CANCEL_SENDERS` registry — killing the child of *every*
    // in-flight command in this process, including one another test's.
    // These must not overlap a test running a real sandboxed command.
    #[tokio::test]
    #[serial]
    async fn window_pump_esc_cancels_the_inflight_attempt() {
        let keys = FakeWindow::new();
        keys.push(esc_event());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let content = null_content();
        let started = std::time::Instant::now();
        let r = retry_summarization(
            content.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async {
                std::future::pending::<std::result::Result<String, crate::PicocodeError>>().await
            },
        )
        .await;
        assert!(matches!(r, Err(crate::PicocodeError::Cancelled(_))));
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the in-flight attempt ignored the Esc"
        );
    }

    /// The reported bug shape: an `Esc` during a *retry back-off* cuts the
    /// wait instead of letting the provider-hint floor (10s) run out.
    // `#[serial]`: an `Esc` pumped through the window reaches
    // `pump_window_key`, which broadcasts on the process-global
    // `TOOL_CANCEL_SENDERS` registry — killing the child of *every*
    // in-flight command in this process, including one another test's.
    // These must not overlap a test running a real sandboxed command.
    #[tokio::test]
    #[serial]
    async fn window_pump_esc_cuts_the_retry_backoff() {
        let keys = std::sync::Arc::new(FakeWindow::new());
        let keys2 = keys.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            keys2.push(esc_event());
        });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let content = null_content();
        let mut calls = 0;
        let started = std::time::Instant::now();
        let r = retry_summarization(
            content.as_ref(),
            &cancel,
            &notify,
            Some(keys.as_ref()),
            &mut || {
                calls += 1;
                async {
                    Err(crate::PicocodeError::LlmRateLimited {
                        retry_after_secs: Some(10),
                    })
                }
            },
        )
        .await;
        let _ = handle.await;
        assert!(matches!(r, Err(crate::PicocodeError::Cancelled(_))));
        assert_eq!(calls, 1, "no retry attempt may start after the Esc");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the 10s back-off ran its course instead of yielding to the Esc"
        );
        assert_eq!(keys.parked_len(), 0, "an Esc is consumed, not parked");
    }

    /// A plain key during the window must NOT cancel: it is parked for
    /// `read_input` and the operation completes normally.
    #[tokio::test]
    async fn window_pump_plain_key_is_parked_and_not_cancelled() {
        let keys = FakeWindow::new();
        keys.push(char_event('x'));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let content = null_content();
        let r = retry_summarization(
            content.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok("done".to_string())
            },
        )
        .await;
        assert_eq!(r.unwrap(), "done");
        assert_eq!(
            keys.parked_len(),
            1,
            "the key must wait parked for read_input's drain"
        );
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// An inactive window (piped/batch — no REPL) must not touch its source
    /// at all: the arms stay pending, the ready event is never consumed.
    #[tokio::test]
    async fn window_pump_inactive_never_polls() {
        let keys = FakeWindow::inactive();
        keys.push(esc_event());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let content = null_content();
        let r = retry_summarization(
            content.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async { Ok("fine".to_string()) },
        )
        .await;
        assert_eq!(r.unwrap(), "fine");
        assert_eq!(
            keys.ready_len(),
            1,
            "an inactive window must never read its source"
        );
        assert_eq!(keys.parked_len(), 0);
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The real source on a test state: its manager carries no mode guard
    /// (test states never install one), so the window is inactive (poll
    /// yields nothing — stdin untouched), and the park round-trips in order:
    /// that queue is what `read_input` drains.
    #[test]
    fn appstate_window_gate_and_park_roundtrip() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        assert!(!app_state.window_pump_active());
        assert!(app_state.poll_key().is_none(), "inactive must not read");
        app_state.park_key(char_event('a'));
        app_state.park_key(char_event('b'));
        // The park converts to the input channel's vocabulary (step 4a) —
        // that queue is what `read_input` drains, and it speaks `InputEvent`.
        let expect = |c: char| {
            Some(InputEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            )))
        };
        assert_eq!(app_state.take_parked_key(), expect('a'));
        assert_eq!(app_state.take_parked_key(), expect('b'));
        assert_eq!(app_state.take_parked_key(), None);
    }
}
