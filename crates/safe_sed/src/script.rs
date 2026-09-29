//! Turning the caller's `expressions` string into one validated sed script.
//!
//! Three jobs, in order:
//!
//! 1. **Split.** One expression per line, blank lines dropped. This is exactly
//!    `sed -e A -e B` (GNU concatenates `-e` scripts with newlines), so
//!    `326d;327d` on one line and two separate lines mean the same thing.
//! 2. **Screen.** Each expression is parsed and its AST walked. The four
//!    commands that touch the outside world — write, shell-execute, early-exit —
//!    are rejected *before* the engine runs, so a hole in this file cannot
//!    become a file write or a process spawn. The walk is a `match` with no
//!    wildcard arm: if `sed-rs` ever adds a command, this crate stops compiling
//!    rather than silently permitting it.
//! 3. **Rewrite case escapes.** `\u \l \U \L \E` in a replacement are GNU
//!    features `sed-rs` passes through literally, which would produce a preview
//!    that *looks* right and is wrong. They are replaced with sentinels that a
//!    post-pass expands ([`crate::engine`]).

use sed_rs::command::{AddressRange, Command, SedCommand, SubstituteCmd};

/// A rejected expression, with enough context for the model to fix it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    /// 1-based index of the offending expression among the non-blank lines.
    pub expression: usize,
    /// The expression text, echoed so the model can see which line is at fault.
    pub line: String,
    /// Why it was rejected.
    pub reason: String,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "expression {}: {}", self.expression, self.reason)?;
        if !self.line.is_empty() {
            write!(f, "\n  {}", self.line)?;
        }
        Ok(())
    }
}

/// A validated script: the joined text to hand the engine, plus whether any
/// expression needed the case-escape rewrite.
pub struct Script {
    pub text: String,
    /// How many expressions were screened (for the banner).
    pub count: usize,
    /// True when at least one replacement carried a case escape, which means
    /// the post-pass must find the sentinels or fail loudly.
    pub used_case_sentinels: bool,
}

/// Split, screen, and rewrite. Never executes anything.
pub fn build(expressions: &str) -> Result<Script, ScriptError> {
    let mut kept: Vec<(usize, String)> = Vec::new();
    for line in expressions.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        kept.push((kept.len() + 1, line.to_string()));
    }
    if kept.is_empty() {
        return Err(ScriptError {
            expression: 0,
            line: String::new(),
            reason: "no sed expressions given".into(),
        });
    }

    let mut used_case_sentinels = false;
    let mut out: Vec<String> = Vec::new();
    for (n, line) in &kept {
        let cmds = sed_rs::command::parse(line).map_err(|e| ScriptError {
            expression: *n,
            line: line.clone(),
            reason: format!("{e}"),
        })?;
        screen(&cmds, *n, line)?;
        if let Some(rewritten) = rewrite_case_escapes(&cmds) {
            out.push(rewritten);
            used_case_sentinels = true;
        } else {
            out.push(line.clone());
        }
    }

    Ok(Script {
        text: out.join("\n"),
        count: kept.len(),
        used_case_sentinels,
    })
}

/// Reject the commands that would read or write the world beyond this file.
///
/// Kept deliberately small. `r`/`R` are **allowed**: the process already runs
/// in a read-only namespace where the same files are readable, so refusing them
/// would be a functional restriction dressed as a security boundary, and
/// splicing a file in is a real refactor idiom.
fn screen(cmds: &[SedCommand], n: usize, line: &str) -> Result<(), ScriptError> {
    for c in cmds {
        screen_one(c, n, line)?;
    }
    Ok(())
}

