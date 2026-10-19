//! A modal, lossless Kotlin lexer.
//!
//! Every byte of the input is covered by exactly one token. String templates are
//! handled with a mode stack so that `"a ${ "b ${c}" } d"` lexes correctly.

use super::kind::SyntaxKind::{self, *};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub len: u32,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Regular code. `depth` counts unmatched `{` so we know when a `}` closes a
    /// long template entry.
    Code { depth: u32 },
    Str { triple: bool, dollars: usize },
}

pub fn lex(text: &str) -> Vec<Token> {
    let mut lx = Lexer { text, pos: 0, tokens: Vec::new(), modes: vec![Mode::Code { depth: 0 }] };
    lx.run();
    lx.tokens
}

struct Lexer<'a> {
    text: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    modes: Vec<Mode>,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

impl<'a> Lexer<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn push(&mut self, kind: SyntaxKind, len: usize) {
        debug_assert!(len > 0);
        self.tokens.push(Token { kind, len: len as u32 });
        self.pos += len;
    }

    fn run(&mut self) {
        // A UTF-8 byte order mark is insignificant.
        if self.rest().starts_with('\u{feff}') {
            self.push(WHITESPACE, '\u{feff}'.len_utf8());
        }
        if self.rest().starts_with("#!") {
            let len = self.rest().find('\n').unwrap_or(self.rest().len());
            self.push(SHEBANG, len);
        }
        while self.pos < self.text.len() {
            match *self.modes.last().unwrap() {
                Mode::Code { .. } => self.lex_code(),
                Mode::Str { triple, dollars } => self.lex_string(triple, dollars),
            }
        }
    }

    fn in_template(&self) -> bool {
        self.modes.len() > 1
    }

    fn lex_code(&mut self) {
        let rest = self.rest();
        let c = self.peek().unwrap();

        // whitespace / newlines
        if c == '\n' {
            return self.push(NEWLINE, 1);
        }
        if c == '\r' {
            let len = if rest.starts_with("\r\n") { 2 } else { 1 };
            return self.push(NEWLINE, len);
        }
        if c == ' ' || c == '\t' || c == '\u{c}' || c == '\u{a0}' || c == '\u{feff}' {
            let len = rest.find(|c| c != ' ' && c != '\t' && c != '\u{c}' && c != '\u{a0}' && c != '\u{feff}').unwrap_or(rest.len());
            return self.push(WHITESPACE, len);
        }

        // comments
        if rest.starts_with("//") {
            let len = rest.find(['\n', '\r']).unwrap_or(rest.len());
            return self.push(LINE_COMMENT, len);
        }
        if rest.starts_with("/*") {
            let kind = if rest.starts_with("/**") && !rest.starts_with("/**/") { KDOC } else { BLOCK_COMMENT };
            let len = block_comment_len(rest);
            return self.push(kind, len);
        }

        // strings (possibly with a multi-dollar prefix)
        if c == '"' || (c == '$' && rest.trim_start_matches('$').starts_with('"')) {
            let dollars = rest.len() - rest.trim_start_matches('$').len();
            let after = &rest[dollars..];
            let triple = after.starts_with("\"\"\"");
            let len = dollars + if triple { 3 } else { 1 };
            self.push(STRING_OPEN, len);
            self.modes.push(Mode::Str { triple, dollars: dollars.max(1) });
            return;
        }

        if c == '\'' {
            return self.lex_char();
        }

        if c.is_ascii_digit() || (c == '.' && self.peek_nth(1).is_some_and(|c| c.is_ascii_digit()) && !self.prev_is_dot_operand()) {
            return self.lex_number();
        }

        if c == '`' {
            let len = rest[1..].find(['`', '\n']).map(|i| i + 2).unwrap_or(rest.len());
            return self.push(IDENT, len);
        }

        if is_ident_start(c) {
            let len = rest.find(|c: char| !is_ident_continue(c)).unwrap_or(rest.len());
            let word = &rest[..len];
            let kind = SyntaxKind::from_keyword(word).unwrap_or(IDENT);
            if kind == AS_KW && rest[len..].starts_with('?') {
                return self.push(AS_SAFE_KW, len + 1);
            }
            return self.push(kind, len);
        }

        // braces interacting with templates
        if c == '{' {
            if let Some(Mode::Code { depth }) = self.modes.last_mut() {
                *depth += 1;
            }
            return self.push(L_CURLY, 1);
        }
        if c == '}' {
            let in_template = self.in_template();
            if let Some(Mode::Code { depth }) = self.modes.last_mut() {
                if *depth == 0 && in_template {
                    self.modes.pop();
                    return self.push(LONG_TEMPLATE_END, 1);
                }
                *depth = depth.saturating_sub(1);
            }
            return self.push(R_CURLY, 1);
        }

        if let Some((kind, len)) = self.operator() {
            return self.push(kind, len);
        }

        self.push(ERROR_TOKEN, c.len_utf8());
    }

    /// `x.1` is not valid Kotlin, but `foo.5f` would be weird; treat a `.digit`
    /// immediately after an identifier/`)` as member access rather than a float.
    fn prev_is_dot_operand(&self) -> bool {
        matches!(self.tokens.last(), Some(Token { kind: IDENT | R_PAREN | R_BRACK | INT_LITERAL, .. }))
    }

    fn operator(&self) -> Option<(SyntaxKind, usize)> {
        let rest = self.rest();
        const OPS: &[(&str, SyntaxKind)] = &[
            ("..<", RANGE_UNTIL),
            ("===", EQ3),
            ("!==", NOT_EQ2),
            ("?:", ELVIS),
            ("::", COLON2),
            ("->", ARROW),
            ("=>", DOUBLE_ARROW),
            ("..", RANGE),
            ("==", EQ2),
            ("!=", NOT_EQ),
            ("<=", LT_EQ),
            (">=", GT_EQ),
            ("&&", AMP2),
            ("||", PIPE2),
            ("++", PLUS2),
            ("--", MINUS2),
            ("+=", PLUS_EQ),
            ("-=", MINUS_EQ),
            ("*=", STAR_EQ),
            ("/=", SLASH_EQ),
            ("%=", PERCENT_EQ),
            ("!!", EXCL2),
            ("(", L_PAREN),
            (")", R_PAREN),
            ("[", L_BRACK),
            ("]", R_BRACK),
            (",", COMMA),
            (".", DOT),
            (";", SEMI),
            (":", COLON),
            ("?", QUESTION),
            ("@", AT),
            ("#", HASH),
            ("$", DOLLAR),
            ("=", EQ),
            ("+", PLUS),
            ("-", MINUS),
            ("*", STAR),
            ("/", SLASH),
            ("%", PERCENT),
            ("&", AMP),
            ("!", EXCL),
            ("<", LT),
            (">", GT),
        ];
        // `!in` / `!is` are single tokens when followed by a non-identifier char.
        for (kw, kind) in [("!in", NOT_IN_KW), ("!is", NOT_IS_KW)] {
            if rest.starts_with(kw) && !rest[kw.len()..].chars().next().is_some_and(is_ident_continue) {
                return Some((kind, kw.len()));
            }
        }
        if rest.starts_with("?::") {
            return Some((QUESTION, 1));
        }
        OPS.iter().find(|(op, _)| rest.starts_with(op)).map(|(op, kind)| (*kind, op.len()))
    }

    fn lex_number(&mut self) {
        let bytes = self.rest().as_bytes();
        let mut i = 0;
        let mut is_float = false;
        let digits = |i: &mut usize, pred: fn(u8) -> bool| {
            while *i < bytes.len() && (pred(bytes[*i]) || bytes[*i] == b'_') {
                *i += 1;
            }
        };
        if bytes.len() > 1 && bytes[0] == b'0' && matches!(bytes[1], b'x' | b'X') {
            i = 2;
            digits(&mut i, |b| b.is_ascii_hexdigit());
        } else if bytes.len() > 1 && bytes[0] == b'0' && matches!(bytes[1], b'b' | b'B') {
            i = 2;
            digits(&mut i, |b| b == b'0' || b == b'1');
        } else {
            digits(&mut i, |b| b.is_ascii_digit());
            if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
                is_float = true;
                i += 1;
                digits(&mut i, |b| b.is_ascii_digit());
            }
            if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                let mut j = i + 1;
                if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    is_float = true;
                    i = j;
                    digits(&mut i, |b| b.is_ascii_digit());
                }
            }
            if i < bytes.len() && matches!(bytes[i], b'f' | b'F') {
                is_float = true;
                i += 1;
            }
        }
        if !is_float {
            if i < bytes.len() && matches!(bytes[i], b'u' | b'U') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'L' {
                i += 1;
            }
        }
        // Swallow trailing identifier characters into the literal (it is an error
        // either way, but keeps the token stream sane).
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        self.push(if is_float { FLOAT_LITERAL } else { INT_LITERAL }, i);
    }

    fn lex_char(&mut self) {
        let rest = self.rest();
        let mut chars = rest.char_indices().skip(1);
        let mut end = rest.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '\'' => {
                    end = i + 1;
                    break;
                }
                '\n' | '\r' => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        self.push(CHAR_LITERAL, end);
    }

    fn lex_string(&mut self, triple: bool, dollars: usize) {
        let rest = self.rest();
        let c = self.peek().unwrap();

        if c == '"' {
            if !triple {
                self.modes.pop();
                return self.push(STRING_CLOSE, 1);
            }
            let run = rest.len() - rest.trim_start_matches('"').len();
            if run >= 3 {
                if run > 3 {
                    return self.push(STRING_TEXT, run - 3);
                }
                self.modes.pop();
                return self.push(STRING_CLOSE, 3);
            }
            return self.push(STRING_TEXT, run);
        }

        if !triple && (c == '\n' || c == '\r') {
            // Unterminated single-line string: leave string mode, the parser reports it.
            self.modes.pop();
            return;
        }

        if !triple && c == '\\' {
            let len = escape_len(rest);
            return self.push(STRING_ESCAPE, len);
        }

        if c == '$' {
            let run = rest.len() - rest.trim_start_matches('$').len();
            let after = &rest[run..];
            if run >= dollars {
                let extra = run - dollars;
                let next = after.chars().next();
                if next == Some('{') {
                    if extra > 0 {
                        return self.push(STRING_TEXT, extra);
                    }
                    self.push(LONG_TEMPLATE_START, dollars + 1);
                    self.modes.push(Mode::Code { depth: 0 });
                    return;
                }
                if next.is_some_and(|c| is_ident_start(c) || c == '`') {
                    if extra > 0 {
                        return self.push(STRING_TEXT, extra);
                    }
                    self.push(SHORT_TEMPLATE_START, dollars);
                    let rest = self.rest();
                    let len = if let Some(quoted) = rest.strip_prefix('`') {
                        quoted.find(['`', '\n']).map(|i| i + 2).unwrap_or(rest.len())
                    } else {
                        rest.find(|c: char| !is_ident_continue(c)).unwrap_or(rest.len())
                    };
                    let kind = match SyntaxKind::from_keyword(&rest[..len]) {
                        Some(THIS_KW) => THIS_KW,
                        _ => IDENT,
                    };
                    return self.push(kind, len);
                }
            }
            return self.push(STRING_TEXT, run);
        }

        // plain text up to the next interesting character
        let stop: &[char] = if triple { &['"', '$'] } else { &['"', '$', '\\', '\n', '\r'] };
        let len = rest.find(stop).unwrap_or(rest.len());
        self.push(STRING_TEXT, len.max(c.len_utf8()));
    }
}

