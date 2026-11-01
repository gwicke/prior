//! What the turn is doing, as one value the status bar reads.
//!
//! The bar's dot is the only thing that shows this today, but the fact is
//! wider than "a request is on the wire": a retried attempt waits out a
//! back-off while the turn is still in flight, and that is a different state
//! from streaming even though both mean "the turn is running".
//!
//! It lives in `AppState` because it is turn state, not content state — the
//! same shelf as the model, the reasoning level and the session identity, all
//! of which the bar reads at render. Content writes text; it does not decide
//! what the bar says about the turn.
//!
//! One enum rather than two booleans: the states are exclusive by
//! construction (an attempt cannot be streaming and waiting at once), and a
//! caller that could set both would have to decide what a contradictory pair
//! means.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// The turn's phase, as the bar paints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    /// No turn in flight: the REPL is between prompts.
    Idle,
    /// A request is on the wire and its items are arriving.
    Streaming,
    /// An attempt failed and the turn is waiting out the retry delay before
    /// re-issuing it. The turn is **not** over.
    WaitingRetry,
}

impl TurnPhase {
    /// Whether a turn is in flight. The input row asks this and not which
    /// phase it is: an empty buffer mid-turn claims no row, whether the turn
    /// is streaming or waiting to retry.
    pub fn is_running(self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Streaming => 1,
            Self::WaitingRetry => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Streaming,
            2 => Self::WaitingRetry,
            // Unreachable in practice — the only writer is `set`, from this
            // enum — but the fallback is still the conservative direction:
            // claiming work that is not happening is worse than a missing
            // frame.
            _ => Self::Idle,
        }
    }
}

/// A shared [`TurnPhase`]: written by the turn, read at render by the bar and
/// queried by the input row.
///
/// An `AtomicU8` rather than a cell in `AppState`'s state mutex: it moves on
/// every attempt boundary and is read on every paint, and it is one plain
/// value with no invariant spanning two fields, so there is nothing for a lock
/// to protect — only contention to add to the menu's.
#[derive(Debug, Clone)]
pub struct TurnPhaseHandle(Arc<AtomicU8>);

impl Default for TurnPhaseHandle {
    fn default() -> Self {
        Self::new(TurnPhase::Idle)
    }
}

impl TurnPhaseHandle {
    pub fn new(phase: TurnPhase) -> Self {
        Self(Arc::new(AtomicU8::new(phase.code())))
    }

    pub fn get(&self) -> TurnPhase {
        TurnPhase::from_code(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, phase: TurnPhase) {
        self.0.store(phase.code(), Ordering::Relaxed);
    }

    /// Whether a turn is in flight — the input row's question.
    pub fn is_running(&self) -> bool {
        self.get().is_running()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_idle_is_not_running() {
        assert!(!TurnPhase::Idle.is_running());
        assert!(TurnPhase::Streaming.is_running());
        assert!(
            TurnPhase::WaitingRetry.is_running(),
            "a back-off is the turn still working, so the input row stays hidden"
        );
    }

    #[test]
    fn the_handle_round_trips_every_phase() {
        let handle = TurnPhaseHandle::default();
        assert_eq!(handle.get(), TurnPhase::Idle);
        for phase in [
            TurnPhase::Streaming,
            TurnPhase::WaitingRetry,
            TurnPhase::Idle,
        ] {
            handle.set(phase);
            assert_eq!(handle.get(), phase);
            assert_eq!(handle.is_running(), phase.is_running());
        }
    }

    #[test]
    fn an_unknown_code_reads_as_idle() {
        assert_eq!(TurnPhase::from_code(9), TurnPhase::Idle);
    }
}
