//! Intentions at the cursor: invert `if`, flip a comparison, De Morgan, split / merge
//! `if`s, introduce / remove a `when` subject, `for` ↔ `forEach`.

use super::infer::{Def, Engine, LocalKind};
use crate::analysis::FileData;
use crate::analysis::lints::{Edit, Fix, binary_op, binary_operands, has_comments, indent_at, is_primary};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

pub fn intentions(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Vec<Fix> {
    let mut out = Vec::new();
    out.extend(invert_if(tok));
    out.extend(flip_comparison(tok));
    out.extend(de_morgan(tok));
    out.extend(split_if(file, tok));
    out.extend(merge_if(file, tok));
    out.extend(introduce_subject(tok));
    out.extend(remove_subject(tok));
    out.extend(for_to_for_each(file, tok));
    out.extend(for_each_to_for(e, file, tok));
    out
}

/// Shift the lines after the first from `from` to `to` indentation.
fn reindent(text: &str, from: &str, to: &str) -> String {
    super::rewrites::reindent(text, from, to)
}

/// Logical `&&`/`||` expressions need parentheses as operands of the other one or of `!`.
fn is_logical(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::BINARY_EXPR && binary_op(n).is_some_and(|o| matches!(o.kind(), SyntaxKind::AMP2 | SyntaxKind::PIPE2))
}

const NEGATED: &[(SyntaxKind, &str)] = &[
    (SyntaxKind::EQ2, "!="),
    (SyntaxKind::NOT_EQ, "=="),
    (SyntaxKind::EQ3, "!=="),
    (SyntaxKind::NOT_EQ2, "==="),
    (SyntaxKind::LT, ">="),
    (SyntaxKind::GT, "<="),
    (SyntaxKind::LT_EQ, ">"),
    (SyntaxKind::GT_EQ, "<"),
    (SyntaxKind::IN_KW, "!in"),
    (SyntaxKind::NOT_IN_KW, "in"),
];

/// The text of `!(n)`, simplified: `!x` → `x`, `a == b` → `a != b`, `x is T` → `x !is T`.
fn negate(n: &SyntaxNode) -> String {
    match n.kind() {
        SyntaxKind::PAREN_EXPR if !n.children().last().as_ref().is_some_and(is_logical) => match n.children().last() {
            Some(inner) => negate(&inner),
            None => format!("!{}", n.text()),
        },
        SyntaxKind::PREFIX_EXPR if binary_op(n).is_some_and(|o| o.kind() == SyntaxKind::EXCL) => match n.children().last() {
            Some(inner) if inner.kind() == SyntaxKind::PAREN_EXPR => inner.children().last().map_or_else(|| inner.text().to_string(), |i| i.text().to_string()),
            Some(inner) => inner.text().to_string(),
            None => n.text().to_string(),
        },
        SyntaxKind::LITERAL if n.text() == "true" => "false".into(),
        SyntaxKind::LITERAL if n.text() == "false" => "true".into(),
        SyntaxKind::IS_EXPR => {
            let Some(op) = binary_op(n) else { return format!("!({})", n.text()) };
            let flipped = if op.kind() == SyntaxKind::IS_KW { "!is" } else { "is" };
            replace_token(n, &op, flipped)
        }
        SyntaxKind::BINARY_EXPR => match binary_op(n).and_then(|o| NEGATED.iter().find(|(k, _)| *k == o.kind()).map(|(_, r)| (o, *r))) {
            Some((op, r)) => replace_token(n, &op, r),
            None => format!("!({})", n.text()),
        },
        _ if is_primary(n) => format!("!{}", n.text()),
        _ => format!("!({})", n.text()),
    }
}

/// The text of `n` with `tok` replaced.
fn replace_token(n: &SyntaxNode, tok: &SyntaxToken, with: &str) -> String {
    let text = n.text().to_string();
    let start = usize::from(tok.text_range().start() - n.text_range().start());
    let end = usize::from(tok.text_range().end() - n.text_range().start());
    format!("{}{with}{}", &text[..start], &text[end..])
}

// ------------------------------------------------------------ invert if

/// `if (c) A else B` → `if (!c) B else A`.
fn invert_if(tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::IF_KW {
        return None;
    }
    let stmt = tok.parent().filter(|p| p.kind() == SyntaxKind::IF_EXPR)?;
    let cond = stmt.children().find(|c| c.kind() == SyntaxKind::CONDITION)?.first_child()?;
    let then = stmt.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?.first_child()?;
    let els = stmt.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH)?.first_child()?;
    // An `if` moved before `else` would take that `else`; `else if` chains stay as they are.
    if then.kind() == SyntaxKind::IF_EXPR || els.kind() == SyntaxKind::IF_EXPR {
        return None;
    }
    Some(Fix::new(
        "Invert 'if' condition",
        vec![
            Edit { range: cond.text_range(), text: negate(&cond) },
            Edit { range: then.text_range(), text: els.text().to_string() },
            Edit { range: els.text_range(), text: then.text().to_string() },
        ],
    ))
}

