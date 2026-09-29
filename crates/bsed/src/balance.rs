//! A language-agnostic bracket/string scanner, used to decide whether a block
//! ends inside an unfinished construct.
//!
//! The question it answers is "is this text balanced?", not "is this valid
//! code?" — it has no grammar, so it cannot tell a macro body from a heredoc
//! from a string that happens to contain a brace. That is fine, because the only
//! consequence of being wrong is a block that carries a few extra unchanged
//! lines, capped and clamped elsewhere.
//!
//! What it does model, because getting these wrong is noisy: `//` line
//! comments, `/* */` block comments, `"` and `'` strings with backslash escapes,
//! and the Rust/C distinction that `'a` is a lifetime while `'a'` is a
//! character.

/// Why a piece of text was considered balanced or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Balance {
    /// Every opener closed, no string or comment left open.
    Balanced,
    /// More openers than closers by `n`, or an unterminated string/comment.
    Unbalanced,
    /// Fewer closers than openers: the construct started *before* this text, so
    /// growing forward will not help — the caller should leave the block alone.
    OverClosed,
}

/// Scan `text` and report where it leaves off.
pub fn scan(text: &str) -> Balance {
    let mut stack: Vec<char> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut prev = '\0';

    while let Some(c) = chars.next() {
        // Comments.
        if c == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            prev = '\n';
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev_c = '\0';
            let mut closed = false;
            for c in chars.by_ref() {
                if prev_c == '*' && c == '/' {
                    chars.next();
                    closed = true;
                    break;
                }
                prev_c = c;
            }
            if !closed {
                return Balance::Unbalanced; // unterminated block comment
            }
            prev = 'x';
            continue;
        }

        // Strings.
        if c == '"' {
            if !consume_quoted(&mut chars, '"') {
                return Balance::Unbalanced;
            }
            prev = 'x';
            continue;
        }
        if c == '\'' {
            // `'a` is a lifetime (Rust), `'a'` is a char literal. Only treat it
            // as a string when a closing quote shows up within a few chars.
            let mut probe = chars.clone();
            let mut seen = 0;
            let mut closes = false;
            while seen < 4 {
                match probe.next() {
                    Some('\\') => {
                        probe.next();
                        seen += 2;
                    }
                    Some('\'') => {
                        closes = true;
                        break;
                    }
                    Some('\n') | None => break,
                    Some(_) => seen += 1,
                }
            }
            if !closes {
                prev = c;
                continue; // a lifetime, not a literal
            }
            if !consume_quoted(&mut chars, '\'') {
                return Balance::Unbalanced;
            }
            prev = 'x';
            continue;
        }

        // Brackets.
        match c {
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let want = closer_for(c);
                match stack.pop() {
                    Some(open) if open == want => {}
                    // A closer with nothing open (or the wrong thing open)
                    // means the construct began before this text.
                    _ => return Balance::OverClosed,
                }
            }
            '<' => {
                // Angle brackets are the noisy case: `a < b` is a comparison,
                // `->` is an arrow, `Vec<T>` is a generic. Count an opener only
                // when it is neither of those *and* something closes it within
                // the scan; `scan` cannot look ahead past its input, so the
                // caller re-scans the grown text and an unmatched `<` simply
                // leaves the stack empty.
                let is_arrow = prev == '-' || chars.peek() == Some(&'-');
                if !is_arrow {
                    stack.push('<');
                }
            }
            '>' => {
                // Only pop if an angle bracket is actually open, so `a > b`
                // does not consume an unrelated `(`.
                if stack.last() == Some(&'<') {
                    stack.pop();
                }
            }
            _ => {}
        }
        prev = c;
    }

    // A dangling `<` is treated as a comparison, not an unclosed generic: `a < b`
    // is far commoner in real source than a generic cut off mid-line, and the
    // growth pass is a nicety — refusing to grow because of one is worse than
    // skipping a growth that might have helped. Every other unclosed opener is
    // believed.
    if stack.iter().all(|c| *c == '<') {
        Balance::Balanced
    } else if stack.is_empty() {
        Balance::Balanced
    } else {
        Balance::Unbalanced
    }
}

/// Consume a quoted run, honouring backslash escapes. Returns false if the
/// closing quote never arrives.
fn consume_quoted<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    quote: char,
) -> bool {
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '\n' => return false, // unterminated: real strings do not span lines here
            c if c == quote => return true,
            _ => {}
        }
    }
    false
}

fn closer_for(c: char) -> char {
    match c {
        ')' => '(',
        ']' => '[',
        _ => '{',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_code() {
        assert_eq!(scan("fn main() { let x = 1; }"), Balance::Balanced);
        assert_eq!(scan("call(a, b)"), Balance::Balanced);
        assert_eq!(scan("items[0].name()"), Balance::Balanced);
    }

    #[test]
    fn unclosed_constructs() {
        assert_eq!(scan("let v = compute("), Balance::Unbalanced);
        assert_eq!(scan("let v = foo(bar,"), Balance::Unbalanced);
        assert_eq!(scan("fn outer() {"), Balance::Unbalanced);
        assert_eq!(scan("if (a) { b(); "), Balance::Unbalanced);
        assert_eq!(scan("let s = \"unterminated"), Balance::Unbalanced);
        assert_eq!(scan("/* never closed"), Balance::Unbalanced);
    }

    #[test]
    fn braces_inside_strings_and_comments_do_not_count() {
        assert_eq!(scan("let s = \"{\""), Balance::Balanced);
        assert_eq!(scan("let s = '(' ;"), Balance::Balanced);
        assert_eq!(scan("// a comment with {"), Balance::Balanced);
        assert_eq!(scan("/* a block with { */"), Balance::Balanced);
        assert_eq!(scan("let s = \"a\\\"b{\""), Balance::Balanced);
    }

    #[test]
    fn over_closed_means_growing_will_not_help() {
        assert_eq!(scan("    );"), Balance::OverClosed);
        assert_eq!(scan("}"), Balance::OverClosed);
    }

    #[test]
    fn comparison_is_not_a_generic() {
        // `a < b` alone leaves an unmatched opener, which is Unbalanced — but
        // a full line with the `>` later is fine either way.
        assert_eq!(scan("if a < b { }"), Balance::Balanced);
        assert_eq!(scan("let v: Vec<String> = x;"), Balance::Balanced);
        assert_eq!(scan("x -> y"), Balance::Balanced);
    }

    #[test]
    fn rust_lifetime_is_not_a_char_literal() {
        // If `'a` were treated as an opening quote, this would look unterminated.
        assert_eq!(
            scan("fn f<'a>(x: &'a str) { let _ = x; }"),
            Balance::Balanced
        );
    }
}
