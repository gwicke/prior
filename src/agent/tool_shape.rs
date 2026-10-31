//! What each registered tool's call *is*, as the tool reads it.
//!
//! A call arrives as the model spelled it: an absolute path it copied from a
//! previous read, a `cd <cwd> && …` prefix it added to be safe, a `./` it did
//! not need. Every consumer of that call — the panel that labels it, the session
//! that stores it, the history that sends it back — wants the short form, and
//! each used to rewrite it independently. Three rewrites of one thing is three
//! chances to drift.
//!
//! So the rewrite is one function per tool, keyed by name, and it runs at two
//! moments:
//!
//! - **dispatch**, to label the call before the tool runs (a slow `bash` must be
//!   labelled with the same bytes that will be recorded, or the panel and the
//!   record disagree about a call that never changed);
//! - **part building**, where the args a [`crate::agent::parts::CallPart`]
//!   carries are exactly this function's output.
//!
//! It is pure and idempotent, which is what makes calling it twice harmless and
//! a loaded session (already in this form) a no-op.
//!
//! ## Why a table and not a method on the tool
//!
//! `#[rig_tool]` generates the tool struct, so it cannot implement a prior-side
//! trait; and `Guard<Bash>` has to forward to whatever it wraps. A table beside
//! the registry entries is where both facts fit, and it keeps the decision in
//! one file rather than in six.
//!
//! ## Why this is not `tool_args::normalize` again
//!
//! That matcher carried twelve names, eight of them registry-dead, because it
//! also had to rewrite *replayed* sessions. Replay needs no rewriting — the
//! stored form is already this function's output — so the dead arms are gone and
//! every entry here is a tool that exists. The live set is small enough to
//! enumerate exactly. See `doc/261030_rig_direct_drive_impl.md`.

use crate::tool_args::PathContext;

/// Rewrite one call's arguments into the form prior stores and sends.
///
/// `None` when this call is not one the table governs — an unknown name, or a
/// tool that rewrites nothing. The call then records exactly as the model
/// spelled it, which is the honest fallback: the alternative is a guess.
///
/// **Idempotent** for every entry, which the tests assert: a loaded session's
/// calls are fed back through this on their way out, and a second pass must not
/// shift them.
pub(crate) fn preprocess(
    name: &str,
    args: &serde_json::Value,
    ctx: &PathContext,
) -> Option<serde_json::Value> {
    let mut args = args.clone();
    let changed = match name {
        // `read` and `write` take one path; `sed` previews one; `grep` searches
        // one. Same rule, so same arm.
        "read" | "write" | "sed" | "grep" => relativize_field(&mut args, "path", ctx),
        // The shell gets a command the model wrapped in `cd <cwd> &&` because it
        // wanted to be sure of its footing; the cwd is already in the system
        // prompt and in the child's own working directory, so the prefix only
        // costs tokens and hides the real command from the panel.
        "bash" => match args.get("command").and_then(|c| c.as_str()) {
            Some(command) => match crate::tool_args::strip_cwd_prefix(command, ctx) {
                Some(stripped) => insert(&mut args, "command", stripped),
                None => false,
            },
            None => false,
        },
        // `edit` is absent by construction, not by omission: its args are a
        // block blob naming zero or more files, so it has no single shape until
        // it has split — which needs execution. Its parts carry the per-file
        // form instead, and it is the only tool that returns parts itself.
        _ => return None,
    };
    changed.then_some(args)
}

/// [`preprocess`] for one string field of `args`.
fn relativize_field(args: &mut serde_json::Value, key: &str, ctx: &PathContext) -> bool {
    let Some(current) = args.get(key).and_then(|v| v.as_str()) else {
        return false;
    };
    match crate::tool_args::relativize_arg(current, ctx) {
        Some(rewritten) => insert(args, key, rewritten),
        None => false,
    }
}

/// Set `key` to `value`, reporting whether it changed.
fn insert(args: &mut serde_json::Value, key: &str, value: String) -> bool {
    let changed = args.get(key).and_then(|v| v.as_str()) != Some(value.as_str());
    if changed {
        if let Some(obj) = args.as_object_mut() {
            obj.insert(key.to_string(), serde_json::Value::String(value));
        }
    }
    changed
}

