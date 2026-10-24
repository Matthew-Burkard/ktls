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
                operator_assignment(cx, &n, out);
                boolean_constants(cx, &n, out);
                ignore_case_equals(cx, &n, out);
            }
            SyntaxKind::DOT_QUALIFIED_EXPR => {
                operator_calls(cx, &n, out);
                put_assignment(cx, &n, out);
                call_chain(cx, &n, out);
                count_with_size(cx, &n, out);
                java_static_method(cx, &n, out);
            }
            SyntaxKind::PREFIX_EXPR => {
                negated_is_empty(cx, &n, out);
                negated_binary(cx, &n, out);
            }
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

const OPERATOR_ASSIGNMENTS: &[(SyntaxKind, &str)] = &[(SyntaxKind::PLUS, "+="), (SyntaxKind::MINUS, "-="), (SyntaxKind::STAR, "*="), (SyntaxKind::SLASH, "/="), (SyntaxKind::PERCENT, "%=")];

/// Types whose `+=` is `x = x + y` (others may have a `plusAssign`, or both).
const PLAIN_ARITHMETIC: &[&str] = &["kotlin.Int", "kotlin.Long", "kotlin.Short", "kotlin.Byte", "kotlin.Double", "kotlin.Float", "kotlin.Char", "kotlin.String", "kotlin.UInt", "kotlin.ULong"];

/// A name or a chain of names (`x`, `this.x`, `a.b.c`): evaluating it twice has no effect.
fn plain_target(n: &SyntaxNode) -> bool {
    match n.kind() {
        SyntaxKind::NAME_REF => true,
        SyntaxKind::THIS_EXPR => n.text() == "this",
        SyntaxKind::DOT_QUALIFIED_EXPR => n.children().count() == 2 && n.children().all(|c| plain_target(&c)),
        _ => false,
    }
}

/// `x = x + y` → `x += y`.
fn operator_assignment(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if assignment_op(n).is_none_or(|t| t.kind() != SyntaxKind::EQ) || !cx.on(&OPERATOR_ASSIGNMENT) || has_comments(n) {
        return;
    }
    let (Some(target), Some(value)) = (n.first_child(), n.last_child()) else { return };
    if !plain_target(&target) || value.kind() != SyntaxKind::BINARY_EXPR {
        return;
    }
    let Some(op) = binary_op(&value).and_then(|o| OPERATOR_ASSIGNMENTS.iter().find(|(k, _)| *k == o.kind())).map(|(_, a)| *a) else { return };
    let Some((l, r)) = binary_operands(&value) else { return };
    let squash = |n: &SyntaxNode| n.text().to_string().chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if squash(&l) != squash(&target) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let plain = e.type_of(&target).is_some_and(|t| matches!(&t, TypeRef::Named { nullable: false, fq: Some(fq), .. } if PLAIN_ARITHMETIC.contains(&&**fq)));
    if plain {
        let fix = Fix::replace("Replace with operator-assignment", n.text_range(), format!("{} {op} {}", target.text(), r.text()));
        out.report(&OPERATOR_ASSIGNMENT, n.text_range(), format!("Replaceable with '{op}'"), Some(fix));
    }
}

/// The value of statement `n` is not used: it is not the last statement of a block whose
/// value counts (lambdas, `if`/`when` branches, `try`).
fn value_unused(n: &SyntaxNode) -> bool {
    let Some(block) = n.parent().filter(|p| p.kind() == SyntaxKind::BLOCK) else { return false };
    block.last_child().as_ref() != Some(n)
        || block.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::FUN | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::BODY))
}

/// Plain positional value arguments of a call (no lambdas, names, spreads or type arguments).
fn plain_arguments(call: &SyntaxNode) -> Option<Vec<SyntaxNode>> {
    if call.children().any(|c| matches!(c.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return None;
    }
    let list = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    list.children()
        .filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT)
        .map(|a| (!a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) && ast::token(&a, SyntaxKind::STAR).is_none()).then(|| a.first_child()).flatten())
        .collect()
}

