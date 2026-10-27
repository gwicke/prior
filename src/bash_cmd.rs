//! Strip a redundant `cd <cwd> && …` prefix off `bash` commands.
//!
//! Models routinely open every shell command with `cd /full/path/to/cwd && …`
//! even though the sandboxed shell already starts in that directory. The prefix
//! costs tokens, adds noise to every result, and — because it re-appears in the
//! history — teaches the next turn to do it again.
//!
//! One pure rewrite, applied at every point that observes a `bash` call:
//!
//! - execution ([`crate::tools::Bash`]), so the shell runs what the model meant;
//! - the transcript (`crate::agent::transcript::prepare`), so the persisted call
//!   is the compact one;
//! - session replay (`crate::session::storage`), so an old call reads the same
//!   way after a reload;
//! - the model-facing history
//!   (`crate::agent::build::LoggingHook::on_completion_call`), so the next turn
//!   sees the call without the prefix.
//!
//! Because every point uses this same function on the same process working
//! directory, the four views cannot disagree.

use std::path::{Path, PathBuf};

use rig_core::message::{AssistantContent, Message, ToolFunction};

/// The command with every leading `cd <cwd> &&` / `cd <cwd>;` removed, or
/// `None` when there is nothing to strip (so callers can leave the original
/// value — and its exact bytes — untouched).
///
/// Idempotent: a stripped command has no such prefix left to remove.
pub fn strip_cwd_prefix(command: &str, cwd: &Path) -> Option<String> {
    let cwd = crate::path_util::normalize(cwd);
    let mut rest = command;
    let mut changed = false;
    while let Some(next) = strip_one(rest, &cwd) {
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

/// [`strip_cwd_prefix`] for one whole message list, resolving the working
/// directory from the process. A message list is returned unchanged when the
/// working directory cannot be read.
pub fn rewrite_bash_calls(messages: Vec<Message>) -> Vec<Message> {
    match std::env::current_dir() {
        Ok(cwd) => rewrite_bash_calls_in(messages, &cwd),
        Err(_) => messages,
    }
}

/// [`rewrite_bash_calls`] against an explicit working directory (for tests and
/// callers that already hold it).
pub fn rewrite_bash_calls_in(messages: Vec<Message>, cwd: &Path) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant(mut turn) => {
                let content = std::mem::take(&mut turn.content)
                    .into_iter()
                    .map(|c| rewrite_content(c, cwd))
                    .collect();
                Message::Assistant(turn.with_content(content))
            }
            other => other,
        })
        .collect()
}

/// [`rewrite_bash_calls`] for a single message.
pub(crate) fn rewrite_in_message(msg: Message, cwd: &Path) -> Message {
    rewrite_bash_calls_in(vec![msg], cwd)
        .pop()
        .expect("one message in, one out")
}

/// Rewrite one content block: a `bash` call whose `command` begins with the
/// redundant `cd` gets the trimmed command; everything else is returned as is.
fn rewrite_content(content: AssistantContent, cwd: &Path) -> AssistantContent {
    let AssistantContent::ToolCall(tc) = content else {
        return content;
    };
    if tc.function.name != "bash" {
        return AssistantContent::ToolCall(tc);
    }
    let mut args = tc.function.arguments_value();
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let Some(rewritten) = command.as_deref().and_then(|c| strip_cwd_prefix(c, cwd)) else {
        return AssistantContent::ToolCall(tc);
    };
    if let Some(obj) = args.as_object_mut() {
        obj.insert("command".into(), serde_json::Value::String(rewritten));
    }
    let mut tc = tc;
    tc.function = ToolFunction::new(tc.function.name.clone(), args);
    AssistantContent::ToolCall(tc)
}