// ------------------------------------------------------------ flip comparison

const MIRRORED: &[(SyntaxKind, &str)] = &[
    (SyntaxKind::EQ2, "=="),
    (SyntaxKind::NOT_EQ, "!="),
    (SyntaxKind::EQ3, "==="),
    (SyntaxKind::NOT_EQ2, "!=="),
    (SyntaxKind::LT, ">"),
    (SyntaxKind::GT, "<"),
    (SyntaxKind::LT_EQ, ">="),
    (SyntaxKind::GT_EQ, "<="),
];

/// The binary expression whose operator is `tok`.
fn operator_of(tok: &SyntaxToken) -> Option<SyntaxNode> {
    tok.parent().filter(|p| p.kind() == SyntaxKind::OPERATION_REFERENCE)?.parent().filter(|b| b.kind() == SyntaxKind::BINARY_EXPR)
}

/// `a < b` → `b > a` (on the operator).
fn flip_comparison(tok: &SyntaxToken) -> Option<Fix> {
    let n = operator_of(tok)?;
    let &(_, mirrored) = MIRRORED.iter().find(|(k, _)| *k == tok.kind())?;
    let (l, r) = binary_operands(&n)?;
    Some(Fix::replace(format!("Flip '{}'", tok.text()), n.text_range(), format!("{} {mirrored} {}", r.text(), l.text())))
}

// ------------------------------------------------------------ De Morgan

/// The operands of a chain of one logical operator (`a && b && c`).
fn chain_operands(n: &SyntaxNode, op: SyntaxKind, out: &mut Vec<SyntaxNode>) {
    if n.kind() == SyntaxKind::BINARY_EXPR
        && binary_op(n).is_some_and(|o| o.kind() == op)
        && let Some((l, r)) = binary_operands(n)
    {
        chain_operands(&l, op, out);
        chain_operands(&r, op, out);
        return;
    }
    out.push(n.clone());
}

/// `a && b` → `!(!a || !b)`, and `!(a && b)` → `!a || !b` (on the operator).
fn de_morgan(tok: &SyntaxToken) -> Option<Fix> {
    let (op, other) = match tok.kind() {
        SyntaxKind::AMP2 => (SyntaxKind::AMP2, "||"),
        SyntaxKind::PIPE2 => (SyntaxKind::PIPE2, "&&"),
        _ => return None,
    };
    let mut top = operator_of(tok)?;
    while let Some(p) = top.parent().filter(|p| p.kind() == SyntaxKind::BINARY_EXPR && binary_op(p).is_some_and(|o| o.kind() == op)) {
        top = p;
    }
    let mut operands = Vec::new();
    chain_operands(&top, op, &mut operands);
    let negated: Vec<String> = operands
        .iter()
        .map(|o| {
            let t = negate(o);
            // `!(x || y)` negated is `x || y`: a chain of the other operator needs parentheses.
            let inner = match o.kind() {
                SyntaxKind::PREFIX_EXPR => o.children().last().and_then(|p| p.children().last()).filter(is_logical),
                _ => None,
            };
            if inner.is_some() { format!("({t})") } else { t }
        })
        .collect();
    let joined = negated.join(&format!(" {other} "));
    // Already negated (`!(a && b)`): drop the negation instead of adding one.
    let negation = top.parent().filter(|p| p.kind() == SyntaxKind::PAREN_EXPR).and_then(|p| p.parent()).filter(|p| p.kind() == SyntaxKind::PREFIX_EXPR && binary_op(p).is_some_and(|o| o.kind() == SyntaxKind::EXCL));
    let (range, text) = match negation {
        Some(neg) => {
            let needs_parens = neg.parent().is_some_and(|p| p.kind() == SyntaxKind::BINARY_EXPR || p.kind() == SyntaxKind::PREFIX_EXPR);
            (neg.text_range(), if needs_parens { format!("({joined})") } else { joined })
        }
        None => (top.text_range(), format!("!({joined})")),
    };
    Some(Fix::replace(format!("Replace '{}' with '{other}'", tok.text()), range, text))
}