/// `map.put(k, v)` as a statement → `map[k] = v`.
fn put_assignment(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if call.kind() != SyntaxKind::CALL_EXPR || recv.kind() == SyntaxKind::SUPER_EXPR || !cx.on(&PUT_ASSIGNMENT) || has_comments(n) || !value_unused(n) {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF && c.text() == "put") else { return };
    let Some(args) = plain_arguments(&call).filter(|a| a.len() == 2) else { return };
    let Some(e) = cx.engine() else { return };
    let mutable_map = e.type_of(&recv).is_some_and(|t| e.supertype_closure(&t).iter().any(|s| fq_of(s) == Some("kotlin.collections.MutableMap")));
    let member = e.resolve_name_ref(&callee).iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind == SymbolKind::Function && sym.receiver.is_none() && sym.params.len() == 2));
    if mutable_map && member {
        let fix = Fix::replace("Convert put to assignment", n.text_range(), format!("{}[{}] = {}", recv.text(), args[0].text(), args[1].text()));
        out.report(&PUT_ASSIGNMENT, callee.text_range(), "'put' call can be converted to indexing assignment", Some(fix));
    }
}

/// `(first call, second call or property, replacement)`; `size` is the property.
const CALL_CHAINS: &[(&str, &str, &str)] = &[
    ("filter", "first", "first"),
    ("filter", "firstOrNull", "firstOrNull"),
    ("filter", "last", "last"),
    ("filter", "lastOrNull", "lastOrNull"),
    ("filter", "single", "single"),
    ("filter", "singleOrNull", "singleOrNull"),
    ("filter", "count", "count"),
    ("filter", "size", "count"),
    ("filter", "any", "any"),
    ("filter", "none", "none"),
    ("filter", "isEmpty", "none"),
    ("filter", "isNotEmpty", "any"),
    ("map", "filterNotNull", "mapNotNull"),
    ("map", "joinToString", "joinToString"),
    ("sortedBy", "first", "minBy"),
    ("sortedBy", "firstOrNull", "minByOrNull"),
    ("sortedByDescending", "first", "maxBy"),
    ("sortedByDescending", "firstOrNull", "maxByOrNull"),
];

/// Receivers of the standard library's collection operations.
const CHAIN_RECEIVERS: &[&str] = &["kotlin.collections.Iterable", "kotlin.Array", "kotlin.sequences.Sequence"];

/// `xs.filter { … }.first()` → `xs.first { … }`, and the like.
fn call_chain(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(inner), Some(second), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if inner.kind() != SyntaxKind::DOT_QUALIFIED_EXPR || !cx.on(&SIMPLIFIABLE_CALL_CHAIN) || has_comments(n) {
        return;
    }
    let mut inner_parts = inner.children();
    let (Some(recv), Some(first), None) = (inner_parts.next(), inner_parts.next(), inner_parts.next()) else { return };
    if first.kind() != SyntaxKind::CALL_EXPR || recv.kind() == SyntaxKind::SUPER_EXPR {
        return;
    }
    // The second link: a call without arguments, or `size`.
    let (second_name, second_callee) = match second.kind() {
        SyntaxKind::NAME_REF => (second.text().to_string(), second.clone()),
        SyntaxKind::CALL_EXPR if plain_arguments(&second).is_some_and(|a| a.is_empty()) => match second.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) {
            Some(c) => (c.text().to_string(), c),
            None => return,
        },
        _ => return,
    };
    if second.kind() == SyntaxKind::NAME_REF && second_name != "size" || second.kind() == SyntaxKind::CALL_EXPR && second_name == "size" {
        return;
    }
    // The first link: a call with only a trailing lambda (no labels naming the call).
    let Some(first_callee) = first.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let first_name = first_callee.text().to_string();
    let Some(&(_, _, merged)) = CALL_CHAINS.iter().find(|(a, b, _)| *a == first_name && *b == second_name) else { return };
    if matches!(merged, "minBy" | "maxBy") && cx.language_version().is_some_and(|v| v < 107) {
        return;
    }
    let lambdas: Vec<SyntaxNode> = first.children().filter(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT).collect();
    let empty_parens = first.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).is_none_or(|l| !l.children().any(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT));
    let [la] = lambdas.as_slice() else { return };
    let Some(lambda) = la.first_child().filter(|l| l.kind() == SyntaxKind::LAMBDA_EXPR) else { return };
    if !empty_parens || first.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) || lambda.text().to_string().contains(&format!("@{first_name}")) {
        return;
    }
    // Both links are the standard library's, on an iterable, array or sequence.
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&first_callee);
    let stdlib_op = !defs.is_empty()
        && defs.iter().all(|d| match d {
            Def::Symbol { sym, .. } => {
                (sym.fq_name.starts_with("kotlin.collections.") || sym.fq_name.starts_with("kotlin.sequences."))
                    && sym.receiver.as_ref().is_some_and(|r| fq_of(&e.resolve_declared(sym, r)).is_some_and(|f| CHAIN_RECEIVERS.contains(&f)))
            }
            _ => false,
        });
    let second_defs = e.resolve_name_ref(&second_callee);
    let stdlib_second = !second_defs.is_empty() && second_defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.fq_name.starts_with("kotlin.")));
    if !stdlib_op || !stdlib_second {
        return;
    }
    // `joinToString`'s transform is not inline: no suspend calls or non-local returns.
    if merged == "joinToString" {
        let returns = lambda.descendants().any(|d| d.kind() == SyntaxKind::RETURN_EXPR && !d.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER));
        let suspends = lambda.descendants().filter(|d| d.kind() == SyntaxKind::CALL_EXPR).filter_map(|c| c.first_child().filter(|n| n.kind() == SyntaxKind::NAME_REF)).any(|callee| {
            let defs = e.resolve_name_ref(&callee);
            defs.is_empty() || defs.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.modifiers.has(Modifiers::SUSPEND)))
        });
        if returns || suspends {
            return;
        }
    }
    let text = cx.text;
    let head = &text[usize::from(inner.text_range().start())..usize::from(first.text_range().start())];
    let fix = Fix::replace(format!("Merge call chain to '{merged}'"), n.text_range(), format!("{head}{merged} {}", lambda.text()));
    out.report(&SIMPLIFIABLE_CALL_CHAIN, TextRange::new(first.text_range().start(), n.text_range().end()), format!("Call chain on collection type can be simplified to '{merged}'"), Some(fix));
}