/// Remove one `cd <cwd> &&` / `cd <cwd>;` prefix, returning the rest.
fn strip_one<'a>(command: &'a str, cwd: &Path) -> Option<&'a str> {
    // `cd` must be a word of its own (`cdrom` is not a `cd`).
    let after = command.trim_start().strip_prefix("cd")?;
    let after = after.strip_prefix([' ', '\t'])?.trim_start();
    let (target, rest) = split_word(after)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("&&").or_else(|| rest.strip_prefix(';'))?;
    let rest = rest.trim_start();
    if rest.is_empty() || !is_cwd(target, cwd) {
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

/// Whether shell path `target` names `cwd`. `~` expands against the host
/// `HOME` (the value the model was shown in its working-directory line) and a
/// relative target is joined to `cwd`, so `cd .`, `cd ..//cwd` and
/// `cd /abs/cwd/` all match.
fn is_cwd(target: &str, cwd: &Path) -> bool {
    let resolved: PathBuf = crate::path_util::resolve_with_home(target, cwd, None);
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
        rig_core::message::ToolCall::new(
            CallId::from_wire(id),
            ToolFunction::new(crate::agent::transcript::tool_name("bash"), args),
        )
    }

    #[test]
    fn strips_absolute_cwd_prefix() {
        let cwd = Path::new("/home/u/project");
        assert_eq!(
            strip_cwd_prefix("cd /home/u/project && cargo test", cwd).as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn strips_quoted_and_semicolon_forms() {
        let cwd = Path::new("/home/u/project");
        assert_eq!(
            strip_cwd_prefix("cd \"/home/u/project\" && ls", cwd).as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip_cwd_prefix("cd '/home/u/project/'; rg foo", cwd).as_deref(),
            Some("rg foo")
        );
        // Trailing slash, `..` and `.` normalize to the same directory.
        assert_eq!(
            strip_cwd_prefix("cd /home/u/project/ && ls", cwd).as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip_cwd_prefix("cd /home/u/project/sub/.. && ls", cwd).as_deref(),
            Some("ls")
        );
        assert_eq!(strip_cwd_prefix("cd . && ls", cwd).as_deref(), Some("ls"));
    }

    #[test]
    fn strips_repeated_and_multiline_prefixes() {
        let cwd = Path::new("/home/u/project");
        assert_eq!(
            strip_cwd_prefix("cd /home/u/project && cd /home/u/project && ls", cwd).as_deref(),
            Some("ls")
        );
        assert_eq!(
            strip_cwd_prefix("cd /home/u/project &&\n\tls -la", cwd).as_deref(),
            Some("ls -la")
        );
    }

    #[test]
    fn leaves_other_commands_alone() {
        let cwd = Path::new("/home/u/project");
        // Another directory is a real `cd`, not a no-op.
        assert_eq!(strip_cwd_prefix("cd /tmp && ls", cwd), None);
        // No separator, no prefix.
        assert_eq!(strip_cwd_prefix("cd /home/u/project ls", cwd), None);
        // Not a `cd` word.
        assert_eq!(strip_cwd_prefix("cdrom /home/u/project && ls", cwd), None);
        // Bare command.
        assert_eq!(strip_cwd_prefix("ls -la", cwd), None);
        // Stripping would leave nothing to run.
        assert_eq!(strip_cwd_prefix("cd /home/u/project &&", cwd), None);
    }

    #[test]
    fn is_idempotent() {
        let cwd = Path::new("/home/u/project");
        let once = strip_cwd_prefix("cd /home/u/project && ls", cwd).unwrap();
        assert_eq!(strip_cwd_prefix(&once, cwd), None);
    }

    #[test]
    fn rewrites_bash_calls_and_keeps_other_args() {
        let cwd = Path::new("/home/u/project");
        let msg = assistant(vec![
            bash_call(
                "a",
                serde_json::json!({"command": "cd /home/u/project && ls", "timeout": 5}),
            ),
            bash_call("b", serde_json::json!({"command": "cd /tmp && ls"})),
        ]);
        let out = rewrite_bash_calls_in(vec![msg], cwd);
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
}
