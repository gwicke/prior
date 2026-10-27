//! Rewrites applied to tool arguments at every point that observes them.
//!
//! Two rewrites, one pass, one [`PathContext`]:
//!
//! 1. **`bash`**: strip a redundant `cd <cwd> && …` prefix. Models routinely
//!    open every shell command with `cd /full/path/to/cwd && …` even though the
//!    sandboxed shell already starts in that directory. The prefix costs
//!    tokens, adds noise to every result, and — because it re-appears in the
//!    history — teaches the next turn to do it again.
//! 2. **Path arguments**: shorten an absolute or `~`-expanded path that lies
//!    under the working directory to its sub-path (`src/tools.rs`, no `./`).
//!    A path outside the cwd keeps the model's spelling. See
//!    `doc/261026_tool_path_relativization.md`.
//!
//! Applied at every point that observes a call:
//!
//! - execution ([`crate::tools::Bash`]), so the shell runs what the model meant;
//! - the transcript (`crate::agent::transcript::prepare`), so the persisted call
//!   is the compact one;
//! - session replay (`crate::session::storage`), so an old call reads the same
//!   way after a reload;
//! - the model-facing history
//!   (`crate::agent::build::LoggingHook::on_completion_call`), so the next turn
//!   sees the same spelling.
//!
//! Because every point uses this same code on the same [`PathContext`], the
//! views cannot disagree. Every rewrite is idempotent, which is what makes it
//! safe to run at each of them.

use std::path::{Path, PathBuf};

use rig_core::message::{AssistantContent, Message, ToolFunction};

/// The working directory and home directory that path arguments are resolved
/// against: the **sandbox** view, not the host's.
///
/// Today the two coincide (no cwd remap, `HOME` passed through), so this is
/// identity with reading `current_dir`/`HOME` directly. The point is that
/// there is exactly one constructor to change when they stop coinciding, and
/// every egress plus the tools follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathContext {
    /// The absolute sandbox cwd: `relativize_path`'s base.
    pub cwd: PathBuf,
    /// The sandbox `HOME` (`None` when the sandbox runs without one, in which
    /// case `~` is left literal, as a shell would).
    pub home: Option<PathBuf>,
}

impl PathContext {
    /// The one constructor: reads the sandbox view, not the host's.
    pub(crate) fn sandbox() -> Self {
        let host_cwd = std::env::current_dir().unwrap_or_default();
        let cwd = match crate::tools::current_path_policy() {
            Some(policy) => policy.sandbox_cwd(&host_cwd),
            None => host_cwd,
        };
        let home = match crate::tools::sandbox_config().as_ref().as_ref() {
            Some(cfg) => crate::config::sandbox_home(cfg),
            None => std::env::var_os("HOME").map(PathBuf::from),
        };
        Self { cwd, home }
    }

    /// An explicit context, for tests.
    #[cfg(test)]
    pub(crate) fn new(cwd: impl Into<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            home,
        }
    }
}

/// The relativized spelling of a path argument, or `None` when there is
/// nothing to shorten (so the caller leaves the exact bytes untouched).
///
/// - empty, or already relative (including `../x`, a non-goal) → `None`;
/// - `~`/`~/x` → expanded with [`PathContext::home`] (no home → `None`);
/// - absolute → used as is;
/// - the resolved path is then relativized against [`PathContext::cwd`]; when
///   that leaves it absolute (not under cwd) → `None`, keeping the model's
///   spelling (`~/notes.txt` stays `~/notes.txt`).
pub(crate) fn relativize_arg(arg: &str, ctx: &PathContext) -> Option<String> {
    if arg.is_empty() {
        return None;
    }
    let resolved = if arg == "~" || arg.starts_with("~/") {
        let home = ctx.home.as_ref()?;
        if arg == "~" {
            home.clone()
        } else {
            home.join(&arg[2..])
        }
    } else if arg.starts_with('/') {
        PathBuf::from(arg)
    } else {
        return None;
    };
    let resolved = crate::path_util::normalize(&resolved);
    // `relativize_path` gives an absolute string back for a path outside `cwd`
    // (and `.` for `cwd` itself); only a relative answer is worth keeping.
    let rel = crate::path_util::relativize_path(&resolved, &ctx.cwd);
    (!rel.starts_with('/')).then_some(rel)
}