// ------------------------------------------------------------ split / merge if

/// `if (a && b) X` → `if (a) { if (b) X }` (no `else`).
fn split_if(file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    let n = operator_of(tok).filter(|_| tok.kind() == SyntaxKind::AMP2)?;
    let cond = n.parent().filter(|c| c.kind() == SyntaxKind::CONDITION)?;
    let stmt = cond.parent().filter(|s| s.kind() == SyntaxKind::IF_EXPR)?;
    if stmt.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) || has_comments(&cond) || !stmt.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK) {
        return None;
    }
    let (l, r) = binary_operands(&n)?;
    let then = stmt.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?.first_child()?;
    let indent = indent_at(&file.text, stmt.text_range().start());
    let inner = format!("{indent}    ");
    let body = reindent(&then.text().to_string(), &indent, &inner);
    Some(Fix::replace("Split into 2 'if's", stmt.text_range(), format!("if ({}) {{\n{inner}if ({}) {body}\n{indent}}}", l.text(), r.text())))
}

/// `if (a) { if (b) X }` → `if (a && b) X` (neither with `else`).
fn merge_if(file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::IF_KW {
        return None;
    }
    let outer = tok.parent().filter(|p| p.kind() == SyntaxKind::IF_EXPR)?;
    if outer.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) {
        return None;
    }
    let block = outer.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?.first_child()?;
    let inner = if block.kind() == SyntaxKind::BLOCK {
        let mut stmts = block.children();
        match (stmts.next(), stmts.next()) {
            (Some(s), None) if !has_comments(&block) => s,
            _ => return None,
        }
    } else {
        block.clone()
    };
    if inner.kind() != SyntaxKind::IF_EXPR || inner.children().any(|c| c.kind() == SyntaxKind::ELSE_BRANCH) {
        return None;
    }
    let a = outer.children().find(|c| c.kind() == SyntaxKind::CONDITION)?.first_child()?;
    let b = inner.children().find(|c| c.kind() == SyntaxKind::CONDITION)?.first_child()?;
    let body = inner.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?.first_child()?;
    let wrap = |c: &SyntaxNode| if c.kind() == SyntaxKind::BINARY_EXPR && binary_op(c).is_some_and(|o| o.kind() == SyntaxKind::PIPE2) { format!("({})", c.text()) } else { c.text().to_string() };
    let indent = indent_at(&file.text, outer.text_range().start());
    let body = reindent(&body.text().to_string(), &format!("{indent}    "), &indent);
    Some(Fix::replace("Merge with nested 'if'", outer.text_range(), format!("if ({} && {}) {body}", wrap(&a), wrap(&b))))
}

// ------------------------------------------------------------ when subject

