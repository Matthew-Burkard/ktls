//! Idiomatic-Kotlin suggestions.

use super::*;
use crate::analysis::ide::infer::{Def, fq_of};
use crate::symbols::{Modifiers, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::FUN => expression_body(&n, out),
            SyntaxKind::PROPERTY => may_be_constant(&n, out),
            SyntaxKind::BINARY_EXPR => {
                ranges(cx, &n, out);
                size_check(cx, &n, out);
            }
            SyntaxKind::DOT_QUALIFIED_EXPR => operator_calls(cx, &n, out),
            SyntaxKind::IF_EXPR | SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR => empty_bodies(&n, out),
            _ => {}
        }
    }
}

fn expression_body(f: &SyntaxNode, out: &mut Sink) {
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let (Some(_), Some(block)) = (decl.return_type(), decl.body_block()) else { return };
    let block = block.syntax().clone();
    let Some(ret) = only_statement(&block) else { return };
    if ret.kind() != SyntaxKind::RETURN_EXPR || ret.children().any(|c| c.kind() == SyntaxKind::LABEL) || has_comments(&block) {
        return;
    }
    let (Some(value), Some(kw)) = (ret.first_child(), ret.first_token()) else { return };
    out.report(
        &USE_EXPRESSION_BODY,
        kw.text_range(),
        "Use expression body instead of 'return'",
        Some(Fix::replace("Convert to expression body", block.text_range(), format!("= {}", value.text()))),
    );
}

const CONST_TYPES: &[&str] = &["Int", "Long", "Short", "Byte", "Float", "Double", "Boolean", "Char", "String", "UInt", "ULong", "UShort", "UByte"];

fn may_be_constant(p: &SyntaxNode, out: &mut Sink) {
    let Some(parent) = p.parent() else { return };
    let top_level_or_object = parent.kind() == SyntaxKind::KOTLIN_FILE
        || parent.kind() == SyntaxKind::CLASS_BODY && parent.parent().is_some_and(|o| o.kind() == SyntaxKind::OBJECT_DECLARATION);
    if !top_level_or_object {
        return;
    }
    let decl = ast::PropertyDecl::cast(p.clone()).unwrap();
    if decl.is_var() || decl.has_delegate() || decl.receiver().is_some() || decl.accessors().next().is_some() || !decl.type_parameters().is_empty() {
        return;
    }
    if let Some(m) = modifiers(p)
        && (m.annotations().next().is_some() || m.modifiers().any(|t| !matches!(t.text(), "private" | "internal" | "public" | "protected")))
    {
        return;
    }
    if decl.type_ref().is_some_and(|t| !CONST_TYPES.contains(&t.syntax().text().to_string().as_str())) {
        return;
    }
    let Some(init) = initializer(p) else { return };
    let constant = match init.kind() {
        SyntaxKind::LITERAL => init.first_token().is_some_and(|t| t.kind() != SyntaxKind::NULL_KW),
        SyntaxKind::PREFIX_EXPR => {
            binary_op(&init).is_some_and(|o| o.kind() == SyntaxKind::MINUS)
                && init.children().last().is_some_and(|l| l.kind() == SyntaxKind::LITERAL && l.first_token().is_some_and(|t| matches!(t.kind(), SyntaxKind::INT_LITERAL | SyntaxKind::FLOAT_LITERAL)))
        }
        SyntaxKind::STRING_TEMPLATE => init.children().all(|c| matches!(c.kind(), SyntaxKind::LITERAL_STRING_ENTRY | SyntaxKind::ESCAPE_STRING_ENTRY)),
        _ => false,
    };
    let (Some(name), Some(val)) = (name_token(p), ast::token(p, SyntaxKind::VAL_KW)) else { return };
    if constant {
        out.report(
            &MAY_BE_CONSTANT,
            name.text_range(),
            format!("'{}' can be a 'const val'", ast::ident_text(&name)),
            Some(Fix::replace("Add 'const' modifier", TextRange::empty(val.text_range().start()), "const ")),
        );
    }
}

/// What kind of sized container a value is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Collection,
    Map,
    Array,
    CharSequence,
}

fn container(cx: &Cx, recv: &SyntaxNode) -> Option<Container> {
    let e = cx.engine()?;
    let t = e.type_of(recv)?;
    if matches!(t, TypeRef::Named { nullable: true, .. }) {
        return None;
    }
    e.supertype_closure(&t).iter().find_map(|s| match fq_of(s)? {
        "kotlin.collections.Collection" => Some(Container::Collection),
        "kotlin.collections.Map" => Some(Container::Map),
        "kotlin.Array" | "kotlin.IntArray" | "kotlin.LongArray" | "kotlin.ShortArray" | "kotlin.ByteArray" | "kotlin.CharArray" | "kotlin.FloatArray"
        | "kotlin.DoubleArray" | "kotlin.BooleanArray" => Some(Container::Array),
        "kotlin.CharSequence" => Some(Container::CharSequence),
        _ => None,
    })
}