/// Rewrite the path-carrying arguments of one tool call in place. Returns
/// `true` when something changed, so callers can skip rebuilding the call.
///
/// Idempotent: a second pass changes nothing.
pub(crate) fn normalize(name: &str, args: &mut serde_json::Value, ctx: &PathContext) -> bool {
    match name.to_lowercase().as_str() {
        "bash" => match args.get("command").and_then(|c| c.as_str()) {
            Some(command) => match strip_cwd_prefix(command, ctx) {
                Some(rewritten) => insert(args, "command", rewritten),
                None => false,
            },
            None => false,
        },
        // `path`-carrying tools, canonical names plus the retired spellings a
        // replayed session may still hold.
        "read" | "write" | "sed" | "grep" | "ls" | "list_dir" | "find" | "glob_files" | "mkdir"
        | "make_dir" | "rm" | "remove" => relativize_field(args, "path", ctx),
        "mv" | "cp" | "move_file" | "copy_file" => {
            let src = relativize_field(args, "src", ctx);
            let dst = relativize_field(args, "dst", ctx);
            src || dst
        }
        // `edit` is fully parsed downstream (`prepare_edit_call`), which
        // relativizes `FileEdit::path` itself.
        _ => false,
    }
}

/// [`normalize`] for one string field of `args`.
fn relativize_field(args: &mut serde_json::Value, key: &str, ctx: &PathContext) -> bool {
    let Some(current) = args.get(key).and_then(|v| v.as_str()) else {
        return false;
    };
    match relativize_arg(current, ctx) {
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

/// [`normalize`] for every tool call in one message. Non-assistant messages and
/// non-path arguments are untouched.
pub(crate) fn normalize_in_message(msg: &mut Message, ctx: &PathContext) {
    let Message::Assistant(turn) = msg else {
        return;
    };
    for content in &mut turn.content {
        let AssistantContent::ToolCall(tc) = content else {
            continue;
        };
        let mut args = tc.function.arguments_value();
        if normalize(tc.function.name.as_str(), &mut args, ctx) {
            // Rebuild the function in place so `native` (the provider item)
            // survives: only the arguments changed.
            tc.function = ToolFunction::new(tc.function.name.clone(), args);
        }
    }
}

/// [`normalize_in_message`] for a slice.
pub(crate) fn normalize_messages(msgs: &mut [Message], ctx: &PathContext) {
    for msg in msgs {
        normalize_in_message(msg, ctx);
    }
}

/// The command with every leading `cd <cwd> &&` / `cd <cwd>;` removed, or
/// `None` when there is nothing to strip (so callers can leave the original
/// value — and its exact bytes — untouched).
///
/// Idempotent: a stripped command has no such prefix left to remove.
pub(crate) fn strip_cwd_prefix(command: &str, ctx: &PathContext) -> Option<String> {
    let cwd = crate::path_util::normalize(&ctx.cwd);
    let mut rest = command;
    let mut changed = false;
    while let Some(next) = strip_one(rest, &cwd, ctx.home.as_deref()) {
        rest = next;
        changed = true;
    }
    if !changed {
        return None;
    }
    // `cd <cwd> &&` with nothing after it is a shell syntax error today; keep
    // the original so the model still sees the failure it wrote.
    if rest.trim().is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Remove one `cd <cwd> &&` / `cd <cwd>;` prefix, returning the rest.
fn strip_one<'a>(command: &'a str, cwd: &Path, home: Option<&Path>) -> Option<&'a str> {
    // `cd` must be a word of its own (`cdrom` is not a `cd`).
    let after = command.trim_start().strip_prefix("cd")?;
    let after = after.strip_prefix([' ', '\t'])?.trim_start();
    let (target, rest) = split_word(after)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("&&").or_else(|| rest.strip_prefix(';'))?;
    let rest = rest.trim_start();
    if rest.is_empty() || !is_cwd(target, cwd, home) {
        return None;
    }
    Some(rest)
}

/// Split the first shell word off `s`: the word and the remaining text, which
/// still begins at the delimiter. Single- and double-quoted words are unwrapped.
fn split_word(s: &str) -> Option<(&str, &str)> {
    let first = s.chars().next()?;
    if first == '\'' || first == '"' {
        let body = &s[1..];
        let end = body.find(first)?;
        return Some((&body[..end], &body[end + first.len_utf8()..]));
    }
    let end = s
        .find(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>'))
        .unwrap_or(s.len());
    (end > 0).then(|| (&s[..end], &s[end..]))
}

/// Whether shell path `target` names `cwd`. `~` expands against the context's
/// home (the value the model was shown in its working-directory line) and a
/// relative target is joined to `cwd`, so `cd .`, `cd ..//cwd` and
/// `cd /abs/cwd/` all match.
fn is_cwd(target: &str, cwd: &Path, home: Option<&Path>) -> bool {
    let resolved: PathBuf = crate::path_util::resolve_with_home(target, cwd, home);
    crate::path_util::normalize(&resolved) == cwd
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::{CallId, Message};

    fn assistant(calls: Vec<rig_core::message::ToolCall>) -> Message {
        crate::agent::transcript::assistant_message(
            Some("resp".into()),
            calls.into_iter().map(AssistantContent::ToolCall).collect(),
        )
    }

    fn bash_call(id: &str, args: serde_json::Value) -> rig_core::message::ToolCall {
        call(id, "bash", args)
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> rig_core::message::ToolCall {
        rig_core::message::ToolCall::new(
            CallId::from_wire(id),
            ToolFunction::new(crate::agent::transcript::tool_name(name), args),
        )
    }

    #[test]
    fn strips_absolute_cwd_prefix() {
        assert_eq!(
            strip("cd /home/u/project && cargo test").as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn strips_quoted_and_semicolon_forms() {
        assert_eq!(strip("cd \"/home/u/project\" && ls").as_deref(), Some("ls"));
        assert_eq!(
            strip("cd '/home/u/project/'; rg foo").as_deref(),
            Some("rg foo")
        );
        // Trailing slash, `..` and `.` normalize to the same directory.
        assert_eq!(strip("cd /home/u/project/ && ls").as_deref(), Some("ls"));
        assert_eq!(
            strip("cd /home/u/project/sub/.. && ls").as_deref(),
            Some("ls")
        );
        assert_eq!(strip("cd . && ls").as_deref(), Some("ls"));
        // `~` resolves against the sandbox home, which is also the cwd's parent.
        assert_eq!(strip("cd ~/project && ls").as_deref(), Some("ls"));
    }

    #[test]
    fn strips_repeated_and_multiline_prefixes() {
        assert_eq!(
            strip("cd /home/u/project && cd /home/u/project && ls").as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip("cd /home/u/project &&\n\tls -la").as_deref(),
            Some("ls -la")
        );
    }

    #[test]
    fn leaves_other_commands_alone() {
        // Another directory is a real `cd`, not a no-op.
        assert_eq!(strip("cd /tmp && ls"), None);
        // No separator, no prefix.
        assert_eq!(strip("cd /home/u/project ls"), None);
        // Not a `cd` word.
        assert_eq!(strip("cdrom /home/u/project && ls"), None);
        // Bare command.
        assert_eq!(strip("ls -la"), None);
        // Stripping would leave nothing to run.
        assert_eq!(strip("cd /home/u/project &&"), None);
    }

    #[test]
    fn is_idempotent() {
        let once = strip("cd /home/u/project && ls").unwrap();
        assert_eq!(strip(&once), None);
    }

    fn ctx() -> PathContext {
        PathContext::new("/home/u/project", Some("/home/u".into()))
    }

    /// `strip_cwd_prefix` against the fixture cwd.
    fn strip(command: &str) -> Option<String> {
        strip_cwd_prefix(command, &ctx())
    }

    /// [`normalize_messages`] on one owned message list, the shape tests build.
    fn rewrite_messages(mut msgs: Vec<Message>, ctx: &PathContext) -> Vec<Message> {
        normalize_messages(&mut msgs, ctx);
        msgs
    }

    /// The arguments of the first call in a message, as JSON text.
    fn first_args(msg: &Message) -> String {
        crate::agent::transcript::assistant_content(msg)
            .iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.function.arguments_value().to_string()),
                _ => None,
            })
            .expect("a call")
    }

    fn path_call(id: &str, name: &str, path: &str) -> rig_core::message::ToolCall {
        call(id, name, serde_json::json!({ "path": path }))
    }

    #[test]
    fn rewrites_bash_calls_and_keeps_other_args() {
        let cf = ctx();
        let msg = assistant(vec![
            bash_call(
                "a",
                serde_json::json!({"command": "cd /home/u/project && ls", "timeout": 5}),
            ),
            bash_call("b", serde_json::json!({"command": "cd /tmp && ls"})),
        ]);
        let out = rewrite_messages(vec![msg], &cf);
        let calls: Vec<String> = crate::agent::transcript::assistant_content(&out[0])
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.function.arguments_value().to_string()),
                _ => None,
            })
            .collect();
        assert!(calls[0].contains(r#""command":"ls""#), "{calls:?}");
        assert!(calls[0].contains(r#""timeout":5"#), "{calls:?}");
        assert!(
            calls[1].contains(r#""command":"cd /tmp && ls""#),
            "{calls:?}"
        );
    }

    #[test]
    fn relativizes_each_path_tool() {
        let cf = ctx();
        let cwd_path = "/home/u/project/src/x.rs";
        for name in [
            "read",
            "write",
            "sed",
            "grep",
            "ls",
            "find",
            "mkdir",
            "rm",
            "list_dir",
            "glob_files",
            "make_dir",
            "remove",
        ] {
            let msg = assistant(vec![path_call("a", name, cwd_path)]);
            let out = rewrite_messages(vec![msg], &cf);
            assert_eq!(first_args(&out[0]), r#"{"path":"src/x.rs"}"#, "tool {name}");
        }
    }

    #[test]
    fn relativizes_both_sides_of_move_and_copy() {
        let cf = ctx();
        for name in ["mv", "cp", "move_file", "copy_file"] {
            let msg = assistant(vec![call(
                "a",
                name,
                serde_json::json!({
                    "src": "/home/u/project/a.rs",
                    "dst": "/home/u/project/src/b.rs",
                }),
            )]);
            let out = rewrite_messages(vec![msg], &cf);
            assert_eq!(
                first_args(&out[0]),
                r#"{"src":"a.rs","dst":"src/b.rs"}"#,
                "tool {name}"
            );
        }
    }

    #[test]
    fn leaves_a_path_outside_cwd_alone() {
        let cf = ctx();
        // Sibling of cwd under the same home: still absolute, untouched.
        assert_eq!(relativize_arg("/home/u/other/x.rs", &cf), None);
        // Under the home but not the cwd: `~` spelling is kept.
        assert_eq!(relativize_arg("~/notes.txt", &cf), None);
        assert_eq!(relativize_arg("/home/u/notes.txt", &cf), None);
        // Already relative, empty, and `..` are all non-goals.
        assert_eq!(relativize_arg("src/x.rs", &cf), None);
        assert_eq!(relativize_arg("./src/x.rs", &cf), None);
        assert_eq!(relativize_arg("../x.rs", &cf), None);
        assert_eq!(relativize_arg("", &cf), None);
        // `~` with no known home is left alone rather than guessed.
        let no_home = PathContext::new("/home/u/project", None);
        assert_eq!(relativize_arg("~/src/x.rs", &no_home), None);
    }

    #[test]
    fn expands_tilde_under_cwd_and_keeps_the_original_otherwise() {
        let cf = ctx();
        assert_eq!(
            relativize_arg("~/project/src/x.rs", &cf).as_deref(),
            Some("src/x.rs")
        );
        // `~` alone is the home, which is not under cwd.
        assert_eq!(relativize_arg("~", &cf), None);
        // A cwd whose child would read as a flag keeps the `./`.
        let flagged = PathContext::new("/home/u/project", Some("/home/u".into()));
        assert_eq!(
            relativize_arg("/home/u/project/-x.txt", &flagged).as_deref(),
            Some("./-x.txt")
        );
    }

    #[test]
    fn normalize_is_idempotent_and_reports_change() {
        let cf = ctx();
        let mut args = serde_json::json!({
            "path": "/home/u/project/src/x.rs",
            "other": "unchanged",
        });
        assert!(normalize("read", &mut args, &cf));
        assert_eq!(args["path"], "src/x.rs");
        // A second pass is a byte-level no-op and reports no change, which is
        // what makes it safe to run at every egress.
        let once = args.clone();
        assert!(!normalize("read", &mut args, &cf));
        assert_eq!(args, once);
        // A non-path tool is untouched.
        let mut bash = serde_json::json!({"command": "ls"});
        assert!(!normalize("bash", &mut bash, &cf));
        assert_eq!(bash, serde_json::json!({"command": "ls"}));
    }

    #[test]
    fn non_assistant_messages_pass_through() {
        let cf = ctx();
        let msg = Message::user("see /home/u/project/src/x.rs");
        let out = rewrite_messages(vec![msg], &cf);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Message::User { .. }));
    }
}