fn block_comment_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    text.len()
}

fn escape_len(text: &str) -> usize {
    // text starts with '\'
    let mut chars = text.char_indices().skip(1);
    match chars.next() {
        Some((_, 'u')) => {
            let hex = text[2..].chars().take(4).take_while(|c| c.is_ascii_hexdigit()).count();
            2 + hex
        }
        Some((i, c)) => i + c.len_utf8(),
        None => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(SyntaxKind, String)> {
        let mut pos = 0;
        lex(text)
            .into_iter()
            .map(|t| {
                let s = text[pos..pos + t.len as usize].to_string();
                pos += t.len as usize;
                (t.kind, s)
            })
            .filter(|(k, _)| *k != WHITESPACE)
            .collect()
    }

    fn lossless(text: &str) {
        let total: u32 = lex(text).iter().map(|t| t.len).sum();
        assert_eq!(total as usize, text.len(), "lexer lost bytes for {text:?}");
    }

    #[test]
    fn keywords_and_operators() {
        let toks: Vec<_> = lex("val x = a ?: b.c!! as? T !in xs !is Y ..< 1..2")
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != WHITESPACE)
            .collect();
        assert_eq!(
            toks,
            vec![
                VAL_KW, IDENT, EQ, IDENT, ELVIS, IDENT, DOT, IDENT, EXCL2, AS_SAFE_KW, IDENT, NOT_IN_KW, IDENT,
                NOT_IS_KW, IDENT, RANGE_UNTIL, INT_LITERAL, RANGE, INT_LITERAL
            ]
        );
    }

    #[test]
    fn numbers() {
        let ks: Vec<_> = kinds("1 1_000L 0xFFu 0b1010 1.5 1e10 2f .5 3.toString()").into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            ks,
            vec![
                INT_LITERAL,
                INT_LITERAL,
                INT_LITERAL,
                INT_LITERAL,
                FLOAT_LITERAL,
                FLOAT_LITERAL,
                FLOAT_LITERAL,
                FLOAT_LITERAL,
                INT_LITERAL,
                DOT,
                IDENT,
                L_PAREN,
                R_PAREN
            ]
        );
    }

    #[test]
    fn string_templates() {
        let toks = kinds(r#""a $b ${c + "d${e}"} \n""#);
        let ks: Vec<_> = toks.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            ks,
            vec![
                STRING_OPEN,
                STRING_TEXT,
                SHORT_TEMPLATE_START,
                IDENT,
                STRING_TEXT,
                LONG_TEMPLATE_START,
                IDENT,
                PLUS,
                STRING_OPEN,
                STRING_TEXT,
                LONG_TEMPLATE_START,
                IDENT,
                LONG_TEMPLATE_END,
                STRING_CLOSE,
                LONG_TEMPLATE_END,
                STRING_TEXT,
                STRING_ESCAPE,
                STRING_CLOSE
            ]
        );
    }

    #[test]
    fn raw_and_multi_dollar_strings() {
        let toks = kinds("$$\"\"\"a $b $$c \"\"\"\"");
        let ks: Vec<_> = toks.iter().map(|(k, _)| *k).collect();
        assert_eq!(ks, vec![STRING_OPEN, STRING_TEXT, STRING_TEXT, STRING_TEXT, SHORT_TEMPLATE_START, IDENT, STRING_TEXT, STRING_TEXT, STRING_CLOSE]);
    }

    #[test]
    fn braces_inside_template_lambdas() {
        let toks = kinds(r#""${list.map { it }}""#);
        assert_eq!(toks.last().unwrap().0, STRING_CLOSE);
        assert!(toks.iter().any(|(k, _)| *k == R_CURLY));
        assert!(toks.iter().any(|(k, _)| *k == LONG_TEMPLATE_END));
    }

    #[test]
    fn comments() {
        let toks = kinds("/* a /* nested */ b */ /** doc */ // line\nx");
        let ks: Vec<_> = toks.iter().map(|(k, _)| *k).collect();
        assert_eq!(ks, vec![BLOCK_COMMENT, KDOC, LINE_COMMENT, NEWLINE, IDENT]);
    }

    #[test]
    fn byte_order_mark() {
        assert_eq!(kinds("\u{feff}package a"), vec![(PACKAGE_KW, "package".into()), (IDENT, "a".into())]);
    }

    #[test]
    fn is_lossless() {
        for t in ["\"unterminated\nfoo", "'a' '\\n' '\\u0041'", "`weird name` @Ann x@", "/* open", "\"${", "#!/usr/bin/env kotlin\nprintln()", "é ü 中文"] {
            lossless(t);
        }
    }
}