fn screen_one(c: &SedCommand, n: usize, line: &str) -> Result<(), ScriptError> {
    let reject = |reason: String| ScriptError {
        expression: n,
        line: line.to_string(),
        reason,
    };
    match &c.command {
        // Writes a file. The whole point of the tool is that it does not.
        Command::WriteFile(p) => {
            return Err(reject(format!(
                "'w {p}' writes to a file. The sed preview never writes; use the `edit` tool \
                 with the blocks this preview returns."
            )))
        }
        Command::WriteFirstLine(p) => {
            return Err(reject(format!(
                "'W {p}' writes to a file. The sed preview never writes; use the `edit` tool \
                 with the blocks this preview returns."
            )))
        }
        // `s///w FILE` hides the write inside a substitution.
        Command::Substitute(s) => screen_substitute(s, n, line)?,
        // Shell execution. Also reached by the `e` flag on `s///`, which
        // `sed-rs` parses as a separate `Execute` node.
        Command::Execute(Some(cmd)) => {
            return Err(reject(format!(
                "'e {cmd}' runs a shell command. The sed preview does not execute anything; \
                 use the `bash` tool if you need to run something."
            )))
        }
        Command::Execute(None) => {
            return Err(reject(
                "'e' runs the pattern space as a shell command. The sed preview does not \
                 execute anything; use the `bash` tool instead."
                    .into(),
            ))
        }
        // Early exit truncates the stream, which the diff would then report as
        // "delete everything after here". Useless here, maximally misleading.
        Command::Quit(code) => return Err(reject(quit_reason("q", code))),
        Command::QuitNoprint(code) => return Err(reject(quit_reason("Q", code))),
        // `c` over a *range*: GNU replaces the whole range with one copy, but
        // `sed-rs` emits a copy per line. That is a wrong preview, not a
        // preview we can fix afterwards, so it is refused rather than guessed at.
        Command::Change(_) if matches!(c.address, AddressRange::Range { .. }) => {
            return Err(reject(
                "'c' over a line range is not supported: GNU replaces the whole range with \
                 one copy of the text, and the preview could not show that faithfully. Make \
                 one `edit` call per range, or use a single-line address."
                    .into(),
            ))
        }
        // Recurse into a `{ … }` block; a write can hide in there.
        Command::Block(inner) => {
            for c in inner {
                screen_one(c, n, line)?;
            }
        }
        // Everything else is a pure text transform, or a read we allow.
        Command::Transliterate { .. }
        | Command::Print
        | Command::PrintFirstLine
        | Command::PrintLineNumber
        | Command::List
        | Command::Delete
        | Command::DeleteFirstLine
        | Command::Next
        | Command::NextAppend
        | Command::HoldReplace
        | Command::HoldAppend
        | Command::GetReplace
        | Command::GetAppend
        | Command::Exchange
        | Command::Append(_)
        | Command::Insert(_)
        | Command::Change(_)
        | Command::Label(_)
        | Command::Branch(_)
        | Command::BranchIfSub(_)
        | Command::BranchIfNotSub(_)
        | Command::ReadFile(_)
        | Command::ReadLine(_)
        | Command::PrintFileName
        | Command::ClearPattern
        | Command::Noop => {}
    }
    Ok(())
}

fn quit_reason(cmd: &str, code: &Option<i32>) -> String {
    let tail = match code {
        Some(0) | None => String::new(),
        Some(n) => format!(" (exit status {n})"),
    };
    format!(
        "'{cmd}'{tail} stops processing early, which this tool would report as \"delete the \
         rest of the file\". There is no useful form of it here — the whole file is already \
         loaded. Use an address or a range instead, e.g. '1,400d'."
    )
}