/// The operand of `!x` (through parentheses with `paren`).
fn negated(n: &SyntaxNode) -> Option<SyntaxNode> {
    let op = n.first_token()?;
    if op.kind() != SyntaxKind::EXCL || has_comments(n) {
        return None;
    }
    n.children().find(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE)
}

/// `recv.name()` without arguments: (receiver, name).
fn no_arg_call(n: &SyntaxNode) -> Option<(SyntaxNode, SyntaxNode)> {
    if n.kind() != SyntaxKind::DOT_QUALIFIED_EXPR {
        return None;
    }
    let mut parts = n.children();
    let (recv, call) = (parts.next()?, parts.next()?);
    if call.kind() != SyntaxKind::CALL_EXPR || call.children().any(|c| matches!(c.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return None;
    }
    let args = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    if args.children().next().is_some() {
        return None;
    }
    Some((recv, call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)?))
}

/// Whether `callee` is a Kotlin standard library function (not a Java method of that name).
fn stdlib_call(cx: &Cx, callee: &SyntaxNode) -> bool {
    let Some(e) = cx.engine() else { return false };
    let defs = e.resolve_name_ref(callee);
    !defs.is_empty() && defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.fq_name.starts_with("kotlin.")))
}

/// `!xs.isEmpty()` → `xs.isNotEmpty()`.
fn negated_is_empty(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&NEGATED_IS_EMPTY) {
        return;
    }
    let Some(x) = negated(n) else { return };
    let Some((recv, callee)) = no_arg_call(&x) else { return };
    let name = callee.text().to_string();
    let Some(opposite) = [("isEmpty", "isNotEmpty"), ("isNotEmpty", "isEmpty"), ("isBlank", "isNotBlank"), ("isNotBlank", "isBlank")].iter().find(|(a, _)| *a == name).map(|(_, b)| *b) else { return };
    if !stdlib_call(cx, &callee) {
        return;
    }
    out.report(&NEGATED_IS_EMPTY, n.text_range(), format!("Replace negated '{name}' with '{opposite}'"), Some(Fix::replace(format!("Replace with '{opposite}()'"), n.text_range(), format!("{}.{opposite}()", recv.text()))));
}

