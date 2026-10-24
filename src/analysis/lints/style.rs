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
                string_template(cx, &n, out);
            }
            SyntaxKind::DOT_QUALIFIED_EXPR => {
                operator_calls(cx, &n, out);
                put_assignment(cx, &n, out);
                call_chain(cx, &n, out);
                count_with_size(cx, &n, out);
                java_static_method(cx, &n, out);
                copy_without_names(cx, &n, out);
                substring(cx, &n, out);
                simplifiable_call(cx, &n, out);
                associate_function(cx, &n, out);
            }
            SyntaxKind::MODIFIER_LIST => protected_in_final(cx, &n, out),
            SyntaxKind::CALL_EXPR => {
                readln(cx, &n, out);
                property_access(cx, &n, out);
            }
            SyntaxKind::PREFIX_EXPR => {
                negated_is_empty(cx, &n, out);
                negated_binary(cx, &n, out);
            }
            SyntaxKind::IF_EXPR => {
                empty_bodies(&n, out);
                if_to_elvis_or_safe_access(cx, &n, out);
                cascade_if(cx, &n, out);
                guard_clause(cx, &n, out);
                lift_return_or_assignment(cx, &n, out);
            }
            SyntaxKind::WHEN_EXPR => lift_return_or_assignment(cx, &n, out),
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR => empty_bodies(&n, out),
            SyntaxKind::BLOCK => {
                fold_initializer(cx, &n, out);
                join_declaration(cx, &n, out);
            }
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

/// `readLine()!!` → `readln()`, `readLine()` → `readlnOrNull()` (Kotlin 1.6+).
fn readln(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF && c.text() == "readLine") else { return };
    if !cx.on(&READLN) || cx.language_version().is_some_and(|v| v < 106) || plain_arguments(call).is_none_or(|a| !a.is_empty()) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    if !matches!(e.resolve_name_ref(&callee).as_slice(), [Def::Symbol { sym, .. }] if sym.fq_name == "kotlin.io.readLine") {
        return;
    }
    let asserted = call.parent().filter(|p| p.kind() == SyntaxKind::POSTFIX_EXPR && binary_op(p).is_some_and(|o| o.kind() == SyntaxKind::EXCL2));
    let (range, text) = match &asserted {
        Some(p) => (p.text_range(), "readln()"),
        None => (call.text_range(), "readlnOrNull()"),
    };
    out.report(&READLN, range, format!("Replace with '{text}'"), Some(Fix::replace(format!("Replace with '{text}'"), range, text)));
}

/// `user.copy("a", 2)` → `user.copy(name = "a", age = 2)`.
fn copy_without_names(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(call) = n.children().nth(1).filter(|c| c.kind() == SyntaxKind::CALL_EXPR) else { return };
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF && c.text() == "copy") else { return };
    if !cx.on(&COPY_WITHOUT_NAMED_ARGUMENTS) {
        return;
    }
    let Some(args) = plain_arguments(&call) else { return };
    if args.is_empty() {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    let Some((owner, _)) = sym.fq_name.rsplit_once('.') else { return };
    if !e.classifier(owner).is_some_and(|c| c.symbol().modifiers.has(Modifiers::DATA)) || args.len() > sym.params.len() {
        return;
    }
    let edits = args.iter().zip(&sym.params).map(|(a, p)| Edit { range: TextRange::empty(a.text_range().start()), text: format!("{} = ", p.name) }).collect();
    out.report(&COPY_WITHOUT_NAMED_ARGUMENTS, callee.text_range(), "'copy' method of data class is called without named arguments", Some(Fix::new("Add names to call arguments", edits)));
}

