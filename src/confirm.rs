//! The tool-confirmation port: how a tool asks the REPL for a decision.
//!
//! A gated tool call runs **inline inside `prompt()`'s `select!`** — the stream
//! arm is rig's `async_stream`, and `tool_snapshot.dispatch(...).await` is
//! inside it (rig `runner.rs:709`), with no `tokio::spawn` on the path. So a
//! confirm used to be a *blocking* call on the same task as the loop, which is
//! why it had to own stdin (`confirm_gate`), anchor itself with a DSR, and
//! erase itself with a `Frame::empty()`. See
//! [`doc/confirm_as_menu_261002.md`](../../doc/confirm_as_menu_261002.md).
//!
//! Here it is an `await` instead. The guard sends a [`ConfirmRequest`] and parks
//! on the oneshot; the stream arm returns `Pending`, the loop's other arms keep
//! running, the REPL paints the dialog as an ordinary overlay element, and the
//! answer goes back through the reply channel.
//!
//! What the agent gets back is [`ConfirmDecision`] — an accept/deny bit — so
//! "always" cannot leak across the boundary. Picking *always* is the REPL's
//! business: it records the approval and answers `Accept`, exactly as it
//! answers `Accept` for *yes*. The agent has one bit, and the one bit means one
//! thing.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{mpsc, oneshot};

use crate::slashdot_menu::render::{LogicalLine, RenderPlan};

/// What the REPL decided. One bit, deliberately: the agent cannot tell *yes*
/// from *always*, which is what keeps "always" out of its vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// Run the tool. Covers both `yes` and `always` — the REPL has already
    /// recorded an `always` choice by the time it answers.
    Accept,
    /// Don't. Also covers a dismissal (`Esc`), which is a denial.
    Deny,
}

/// One gated tool call asking to run.
///
/// `subject` is what the decision line names (`tool BASH`), derived from
/// `header` by [`confirm_subject`] so the convention lives in one place.
pub struct ConfirmRequest {
    /// The tool's name as the agent knows it (`bash`) — the approval store is
    /// keyed by it, and the header only carries the capitalised form.
    pub tool: String,
    pub header: String,
    pub subject: String,
    pub reply: oneshot::Sender<ConfirmDecision>,
}

/// Tools approved for the rest of the session, by name.
///
/// **Session-scoped, deliberately** — the same lifetime the old
/// `Guard::always: Arc<AtomicBool>` had. There is a settings surface that
/// already means "don't ask me about these" (`auto_allow`, settings.toml), and
/// writing here into it would make an approval outlive the process, which is a
/// product decision rather than a refactor. Per-tool, so approving `bash` does
/// not approve `edit` — matching the old per-`Guard` flag.
#[derive(Clone, Default)]
pub struct Approvals(Arc<Mutex<HashSet<String>>>);

impl Approvals {
    pub fn approve(&self, tool: &str) {
        self.0.lock().unwrap().insert(tool.to_string());
    }

    pub fn is_approved(&self, tool: &str) -> bool {
        self.0.lock().unwrap().contains(tool)
    }
}

/// The agent's end of the port: ask, and wait to be told.
///
/// Cheap to clone (two `Arc`s + a channel sender) and held for the life of the
/// agent, so a `/model` rebuild hands the *same* port to the new guards and an
/// approval taken before the rebuild still holds after it.
#[derive(Clone)]
pub struct ConfirmPort {
    tx: mpsc::UnboundedSender<ConfirmRequest>,
    approvals: Approvals,
}

impl ConfirmPort {
    /// Ask the REPL whether `tool` may run, and wait for the answer.
    ///
    /// `Deny` is the answer in every case where no one is there to decide —
    /// the REPL went away mid-turn, the reply channel was dropped without a
    /// decision, the session ended with the dialog open. Failing closed is the
    /// only safe direction for a consent prompt: a hang would freeze the turn,
    /// and a yes would run a tool the user never agreed to.
    pub async fn request(&self, tool: &str, header: String) -> ConfirmDecision {
        if self.approvals.is_approved(tool) {
            return ConfirmDecision::Accept;
        }
        let (reply, answer) = oneshot::channel();
        if self
            .tx
            .send(ConfirmRequest {
                tool: tool.to_string(),
                subject: confirm_subject(&header),
                header,
                reply,
            })
            .is_err()
        {
            return ConfirmDecision::Deny;
        }
        answer.await.unwrap_or(ConfirmDecision::Deny)
    }

