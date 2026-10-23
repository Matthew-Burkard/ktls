//! Constant folding for hover: the value of `const val`s and of `val`s initialized with
//! constant expressions (literals, operators, string templates, other such `val`s).

use super::infer::{Def, Engine, LocalKind};
use crate::analysis::lints::{binary_op, initializer};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind};
use crate::syntax::{SyntaxKind, SyntaxNode};

#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i32),
    Long(i64),
    Double(f64),
    Float(f32),
    Bool(bool),
    Char(char),
    Str(String),
}

impl Const {
    /// As written in Kotlin source.
    pub fn render(&self) -> String {
        match self {
            Const::Int(v) => v.to_string(),
            Const::Long(v) => format!("{v}L"),
            Const::Double(v) => {
                let s = v.to_string();
                if s.contains(['.', 'e', 'E', 'N', 'i']) { s } else { format!("{s}.0") }
            }
            Const::Float(v) => format!("{v}f"),
            Const::Bool(v) => v.to_string(),
            Const::Char(c) => format!("'{}'", c.escape_default()),
            Const::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")),
        }
    }

    /// The value as a string template inserts it.
    fn text(&self) -> String {
        match self {
            Const::Str(s) => s.clone(),
            Const::Char(c) => c.to_string(),
            Const::Long(v) => v.to_string(),
            Const::Float(v) => v.to_string(),
            other => other.render(),
        }
    }
}

/// The value of `x`, if it is a constant expression.
pub fn evaluate(e: &Engine, x: &SyntaxNode) -> Option<Const> {
    eval(e, x, 0)
}

fn eval(e: &Engine, x: &SyntaxNode, depth: u32) -> Option<Const> {
    if depth > 16 {
        return None;
    }
    match x.kind() {
        SyntaxKind::LITERAL => literal(x),
        SyntaxKind::PAREN_EXPR => eval(e, &x.children().last()?, depth + 1),
        SyntaxKind::STRING_TEMPLATE => {
            let mut s = String::new();
            for part in x.children() {
                match part.kind() {
                    SyntaxKind::LITERAL_STRING_ENTRY => s.push_str(&part.text().to_string()),
                    SyntaxKind::ESCAPE_STRING_ENTRY => s.push(unescape(&part.text().to_string())?),
                    SyntaxKind::SHORT_STRING_TEMPLATE_ENTRY | SyntaxKind::LONG_STRING_TEMPLATE_ENTRY => s.push_str(&eval(e, &part.children().last()?, depth + 1)?.text()),
                    _ => return None,
                }
            }
            Some(Const::Str(s))
        }
        SyntaxKind::PREFIX_EXPR => {
            let op = binary_op(x)?;
            let v = eval(e, &x.children().last()?, depth + 1)?;
            match (op.kind(), v) {
                (SyntaxKind::MINUS, Const::Int(v)) => Some(Const::Int(v.wrapping_neg())),
                (SyntaxKind::MINUS, Const::Long(v)) => Some(Const::Long(v.wrapping_neg())),
                (SyntaxKind::MINUS, Const::Double(v)) => Some(Const::Double(-v)),
                (SyntaxKind::MINUS, Const::Float(v)) => Some(Const::Float(-v)),
                (SyntaxKind::PLUS, v @ (Const::Int(_) | Const::Long(_) | Const::Double(_) | Const::Float(_))) => Some(v),
                (SyntaxKind::EXCL, Const::Bool(b)) => Some(Const::Bool(!b)),
                _ => None,
            }
        }
        SyntaxKind::BINARY_EXPR => {
            let op = binary_op(x)?;
            let mut operands = x.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE);
            let (l, r) = (eval(e, &operands.next()?, depth + 1)?, eval(e, &operands.next()?, depth + 1)?);
            binary(op.kind(), l, r)
        }
        SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR => {
            let name = if x.kind() == SyntaxKind::NAME_REF { x.clone() } else { x.last_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)? };
            match e.resolve_name_ref(&name).as_slice() {
                [Def::Local(l)] if l.kind == LocalKind::Val => {
                    let decl = crate::syntax::covering(&e.root, l.name_range)?.ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY)?;
                    eval(e, &initializer(&decl)?, depth + 1)
                }
                [Def::Symbol { sym, .. }] => symbol_value(e, sym, depth + 1),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The value of a project `val` (not `var`, no getter, no delegate) with a constant initializer.
pub fn symbol_value(e: &Engine, sym: &Symbol, depth: u32) -> Option<Const> {
    if sym.kind != SymbolKind::Property || sym.modifiers.has(Modifiers::VAR) {
        return None;
    }
    let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
    let other;
    let e2: &Engine = if *file == e.path {
        e
    } else {
        other = Engine::new(e.snap, file)?;
        &other
    };
    let decl = crate::syntax::covering(&e2.root, *name_range)?.ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY)?;
    if decl.children().any(|c| matches!(c.kind(), SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::PROPERTY_DELEGATE)) {
        return None;
    }
    eval(e2, &initializer(&decl)?, depth)
}

/// The value of a declaration (`const val`, or `val` whose initializer is not already
/// that value written out), for hover.
pub fn shown_value(e: &Engine, decl: &SyntaxNode, is_const: bool) -> Option<String> {
    let init = initializer(decl)?;
    let v = eval(e, &init, 0)?.render();
    (is_const || v != init.text().to_string().trim()).then_some(v)
}