/// `s.substring(0, n)` → `s.take(n)`, `s.substring(0, s.length - n)` → `s.dropLast(n)`.
fn substring(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let mut parts = n.children();
    let (Some(recv), Some(call), None) = (parts.next(), parts.next(), parts.next()) else { return };
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF && c.text() == "substring") else { return };
    if !cx.on(&SUBSTRING_WITH_TAKE) && !cx.on(&SUBSTRING_WITH_DROP_LAST) {
        return;
    }
    let Some(args) = plain_arguments(&call) else { return };
    let [start, end] = args.as_slice() else { return };
    if !is_literal(start, "0") || !stdlib_call(cx, &callee) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    if !e.type_of(&recv).is_some_and(|t| fq_of(&t) == Some("kotlin.String")) {
        return;
    }
    let recv_text = recv.text().to_string();
    // `s.length - n` with the same receiver.
    let dropped = (end.kind() == SyntaxKind::BINARY_EXPR && binary_op(end).is_some_and(|o| o.kind() == SyntaxKind::MINUS))
        .then(|| binary_operands(end))
        .flatten()
        .filter(|(l, _)| l.text().to_string() == format!("{recv_text}.length"))
        .map(|(_, r)| r);
    let (rule, text) = match dropped {
        Some(k) => (&SUBSTRING_WITH_DROP_LAST, format!("{recv_text}.dropLast({})", k.text())),
        None => (&SUBSTRING_WITH_TAKE, format!("{recv_text}.take({})", end.text())),
    };
    if !cx.on(rule) {
        return;
    }
    let name = if rule.id == SUBSTRING_WITH_TAKE.id { "take" } else { "dropLast" };
    out.report(rule, callee.text_range(), format!("Replace 'substring' call with '{name}' call"), Some(Fix::replace(format!("Replace with '{name}'"), n.text_range(), text)));
}

/// `protected` on a member of a final class.
fn protected_in_final(cx: &Cx, m: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = ast::ModifierList::cast(m.clone()).and_then(|l| l.modifiers().find(|t| t.text() == "protected")) else { return };
    if !cx.on(&PROTECTED_IN_FINAL) {
        return;
    }
    let Some(decl) = m.parent() else { return };
    if has_modifier(&decl, "override") {
        return;
    }
    let Some(class) = decl.parent().filter(|b| b.kind() == SyntaxKind::CLASS_BODY).and_then(|b| b.parent()).or_else(|| decl.parent().filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER_LIST).and_then(|l| l.parent()).and_then(|c| c.parent())) else { return };
    if class.kind() != SyntaxKind::CLASS || ["open", "abstract", "sealed", "enum"].iter().any(|x| has_modifier(&class, x)) || ast::token(&class, SyntaxKind::INTERFACE_KW).is_some() {
        return;
    }
    out.report(&PROTECTED_IN_FINAL, kw.text_range(), "'protected' visibility is effectively 'private' in a final class", Some(Fix::replace("Make private", kw.text_range(), "private")));
}