/// `recv.size` / `recv.length`: the receiver, if its type really has that property.
fn size_of(cx: &Cx, e: &SyntaxNode, indices: bool) -> Option<SyntaxNode> {
    if e.kind() != SyntaxKind::DOT_QUALIFIED_EXPR {
        return None;
    }
    let mut parts = e.children();
    let (recv, member) = (parts.next()?, parts.next()?);
    if member.kind() != SyntaxKind::NAME_REF || recv.kind() == SyntaxKind::SUPER_EXPR {
        return None;
    }
    let ok = match (member.text().to_string().as_str(), container(cx, &recv)?) {
        ("size", Container::Collection | Container::Array) => true,
        ("size", Container::Map) => !indices,
        ("length", Container::CharSequence) => true,
        _ => false,
    };
    ok.then_some(recv)
}

fn is_literal(n: &SyntaxNode, text: &str) -> bool {
    n.kind() == SyntaxKind::LITERAL && n.text() == text
}

/// `x - 1`: the `x`.
fn minus_one(n: &SyntaxNode) -> Option<SyntaxNode> {
    if n.kind() != SyntaxKind::BINARY_EXPR || binary_op(n)?.kind() != SyntaxKind::MINUS {
        return None;
    }
    let (l, r) = binary_operands(n)?;
    is_literal(&r, "1").then_some(l)
}

fn ranges(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    let Some((lo, hi)) = binary_operands(n) else { return };
    // The exclusive upper bound, if this is a half-open range.
    let (upper, closed_minus_one) = match op.kind() {
        SyntaxKind::RANGE => match minus_one(&hi) {
            Some(u) => (u, true),
            None => return,
        },
        SyntaxKind::RANGE_UNTIL => (hi, false),
        SyntaxKind::IDENT if op.text() == "until" => (hi, false),
        _ => return,
    };
    if is_literal(&lo, "0") && cx.on(&MANUAL_INDICES)
        && let Some(recv) = size_of(cx, &upper, true)
    {
        out.report(
            &MANUAL_INDICES,
            n.text_range(),
            "Range can be replaced with 'indices'",
            Some(Fix::replace("Replace with 'indices'", n.text_range(), format!("{}.indices", recv.text()))),
        );
        return;
    }
    if closed_minus_one {
        let modern = cx.language_version().is_none_or(|v| v >= 109);
        let text = if modern { format!("{}..<{}", lo.text(), upper.text()) } else { format!("{} until {}", lo.text(), upper.text()) };
        let op_name = if modern { "'..<'" } else { "'until'" };
        out.report(&RANGE_UNTIL, n.text_range(), format!("'..' with '- 1' can be replaced with {op_name}"), Some(Fix::replace(format!("Replace with {op_name}"), n.text_range(), text)));
    }
}

fn size_check(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(op) = binary_op(n) else { return };
    if !matches!(op.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::GT | SyntaxKind::GT_EQ | SyntaxKind::LT | SyntaxKind::LT_EQ) {
        return;
    }
    if !cx.on(&SIZE_CHECK_IS_EMPTY) && !cx.on(&SIZE_CHECK_IS_NOT_EMPTY) {
        return;
    }
    let Some((l, r)) = binary_operands(n) else { return };
    // Normalise to `size <op> literal`.
    let (size, lit, op) = if l.kind() == SyntaxKind::LITERAL {
        let flipped = match op.kind() {
            SyntaxKind::GT => SyntaxKind::LT,
            SyntaxKind::GT_EQ => SyntaxKind::LT_EQ,
            SyntaxKind::LT => SyntaxKind::GT,
            SyntaxKind::LT_EQ => SyntaxKind::GT_EQ,
            k => k,
        };
        (r, l, flipped)
    } else {
        (l, r, op.kind())
    };
    let empty = match (op, lit.text().to_string().as_str()) {
        (SyntaxKind::EQ2 | SyntaxKind::LT_EQ, "0") | (SyntaxKind::LT, "1") => true,
        (SyntaxKind::NOT_EQ | SyntaxKind::GT, "0") | (SyntaxKind::GT_EQ, "1") => false,
        _ => return,
    };
    if lit.kind() != SyntaxKind::LITERAL {
        return;
    }
    let Some(recv) = size_of(cx, &size, false) else { return };
    let (rule, call) = if empty { (&SIZE_CHECK_IS_EMPTY, "isEmpty()") } else { (&SIZE_CHECK_IS_NOT_EMPTY, "isNotEmpty()") };
    out.report(rule, n.text_range(), format!("Size check can be replaced with '{call}'"), Some(Fix::replace(format!("Replace with '{call}'"), n.text_range(), format!("{}.{call}", recv.text()))));
}

const BINARY_OPERATORS: &[(&str, &str)] = &[("plus", "+"), ("minus", "-"), ("times", "*"), ("div", "/"), ("rem", "%"), ("rangeTo", ".."), ("rangeUntil", "..<")];