fn literal(x: &SyntaxNode) -> Option<Const> {
    let tok = x.first_token()?;
    let text = tok.text().replace('_', "");
    match tok.kind() {
        SyntaxKind::TRUE_KW => Some(Const::Bool(true)),
        SyntaxKind::FALSE_KW => Some(Const::Bool(false)),
        SyntaxKind::CHAR_LITERAL => {
            let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
            if inner.starts_with('\\') { unescape(inner).map(Const::Char) } else { inner.chars().next().map(Const::Char) }
        }
        SyntaxKind::INT_LITERAL => {
            let lower = text.to_ascii_lowercase();
            if lower.contains('u') {
                return None;
            }
            let (digits, long) = match lower.strip_suffix('l') {
                Some(d) => (d, true),
                None => (lower.as_str(), false),
            };
            let v = if let Some(h) = digits.strip_prefix("0x") {
                i64::from_str_radix(h, 16).ok()?
            } else if let Some(b) = digits.strip_prefix("0b") {
                i64::from_str_radix(b, 2).ok()?
            } else {
                digits.parse::<i64>().ok()?
            };
            // Without `L`, a value that fits an Int is an Int.
            Some(if !long && i32::try_from(v).is_ok() { Const::Int(v as i32) } else { Const::Long(v) })
        }
        SyntaxKind::FLOAT_LITERAL => {
            let lower = text.to_ascii_lowercase();
            match lower.strip_suffix('f') {
                Some(f) => f.parse().ok().map(Const::Float),
                None => lower.parse().ok().map(Const::Double),
            }
        }
        _ => None,
    }
}

fn unescape(s: &str) -> Option<char> {
    let rest = s.strip_prefix('\\')?;
    Some(match rest.chars().next()? {
        't' => '\t',
        'b' => '\u{8}',
        'n' => '\n',
        'r' => '\r',
        '\'' => '\'',
        '"' => '"',
        '\\' => '\\',
        '$' => '$',
        'u' => char::from_u32(u32::from_str_radix(rest.get(1..5)?, 16).ok()?)?,
        _ => return None,
    })
}

/// A number widened to the type both operands share (Int < Long < Float < Double).
fn widen(l: &Const, r: &Const) -> Option<(Const, Const)> {
    use Const::*;
    let rank = |c: &Const| match c {
        Int(_) => Some(0),
        Long(_) => Some(1),
        Float(_) => Some(2),
        Double(_) => Some(3),
        _ => None,
    };
    let to = |c: &Const, r: u8| -> Option<Const> {
        let f = match c {
            Int(v) => *v as f64,
            Long(v) => *v as f64,
            Float(v) => *v as f64,
            Double(v) => *v,
            _ => return None,
        };
        Some(match (c, r) {
            (Int(v), 1) => Long(*v as i64),
            (_, 2) => Float(f as f32),
            (_, 3) => Double(f),
            _ => c.clone(),
        })
    };
    let target = rank(l)?.max(rank(r)?);
    Some((to(l, target)?, to(r, target)?))
}

fn binary(op: SyntaxKind, l: Const, r: Const) -> Option<Const> {
    use Const::*;
    use SyntaxKind::*;
    // Strings: `+` with anything, equality.
    if let (Str(a), b) = (&l, &r) {
        return match op {
            PLUS => Some(Str(format!("{a}{}", b.text()))),
            EQ2 => Some(Bool(Str(a.clone()) == *b)),
            NOT_EQ => Some(Bool(Str(a.clone()) != *b)),
            _ => None,
        };
    }
    if let (Bool(a), Bool(b)) = (&l, &r) {
        return match op {
            AMP2 => Some(Bool(*a && *b)),
            PIPE2 => Some(Bool(*a || *b)),
            EQ2 => Some(Bool(a == b)),
            NOT_EQ => Some(Bool(a != b)),
            _ => None,
        };
    }
    let (l, r) = widen(&l, &r)?;
    let cmp = |o: std::cmp::Ordering| -> Option<Const> {
        Some(Bool(match op {
            LT => o.is_lt(),
            GT => o.is_gt(),
            LT_EQ => o.is_le(),
            GT_EQ => o.is_ge(),
            EQ2 => o.is_eq(),
            NOT_EQ => o.is_ne(),
            _ => return None,
        }))
    };
    match (l, r) {
        (Int(a), Int(b)) => match op {
            PLUS => Some(Int(a.wrapping_add(b))),
            MINUS => Some(Int(a.wrapping_sub(b))),
            STAR => Some(Int(a.wrapping_mul(b))),
            SLASH => a.checked_div(b).map(Int),
            PERCENT => a.checked_rem(b).map(Int),
            _ => cmp(a.cmp(&b)),
        },
        (Long(a), Long(b)) => match op {
            PLUS => Some(Long(a.wrapping_add(b))),
            MINUS => Some(Long(a.wrapping_sub(b))),
            STAR => Some(Long(a.wrapping_mul(b))),
            SLASH => a.checked_div(b).map(Long),
            PERCENT => a.checked_rem(b).map(Long),
            _ => cmp(a.cmp(&b)),
        },
        (Double(a), Double(b)) => match op {
            PLUS => Some(Double(a + b)),
            MINUS => Some(Double(a - b)),
            STAR => Some(Double(a * b)),
            SLASH => Some(Double(a / b)),
            PERCENT => Some(Double(a % b)),
            _ => cmp(a.partial_cmp(&b)?),
        },
        (Float(a), Float(b)) => match op {
            PLUS => Some(Float(a + b)),
            MINUS => Some(Float(a - b)),
            STAR => Some(Float(a * b)),
            SLASH => Some(Float(a / b)),
            PERCENT => Some(Float(a % b)),
            _ => cmp(a.partial_cmp(&b)?),
        },
        _ => None,
    }
}
