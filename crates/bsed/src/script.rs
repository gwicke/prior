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

use sed_rs::command::{Address, AddressRange, Command, SedCommand, SubstituteCmd};

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

/// A change-range command: replaces an entire address range with new text.
/// This is the GNU sed `c` command over a range (e.g., `2,4c\new text`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRange {
    /// 1-based start line of the range (inclusive)
    pub start_line: usize,
    /// 1-based end line of the range (inclusive)
    pub end_line: usize,
    /// The replacement text (can contain newlines)
    pub replacement: String,
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
    /// Change-range commands extracted from the script (GNU `c` over range).
    /// These are handled separately by the engine because sed-rs applies `c`
    /// per-line instead of once-per-range like GNU sed.
    pub change_ranges: Vec<ChangeRange>,
}

/// Split, screen, and rewrite. Never executes anything.
pub fn build(expressions: &str) -> Result<Script, ScriptError> {
    // Parse the entire script at once to handle multi-line commands (like `c\`).
    // sed-rs's parse function handles newlines and semicolons as command separators.
    let cmds = sed_rs::command::parse(expressions).map_err(|e| ScriptError {
        expression: 0,
        line: expressions.to_string(),
        reason: format!("{e}"),
    })?;

    if cmds.is_empty() {
        return Err(ScriptError {
            expression: 0,
            line: String::new(),
            reason: "no sed expressions given".into(),
        });
    }

    // For error reporting, we need to map commands back to expression numbers.
    // We'll approximate by counting non-blank lines in the original.
    let expr_lines: Vec<&str> = expressions
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let expr_count = expr_lines.len().max(1);

    let mut used_case_sentinels = false;
    let mut change_ranges: Vec<ChangeRange> = Vec::new();

    // Extract change ranges and screen all commands
    for (i, cmd) in cmds.iter().enumerate() {
        let expr_num = (i % expr_count) + 1;
        let line = expr_lines.get(i).unwrap_or(&expressions);
        extract_change_ranges(&[cmd.clone()], expr_num, line, &mut change_ranges)?;
        screen(&[cmd.clone()], expr_num, line)?;
        if cmd_has_case_escape(cmd) {
            used_case_sentinels = true;
        }
    }

    // Rewrite case escapes for the whole script
    let rewritten = rewrite_case_escapes(&cmds);

    // Remove change-range commands from the script text passed to sed-rs,
    // since we handle them separately with correct GNU semantics.
    let script_text = if let Some(r) = rewritten {
        remove_change_ranges(r)
    } else {
        remove_change_ranges(expressions.to_string())
    };

    Ok(Script {
        text: script_text,
        count: expr_count,
        used_case_sentinels,
        change_ranges,
    })
}

/// Check if any command in the tree has a case escape.
fn cmd_has_case_escape(cmd: &SedCommand) -> bool {
    match &cmd.command {
        Command::Substitute(s) => has_case_escape(&s.replacement),
        Command::Block(inner) => inner.iter().any(cmd_has_case_escape),
        _ => false,
    }
}

/// Extract `c` (change) commands over ranges from parsed commands.
fn extract_change_ranges(
    cmds: &[SedCommand],
    expr_num: usize,
    line: &str,
    ranges: &mut Vec<ChangeRange>,
) -> Result<(), ScriptError> {
    for c in cmds {
        if let Command::Change(replacement) = &c.command {
            if let AddressRange::Range {
                start,
                end,
                negated: false,
            } = &c.address
            {
                // Parse start and end addresses to line numbers
                let (start_line, end_line) = match (start, end) {
                    (Address::Line(s), Address::Line(e)) => (*s, *e),
                    (Address::Line(s), Address::Last) => (*s, usize::MAX), // Will resolve at runtime
                    _ => {
                        return Err(ScriptError {
                            expression: expr_num,
                            line: line.to_string(),
                            reason: "only line-number addresses supported for 'c' over range"
                                .into(),
                        });
                    }
                };
                ranges.push(ChangeRange {
                    start_line,
                    end_line,
                    // GNU expands `\n` and friends in the text of `c`; `sed-rs`
                    // passes the backslash through literally, so the preview
                    // would show a literal `\n` where GNU would insert a newline.
                    // Expand here, where the parsed form is authoritative, so the
                    // stored range carries the text GNU would actually write.
                    replacement: expand_text_escapes(replacement),
                });
            }
        }
        if let Command::Block(inner) = &c.command {
            extract_change_ranges(inner, expr_num, line, ranges)?;
        }
    }
    Ok(())
}

