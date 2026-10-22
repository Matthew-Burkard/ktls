//! Lexical errors the lexer lets through: malformed character, string and number literals,
//! and comments that are never closed.

use super::kind::SyntaxKind::*;
use super::lexer::Token;
use super::parser::SyntaxError;
use rowan::{TextRange, TextSize};

pub(crate) fn errors(text: &str, tokens: &[Token]) -> Vec<SyntaxError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    for t in tokens {
        let end = at + t.len as usize;
        let s = &text[at..end];
        let range = TextRange::new(TextSize::from(at as u32), TextSize::from(end as u32));
        let message = match t.kind {
            CHAR_LITERAL => char_literal(s),
            STRING_ESCAPE => escape(s).err(),
            INT_LITERAL => int_literal(s),
            FLOAT_LITERAL => float_literal(s),
            BLOCK_COMMENT | KDOC if !s.ends_with("*/") || s.len() < 4 => Some("Unclosed comment".to_string()),
            _ => None,
        };
        if let Some(message) = message {
            out.push(SyntaxError { message, range });
        }
        at = end;
    }
    out
}

/// An escape sequence (`\n`, `\u0041`); its error message if illegal.
fn escape(s: &str) -> Result<(), String> {
    let rest = s.strip_prefix('\\').ok_or_else(|| "Illegal escape".to_string())?;
    match rest.chars().next() {
        Some('t' | 'b' | 'n' | 'r' | '\'' | '"' | '\\' | '$') if rest.len() == 1 => Ok(()),
        Some('u') if rest.len() == 5 && rest[1..].chars().all(|c| c.is_ascii_hexdigit()) => Ok(()),
        _ => Err(format!("Illegal escape: '{s}'")),
    }
}

fn char_literal(s: &str) -> Option<String> {
    let Some(inner) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')).filter(|_| s.len() >= 2) else { return Some("Incorrect character literal".into()) };
    if inner.is_empty() {
        return Some("Empty character literal".into());
    }
    if inner.starts_with('\\') {
        return escape(inner).err();
    }
    (inner.chars().count() > 1).then(|| "Too many characters in a character literal".into())
}

/// Digits with `_` separators: not first, not last, and not next to a prefix or point.
fn digits_ok(digits: &str, radix: u32) -> bool {
    !digits.is_empty() && !digits.starts_with('_') && !digits.ends_with('_') && digits.chars().all(|c| c == '_' || c.is_digit(radix))
}

fn int_literal(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    // Suffixes: `L`, `u`, `uL` (lowercase `l` is not allowed).
    let (body, unsigned, long) = if let Some(b) = lower.strip_suffix("ul") {
        (&s[..b.len()], true, true)
    } else if let Some(b) = lower.strip_suffix('l') {
        (&s[..b.len()], false, true)
    } else if let Some(b) = lower.strip_suffix('u') {
        (&s[..b.len()], true, false)
    } else {
        (s, false, false)
    };
    if long && s.ends_with('l') {
        return Some("Use 'L' instead of 'l'".into());
    }
    let (digits, radix) = if let Some(d) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        (d, 16)
    } else if let Some(d) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        (d, 2)
    } else {
        (body, 10)
    };
    if !digits_ok(digits, radix) {
        return Some(if digits.contains('_') && digits.chars().all(|c| c == '_' || c.is_digit(radix)) { "Illegal underscore".into() } else { "Malformed number literal".into() });
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let Ok(value) = u128::from_str_radix(&clean, radix) else { return Some("The value is out of range".into()) };
    // Literals without `L` may still be Long when they do not fit an Int; signed ones
    // (hexadecimal and binary too) must fit a Long.
    let max: u128 = if unsigned { u64::MAX as u128 } else { i64::MAX as u128 };
    (value > max).then(|| "The value is out of range".into())
}

fn float_literal(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    if lower.ends_with('l') || lower.ends_with('u') {
        return Some(format!("Unsupported suffix '{}' on a floating-point literal", &s[s.len() - 1..]));
    }
    let float = lower.ends_with('f');
    let body = if float { &s[..s.len() - 1] } else { s };
    // `_` must sit between digits.
    let bad_underscore = body.char_indices().any(|(i, c)| {
        c == '_' && !(body[..i].chars().last().is_some_and(|p| p.is_ascii_digit() || p == '_') && body[i + 1..].chars().next().is_some_and(|n| n.is_ascii_digit() || n == '_'))
    }) || body.starts_with('_') || body.ends_with('_');
    if bad_underscore {
        return Some("Illegal underscore".into());
    }
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    let out_of_range = if float { clean.parse::<f32>().is_ok_and(|v| v.is_infinite()) } else { clean.parse::<f64>().is_ok_and(|v| v.is_infinite()) };
    if out_of_range {
        return Some("The value is out of range".into());
    }
    None
}

#[cfg(test)]
mod tests {
    fn check(src: &str) -> Vec<String> {
        crate::syntax::parse(src, false).errors().iter().map(|e| format!("{}: {}", &src[usize::from(e.range.start())..usize::from(e.range.end())], e.message)).collect()
    }

    #[test]
    fn malformed_literals() {
        let errs = check("val a = ''\nval b = 'ab'\nval c = \"\\q\"\nval d = 1_\nval e = 0x\nval f = 1.0L\nval g = 0b2\nval h = 1.5u\nval i = 100000000000000000000L\nval j = '\\u12'\nval k = 10l\nval l = 1e999\n");
        let expected = [
            "'': Empty character literal",
            "'ab': Too many characters in a character literal",
            "\\q: Illegal escape: '\\q'",
            "1_: Illegal underscore",
            "0x: Malformed number literal",
            "1.0L: Unsupported suffix 'L' on a floating-point literal",
            "0b2: Malformed number literal",
            "1.5u: Unsupported suffix 'u' on a floating-point literal",
            "100000000000000000000L: The value is out of range",
            "'\\u12': Illegal escape: '\\u12'",
            "10l: Use 'L' instead of 'l'",
            "1e999: The value is out of range",
        ];
        assert_eq!(errs, expected);
        // Valid ones.
        assert!(check("val a = 'x'\nval b = '\\n'\nval c = '\\u0041'\nval d = \"\\t\\$\"\nval e = 1_000_000L\nval f = 0xFF_FFu\nval g = 1__0\nval h = 2147483648\nval i = 0xFFFFFFFFFFFFFFFFuL\nval j = 1.5e10f\nval k = 3.14_15\n").is_empty());
        assert_eq!(check("/* open"), ["/* open: Unclosed comment"]);
        assert_eq!(check("val x = 0xFFFFFFFFFFFFFFFF"), ["0xFFFFFFFFFFFFFFFF: The value is out of range"]);
    }
}
