//! The synthetic continuation a rebuilt retry ends on.
//!
//! A completion that fails mid-stream leaves text the user already saw. That
//! text is written to the session as an ordinary assistant message and the turn
//! is rebuilt from the session
//! (`doc/261102_partial_turn_retention.md` §3), which needs one more message at
//! the end: the prompt that asks the model to carry on. `AgentRun` always takes
//! its last message as its prompt (`call_model_step`'s `split_last`), so a
//! continuation is what makes `[user] [assistant: partial] [user: continue]`
//! reach the wire.
//!
//! Two things make it more than a `"continue"`:
//!
//! - It is written as a pi `custom_message` with [`CUSTOM_TYPE`] rather than as
//!   a plain user message, so it is identifiable by entry type on disk — never
//!   by matching its text — and cannot be confused with a genuine user steer.
//! - It is written *once per incomplete turn*, not once per attempt: the
//!   session is append-only, so a second one could not be removed. See
//!   [`Session::ensure_custom_message`](crate::session::Session::ensure_custom_message).

/// The `customType` prior writes for a synthetic continuation.
///
/// pi's `custom_message` entry type is the one that "does participate in LLM
/// context, rendered as user message" — context, distinguishable structurally.
pub(crate) const CUSTOM_TYPE: &str = "prior_continue";

/// The continuation's text.
///
/// It must not be empty: an empty (or whitespace-only) user message passes
/// request validation but the encoder drops it, which silently turns the
/// request into a *prefill* — a shape most providers refuse (§4 of the
/// retention doc). Nor should it restate the corrective feedback, which already
/// rides `extra_context` via the retry hint.
pub(crate) const TEXT: &str = "continue";