/// `file.getName()` → `file.name` for Java getters.
fn property_access(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let name = callee.text().to_string();
    let property = if let Some(r) = name.strip_prefix("get").filter(|r| r.starts_with(char::is_uppercase)) {
        let mut c = r.chars();
        match c.next() {
            Some(f) => f.to_lowercase().chain(c).collect::<String>(),
            None => return,
        }
    } else if name.strip_prefix("is").is_some_and(|r| r.starts_with(char::is_uppercase)) {
        name.clone()
    } else {
        return;
    };
    // `getURL()`: the property would be `url`... no, `uRL`; leave all-caps names alone.
    // `getAndIncrement()` reads as an action, not a property.
    if property.chars().nth(1).is_some_and(char::is_uppercase) || name.starts_with("getAnd") || !cx.on(&PROPERTY_ACCESS_SYNTAX) || plain_arguments(call).is_none_or(|a| !a.is_empty()) {
        return;
    }
    // Not `super.getX()` (that is not a property access).
    if call.parent().is_some_and(|p| p.first_child().is_some_and(|f| f.kind() == SyntaxKind::SUPER_EXPR)) {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let defs = e.resolve_name_ref(&callee);
    let [Def::Symbol { sym, .. }] = defs.as_slice() else { return };
    if sym.kind != SymbolKind::Function || !sym.params.is_empty() || !sym.type_params.is_empty() || sym.ty.as_ref().is_some_and(|t| fq_of(t) == Some("kotlin.Unit")) {
        return;
    }
    // `isX()` getters return Booleans (`isNotNull()` of assertion libraries does not).
    if name.starts_with("is") && !sym.ty.as_ref().is_some_and(|t| fq_of(t) == Some("kotlin.Boolean")) {
        return;
    }
    let Some((owner, _)) = sym.fq_name.rsplit_once('.') else { return };
    if !matches!(e.res.find_classifier(owner), Some(crate::analysis::resolve::Classifier::Library(lc)) if !lc.is_kotlin) {
        return;
    }
    // The property must find this getter (no Kotlin member or extension of that name wins).
    let recv_type = match call.parent().filter(|p| p.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && p.last_child().as_ref() == Some(call)).and_then(|p| p.first_child()) {
        Some(r) => e.type_of(&r),
        None => return,
    };
    let Some(t) = recv_type else { return };
    // Methods of types mapped to Kotlin (`Collection.isEmpty()`) are functions there.
    if e.supertype_closure(&t).iter().filter_map(fq_of).any(|fq| fq.starts_with("kotlin.") && e.declared_members(fq).iter().any(|m| m.name == name)) {
        return;
    }
    if !e.members_named(&t, &property).iter().all(|(m, _)| m.name == name) || e.members_named(&t, &property).is_empty() || !e.extensions(&t, &property).is_empty() {
        return;
    }
    out.report(&PROPERTY_ACCESS_SYNTAX, callee.text_range(), format!("Use of getter method instead of property access syntax ('{property}')"), Some(Fix::replace("Use property access syntax", call.text_range(), property)));
}

/// The single expression of an `if` branch (`x` or `{ x }`).
fn branch_expr(b: &SyntaxNode) -> Option<SyntaxNode> {
    let e = b.first_child()?;
    if e.kind() == SyntaxKind::BLOCK { only_statement(&e) } else { Some(e) }
}

fn is_null_literal(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::LITERAL && n.first_token().is_some_and(|t| t.kind() == SyntaxKind::NULL_KW)
}

/// `x == null` / `x != null` with a plain name `x`: (x, whether it is `!=`).
fn null_comparison(c: &SyntaxNode) -> Option<(SyntaxNode, bool)> {
    let op = binary_op(c)?;
    let not = match op.kind() {
        SyntaxKind::EQ2 => false,
        SyntaxKind::NOT_EQ => true,
        _ => return None,
    };
    let (l, r) = binary_operands(c)?;
    let x = if is_null_literal(&r) { l } else if is_null_literal(&l) { r } else { return None };
    (x.kind() == SyntaxKind::NAME_REF).then_some((x, not))
}

fn condition_of(i: &SyntaxNode) -> Option<SyntaxNode> {
    i.children().find(|c| c.kind() == SyntaxKind::CONDITION)?.children().next()
}

/// `if (x != null) x else y` → `x ?: y`; `if (x != null) x.f() else null` → `x?.f()`.
fn if_to_elvis_or_safe_access(cx: &Cx, i: &SyntaxNode, out: &mut Sink) {
    if (!cx.on(&IF_THEN_TO_ELVIS) && !cx.on(&IF_THEN_TO_SAFE_ACCESS)) || has_comments(i) {
        return;
    }
    let Some((x, not_null)) = condition_of(i).and_then(|c| null_comparison(&c)) else { return };
    let then = i.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH).and_then(|b| branch_expr(&b));
    let els = i.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH).and_then(|b| branch_expr(&b));
    let has_else = i.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH);
    let (value, other) = if not_null { (then, els) } else { (els, then) };
    let Some(value) = value else { return };
    let name = x.text().to_string();
    if value.text() == name.as_str() {
        // `x ?: y` (not `x ?: null`).
        let Some(other) = other.filter(|o| !is_null_literal(o)) else { return };
        if !cx.on(&IF_THEN_TO_ELVIS) || other.kind() == SyntaxKind::IF_EXPR {
            return;
        }
        out.report(&IF_THEN_TO_ELVIS, i.first_token().map_or(i.text_range(), |t| t.text_range()), "If-Then foldable to '?:'", Some(Fix::replace("Replace 'if' expression with elvis expression", i.text_range(), format!("{name} ?: {}", other.text()))));
        return;
    }
    // `x.f()` with `null` (or nothing, as a statement) otherwise.
    let receiver_is_x = value.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && value.first_child().is_some_and(|f| f.text() == name.as_str());
    let otherwise_null = match &other {
        Some(o) => is_null_literal(o),
        None => !has_else && not_null && i.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK),
    };
    if !receiver_is_x || !otherwise_null || !cx.on(&IF_THEN_TO_SAFE_ACCESS) {
        return;
    }
    let text = value.text().to_string();
    let rest = &text[name.len()..];
    let Some(rest) = rest.strip_prefix('.') else { return };
    out.report(&IF_THEN_TO_SAFE_ACCESS, i.first_token().map_or(i.text_range(), |t| t.text_range()), "Foldable if-then", Some(Fix::replace("Replace 'if' expression with safe access expression", i.text_range(), format!("{name}?.{rest}"))));
}