    /// The REPL's approval store, so it can record an *always* choice where the
    /// decision is made.
    pub fn approvals(&self) -> &Approvals {
        &self.approvals
    }
}

/// Both ends of one session's confirm port, built once by `run_interactive`
/// before the agent that carries the sender half.
///
/// The receiver is `&mut`-borrowed per turn, exactly like
/// `InTurnInput::keys`, so a request can never outlive the pump that would
/// answer it.
pub struct ConfirmBridge {
    pub port: ConfirmPort,
    pub requests: mpsc::UnboundedReceiver<ConfirmRequest>,
}

impl ConfirmBridge {
    pub fn new() -> Self {
        // Unbounded, for the reason `route_event` records: a bounded channel
        // that filled would block the *sender*, and the sender is a tool call
        // the turn is waiting on.
        let (tx, requests) = mpsc::unbounded_channel();
        Self {
            port: ConfirmPort {
                tx,
                approvals: Approvals::default(),
            },
            requests,
        }
    }
}

impl Default for ConfirmBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// Subject of a tool-confirmation decision line.
///
/// The caller's message follows the `Confirm tool {NAME} call?` convention (the
/// confirm gate in `agent.rs`) → `tool {NAME}`. Surrounding convention furniture
/// is stripped independently; anything left over passes through unchanged.
pub fn confirm_subject(header: &str) -> String {
    let s = header.strip_prefix("Confirm ").unwrap_or(header);
    s.strip_suffix(" call?").unwrap_or(s).to_string()
}

/// The dialog the REPL shows on a gated tool's behalf.
///
/// This is the **whole** presentation: the option list, the default, the quick
/// keys and the decision line all live here rather than in the agent, because
/// the agent has no screen. It is ordinary menu state — a `RenderPlan` and a
/// cursor — and it paints through the same element builder the slash menu uses,
/// appended below whatever overlay is already open. A short dialog over a menu
/// beats clearing the prompt that drives the menu.
pub struct ConfirmPrompt {
    plan: RenderPlan,
    sel: usize,
    subject: String,
    tool: String,
    reply: Option<oneshot::Sender<ConfirmDecision>>,
    approvals: Approvals,
}

/// The options, in the order they have always appeared. *No* is preselected
/// (index 1): a consent prompt that opens on "run it" is a consent prompt that
/// gets muscle-memory'd into running things.
const OPTIONS: [(&str, &str); 3] = [("yes", "Yes"), ("no", "No"), ("always", "Always (session)")];
const DEFAULT_SEL: usize = 1;

/// Quick keys. Spelled out rather than derived, because the obvious derivation
/// is wrong for one of them: `always` starts with `a`, and the key that has
/// always meant it is `s` (session). The old `ConfirmSpec.quick_keys` said
/// `[('y', 0), ('n', 1), ('s', 2)]`; this is the same table.
const QUICK: [(char, usize); 3] = [('y', 0), ('n', 1), ('s', 2)];

/// Index of the `no` row — the answer every unmapped key resolves to.
const DENY: usize = 1;

impl ConfirmPrompt {
    pub fn new(req: ConfirmRequest, approvals: Approvals) -> Self {
        let detail = req
            .header
            .split('\n')
            .map(LogicalLine::new)
            .collect::<Vec<_>>();
        let rows = Arc::new(
            OPTIONS
                .iter()
                .map(|(id, desc)| LogicalLine::new(format!("{}  {}", id, desc)))
                .collect(),
        );
        Self {
            plan: RenderPlan {
                table_header: None,
                detail,
                rows,
                selected: Some(DEFAULT_SEL),
                active_rows: vec![],
                view: None,
                columns: Vec::new(),
            },
            sel: DEFAULT_SEL,
            subject: req.subject,
            tool: req.tool,
            reply: Some(req.reply),
            approvals,
        }
    }

    /// The frame elements for the dialog, to append after the open overlay's.
    ///
    /// The option rows are pinned to the full option count, so a starved
    /// budget cannot squeeze the choices away — a consent prompt you cannot read
    /// is not a consent prompt.
    pub fn elements(&self) -> Vec<crate::render::Element> {
        self.plan.elements(OPTIONS.len())
    }