/// `a.get(i)`, `a.set(i, v)`, `a.plus(b)`... calling an operator function by name.
fn operator_calls(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if call.kind() != SyntaxKind::CALL_EXPR || recv.kind() == SyntaxKind::SUPER_EXPR {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let name = callee.text().to_string();
    let binary = BINARY_OPERATORS.iter().find(|(n, _)| *n == name).map(|(_, op)| *op);
    if name != "get" && name != "set" && binary.is_none() {
        return;
    }
    let rule = if binary.is_some() { &REPLACE_CALL_WITH_OPERATOR } else { &REPLACE_GET_OR_SET };
    if !cx.on(rule) || has_comments(n) {
        return;
    }
    // Plain positional arguments only.
    if call.children().any(|c| matches!(c.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return;
    }
    let Some(list) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    let args: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let plain = args.iter().all(|a| !a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) && ast::token(a, SyntaxKind::STAR).is_none());
    let values: Vec<SyntaxNode> = args.iter().filter_map(|a| a.first_child()).collect();
    if !plain || values.len() != args.len() {
        return;
    }
    let (replacement, message) = match (name.as_str(), binary) {
        ("get", _) if !values.is_empty() => {
            let idx: Vec<String> = values.iter().map(|v| v.text().to_string()).collect();
            (format!("{}[{}]", recv.text(), idx.join(", ")), "Explicit 'get' call can be replaced with indexing")
        }
        // `a.set(i, v)` only as a statement (an indexed assignment has no value).
        ("set", _) if values.len() >= 2 && n.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK) && !is_block_value(n) => {
            let (v, idx) = values.split_last().unwrap();
            let idx: Vec<String> = idx.iter().map(|v| v.text().to_string()).collect();
            (format!("{}[{}] = {}", recv.text(), idx.join(", "), v.text()), "Explicit 'set' call can be replaced with indexed assignment")
        }
        (_, Some(op)) if values.len() == 1 => {
            let text = format!("{} {op} {}", recv.text(), operand(&values[0]));
            let bare = n.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    SyntaxKind::BLOCK
                        | SyntaxKind::VALUE_ARGUMENT
                        | SyntaxKind::PROPERTY
                        | SyntaxKind::RETURN_EXPR
                        | SyntaxKind::PAREN_EXPR
                        | SyntaxKind::CONDITION
                        | SyntaxKind::THEN_BRANCH
                        | SyntaxKind::ELSE_BRANCH
                        | SyntaxKind::LONG_STRING_TEMPLATE_ENTRY
                        | SyntaxKind::WHEN_ENTRY
                        | SyntaxKind::LOOP_RANGE
                ) || assignment_op(&p).is_some()
            });
            (if bare { text } else { format!("({text})") }, "Call can be replaced with a binary operator")
        }
        _ => return,
    };
    let Some(e) = cx.engine() else { return };
    let operator = e.resolve_name_ref(&callee).iter().any(|d| {
        matches!(d, Def::Symbol { sym, .. } if sym.kind == SymbolKind::Function && sym.modifiers.has(Modifiers::OPERATOR))
    });
    if operator {
        out.report(rule, callee.text_range(), message, Some(Fix::replace("Replace with operator", n.text_range(), replacement)));
    }
}

/// The node is the last statement of a lambda (its value).
fn is_block_value(n: &SyntaxNode) -> bool {
    let Some(block) = n.parent() else { return false };
    block.parent().is_some_and(|p| p.kind() == SyntaxKind::LAMBDA_EXPR) && block.children().last().as_ref() == Some(n)
}

fn empty_bodies(n: &SyntaxNode, out: &mut Sink) {
    let body_of = |kind: SyntaxKind| n.children().find(|c| c.kind() == kind).and_then(|b| b.first_child()).filter(is_empty_block);
    match n.kind() {
        SyntaxKind::IF_EXPR => {
            if body_of(SyntaxKind::THEN_BRANCH).is_some()
                && let Some(kw) = ast::token(n, SyntaxKind::IF_KW)
            {
                out.report(&EMPTY_CONTROL_FLOW, kw.text_range(), "'if' has empty body", None);
            }
            if let (Some(_), Some(kw)) = (body_of(SyntaxKind::ELSE_BRANCH), ast::token(n, SyntaxKind::ELSE_KW)) {
                let start = prev_significant(&kw).map_or(kw.text_range().start(), |p| p.text_range().end());
                let fix = Fix::delete("Remove empty 'else'", TextRange::new(start, n.text_range().end()));
                out.report(&EMPTY_CONTROL_FLOW, kw.text_range(), "'else' has empty body", Some(fix));
            }
        }
        _ => {
            if body_of(SyntaxKind::BODY).is_some()
                && let Some(kw) = n.first_token()
            {
                out.report(&EMPTY_CONTROL_FLOW, kw.text_range(), format!("'{}' has empty body", kw.text()), None);
            }
        }
    }
}