/// Apply [`preprocess`] to a parsed call's arguments, leaving the original when
/// the table governs nothing.
pub(crate) fn preprocess_or_raw(
    name: &str,
    args: &serde_json::Value,
    ctx: &PathContext,
) -> serde_json::Value {
    preprocess(name, args, ctx).unwrap_or_else(|| args.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PathContext {
        PathContext::new("/home/u/project", Some("/home/u".into()))
    }

    fn cwd() -> String {
        ctx().cwd.display().to_string()
    }

    #[test]
    fn a_path_inside_cwd_is_relativized() {
        let args = serde_json::json!({ "path": format!("{}/src/x.rs", cwd()) });
        assert_eq!(
            preprocess("read", &args, &ctx()),
            Some(serde_json::json!({ "path": "src/x.rs" }))
        );
        // A path already relative needs no rewrite: `None` says "the record can
        // use the model's own bytes".
        assert_eq!(
            preprocess("read", &serde_json::json!({ "path": "src/x.rs" }), &ctx()),
            None
        );
    }

    #[test]
    fn the_bash_cwd_prefix_is_stripped() {
        let args = serde_json::json!({ "command": format!("cd {} && cargo test", cwd()) });
        assert_eq!(
            preprocess("bash", &args, &ctx()),
            Some(serde_json::json!({ "command": "cargo test" }))
        );
    }

    #[test]
    fn a_plain_command_is_untouched() {
        // The tool must run what the model meant, and a rewrite that changes
        // nothing must not re-encode the string.
        assert_eq!(
            preprocess(
                "bash",
                &serde_json::json!({ "command": "cargo test" }),
                &ctx()
            ),
            None
        );
    }

    #[test]
    fn an_unknown_or_unwritten_tool_is_not_governed() {
        assert_eq!(
            preprocess("nope", &serde_json::json!({ "path": "x" }), &ctx()),
            None
        );
        // `edit` deliberately has no entry: it has no single call shape until it
        // has split, which needs execution.
        assert_eq!(
            preprocess(
                "edit",
                &serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\ny\n@@@END" }),
                &ctx()
            ),
            None
        );
    }

    /// A loaded session's calls go back out through this. If a second pass
    /// changed anything, a session would drift further from its model on every
    /// request.
    #[test]
    fn preprocessing_is_idempotent_for_every_live_tool() {
        let cwd = cwd();
        let cases = [
            (
                "read",
                serde_json::json!({ "path": format!("{cwd}/a.rs"), "limit": 5 }),
            ),
            (
                "write",
                serde_json::json!({ "path": format!("{cwd}/a.rs"), "content": "x" }),
            ),
            (
                "sed",
                serde_json::json!({ "path": format!("{cwd}/a.rs"), "expressions": "s/a/b/" }),
            ),
            (
                "grep",
                serde_json::json!({ "path": format!("{cwd}/a.rs"), "pattern": "x" }),
            ),
            (
                "bash",
                serde_json::json!({ "command": format!("cd {cwd} && ls") }),
            ),
        ];
        for (name, raw) in cases {
            let once = preprocess_or_raw(name, &raw, &ctx());
            let twice = preprocess_or_raw(name, &once, &ctx());
            assert_eq!(once, twice, "{name} drifted on a second pass");
            assert_ne!(once, raw, "{name} should have been rewritten");
        }
    }

    /// Every live path-taking tool is in the table. A tool that gained a `path`
    /// argument without an entry would start storing absolute paths silently —
    /// this is the check that makes that a test failure rather than a
    /// review question.
    #[test]
    fn every_path_taking_tool_is_governed() {
        // `edit` is the one exemption, and for a structural reason: its args are
        // a block blob, and its per-file form is stated by its parts instead.
        let path_takers = ["read", "write", "sed", "grep"];
        for name in path_takers {
            let args = serde_json::json!({ "path": format!("{}/a.rs", cwd()) });
            assert!(
                preprocess(name, &args, &ctx()).is_some(),
                "{name} takes a path but is not in the shape table"
            );
        }
    }
}