/// `when { x == 1 -> …; x is T -> …; x in r -> … }` → `when (x) { 1 -> …; is T -> …; in r -> … }`.
fn introduce_subject(tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::WHEN_KW {
        return None;
    }
    let w = tok.parent().filter(|p| p.kind() == SyntaxKind::WHEN_EXPR)?;
    if w.children().any(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) {
        return None;
    }
    let mut subject: Option<String> = None;
    let mut edits = Vec::new();
    for cond in w.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).flat_map(|e| e.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION).collect::<Vec<_>>()) {
        let c = cond.first_child()?;
        let op = binary_op(&c)?;
        let (s, rest) = match (c.kind(), op.kind()) {
            (SyntaxKind::BINARY_EXPR, SyntaxKind::EQ2) => {
                let (l, r) = binary_operands(&c)?;
                (l.text().to_string(), r.text().to_string())
            }
            (SyntaxKind::BINARY_EXPR, SyntaxKind::IN_KW | SyntaxKind::NOT_IN_KW) | (SyntaxKind::IS_EXPR, _) => {
                let l = c.first_child()?;
                let start = usize::from(op.text_range().start() - c.text_range().start());
                (l.text().to_string(), c.text().to_string()[start..].to_string())
            }
            _ => return None,
        };
        match &subject {
            Some(prev) if *prev != s => return None,
            _ => subject = Some(s),
        }
        edits.push(Edit { range: cond.text_range(), text: rest });
    }
    let subject = subject?;
    // A subject evaluated once must mean the same as evaluating it in each condition.
    if subject.contains('(') {
        return None;
    }
    edits.push(Edit { range: tok.text_range(), text: format!("when ({subject})") });
    Some(Fix::new(format!("Introduce '{subject}' as subject of 'when'"), edits))
}

/// `when (x) { 1 -> …; is T -> … }` → `when { x == 1 -> …; x is T -> … }`.
fn remove_subject(tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::WHEN_KW {
        return None;
    }
    let w = tok.parent().filter(|p| p.kind() == SyntaxKind::WHEN_EXPR)?;
    let subject = w.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT)?;
    let expr = subject.first_child()?;
    // `when (val x = …)` and subjects with effects (evaluated once) stay.
    if !matches!(expr.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::THIS_EXPR) || expr.text().to_string().contains('(') {
        return None;
    }
    let s = expr.text().to_string();
    let close = w.children_with_tokens().find(|t| t.kind() == SyntaxKind::R_PAREN)?;
    let mut edits = vec![Edit { range: crate::syntax::TextRange::new(tok.text_range().end(), close.text_range().end()), text: String::new() }];
    for entry in w.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY) {
        let conds: Vec<SyntaxNode> = entry.children().filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION).collect();
        if conds.is_empty() {
            continue;
        }
        let texts: Vec<String> = conds
            .iter()
            .map(|c| {
                let starts_with_op = c.first_child().is_some_and(|f| f.kind() == SyntaxKind::OPERATION_REFERENCE) || c.first_child().is_none();
                if starts_with_op { format!("{s} {}", c.text()) } else { format!("{s} == {}", c.text()) }
            })
            .collect();
        let start = conds[0].text_range().start();
        let end = conds.last().unwrap().text_range().end();
        edits.push(Edit { range: crate::syntax::TextRange::new(start, end), text: texts.join(" || ") });
    }
    Some(Fix::new("Remove subject from 'when'", edits))
}

// ------------------------------------------------------------ for ↔ forEach

/// Whether `n` contains a jump (`break`, `continue`, `return`) outside nested functions
/// and lambdas.
fn has_jumps(n: &SyntaxNode) -> bool {
    n.descendants().any(|d| {
        matches!(d.kind(), SyntaxKind::BREAK_EXPR | SyntaxKind::CONTINUE_EXPR | SyntaxKind::RETURN_EXPR)
            && !d.ancestors().take_while(|a| a != n).any(|a| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::CLASS | SyntaxKind::OBJECT_LITERAL))
    })
}

/// `for (x in xs) body` → `xs.forEach { x -> body }` (no jumps in the body).
fn for_to_for_each(file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::FOR_KW {
        return None;
    }
    let stmt = tok.parent().filter(|p| p.kind() == SyntaxKind::FOR_EXPR)?;
    if !stmt.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK) || stmt.parent().is_some_and(|p| p.kind() == SyntaxKind::LABELED_EXPR) {
        return None;
    }
    let param = stmt.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER)?;
    let range = stmt.children().find(|c| c.kind() == SyntaxKind::LOOP_RANGE)?.first_child()?;
    let body = stmt.children().find(|c| c.kind() == SyntaxKind::BODY)?.first_child()?;
    if has_jumps(&body) {
        return None;
    }
    let receiver = if is_primary(&range) { range.text().to_string() } else { format!("({})", range.text()) };
    let indent = indent_at(&file.text, stmt.text_range().start());
    let inner = if body.kind() == SyntaxKind::BLOCK {
        let text = body.text().to_string();
        let t = text.trim_start_matches('{').trim_end_matches('}').trim_matches(|c| c == ' ' || c == '\n');
        if t.is_empty() {
            return None;
        }
        format!("\n{indent}    {t}\n{indent}")
    } else {
        format!("\n{indent}    {}\n{indent}", reindent(&body.text().to_string(), &indent, &format!("{indent}    ")))
    };
    Some(Fix::replace("Replace with a 'forEach' function call", stmt.text_range(), format!("{receiver}.forEach {{ {} ->{inner}}}", param.text())))
}