/// Remove `c` range commands from script text by replacing them with no-ops.
/// We handle them separately, so we don't want sed-rs to process them incorrectly.
fn remove_change_ranges(script: String) -> String {
    // Parse the script and filter out change-range commands
    let cmds = sed_rs::command::parse(&script).unwrap_or_else(|_| Vec::new());
    let filtered = filter_change_ranges(cmds);
    render_commands(&filtered)
}

/// Filter out `c` commands with range addresses from parsed commands.
fn filter_change_ranges(cmds: Vec<SedCommand>) -> Vec<SedCommand> {
    cmds.into_iter()
        .filter_map(|c| {
            if let Command::Change(_) = &c.command {
                if matches!(c.address, AddressRange::Range { .. }) {
                    return None; // Drop this command
                }
            }
            if let Command::Block(inner) = c.command {
                let filtered_inner = filter_change_ranges(inner);
                if filtered_inner.is_empty() {
                    return None;
                }
                return Some(SedCommand {
                    address: c.address,
                    command: Command::Block(filtered_inner),
                });
            }
            Some(c)
        })
        .collect()
}

/// Render filtered commands back to a script string.
fn render_commands(cmds: &[SedCommand]) -> String {
    let mut out = String::new();
    for (i, c) in cmds.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_command_filtered(c, &mut out);
    }
    out
}

fn render_command_filtered(c: &SedCommand, out: &mut String) {
    // Render address
    match &c.address {
        AddressRange::None => {}
        AddressRange::Single { addr, negated } => {
            render_address(addr, out);
            if *negated {
                out.push('!');
            }
        }
        AddressRange::Range {
            start,
            end,
            negated,
        } => {
            render_address(start, out);
            out.push(',');
            render_address(end, out);
            if *negated {
                out.push('!');
            }
        }
    }
    // Render command
    render_command_inner(&c.command, out);
}

fn render_address(addr: &Address, out: &mut String) {
    match addr {
        Address::Line(n) => out.push_str(&n.to_string()),
        Address::Last => out.push('$'),
        Address::Regex(re) => {
            out.push('/');
            out.push_str(re);
            out.push('/');
        }
        Address::Step { first, step } => {
            out.push_str(&format!("{first}~{step}"));
        }
    }
}

