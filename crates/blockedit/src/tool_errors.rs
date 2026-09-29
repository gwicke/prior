//! Cleanup of the `edit` tool's argument-error text on its way to the model.
//!
//! # Why this exists
//!
//! The self-correcting message the `edit` tool builds for a malformed block (see
//! [`crate::wire::EditArgs::invalid`]) is the model's *only* signal about what it
//! got wrong, and it is written to be terse and line-based. Two wrappers sit on
//! top of it before the model sees it, both of which only add noise:
//!
//! - rig's arg-parse wrapper, `"failed to parse tool arguments: …"` — true, but
//!   restates what the tool name and the message already say;
//! - the tool's own `"Invalid edit tool arguments: …"` prefix — the message
//!   already names the block and the bad marker.
//!
//! A wall of ceremony around the one fact that matters (which marker is
//! malformed) measurably degrades a weak model's chance of self-correcting; it
//! often falls back to "something went wrong, let me try python". [`strip_arg_error_prefixes`]
//! removes exactly those two wrappers and leaves the useful text — the problem,
//! the `Expected:` block, and the `Actual:` structure — untouched.
//!
//! This is a *message* transform, not a control-flow one. The tool result's
//! kind, retryability, and raw structured form are all left alone; only the
//! model-facing string is rewritten (and the human-facing panel, via the same
//! function). Nothing here decides what is an error.

/// rig's arg-parse wrapper, added when `serde` fails to build the tool's args.
const RIG_ARG_PARSE_PREFIX: &str = "failed to parse tool arguments: ";

/// The `edit` tool's own prefix on a self-correcting argument error.
const EDIT_ARG_PREFIX: &str = "Invalid edit tool arguments: ";

/// Strip the redundant `"failed to parse tool arguments: "` and
/// `"Invalid edit tool arguments: "` wrappers from a tool error message, so the
/// model reads the problem, the expected shape, and the offending structure and
/// nothing else.
///
/// Only these two exact prefixes are removed, and only from the front. Any
/// other message (a real execution failure, a provider error) is returned
/// unchanged — this never swallows content, only two known decorations.
pub fn strip_arg_error_prefixes(msg: &str) -> &str {
    let mut out = msg.trim_start();
    // Order matters only for robustness; the two prefixes do not overlap in
    // practice, and looping lets a doubled wrapper (e.g. a future re-wrap)
    // still collapse to the useful text.
    loop {
        let before = out.len();
        if let Some(rest) = out.strip_prefix(RIG_ARG_PARSE_PREFIX) {
            out = rest.trim_start();
        }
        if let Some(rest) = out.strip_prefix(EDIT_ARG_PREFIX) {
            out = rest.trim_start();
        }
        if out.len() == before {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_both_known_prefixes() {
        let msg = format!("{RIG_ARG_PARSE_PREFIX}{EDIT_ARG_PREFIX}Invalid block: bad marker");
        assert_eq!(strip_arg_error_prefixes(&msg), "Invalid block: bad marker");
    }

    #[test]
    fn strips_each_prefix_on_its_own() {
        assert_eq!(
            strip_arg_error_prefixes("failed to parse tool arguments: boom"),
            "boom"
        );
        assert_eq!(
            strip_arg_error_prefixes("Invalid edit tool arguments: boom"),
            "boom"
        );
    }

    #[test]
    fn leaves_other_messages_untouched() {
        for msg in [
            "the tool failed",
            "Replaced 1 block(s) in src/x.rs",
            "no lines matched",
            // A message that merely *contains* a prefix, not at the front.
            "error: failed to parse tool arguments: was not ours to strip",
        ] {
            assert_eq!(strip_arg_error_prefixes(msg), msg, "{msg}");
        }
    }
}