fn screen_substitute(s: &SubstituteCmd, n: usize, line: &str) -> Result<(), ScriptError> {
    if let Some(p) = &s.write_file {
        return Err(ScriptError {
            expression: n,
            line: line.to_string(),
            reason: format!(
                "'s///w {p}' writes to a file. The sed preview never writes; use the `edit` \
                 tool with the blocks this preview returns."
            ),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Case escapes
// ---------------------------------------------------------------------------

/// The private-use characters a case span is wrapped in, carrying its mode.
pub const OPEN_UPPER: &str = "\u{e001}";
pub const OPEN_LOWER: &str = "\u{e002}";
pub const CLOSE: &str = "\u{e003}";

/// True when a replacement carries a GNU case-conversion escape.
fn has_case_escape(replacement: &str) -> bool {
    let b = replacement.as_bytes();
    b.windows(2)
        .any(|w| w[0] == b'\\' && matches!(w[1], b'u' | b'l' | b'U' | b'L' | b'E'))
}

/// Rebuild a command's replacement with case escapes turned into sentinels.
///
/// Returns `None` when nothing in the expression needed it, so the common path
/// passes the caller's text through untouched.
///
/// The trick: GNU case-transforms a span that is otherwise copied verbatim from
/// the matched text, and `sed-rs` copies it verbatim while leaving the escape
/// characters in place. So the span is still *findable* afterwards — mark it
/// now, expand it once the engine has run (`crate::engine::collapse_case`).
/// `&` and backreferences still expand inside the span, because the engine does
/// that itself before we ever look at the output.
fn rewrite_case_escapes(cmds: &[SedCommand]) -> Option<String> {
    if !cmds.iter().any(command_has_case_escape) {
        return None;
    }
    // Re-render the parsed form rather than patching the raw text: the parser
    // already knows where each replacement starts and ends, so delimiter
    // choice, escapes, and trailing flags need no re-guessing here.
    let mut out = String::new();
    for (i, c) in cmds.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        render_command(c, &mut out);
    }
    Some(out)
}

fn command_has_case_escape(c: &SedCommand) -> bool {
    match &c.command {
        Command::Substitute(s) => has_case_escape(&s.replacement),
        Command::Block(inner) => inner.iter().any(command_has_case_escape),
        _ => false,
    }
}

fn render_command(c: &SedCommand, out: &mut String) {
    match &c.command {
        Command::Substitute(s) => {
            out.push('s');
            out.push('/');
            out.push_str(&s.pattern);
            out.push('/');
            out.push_str(&rewrite_replacement(&s.replacement));
            out.push('/');
            if s.nth.is_some() {
                out.push_str(&s.nth.unwrap().to_string());
            }
            if s.global {
                out.push('g');
            }
            if s.case_insensitive {
                out.push('I');
            }
            if s.print {
                out.push('p');
            }
        }
        Command::Block(inner) => {
            out.push('{');
            for (i, c) in inner.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                render_command(c, out);
            }
            out.push('}');
        }
        // Only reached when some *other* command in the expression needed the
        // rewrite; pass anything else through a canonical rendering.
        other => render_fallback(other, out),
    }
}

fn render_fallback(c: &Command, out: &mut String) {
    match c {
        Command::Transliterate { from, to } => {
            out.push('y');
            out.push('/');
            out.extend(from.iter());
            out.push('/');
            out.extend(to.iter());
            out.push('/');
        }
        Command::Print => out.push('p'),
        Command::PrintFirstLine => out.push('P'),
        Command::PrintLineNumber => out.push('='),
        Command::List => out.push('l'),
        Command::Delete => out.push('d'),
        Command::DeleteFirstLine => out.push('D'),
        Command::Next => out.push('n'),
        Command::NextAppend => out.push('N'),
        Command::HoldReplace => out.push('h'),
        Command::HoldAppend => out.push('H'),
        Command::GetReplace => out.push('g'),
        Command::GetAppend => out.push('G'),
        Command::Exchange => out.push('x'),
        Command::Append(t) => out.push_str(&format!("a\\{t}")),
        Command::Insert(t) => out.push_str(&format!("i\\{t}")),
        Command::Change(t) => out.push_str(&format!("c\\{t}")),
        Command::Label(l) => out.push_str(&format!(":{l}")),
        Command::Branch(l) => out.push_str(&format!("b{}", l.as_deref().unwrap_or(""))),
        Command::BranchIfSub(l) => out.push_str(&format!("t{}", l.as_deref().unwrap_or(""))),
        Command::BranchIfNotSub(l) => out.push_str(&format!("T{}", l.as_deref().unwrap_or(""))),
        Command::ReadFile(p) => out.push_str(&format!("r {p}")),
        Command::ReadLine(p) => out.push_str(&format!("R {p}")),
        Command::PrintFileName => out.push('F'),
        Command::ClearPattern => out.push('z'),
        Command::Noop => out.push(':'),
        // Handled by `render_command`, which needs the parsed replacement.
        Command::Substitute(_) | Command::Block(_) => out.push_str("# internal: unreachable"),
        // Screened out before we get here.
        Command::WriteFile(_)
        | Command::WriteFirstLine(_)
        | Command::Execute(_)
        | Command::Quit(_)
        | Command::QuitNoprint(_) => out.push_str("# rejected earlier"),
    }
}

/// Turn `\U`/`\L`/`\u`/`\l`/`\E` in a replacement into sentinel-wrapped spans.
///
/// Mode is sticky (as in GNU): `\U` upper-cases everything after it until
/// `\E`; `\u` upper-cases the next character only and leaves the mode off. Each
/// span is closed immediately, so the modes compose without needing a stack
/// that outlives the replacement.
fn rewrite_replacement(replacement: &str) -> String {
    // Build the replacement as (text, mode) segments, then wrap each converting
    // segment in sentinels. Segments are needed because a mode can start or stop
    // in the middle of a replacement, and because the tokens that *become* text
    // at substitution time (`&`, `\1`) must sit *inside* the sentinel span for
    // the transform to reach them — a per-character walk would wrap nothing at
    // all for `s/x/\U\1/`, and the escape would silently pass through.
    let mut segments: Vec<(String, Mode)> = Vec::new();
    let mut mode = Mode::None;
    let mut chars = replacement.chars();
    while let Some(ch) = chars.next() {
        let next_mode = if ch == '\\' {
            match chars.next() {
                Some('U') => Mode::Upper,
                Some('L') => Mode::Lower,
                Some('u') => Mode::OneUpper,
                Some('l') => Mode::OneLower,
                Some('E') => Mode::None,
                // `&`, `\1`, `\n`, `\t` … left exactly as written; the engine
                // expands them, inside the span we are about to close.
                Some(c) => {
                    push(&mut segments, mode, &mut String::from("\\"));
                    push(&mut segments, mode, &mut String::from(c));
                    continue;
                }
                None => {
                    push(&mut segments, mode, &mut String::from("\\"));
                    continue;
                }
            }
        } else {
            // `\u`/`\l` convert exactly one character, so they act on the
            // character that follows and then switch themselves off.
            let mut c = String::from(ch);
            if mode == Mode::OneUpper || mode == Mode::OneLower {
                c.push_str("");
            }
            let seg_mode = mode;
            if mode == Mode::OneUpper || mode == Mode::OneLower {
                mode = Mode::None;
            }
            push(&mut segments, seg_mode, &mut c);
            continue;
        };
        mode = next_mode;
    }
    push(&mut segments, mode, &mut String::new());

    let mut out = String::new();
    for (text, seg_mode) in segments {
        if text.is_empty() {
            continue;
        }
        if seg_mode == Mode::None {
            out.push_str(&text);
        } else {
            out.push_str(opener(seg_mode));
            out.push_str(&text);
            out.push_str(CLOSE);
        }
    }
    out
}

/// Append `piece` to the last segment when it shares a mode, else start a new
/// one — so `foo\Ubar\1baz` becomes four tidy segments, not one per character.
fn push(segments: &mut Vec<(String, Mode)>, mode: Mode, piece: &mut String) {
    if piece.is_empty() {
        return;
    }
    match segments.last_mut() {
        Some((buf, m)) if *m == mode => buf.push_str(piece),
        _ => segments.push((std::mem::take(piece), mode)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No case conversion in effect.
    None,
    /// `\U` — everything after, until `\E`.
    Upper,
    /// `\L` — everything after, until `\E`.
    Lower,
    /// `\u` — the next character only.
    OneUpper,
    /// `\l` — the next character only.
    OneLower,
}

fn opener(mode: Mode) -> &'static str {
    match mode {
        Mode::Lower | Mode::OneLower => OPEN_LOWER,
        _ => OPEN_UPPER,
    }
}