/// `!(a == b)` → `a != b`, also `===`, `in`, `is`.
fn negated_binary(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&NEGATED_BINARY) {
        return;
    }
    let Some(paren) = negated(n).filter(|p| p.kind() == SyntaxKind::PAREN_EXPR) else { return };
    let Some(inner) = paren.children().next() else { return };
    let op = match inner.kind() {
        SyntaxKind::BINARY_EXPR => binary_op(&inner),
        SyntaxKind::IS_EXPR => binary_op(&inner).or_else(|| inner.children_with_tokens().filter_map(|t| t.into_token()).find(|t| matches!(t.kind(), SyntaxKind::IS_KW | SyntaxKind::NOT_IS_KW))),
        _ => None,
    };
    let Some(op) = op else { return };
    let flipped = match op.kind() {
        SyntaxKind::EQ2 => "!=",
        SyntaxKind::NOT_EQ => "==",
        SyntaxKind::EQ3 => "!==",
        SyntaxKind::NOT_EQ2 => "===",
        SyntaxKind::IN_KW => "!in",
        SyntaxKind::NOT_IN_KW => "in",
        SyntaxKind::IS_KW => "!is",
        SyntaxKind::NOT_IS_KW => "is",
        _ => return,
    };
    let base = inner.text_range().start();
    let mut text = inner.text().to_string();
    let r = op.text_range() - base;
    text.replace_range(usize::from(r.start())..usize::from(r.end()), flipped);
    out.report(&NEGATED_BINARY, n.text_range(), "Negated expression can be simplified", Some(Fix::replace("Simplify negated expression", n.text_range(), text)));
}

fn bool_literal(n: &SyntaxNode) -> Option<bool> {
    if n.kind() != SyntaxKind::LITERAL {
        return None;
    }
    match n.first_token()?.kind() {
        SyntaxKind::TRUE_KW => Some(true),
        SyntaxKind::FALSE_KW => Some(false),
        _ => None,
    }
}

/// `x && true`, `false || x` → `x`; `x == true` → `x`, `x == false` → `!x` (non-null
/// Boolean `x`).
fn boolean_constants(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&BOOLEAN_CONSTANTS) || has_comments(n) {
        return;
    }
    let (Some(op), Some((l, r))) = (binary_op(n), binary_operands(n)) else { return };
    let (value, other) = match (bool_literal(&l), bool_literal(&r)) {
        (Some(v), None) => (v, r),
        (None, Some(v)) => (v, l),
        _ => return,
    };
    let simple = |x: &SyntaxNode| matches!(x.kind(), SyntaxKind::NAME_REF | SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::PAREN_EXPR | SyntaxKind::THIS_EXPR | SyntaxKind::LITERAL);
    let replacement = match (op.kind(), value) {
        (SyntaxKind::AMP2, true) | (SyntaxKind::PIPE2, false) => other.text().to_string(),
        (SyntaxKind::EQ2, _) | (SyntaxKind::NOT_EQ, _) => {
            let Some(e) = cx.engine() else { return };
            if !matches!(crate::analysis::lints::certain_type(e, &other, true), Some(TypeRef::Named { ref fq, nullable: false, .. }) if fq.as_deref() == Some("kotlin.Boolean")) {
                return;
            }
            if value == (op.kind() == SyntaxKind::EQ2) {
                other.text().to_string()
            } else if simple(&other) {
                format!("!{}", other.text())
            } else {
                format!("!({})", other.text())
            }
        }
        _ => return,
    };
    // Keep precedence when the result is an operand of another operator.
    let replacement = if n.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::BINARY_EXPR | SyntaxKind::PREFIX_EXPR)) && !simple(&other) { format!("({replacement})") } else { replacement };
    out.report(&BOOLEAN_CONSTANTS, n.text_range(), "Boolean expression can be simplified", Some(Fix::replace("Simplify boolean expression", n.text_range(), replacement)));
}

/// `xs.count()` → `xs.size`, `s.count()` → `s.length`.
fn count_with_size(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&COUNT_WITH_SIZE) {
        return;
    }
    let Some((recv, callee)) = no_arg_call(n) else { return };
    if callee.text() != "count" || !stdlib_call(cx, &callee) {
        return;
    }
    let property = match container(cx, &recv) {
        Some(Container::Collection | Container::Map | Container::Array) => "size",
        Some(Container::CharSequence) => "length",
        None => return,
    };
    out.report(&COUNT_WITH_SIZE, n.text_range(), format!("Could be replaced with '{property}'"), Some(Fix::replace(format!("Replace with '{property}'"), n.text_range(), format!("{}.{property}", recv.text()))));
}