fn render_command_inner(cmd: &Command, out: &mut String) {
    match cmd {
        Command::Substitute(s) => {
            out.push('s');
            out.push('/');
            out.push_str(&s.pattern);
            out.push('/');
            out.push_str(&s.replacement);
            out.push('/');
            if s.global {
                out.push('g');
            }
            if let Some(n) = s.nth {
                out.push_str(&n.to_string());
            }
            if s.case_insensitive {
                out.push('I');
            }
            if s.print {
                out.push('p');
            }
            if let Some(w) = &s.write_file {
                out.push_str(&format!("w {w}"));
            }
        }
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
        Command::Append(t) => out.push_str(&render_text_command('a', t)),
        Command::Insert(t) => out.push_str(&render_text_command('i', t)),
        Command::Change(t) => out.push_str(&render_text_command('c', t)),
        Command::Label(l) => out.push_str(&format!(":{l}")),
        Command::Branch(l) => out.push_str(&format!("b{}", l.as_deref().unwrap_or(""))),
        Command::BranchIfSub(l) => out.push_str(&format!("t{}", l.as_deref().unwrap_or(""))),
        Command::BranchIfNotSub(l) => out.push_str(&format!("T{}", l.as_deref().unwrap_or(""))),
        Command::ReadFile(p) => out.push_str(&format!("r {p}")),
        Command::ReadLine(p) => out.push_str(&format!("R {p}")),
        Command::PrintFileName => out.push('F'),
        Command::ClearPattern => out.push('z'),
        Command::Noop => out.push(':'),
        Command::WriteFile(p) => out.push_str(&format!("w {p}")),
        Command::WriteFirstLine(p) => out.push_str(&format!("W {p}")),
        Command::Execute(e) => {
            out.push('e');
            if let Some(c) = e {
                out.push(' ');
                out.push_str(c);
            }
        }
        Command::Quit(code) => {
            out.push('q');
            if let Some(c) = code {
                out.push_str(&c.to_string());
            }
        }
        Command::QuitNoprint(code) => {
            out.push('Q');
            if let Some(c) = code {
                out.push_str(&c.to_string());
            }
        }
        Command::Block(inner) => {
            out.push('{');
            out.push('\n');
            for (i, c) in inner.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str("  ");
                render_command(c, out);
            }
            out.push('\n');
            out.push('}');
        }
    }
}

/// Render one text command (`a`, `i`, or `c`) back into script text.
///
/// The rendered form is re-parsed before it reaches the engine, so a newline
/// inside the text has to be written as a backslash continuation: a bare one
/// would read as the end of the text argument and silently drop the line.
fn render_text_command(cmd: char, text: &str) -> String {
    let mut body = expand_text_escapes(text).replace('\n', "\\\n");
    // `sed-rs` skips exactly one space or tab between the backslash and the text,
    // so text that *begins* with one would come back with it missing. Spend a
    // space of our own on that skip to keep the caller's leading whitespace.
    if body.starts_with(' ') || body.starts_with('\t') {
        body.insert(0, ' ');
    }
    format!("{cmd}\\{body}")
}

/// Expand the backslash escapes GNU honours in the text of `a`, `i`, and `c`.
///
/// `sed-rs` copies that text verbatim, so `2a\ntest` would preview as a literal
/// `\ntest` where GNU writes a newline and then `test`. That is a preview that
/// looks plausible and is wrong, which is the one failure mode this crate exists
/// to prevent — so the escapes are resolved here, on the parsed text, before it
/// reaches the engine.
///
/// The set matches GNU's `a`/`i`/`c` text: the C-style single letters, `\xHH`,
/// `\oNNN`, and `\dNNN`. An unrecognised escape drops the backslash and keeps the
/// character, so `\q` is `q` — the same as GNU. A backslash already consumed as a
/// line continuation is a real newline in the parsed text and is left alone.
///
/// Note this is *not* how `s///` replacements work: those go through the engine,
/// which decodes them itself. Only the text commands are copied verbatim, so only
/// they need rewriting here.
///
/// Known gap, predating this: `sed-rs` drops one space or tab between the `a`/`i`/
/// `c` and its text while parsing, where GNU keeps it. So `2a\ indented` previews
/// as `indented`. Recovering it would mean re-reading the raw expression text
/// rather than trusting the parsed text, which is a larger change than this
/// rewrite; leading whitespace is far better signalled with `\t` or `\x20`,
/// which round-trip correctly.
fn expand_text_escapes(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // A trailing backslash escapes nothing. GNU reads it as a continuation
        // onto the (empty) next line and writes no backslash, so dropping it
        // matches; keeping it would put a stray `\` into the file.
        if i + 1 >= chars.len() {
            break;
        }
        let next = chars[i + 1];
        let width = match next {
            'n' => {
                out.push('\n');
                2
            }
            't' => {
                out.push('\t');
                2
            }
            'r' => {
                out.push('\r');
                2
            }
            'a' => {
                out.push('\x07');
                2
            }
            'f' => {
                out.push('\x0C');
                2
            }
            'v' => {
                out.push('\x0B');
                2
            }
            '\\' => {
                out.push('\\');
                2
            }
            'x' | 'o' | 'd' => {
                // GNU reads at most two hex digits, or up to three octal/decimal.
                let (radix, max) = match next {
                    'x' => (16, 2),
                    'o' => (8, 3),
                    _ => (10, 3),
                };
                let mut digits = String::new();
                let mut j = i + 2;
                while digits.len() < max && j < chars.len() && chars[j].is_digit(radix) {
                    digits.push(chars[j]);
                    j += 1;
                }
                if digits.is_empty() {
                    // No digits: GNU keeps the character itself, dropping the
                    // backslash, as with any other unrecognised escape.
                    out.push(next);
                    2
                } else {
                    match u32::from_str_radix(&digits, radix)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        Some(c) => {
                            out.push(c);
                            2 + digits.len()
                        }
                        // Out of range: GNU leaves the digits as text.
                        None => {
                            out.push(next);
                            out.push_str(&digits);
                            2 + digits.len()
                        }
                    }
                }
            }
            // Unrecognised: drop the backslash, keep the character. A trailing
            // lone backslash has already been handled by the bounds check above.
            c => {
                out.push(c);
                2
            }
        };
        i += width;
    }
    out
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
        // `sed-rs` emits a copy per line. We extract these and handle them
        // separately in the engine with correct GNU semantics.
        // Single-address `c` is allowed and works correctly in sed-rs.
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
        Command::Append(t) => out.push_str(&render_text_command('a', t)),
        Command::Insert(t) => out.push_str(&render_text_command('i', t)),
        Command::Change(t) => out.push_str(&render_text_command('c', t)),
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

