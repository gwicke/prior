//! Typed values and permissive parsing (design §7).
//!
//! Only two value kinds exist: `string` and `number` (int or float, parsed
//! permissively). Enum sets are *orthogonal*: they constrain completion only;
//! the interpreter never rejects an out-of-enum token.

use std::cmp::Ordering;

/// A typed value: string or number (design §7).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
}

impl Value {
    /// Permissively parse `s`: try number first, fall back to string.
    pub fn parse(s: &str) -> Value {
        match s.parse::<f64>() {
            Ok(n) if n.is_finite() => Value::Num(n),
            _ => Value::Str(s.to_string()),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Num(_) => None,
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Str(_) => None,
        }
    }

    /// Total-order comparison over mixed values: numbers before strings,
    /// then natural order within the kind.
    pub fn cmp_value(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (Value::Num(_), Value::Str(_)) => Ordering::Less,
            (Value::Str(_), Value::Num(_)) => Ordering::Greater,
        }
    }
}

/// Format a number for display (design §7 / 6.7-5 follow-up): round to at
/// most 2 decimals (for per-unit costs like price/token) and trim trailing
/// zeros so `2.5` never renders as `2.5000000000000000`, while integers keep
/// an integer digit (`4096000` stays whole). This is the crate-side
/// replacement for the removed host `format_num_cell`/`trim_trailing_zeros`.
/// TODO(number-spec): a future `num_decimals`/`zero_pad` on `FieldSpec` will
/// make decimals configurable; 2 decimals + zero-trim is the default today.
pub fn fmt_num(n: f64) -> String {
    if !n.is_finite() {
        // NaN/inf have no sensible numeric rendering; show the raw token.
        return n.to_string();
    }
    let s = format!("{:.2}", n);
    // Trim trailing zeros + point, keeping at least an integer digit.
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() || t == "-" || t == "-0" {
        "0".to_string()
    } else {
        t.to_string()
    }
}

/// A comparison predicate for numeric field filters: `>v`, `<v`, `>=v`, `<=v`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    pub fn matches(&self, v: f64, threshold: f64) -> bool {
        match self {
            Cmp::Lt => v < threshold,
            Cmp::Le => v <= threshold,
            Cmp::Gt => v > threshold,
            Cmp::Ge => v >= threshold,
        }
    }
}

/// Parse a filter value into either a numeric comparison or an exact-match
/// string (design §7.1: `.field:>v` / `.field:<v` compare, anything else is
/// exact match). Returns `None` when the token has no comparable predicate
/// (e.g. `.<` with no number).
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    Exact(Value),
    Compare(Cmp, f64),
}

pub fn parse_filter_value(s: &str) -> Option<FilterValue> {
    // Comparison form: one of > < >= <= followed by a number.
    for (prefix, cmp) in [
        (">=", Cmp::Ge),
        ("<=", Cmp::Le),
        (">", Cmp::Gt),
        ("<", Cmp::Lt),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            // Numeric compare only when the rest parses as a number; a
            // bare `>`/`<` with nothing after is not a valid filter (None),
            // and a non-numeric remainder falls back to exact string match
            // (e.g. `<3.5x` matches the literal string).
            return rest
                .parse::<f64>()
                .ok()
                .map(|n| FilterValue::Compare(cmp, n))
                .or_else(|| {
                    if rest.is_empty() {
                        None
                    } else {
                        Some(FilterValue::Exact(Value::Str(s.to_string())))
                    }
                });
        }
    }
    Some(FilterValue::Exact(Value::parse(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_permissively() {
        assert_eq!(Value::parse("3"), Value::Num(3.0));
        assert_eq!(Value::parse("0.6"), Value::Num(0.6));
        assert_eq!(Value::parse("-12.5"), Value::Num(-12.5));
        assert_eq!(Value::parse("1e3"), Value::Num(1000.0));
        assert_eq!(Value::parse("abc"), Value::Str("abc".into()));
        assert_eq!(Value::parse("12abc"), Value::Str("12abc".into()));
    }

    #[test]
    fn mixed_compare_orders_numbers_before_strings() {
        assert_eq!(Value::Num(1.0).cmp_value(&Value::Str("a".into())), Ordering::Less);
        assert_eq!(Value::Num(9.0).cmp_value(&Value::Num(2.0)), Ordering::Greater);
        assert_eq!(
            Value::Str("b".into()).cmp_value(&Value::Str("a".into())),
            Ordering::Greater
        );
    }

    #[test]
    fn parses_comparison_filters() {
        assert_eq!(
            parse_filter_value(">0.1"),
            Some(FilterValue::Compare(Cmp::Gt, 0.1))
        );
        assert_eq!(
            parse_filter_value("<=10"),
            Some(FilterValue::Compare(Cmp::Le, 10.0))
        );
        assert_eq!(
            parse_filter_value("free"),
            Some(FilterValue::Exact(Value::Str("free".into())))
        );
        assert_eq!(
            parse_filter_value("<3.5x"),
            Some(FilterValue::Exact(Value::Str("<3.5x".into())))
        );
        assert_eq!(parse_filter_value(">"), None);    }

    #[test]
    fn fmt_num_rounds_and_trims() {
        assert_eq!(fmt_num(2.5), "2.5");
        assert_eq!(fmt_num(10.0), "10");
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(0.1234), "0.12");
        assert_eq!(fmt_num(1.5), "1.5");
        assert_eq!(fmt_num(4096000.0), "4096000");
    }

    #[test]
    fn cmp_matches() {
        assert!(Cmp::Lt.matches(1.0, 2.0));
        assert!(!Cmp::Le.matches(2.1, 2.0));
        assert!(Cmp::Ge.matches(2.0, 2.0));
    }
}