/// `val x = f()` then `if (x == null) return` → `val x = f() ?: return`.
fn fold_initializer(cx: &Cx, block: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&FOLD_INITIALIZER_TO_ELVIS) {
        return;
    }
    let stmts: Vec<SyntaxNode> = block.children().collect();
    for w in stmts.windows(2) {
        let (decl, i) = (&w[0], &w[1]);
        if decl.kind() != SyntaxKind::PROPERTY || i.kind() != SyntaxKind::IF_EXPR || i.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) || has_comments(i) {
            continue;
        }
        let Some(p) = ast::PropertyDecl::cast(decl.clone()) else { continue };
        let Some(init) = initializer(decl) else { continue };
        if p.type_ref().is_some() || p.is_destructuring() || p.has_delegate() {
            continue;
        }
        let Some(name) = name_token(decl).map(|t| ast::ident_text(&t)) else { continue };
        let Some((x, false)) = condition_of(i).and_then(|c| null_comparison(&c)) else { continue };
        if x.text() != name.as_str() {
            continue;
        }
        let Some(jump) = i.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH).and_then(|b| branch_expr(&b)) else { continue };
        if !matches!(jump.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR | SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR) {
            continue;
        }
        // `a ?: b` binds tighter than most operators: parenthesize those.
        let init_text = if is_primary(&init) { init.text().to_string() } else { format!("({})", init.text()) };
        let edits = vec![
            Edit { range: init.text_range(), text: format!("{init_text} ?: {}", jump.text()) },
            Edit { range: TextRange::new(decl.text_range().end(), i.text_range().end()), text: String::new() },
        ];
        out.report(&FOLD_INITIALIZER_TO_ELVIS, i.first_token().map_or(i.text_range(), |t| t.text_range()), "If-Null return/break/... foldable to '?:'", Some(Fix::new("Replace 'if' with elvis operator", edits)));
    }
}

/// `if (a) … else if (b) … else …` with three or more branches → `when { … }`.
fn cascade_if(cx: &Cx, i: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&CASCADE_IF) || i.parent().is_some_and(|p| p.kind() == SyntaxKind::ELSE_BRANCH) || has_comments(i) {
        return;
    }
    let mut branches: Vec<(Option<String>, String)> = Vec::new();
    let mut cur = i.clone();
    loop {
        let (Some(cond), Some(then)) = (condition_of(&cur), cur.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)) else { return };
        branches.push((Some(cond.text().to_string()), then.text().to_string()));
        match cur.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH).and_then(|e| e.first_child()) {
            Some(next) if next.kind() == SyntaxKind::IF_EXPR => cur = next,
            Some(last) => {
                branches.push((None, last.text().to_string()));
                break;
            }
            None => break,
        }
    }
    if branches.iter().filter(|(c, _)| c.is_some()).count() < 3 {
        return;
    }
    let indent = indent_at(cx.text, i.text_range().start());
    let shift = |t: &str| t.replace('\n', "\n    ");
    let mut text = String::from("when {");
    for (c, b) in &branches {
        text.push_str(&format!("\n{indent}    {} -> {}", c.as_deref().unwrap_or("else"), shift(b)));
    }
    text.push_str(&format!("\n{indent}}}"));
    out.report(&CASCADE_IF, i.first_token().map_or(i.text_range(), |t| t.text_range()), "Cascade 'if' should be replaced with 'when'", Some(Fix::replace("Replace 'if' with 'when'", i.text_range(), text)));
}