    /// Feed one key. `Some(decision)` means the dialog is finished — the
    /// caller paints without it and writes the decision line.
    ///
    /// `Esc` and every unmapped key are a **denial**, which is the old
    /// dialog's behaviour (dismiss mapped to `No`) and the only safe direction:
    /// a key the dialog does not understand must never read as consent.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<ConfirmDecision> {
        if key.modifiers != KeyModifiers::NONE {
            return None;
        }
        let picked = match key.code {
            KeyCode::Up => {
                self.sel = self.sel.saturating_sub(1);
                self.plan.selected = Some(self.sel);
                return None;
            }
            KeyCode::Down => {
                self.sel = (self.sel + 1).min(OPTIONS.len() - 1);
                self.plan.selected = Some(self.sel);
                return None;
            }
            KeyCode::Char(c) => QUICK
                .iter()
                .find(|(k, _)| *k == c.to_ascii_lowercase())
                .map(|(_, i)| *i)
                .unwrap_or(DENY),
            KeyCode::Enter => self.sel,
            _ => DENY,
        };
        // *Always* is the REPL's to record: it answers `Accept` either way, so
        // the agent never learns which of the two accepting options was picked.
        if OPTIONS[picked].0 == "always" && !self.tool.is_empty() {
            self.approvals.approve(&self.tool);
        }
        let decision = if OPTIONS[picked].0 == "no" {
            ConfirmDecision::Deny
        } else {
            ConfirmDecision::Accept
        };
        // `take`, so a second key cannot answer the same request twice —
        // the session drops the dialog immediately, and `None` here is the
        // belt to that braces.
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(decision);
        }
        Some(decision)
    }

    /// The subject the decision line names (`tool BASH`).
    pub fn subject(&self) -> &str {
        &self.subject
    }
}

/// The line a resolved confirm leaves in the transcript: `accepted tool BASH`.
pub fn decision_line(subject: &str, decision: ConfirmDecision) -> String {
    let verb = match decision {
        ConfirmDecision::Accept => "accepted",
        ConfirmDecision::Deny => "denied",
    };
    format!("{verb} {subject}")
}

/// The probe's stack for one confirm paint: the standing head, with the
/// dialog appended below it while it is open.
///
/// A miniature of the shape `PromptSession::sync_stack` builds for the REPL —
/// its always-present first element (`Element::lines(vec![String::new()])`,
/// the blank row before the bar), then `divider + confirm.elements()` under
/// it. A probe has no `AppState` to build a real bar and input, but the
/// composition is the pinned part (doc/confirm_as_menu_261002.md §9): the
/// dialog is an overlay **below** the standing rows, never the whole area —
/// and the erase is the same operation on both sides: drop the dialog
/// element, apply this same stack again.
fn probe_stack(confirm: Option<&ConfirmPrompt>) -> Vec<crate::render::Element> {
    let mut stack = vec![crate::render::Element::lines(vec![String::new()])];
    if let Some(confirm) = confirm {
        let elements = confirm.elements();
        if !elements.is_empty() {
            stack.push(crate::render::Element::divider());
            stack.extend(elements);
        }
    }
    stack
}

/// Paint one confirmation through the frame pipeline: an overlay appended
/// **below** the probe's standing head, as the session paints it.
///
/// **Test/probe support.** Production composes from
/// `PromptSession::sync_stack`, which appends these elements after the
/// always-present head and any open overlay's — that append is pinned by a
/// session-level test, because it needs an `AppState` to build a session and
/// a probe cannot have one. What a probe *can* do is drive the real reducer
/// ([`ConfirmPrompt::handle_key`]) and the real bytes
/// ([`ConfirmPrompt::elements`] → the manager's `configure` + `render`);
/// the stack it composes is [`probe_stack`], shared with [`clear`], so the
/// paint and the erase cannot drift apart.
pub fn paint(
    confirm: &ConfirmPrompt,
    manager: &std::sync::Arc<std::sync::Mutex<crate::render::RenderManager>>,
) {
    let mut manager = manager.lock().unwrap();
    manager.configure(
        crate::render::FRAME_MAX_ROW_RATIO,
        probe_stack(Some(confirm)),
    );
    manager.render(None);
}