#[cfg(test)]
mod text_escape_tests {
    use super::{expand_text_escapes, render_text_command};

    /// The expansion has to agree with GNU, because the round-trip test in
    /// `lib.rs` compares against it — but only for the escapes it is meant to
    /// cover. Anything not listed here is a pass-through.
    #[test]
    fn expands_the_escapes_gnu_honours_in_text() {
        for (input, want) in [
            (r"two\nlines", "two\nlines"),
            (r"a\tb", "a\tb"),
            (r"a\rb", "a\rb"),
            (r"bell\a", "bell\x07"),
            (r"ff\f", "ff\x0C"),
            (r"vt\v", "vt\x0B"),
            (r"back\\slash", "back\\slash"),
            (r"\x41", "A"),
            (r"\o101", "A"),
            (r"\d065", "A"),
            // At most two hex digits, three of the others.
            (r"\x41B", "AB"),
            (r"\o1011", "A1"),
            // Unrecognised: the backslash goes, the character stays.
            (r"\q", "q"),
            (r"\x", "x"),
            (r"\xZZ", "xZZ"),
            // No escapes at all is the common case, and must be untouched.
            (r"plain text", r"plain text"),
        ] {
            assert_eq!(expand_text_escapes(input), want, "input: {input:?}");
        }
    }

    /// A lone trailing backslash has no character to escape, so GNU drops it.
    #[test]
    fn a_trailing_backslash_is_dropped() {
        assert_eq!(expand_text_escapes("text\\"), "text");
    }

    /// A newline in the rendered text must be a continuation, or the re-parse
    /// reads it as the end of the text argument and loses the line.
    #[test]
    fn rendered_newlines_become_continuations() {
        assert_eq!(render_text_command('a', "one\ntwo"), "a\\one\\\ntwo");
    }

    /// `sed-rs` eats one space or tab after the backslash, so leading
    /// whitespace needs a sacrificial space in front of it.
    #[test]
    fn leading_whitespace_is_protected_from_the_reparse() {
        assert_eq!(render_text_command('a', " lead"), "a\\  lead");
        assert_eq!(render_text_command('a', "\tlead"), "a\\ \tlead");
        // ...but only when there is some.
        assert_eq!(render_text_command('a', "lead"), "a\\lead");
    }
}