/// `!c` for a condition `c`, simplified for `!x`, comparisons and `is`.
fn negated_text(c: &SyntaxNode) -> String {
    if c.kind() == SyntaxKind::PREFIX_EXPR
        && binary_op(c).is_some_and(|o| o.kind() == SyntaxKind::EXCL)
        && let Some(inner) = c.children().find(|x| x.kind() != SyntaxKind::OPERATION_REFERENCE)
    {
        let inner = if inner.kind() == SyntaxKind::PAREN_EXPR { inner.children().next().unwrap_or(inner) } else { inner };
        return inner.text().to_string();
    }
    if c.kind() == SyntaxKind::BINARY_EXPR
        && let Some(op) = binary_op(c)
    {
        let flipped = match op.kind() {
            SyntaxKind::EQ2 => Some("!="),
            SyntaxKind::NOT_EQ => Some("=="),
            SyntaxKind::LT => Some(">="),
            SyntaxKind::GT => Some("<="),
            SyntaxKind::LT_EQ => Some(">"),
            SyntaxKind::GT_EQ => Some("<"),
            _ => None,
        };
        if let Some(f) = flipped {
            let text = c.text().to_string();
            let r = op.text_range() - c.text_range().start();
            return format!("{}{f}{}", &text[..usize::from(r.start())], &text[usize::from(r.end())..]);
        }
    }
    if is_primary(c) { format!("!{}", c.text()) } else { format!("!({})", c.text()) }
}

/// `if (x == null) throw IllegalArgumentException(m)` → `requireNotNull(x) { m }`,
/// `if (!ok) throw IllegalStateException(m)` → `check(ok) { m }`.
fn guard_clause(cx: &Cx, i: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&GUARD_CLAUSE) || i.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) || i.parent().is_none_or(|p| p.kind() != SyntaxKind::BLOCK) || has_comments(i) {
        return;
    }
    let Some(cond) = condition_of(i) else { return };
    let Some(throw) = i.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH).and_then(|b| branch_expr(&b)).filter(|t| t.kind() == SyntaxKind::THROW_EXPR) else { return };
    let Some(call) = throw.children().next().filter(|c| c.kind() == SyntaxKind::CALL_EXPR) else { return };
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(args) = plain_arguments(&call) else { return };
    let (plain, not_null) = match callee.text().to_string().as_str() {
        "IllegalArgumentException" => ("require", "requireNotNull"),
        "IllegalStateException" => ("check", "checkNotNull"),
        _ => return,
    };
    let Some(e) = cx.engine() else { return };
    if !matches!(e.resolve_name_ref(&callee).as_slice(), [Def::Symbol { sym, .. }, ..] if sym.fq_name.starts_with("kotlin.") || sym.fq_name.starts_with("java.lang.")) {
        return;
    }
    let message = match args.as_slice() {
        [] => String::new(),
        [m] => format!(" {{ {} }}", m.text()),
        _ => return,
    };
    let text = match null_comparison(&cond) {
        Some((x, false)) => format!("{not_null}({}){message}", x.text()),
        _ => format!("{plain}({}){message}", negated_text(&cond)),
    };
    let f = text.split('(').next().unwrap_or(plain).to_string();
    out.report(&GUARD_CLAUSE, i.first_token().map_or(i.text_range(), |t| t.text_range()), format!("Replace guard clause with Kotlin's function call ('{f}')"), Some(Fix::replace(format!("Replace with '{f}()' call"), i.text_range(), text)));
}