/// Repaint the probe's area **without** the dialog — the probe's half of
/// [`resolve`]'s erase.
///
/// The same operation the REPL runs on resolve (`sync_stack` +
/// `paint_prompt`): the dialog element is gone, the standing stack is applied
/// again through the front door, and the frame apply's union clear takes the
/// dialog's rows. Deliberately *not* an empty membership — an empty stack
/// means "nothing configured yet", not "clear the screen"; see
/// `RenderManager::render`'s two readings.
pub fn clear(manager: &std::sync::Arc<std::sync::Mutex<crate::render::RenderManager>>) {
    let mut manager = manager.lock().unwrap();
    manager.configure(crate::render::FRAME_MAX_ROW_RATIO, probe_stack(None));
    manager.render(None);
}

/// Resolve a confirmed dialog: **repaint without it, then record the decision**.
///
/// One function for both callers because the *order* is the invariant, not the
/// plumbing: dropping the element and applying the frame is what erases the
/// dialog's rows (there is no second clear — the frame apply's union clear
/// takes them), and only then does the decision line land where the dialog was.
/// A probe that reordered these would pin a different product.
///
/// `redraw` repaints the area without the dialog. The REPL passes the session's
/// own paint (it still owns the prompt and any menu underneath); the probe
/// passes [`clear`].
///
/// Takes content for the decision line: `display_plain` is a content verb —
/// boundary rules included — and the line stays inside this function because
/// the repaint-then-decide order is the invariant, not something each caller
/// may remember.
pub fn resolve(
    content: &crate::content::Content,
    redraw: impl FnOnce(),
    subject: &str,
    decision: ConfirmDecision,
) {
    redraw();
    content.display_plain(&decision_line(subject, decision));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_follows_the_tool_confirm_convention() {
        assert_eq!(confirm_subject("Confirm tool BASH call?"), "tool BASH");
        assert_eq!(confirm_subject("Confirm tool EDIT call?"), "tool EDIT");
    }

    #[test]
    fn subject_strips_convention_furniture_independently() {
        // Suffix only (the probe's message), prefix only, neither.
        assert_eq!(confirm_subject("tool BASH call?"), "tool BASH");
        assert_eq!(confirm_subject("Confirm tool BASH"), "tool BASH");
        assert_eq!(confirm_subject("something else"), "something else");
    }

    /// A dropped reply channel means nobody decided — which is the same
    /// situation as `Esc`, not the same as "yes". Pin the fail-safe: this is
    /// the difference between a cancelled turn and a tool that ran unapproved.
    #[tokio::test]
    async fn a_dropped_receiver_denies_rather_than_hanging() {
        let bridge = ConfirmBridge::new();
        let port = bridge.port.clone();
        drop(bridge.requests);
        assert_eq!(
            port.request("bash", "Confirm tool BASH call?".into()).await,
            ConfirmDecision::Deny
        );
    }

    /// `always` is recorded by the REPL and then reads back as a plain
    /// `Accept` — the agent cannot tell the two apart, which is the point.
    #[tokio::test]
    async fn an_approved_tool_answers_accept_without_asking() {
        let mut bridge = ConfirmBridge::new();
        let port = bridge.port.clone();
        port.approvals().approve("bash");
        // Nothing is reading `requests`, so if the port asked anyway this
        // would park on the oneshot forever rather than return Accept.
        assert_eq!(
            port.request("bash", "Confirm tool BASH call?".into()).await,
            ConfirmDecision::Accept
        );
        // Per-tool: approving bash says nothing about edit.
        assert!(!port.approvals().is_approved("edit"));
        bridge.requests.close();
    }

    /// What the REPL receives is the header plus the derived subject, so the
    /// decision line needs no knowledge of the convention.
    #[tokio::test]
    async fn a_request_carries_the_header_and_its_subject() {
        let mut bridge = ConfirmBridge::new();
        let port = bridge.port.clone();
        let ask = tokio::spawn({
            let port = port.clone();
            async move { port.request("bash", "Confirm tool BASH call?".into()).await }
        });
        let Some(req) = bridge.requests.recv().await else {
            panic!("the request should have arrived");
        };
        assert_eq!(req.header, "Confirm tool BASH call?");
        assert_eq!(req.subject, "tool BASH");
        // Answering is all it takes to release the caller.
        let _ = req.reply.send(ConfirmDecision::Accept);
        assert_eq!(ask.await.unwrap(), ConfirmDecision::Accept);
    }
}
