//! Mid-turn steering: the user's text, waiting for a boundary it can enter at.
//!
//! A steer is an ordinary user message — not an aside, not a `RequestPatch`, not
//! a context document. That is the whole design
//! (`doc/261030_rig_direct_drive.md` §5): a user message needs no marker to
//! survive compaction (`serialize_spur` keeps `role: user` text) and no second
//! place to remember it, because the session holds it.
//!
//! What it needs is a **boundary**. `AgentRun` takes its last message as the
//! prompt and its history must be a valid provider prefix, so a user message can
//! only enter after the last tool result of a batch — that is the next
//! `CallModel`, and a steer typed mid-batch is therefore delivered *with* the
//! results rather than between them. Entering at a boundary means rebuilding the
//! run around the session ([`DriveLoop::rebuild_from_session`]), because rig's
//! committed log cannot be appended to (no public append; `streamed_turn`
//! requires a pending attempt and refuses a turn the policy does not allow).
//!
//! ## A slot, not a queue
//!
//! A steer is editable until it is sent: a second submit mid-turn **replaces**
//! the scheduled one rather than queueing behind it, so a mistyped steer costs
//! one edit rather than a stray message in the transcript. Consuming the slot
//! clears it and the message stays in history like any other, so a consecutive
//! submit schedules a *new* steer — there is no merge guard to get wrong, unlike
//! the synthetic continuation, which is written once per incomplete turn because
//! the session is append-only.
//!
//! The slot lives on [`CodeAgent`](crate::agent::CodeAgent), not on the drive
//! loop: `build_drive` runs *inside* `prompt()`, so a slot there would be
//! invisible to the REPL — and the REPL's key pump is what fills it.

use std::sync::{Arc, Mutex};

use crate::turn_overrides::TurnOverrides;

/// One pending steer: the text, and the overrides that came with it.
///
/// The override lines are split off here rather than at the boundary because
/// that is where the user typed them: a `/reasoning` trailed on a steering
/// message applies to the rest of *that* user turn, which is exactly
/// [`TurnOverrides`]' case (`src/turn_overrides.rs`). A menu commit is the other
/// lifetime — it writes the live params, so it survives the turn — and the two
/// meet in the same place, when the next request is composed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Steer {
    /// The text as the user typed it, override lines and all.
    ///
    /// Kept because a steer can fail to land: typed after the turn's last
    /// boundary, there is no request left to carry it, and the honest thing is
    /// to hand the text back to the input row ([`SteerSlot::take`]'s caller in
    /// `turn.rs`) rather than silently swallow it. Re-rendering the split form
    /// would be a reconstruction, and one free to differ.
    pub(crate) raw: String,
    /// The prompt text, with the override lines removed — what the model reads.
    pub(crate) text: String,
    pub(crate) overrides: TurnOverrides,
}

/// The input buffer a steer was scheduled from, so delivery can tell whether
/// the row still holds *that* text.
///
/// Scheduling must not clear the row: a steer is editable until it is sent, and
/// clearing on submit makes a typo unrecoverable. So the row keeps the text and
/// the delivery is what empties it — and only when it still matches, because
/// anything typed since is the user editing this steer (or starting the next
/// message) and must not be thrown away.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scheduled {
    pub(crate) steer: Steer,
    /// The editor's full text at schedule time, override lines included.
    pub(crate) typed: String,
}

/// The steer waiting to be delivered, if any.
///
/// Shared between the key pump (writer), the agent (owner) and the drive loop
/// (reader). A `std::sync::Mutex` rather than tokio's: every use is a brief
/// non-async section, and holding a lock across an await to schedule a message
/// would be strictly worse.
#[derive(Clone, Default)]
pub(crate) struct SteerSlot(Arc<Mutex<Option<Scheduled>>>);

impl SteerSlot {
    /// Schedule `steer`, replacing any that has not been sent yet.
    ///
    /// `typed` is the input row's full text at this moment. Replacing is the
    /// point (see the module docs), and it is why `typed` travels with the
    /// steer: the pending one's `typed` must be the text the row holds *now*,
    /// or delivery would compare against a superseded buffer.
    pub(crate) fn schedule(&self, steer: Steer, typed: String) {
        *self.lock() = Some(Scheduled { steer, typed });
    }

    /// Take the pending steer, leaving the slot empty.
    ///
    /// Taken, not read: the text becomes one message in the session exactly
    /// once. A cloned read would let two rebuilds deliver the same words.
    pub(crate) fn take(&self) -> Option<Scheduled> {
        self.lock().take()
    }

    /// Clear any pending steer without delivering it.
    ///
    /// Returns the cleared steer if there was one.
    pub(crate) fn clear(&self) -> Option<Scheduled> {
        self.lock().take()
    }

    /// Check if a steer is currently pending without consuming it.
    pub(crate) fn is_pending(&self) -> bool {
        self.lock().is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Scheduled>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steer(text: &str) -> Steer {
        Steer {
            raw: text.to_string(),
            text: text.to_string(),
            overrides: TurnOverrides::default(),
        }
    }

    /// A second submit replaces the first: the slot is a slot, not a queue.
    /// The replacement carries the row's *current* text, which is what delivery
    /// compares against.
    #[test]
    fn scheduling_twice_keeps_only_the_last() {
        let slot = SteerSlot::default();
        slot.schedule(steer("first"), "first".into());
        slot.schedule(steer("second"), "second".into());
        let taken = slot.take().expect("a pending steer");
        assert_eq!(taken.steer, steer("second"));
        assert_eq!(taken.typed, "second");
    }

    /// Delivered once. A rebuild happens per boundary, and a steer that
    /// survived its own delivery would be appended again on the next.
    #[test]
    fn taking_empties_the_slot() {
        let slot = SteerSlot::default();
        slot.schedule(steer("steer"), "steer".into());
        assert!(slot.take().is_some());
        assert!(slot.take().is_none());
    }
}