/// `a.lowercase() == b.lowercase()` → `a.equals(b, ignoreCase = true)`.
fn ignore_case_equals(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&IGNORE_CASE_EQUALS) || has_comments(n) {
        return;
    }
    let (Some(op), Some((l, r))) = (binary_op(n), binary_operands(n)) else { return };
    if !matches!(op.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ) {
        return;
    }
    let (Some((a, fa)), Some((b, fb))) = (no_arg_call(&l), no_arg_call(&r)) else { return };
    let name = fa.text().to_string();
    if fb.text() != name.as_str() || !matches!(name.as_str(), "lowercase" | "uppercase" | "toLowerCase" | "toUpperCase") || !stdlib_call(cx, &fa) || !stdlib_call(cx, &fb) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let string = |x: &SyntaxNode| matches!(e.type_of(x), Some(TypeRef::Named { ref fq, nullable: false, .. }) if fq.as_deref() == Some("kotlin.String"));
    if !string(&a) || !string(&b) {
        return;
    }
    let not = if op.kind() == SyntaxKind::NOT_EQ { "!" } else { "" };
    let text = format!("{not}{}.equals({}, ignoreCase = true)", a.text(), b.text());
    out.report(&IGNORE_CASE_EQUALS, n.text_range(), "Should be replaced with 'equals(..., ignoreCase = true)'", Some(Fix::replace("Replace with 'equals(..., ignoreCase = true)'", n.text_range(), text)));
}

/// How a Java static call becomes Kotlin.
enum Analog {
    /// A top-level function, with the import it needs.
    Function(&'static str, Option<&'static str>),
    /// An extension on the first argument (`Arrays.toString(a)` → `a.contentToString()`),
    /// with the import it needs.
    Extension(&'static str, Option<&'static str>),
}

const JAVA_STATICS: &[(&str, &str, Analog)] = &[
    ("java.lang.Math", "max", Analog::Function("maxOf", None)),
    ("java.lang.Math", "min", Analog::Function("minOf", None)),
    ("java.lang.Math", "abs", Analog::Function("abs", Some("kotlin.math.abs"))),
    ("java.lang.Math", "sqrt", Analog::Function("sqrt", Some("kotlin.math.sqrt"))),
    ("java.lang.Math", "cbrt", Analog::Function("cbrt", Some("kotlin.math.cbrt"))),
    ("java.lang.Math", "sin", Analog::Function("sin", Some("kotlin.math.sin"))),
    ("java.lang.Math", "cos", Analog::Function("cos", Some("kotlin.math.cos"))),
    ("java.lang.Math", "tan", Analog::Function("tan", Some("kotlin.math.tan"))),
    ("java.lang.Math", "atan2", Analog::Function("atan2", Some("kotlin.math.atan2"))),
    ("java.lang.Math", "hypot", Analog::Function("hypot", Some("kotlin.math.hypot"))),
    ("java.lang.Math", "exp", Analog::Function("exp", Some("kotlin.math.exp"))),
    ("java.lang.Math", "log", Analog::Function("ln", Some("kotlin.math.ln"))),
    ("java.lang.Math", "log10", Analog::Function("log10", Some("kotlin.math.log10"))),
    ("java.lang.Math", "floor", Analog::Function("floor", Some("kotlin.math.floor"))),
    ("java.lang.Math", "ceil", Analog::Function("ceil", Some("kotlin.math.ceil"))),
    ("java.lang.Math", "pow", Analog::Extension("pow", Some("kotlin.math.pow"))),
    ("java.util.Arrays", "asList", Analog::Function("listOf", None)),
    ("java.util.Arrays", "toString", Analog::Extension("contentToString", None)),
    ("java.util.Arrays", "deepToString", Analog::Extension("contentDeepToString", None)),
    ("java.util.Arrays", "equals", Analog::Extension("contentEquals", None)),
    ("java.util.Arrays", "hashCode", Analog::Extension("contentHashCode", None)),
    ("java.util.Arrays", "copyOf", Analog::Extension("copyOf", None)),
    ("java.util.Arrays", "sort", Analog::Extension("sort", None)),
    ("java.util.Arrays", "fill", Analog::Extension("fill", None)),
    ("java.lang.Integer", "parseInt", Analog::Extension("toInt", None)),
    ("java.lang.Long", "parseLong", Analog::Extension("toLong", None)),
    ("java.lang.Double", "parseDouble", Analog::Extension("toDouble", None)),
    ("java.lang.Float", "parseFloat", Analog::Extension("toFloat", None)),
    ("java.lang.Short", "parseShort", Analog::Extension("toShort", None)),
    ("java.lang.Byte", "parseByte", Analog::Extension("toByte", None)),
    ("java.lang.Character", "isDigit", Analog::Extension("isDigit", None)),
    ("java.lang.Character", "isLetter", Analog::Extension("isLetter", None)),
    ("java.lang.Character", "isLetterOrDigit", Analog::Extension("isLetterOrDigit", None)),
    ("java.lang.Character", "isWhitespace", Analog::Extension("isWhitespace", None)),
    ("java.lang.Character", "isUpperCase", Analog::Extension("isUpperCase", None)),
    ("java.lang.Character", "isLowerCase", Analog::Extension("isLowerCase", None)),
    ("java.lang.Character", "toUpperCase", Analog::Extension("uppercaseChar", None)),
    ("java.lang.Character", "toLowerCase", Analog::Extension("lowercaseChar", None)),
    ("java.lang.System.out", "println", Analog::Function("println", None)),
    ("java.lang.System.out", "print", Analog::Function("print", None)),
];