/// The parameter name of a lambda with one parameter (`it` when implicit) and its body.
fn single_param_lambda(l: &SyntaxNode) -> Option<(String, SyntaxNode)> {
    let body = l.children().find(|c| c.kind() == SyntaxKind::BLOCK).and_then(|b| only_statement(&b))?;
    let name = match l.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) {
        None => "it".to_string(),
        Some(list) => {
            let params: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).collect();
            let [p] = params.as_slice() else { return None };
            if p.children().any(|c| c.kind() == SyntaxKind::TYPE_REFERENCE || c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                return None;
            }
            name_token(p).map(|t| ast::ident_text(&t))?
        }
    };
    Some((name, body))
}

/// The only argument of `recv.f { … }`: (callee, lambda).
fn lambda_only_call(n: &SyntaxNode) -> Option<(SyntaxNode, SyntaxNode, SyntaxNode)> {
    let mut parts = n.children();
    let (recv, call) = (parts.next()?, parts.next()?);
    if call.kind() != SyntaxKind::CALL_EXPR || call.children().any(|c| matches!(c.kind(), SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return None;
    }
    if call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).is_some_and(|l| l.children().next().is_some()) {
        return None;
    }
    let lambda = call.children().find(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT)?.children().find(|c| c.kind() == SyntaxKind::LAMBDA_EXPR)?;
    Some((recv, call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF)?, lambda))
}

/// `filter { it is T }` → `filterIsInstance<T>()`, `filter { it != null }` → `filterNotNull()`.
fn simplifiable_call(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&SIMPLIFIABLE_CALL) {
        return;
    }
    let Some((recv, callee, lambda)) = lambda_only_call(n) else { return };
    if callee.text() != "filter" || has_comments(&lambda) {
        return;
    }
    let Some((param, body)) = single_param_lambda(&lambda) else { return };
    let replacement = if body.kind() == SyntaxKind::IS_EXPR && body.first_child().is_some_and(|f| f.text() == param.as_str()) && binary_op(&body).is_some_and(|o| o.kind() == SyntaxKind::IS_KW) {
        let Some(ty) = body.children().find(|c| c.kind() == SyntaxKind::TYPE_REFERENCE) else { return };
        format!("filterIsInstance<{}>()", ty.text())
    } else if null_comparison(&body).is_some_and(|(x, not)| not && x.text() == param.as_str()) {
        "filterNotNull()".to_string()
    } else {
        return;
    };
    if !stdlib_call(cx, &callee) {
        return;
    }
    let name = replacement.split(['<', '(']).next().unwrap_or("").to_string();
    out.report(&SIMPLIFIABLE_CALL, callee.text_range(), format!("Filter call could be simplified to '{name}'"), Some(Fix::replace(format!("Replace with '{name}'"), n.text_range(), format!("{}.{replacement}", recv.text()))));
}

/// `associate { it to v }` → `associateWith { v }`, `associate { k to it }` → `associateBy { k }`.
fn associate_function(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&ASSOCIATE_FUNCTION) {
        return;
    }
    let Some((recv, callee, lambda)) = lambda_only_call(n) else { return };
    if callee.text() != "associate" || has_comments(&lambda) {
        return;
    }
    let Some((param, body)) = single_param_lambda(&lambda) else { return };
    if binary_op(&body).is_none_or(|o| o.kind() != SyntaxKind::IDENT || o.text() != "to") {
        return;
    }
    let Some((k, v)) = binary_operands(&body) else { return };
    let (f, kept) = if k.text() == param.as_str() && v.text() != param.as_str() {
        ("associateWith", v)
    } else if v.text() == param.as_str() && k.text() != param.as_str() {
        ("associateBy", k)
    } else {
        return;
    };
    if !stdlib_call(cx, &callee) {
        return;
    }
    let params = lambda.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST).map(|l| format!("{} -> ", l.text())).unwrap_or_default();
    out.report(&ASSOCIATE_FUNCTION, callee.text_range(), format!("Replace 'associate' with '{f}'"), Some(Fix::replace(format!("Replace with '{f}'"), n.text_range(), format!("{}.{f} {{ {params}{} }}", recv.text(), kept.text()))));
}