/// `xs.forEach { x -> body }` → `for (x in xs) { body }` (`return@forEach` becomes
/// `continue`; the implicit `it` is named).
fn for_each_to_for(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::IDENT || tok.text() != "forEach" {
        return None;
    }
    let callee = tok.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF)?;
    let call = callee.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR)?;
    let q = call.parent().filter(|q| q.kind() == SyntaxKind::DOT_QUALIFIED_EXPR && q.last_child().as_ref() == Some(&call))?;
    if !q.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK) || call.children().any(|c| matches!(c.kind(), SyntaxKind::VALUE_ARGUMENT_LIST | SyntaxKind::TYPE_ARGUMENT_LIST)) {
        return None;
    }
    let lambda = call.children().find(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT)?.first_child().filter(|l| l.kind() == SyntaxKind::LAMBDA_EXPR)?;
    let recv = q.first_child()?;
    // The standard library's `forEach` (not Java's `Iterable.forEach(Consumer)`).
    let defs = e.resolve_name_ref(&callee);
    if defs.is_empty() || !defs.iter().all(|d| matches!(d, Def::Symbol { sym, .. } if sym.fq_name.starts_with("kotlin.collections.") || sym.fq_name.starts_with("kotlin.sequences."))) {
        return None;
    }
    let block = lambda.children().find(|c| c.kind() == SyntaxKind::BLOCK)?;
    let mut edits_in_body: Vec<(crate::syntax::TextRange, String)> = Vec::new();
    // `return@forEach` → `continue`, unless inside a nested loop (it would bind there).
    for r in block.descendants().filter(|d| d.kind() == SyntaxKind::RETURN_EXPR) {
        let label = r.children().find(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER).map(|l| l.text().to_string());
        if label.as_deref() != Some("@forEach") {
            continue;
        }
        let in_loop = r.ancestors().take_while(|a| *a != block).any(|a| matches!(a.kind(), SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR | SyntaxKind::LAMBDA_EXPR | SyntaxKind::FUN));
        if in_loop || r.children().any(|c| c.kind() != SyntaxKind::LABEL_QUALIFIER) {
            return None;
        }
        edits_in_body.push((r.text_range(), "continue".into()));
    }
    let param = match lambda.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) {
        Some(list) => {
            let mut ps = list.children();
            match (ps.next(), ps.next()) {
                (Some(p), None) => p.text().to_string(),
                _ => return None,
            }
        }
        None => {
            // Name the implicit `it` after its uses.
            let name = super::assists::unique_name(e, &block, "item");
            for it in block.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == "it") {
                let mine = e.resolve_name_ref(&it).iter().any(|d| matches!(d, Def::Local(l) if l.kind == LocalKind::It && lambda.first_token().is_some_and(|t| t.text_range() == l.name_range)));
                if mine {
                    edits_in_body.push((it.text_range(), name.clone()));
                }
            }
            name
        }
    };
    // The statements, with the edits applied, re-indented one level less deep.
    let indent = indent_at(&file.text, q.text_range().start());
    let start = block.text_range().start();
    let mut text = block.text().to_string();
    edits_in_body.sort_by_key(|(r, _)| std::cmp::Reverse(r.start()));
    for (r, t) in edits_in_body {
        text.replace_range(usize::from(r.start() - start)..usize::from(r.end() - start), &t);
    }
    let body = text.trim();
    if body.is_empty() {
        return None;
    }
    let receiver = recv.text().to_string();
    Some(Fix::replace("Replace with a 'for' loop", q.text_range(), format!("for ({param} in {receiver}) {{\n{indent}    {body}\n{indent}}}")))
}