/// `Math.max(a, b)` → `maxOf(a, b)`, `Arrays.toString(a)` → `a.contentToString()`,
/// `System.out.println(x)` → `println(x)`.
fn java_static_method(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    use crate::analysis::ide::infer::Qualifier;
    if !cx.on(&JAVA_STATIC_METHOD) || has_comments(n) {
        return;
    }
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return };
    if call.kind() != SyntaxKind::CALL_EXPR || call.children().any(|c| matches!(c.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(args) = plain_arguments(&call) else { return };
    let Some(e) = cx.engine() else { return };
    // The class (or `System.out`) the method is called on.
    let owner = match e.resolve_qualifier(&recv) {
        Qualifier::Static(c) => c.fq_name().to_string(),
        _ => {
            let mut p = recv.children();
            let (Some(sys), Some(field)) = (p.next(), p.next()) else { return };
            if field.text() != "out" || !matches!(e.resolve_qualifier(&sys), Qualifier::Static(c) if c.fq_name() == "java.lang.System") {
                return;
            }
            "java.lang.System.out".to_string()
        }
    };
    let name = callee.text().to_string();
    let Some((_, _, analog)) = JAVA_STATICS.iter().find(|(o, m, _)| *o == owner && *m == name) else { return };
    // Overloads whose arguments Kotlin orders differently (`fill(a, from, to, v)`) are left alone.
    let arity_ok = match name.as_str() {
        "fill" | "equals" | "copyOf" | "pow" => args.len() == 2,
        "toString" | "deepToString" | "hashCode" => args.len() == 1,
        "sort" => matches!(args.len(), 1 | 3),
        _ => true,
    };
    if !arity_ok {
        return;
    }
    let arg_text: Vec<String> = args.iter().map(|a| a.text().to_string()).collect();
    let mut edits = Vec::new();
    let import = match analog {
        Analog::Function(_, i) | Analog::Extension(_, i) => *i,
    };
    if let Some(fq) = import
        && !cx.text.contains(&format!("import {fq}\n"))
        && !cx.text.contains(&format!("import {}.*", fq.rsplit_once('.').map_or("", |(p, _)| p)))
        && let Some(file) = cx.snap.file(cx.path)
    {
        let (range, text) = crate::analysis::ide::completion::import_edit(file, fq);
        edits.push(Edit { range, text });
    }
    let (replacement, shown) = match analog {
        Analog::Function(f, _) => (format!("{f}({})", arg_text.join(", ")), format!("{f}()")),
        Analog::Extension(f, _) => {
            let Some((first, rest)) = args.split_first() else { return };
            let simple = matches!(first.kind(), SyntaxKind::NAME_REF | SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::PAREN_EXPR | SyntaxKind::THIS_EXPR | SyntaxKind::STRING_TEMPLATE | SyntaxKind::INDEX_EXPR);
            let receiver = if simple { first.text().to_string() } else { format!("({})", first.text()) };
            let rest: Vec<String> = rest.iter().map(|a| a.text().to_string()).collect();
            (format!("{receiver}.{f}({})", rest.join(", ")), format!("{f}()"))
        }
    };
    edits.push(Edit { range: n.text_range(), text: replacement });
    out.report(&JAVA_STATIC_METHOD, n.text_range(), format!("Should be replaced with Kotlin function '{shown}'"), Some(Fix::new(format!("Replace with '{shown}'"), edits)));
}