/// `val x: T` and on the next line `x = v` → `val x: T = v`.
fn join_declaration(cx: &Cx, block: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&JOIN_DECLARATION_AND_ASSIGNMENT) {
        return;
    }
    let stmts: Vec<SyntaxNode> = block.children().collect();
    for w in stmts.windows(2) {
        let (decl, assign) = (&w[0], &w[1]);
        if decl.kind() != SyntaxKind::PROPERTY || ast::token(decl, SyntaxKind::EQ).is_some() || has_comments(assign) || modifiers(decl).is_some() {
            continue;
        }
        let Some(p) = ast::PropertyDecl::cast(decl.clone()) else { continue };
        if p.type_ref().is_none() || p.has_delegate() || p.is_destructuring() {
            continue;
        }
        if assignment_op(assign).is_none_or(|o| o.kind() != SyntaxKind::EQ) {
            continue;
        }
        let Some((target, value)) = binary_operands(assign) else { continue };
        let Some(name) = name_token(decl).map(|t| ast::ident_text(&t)) else { continue };
        if target.kind() != SyntaxKind::NAME_REF || target.text() != name.as_str() || value.descendants().any(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == name.as_str()) {
            continue;
        }
        let Some(name_tok) = name_token(decl) else { continue };
        let fix = Fix::replace("Join declaration and assignment", TextRange::new(decl.text_range().end(), assign.text_range().end()), format!(" = {}", value.text()));
        out.report(&JOIN_DECLARATION_AND_ASSIGNMENT, name_tok.text_range(), "Can be joined with assignment", Some(fix));
    }
}

/// The operands of a left-nested `a + b + c` chain.
fn plus_operands(n: &SyntaxNode, out: &mut Vec<SyntaxNode>) -> bool {
    if n.kind() == SyntaxKind::BINARY_EXPR && binary_op(n).is_some_and(|o| o.kind() == SyntaxKind::PLUS) {
        let Some((l, r)) = binary_operands(n) else { return false };
        if !plus_operands(&l, out) {
            return false;
        }
        out.push(r);
        return true;
    }
    out.push(n.clone());
    true
}

/// A plain `"…"` literal (not raw, no `$$` prefix): its content.
fn plain_string(n: &SyntaxNode) -> Option<String> {
    if n.kind() != SyntaxKind::STRING_TEMPLATE {
        return None;
    }
    let open = n.first_token()?;
    let close = n.last_token()?;
    if open.text() != "\"" || close.kind() != SyntaxKind::STRING_CLOSE {
        return None;
    }
    Some(n.text().to_string()[1..usize::from(n.text_range().len()) - 1].to_string())
}

/// `"a" + x + "b"` → `"a${x}b"` (left operand a string literal).
fn string_template(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&STRING_TEMPLATE_CONCATENATION) || has_comments(n) || n.text().to_string().contains('\n') {
        return;
    }
    // Only the whole chain.
    if n.parent().is_some_and(|p| p.kind() == SyntaxKind::BINARY_EXPR && binary_op(&p).is_some_and(|o| o.kind() == SyntaxKind::PLUS) && p.first_child().as_ref() == Some(n)) {
        return;
    }
    let mut operands = Vec::new();
    if !plus_operands(n, &mut operands) || operands.len() < 2 || plain_string(&operands[0]).is_none() {
        return;
    }
    if operands.iter().all(|o| plain_string(o).is_some()) {
        return;
    }
    let mut text = String::from("\"");
    for (i, o) in operands.iter().enumerate() {
        if let Some(content) = plain_string(o) {
            text.push_str(&content);
            continue;
        }
        if o.kind() == SyntaxKind::STRING_TEMPLATE || o.kind() == SyntaxKind::LITERAL && o.first_token().is_some_and(|t| t.kind() == SyntaxKind::CHAR_LITERAL) {
            return;
        }
        // `$x` unless a name character follows.
        let next_starts_name = operands.get(i + 1).and_then(plain_string).and_then(|c| c.chars().next()).is_some_and(|c| c.is_alphanumeric() || c == '_');
        let simple = o.kind() == SyntaxKind::NAME_REF && !o.text().to_string().starts_with('`');
        if simple && !next_starts_name {
            text.push_str(&format!("${}", o.text()));
        } else {
            let inner = if o.kind() == SyntaxKind::PAREN_EXPR { o.children().next().map(|c| c.text().to_string()).unwrap_or_default() } else { o.text().to_string() };
            text.push_str(&format!("${{{inner}}}"));
        }
    }
    text.push('"');
    out.report(&STRING_TEMPLATE_CONCATENATION, n.text_range(), "Convert concatenation to template", Some(Fix::replace("Convert concatenation to template", n.text_range(), text)));
}

/// The branch bodies of an `if` (with `else`, not `else if`) or a `when` with `else`.
fn lift_branches(n: &SyntaxNode) -> Option<Vec<SyntaxNode>> {
    match n.kind() {
        SyntaxKind::IF_EXPR => {
            let then = n.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?;
            let els = n.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH)?;
            if els.first_child().is_some_and(|f| f.kind() == SyntaxKind::IF_EXPR) {
                return None;
            }
            Some(vec![then, els])
        }
        SyntaxKind::WHEN_EXPR => {
            let entries: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).collect();
            if !entries.iter().any(|e| ast::token(e, SyntaxKind::ELSE_KW).is_some()) {
                return None;
            }
            entries.iter().map(|e| e.children().find(|c| c.kind() == SyntaxKind::BODY)).collect()
        }
        _ => None,
    }
}

/// `if (c) return a else return b` → `return if (c) a else b`, and the same for
/// assignments to one variable and for `when`.
fn lift_return_or_assignment(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&LIFT_RETURN_OR_ASSIGNMENT) || n.parent().is_none_or(|p| p.kind() != SyntaxKind::BLOCK) || has_comments(n) {
        return;
    }
    let Some(branches) = lift_branches(n) else { return };
    let values: Vec<SyntaxNode> = match branches.iter().map(branch_expr).collect::<Option<Vec<_>>>() {
        Some(v) => v,
        None => return,
    };
    // Every branch `return x` (unlabeled), or every branch `t = x` with the same `t`.
    let returns = values.iter().all(|v| v.kind() == SyntaxKind::RETURN_EXPR && v.first_child().is_some() && !v.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER));
    let target = values.first().and_then(|v| assignment_op(v).filter(|o| o.kind() == SyntaxKind::EQ).and_then(|_| v.first_child())).map(|t| t.text().to_string());
    let assigns = target.as_ref().is_some_and(|t| values.iter().all(|v| assignment_op(v).is_some_and(|o| o.kind() == SyntaxKind::EQ) && v.first_child().is_some_and(|f| f.text() == t.as_str())));
    if !returns && !assigns {
        return;
    }
    // Lifted values must not be jumps themselves.
    let lifted: Vec<SyntaxNode> = values.iter().map(|v| if returns { v.first_child() } else { binary_operands(v).map(|(_, r)| r) }).collect::<Option<Vec<_>>>().unwrap_or_default();
    if lifted.len() != values.len() || lifted.iter().any(|l| matches!(l.kind(), SyntaxKind::RETURN_EXPR | SyntaxKind::THROW_EXPR)) {
        return;
    }
    let base = n.text_range().start();
    let mut text = n.text().to_string();
    for (v, l) in values.iter().zip(&lifted).rev() {
        let r = v.text_range() - base;
        text.replace_range(usize::from(r.start())..usize::from(r.end()), &l.text().to_string());
    }
    let (prefix, what) = if returns { ("return ".to_string(), "return") } else { (format!("{} = ", target.unwrap_or_default()), "assignment") };
    let kw = n.first_token().map_or(n.text_range(), |t| t.text_range());
    out.report(&LIFT_RETURN_OR_ASSIGNMENT, kw, format!("{} can be lifted out of '{}'", if returns { "Return" } else { "Assignment" }, kw_text(n)), Some(Fix::replace(format!("Lift {what} out of '{}'", kw_text(n)), n.text_range(), format!("{prefix}{text}"))));
}

fn kw_text(n: &SyntaxNode) -> String {
    n.first_token().map(|t| t.text().to_string()).unwrap_or_default()
}
