//! Syntax rewrites at the cursor: inline variable, add / remove braces, `if` ↔ `when`,
//! move a lambda out of parentheses, string concatenation → template.

use super::infer::{Def, Engine, LocalKind};
use crate::analysis::FileData;
use crate::analysis::lints::{Edit, Fix, indent_at, initializer};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

pub fn rewrites(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Vec<Fix> {
    let mut out = Vec::new();
    out.extend(inline_variable(e, file, tok));
    out.extend(braces(file, tok));
    out.extend(if_to_when(file, tok));
    out.extend(when_to_if(file, tok));
    out.extend(lambda_out_of_parentheses(tok));
    out.extend(lambda_into_parentheses(tok));
    out.extend(string_template(e, tok));
    out
}

/// Shift the lines after the first from `from` to `to` indentation.
fn reindent(text: &str, from: &str, to: &str) -> String {
    let mut lines = text.split('\n');
    let mut out = lines.next().unwrap_or_default().to_string();
    for l in lines {
        out.push('\n');
        match l.strip_prefix(from) {
            Some(rest) => {
                out.push_str(to);
                out.push_str(rest);
            }
            None => out.push_str(l),
        }
    }
    out
}

/// The whole lines of `n` (indentation through the newline), for deleting a statement.
fn line_range(text: &str, n: &SyntaxNode) -> TextRange {
    let start = usize::from(n.text_range().start());
    let end = usize::from(n.text_range().end());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let s = if text[line_start..start].trim().is_empty() { line_start } else { start };
    let e = if text[end..].starts_with('\n') { end + 1 } else { end };
    TextRange::new(TextSize::from(s as u32), TextSize::from(e as u32))
}

fn is_atomic(n: &SyntaxNode) -> bool {
    use SyntaxKind::*;
    matches!(n.kind(), NAME_REF | CALL_EXPR | DOT_QUALIFIED_EXPR | SAFE_QUALIFIED_EXPR | LITERAL | STRING_TEMPLATE | PAREN_EXPR | INDEX_EXPR | THIS_EXPR | COLLECTION_LITERAL | CALLABLE_REFERENCE)
}

// ------------------------------------------------------------ inline variable

/// On a local `val` (its name or a use): replace every use with the initializer.
fn inline_variable(e: &Engine, file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    let parent = tok.parent()?;
    let decl = match parent.kind() {
        SyntaxKind::PROPERTY => parent,
        SyntaxKind::NAME_REF => {
            let defs = e.resolve_name_ref(&parent);
            let [Def::Local(l)] = defs.as_slice() else { return None };
            if l.kind != LocalKind::Val {
                return None;
            }
            crate::syntax::covering(&e.root, l.name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::PROPERTY)?
        }
        _ => return None,
    };
    let block = decl.parent().filter(|p| p.kind() == SyntaxKind::BLOCK)?;
    let name_tok = ast::token(&decl, SyntaxKind::IDENT)?;
    if ast::token(&decl, SyntaxKind::VAL_KW).is_none() || decl.children().any(|c| c.kind() == SyntaxKind::PROPERTY_DELEGATE) {
        return None;
    }
    let value = initializer(&decl)?;
    let name = ast::ident_text(&name_tok);
    let uses: Vec<SyntaxNode> = block
        .descendants()
        .filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text_range().start() > decl.text_range().end())
        .filter(|d| d.first_token().is_some_and(|t| ast::ident_text(&t) == name))
        .filter(|d| matches!(e.resolve_name_ref(d).as_slice(), [Def::Local(l)] if l.name_range == name_tok.text_range()))
        .collect();
    if uses.is_empty() {
        return None;
    }
    let text = value.text().to_string();
    let mut edits = vec![Edit { range: line_range(&file.text, &decl), text: String::new() }];
    for u in &uses {
        let up = u.parent();
        let replacement = match up.as_ref().map(|p| p.kind()) {
            // `"$x"` → `"${init}"`.
            Some(SyntaxKind::SHORT_STRING_TEMPLATE_ENTRY) => {
                edits.push(Edit { range: up.as_ref()?.text_range(), text: format!("${{{text}}}") });
                continue;
            }
            Some(k) if !is_atomic(&value)
                && matches!(k, SyntaxKind::BINARY_EXPR | SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::BINARY_WITH_TYPE | SyntaxKind::IS_EXPR | SyntaxKind::INDEX_EXPR | SyntaxKind::CALL_EXPR) =>
            {
                format!("({text})")
            }
            _ => text.clone(),
        };
        edits.push(Edit { range: u.text_range(), text: replacement });
    }
    Some(Fix::new(format!("Inline variable '{name}'"), edits))
}

// ------------------------------------------------------------ braces

/// The `if`/`for`/`while`/`do` statement whose keyword (or `else`) is `tok`.
fn control_statement(tok: &SyntaxToken) -> Option<SyntaxNode> {
    match tok.kind() {
        SyntaxKind::IF_KW | SyntaxKind::ELSE_KW | SyntaxKind::FOR_KW | SyntaxKind::WHILE_KW | SyntaxKind::DO_KW => {
            tok.parent().filter(|p| matches!(p.kind(), SyntaxKind::IF_EXPR | SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR))
        }
        _ => None,
    }
}

/// Bodies of a control statement: `if` branches (not an `else if`), loop bodies.
fn bodies(stmt: &SyntaxNode) -> Vec<SyntaxNode> {
    stmt.children()
        .filter(|c| matches!(c.kind(), SyntaxKind::THEN_BRANCH | SyntaxKind::ELSE_BRANCH | SyntaxKind::BODY))
        .filter(|b| !b.children().any(|c| c.kind() == SyntaxKind::IF_EXPR && b.kind() == SyntaxKind::ELSE_BRANCH))
        .collect()
}

fn braces(file: &FileData, tok: &SyntaxToken) -> Vec<Fix> {
    let Some(stmt) = control_statement(tok) else { return Vec::new() };
    if stmt.parent().is_none_or(|p| !matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::ELSE_BRANCH)) {
        return Vec::new();
    }
    let what = match stmt.kind() {
        SyntaxKind::IF_EXPR => "if",
        SyntaxKind::FOR_EXPR => "for",
        SyntaxKind::WHILE_EXPR => "while",
        _ => "do-while",
    };
    // The indentation of the statement chain (an `else if` sits on the `if`'s line).
    let top = stmt.ancestors().take_while(|a| a.kind() != SyntaxKind::BLOCK).last().unwrap_or_else(|| stmt.clone());
    let indent = indent_at(&file.text, top.text_range().start());
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for b in bodies(&stmt) {
        let Some(inner) = b.first_child() else { continue };
        if inner.kind() == SyntaxKind::BLOCK {
            let statements: Vec<SyntaxNode> = inner.children().collect();
            let comments = inner.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT));
            // One plain statement (an `if` inside would capture a following `else`).
            if let [only] = statements.as_slice()
                && !comments
                && !matches!(only.kind(), SyntaxKind::IF_EXPR | SyntaxKind::PROPERTY | SyntaxKind::FUN | SyntaxKind::CLASS)
            {
                remove.push(Edit { range: inner.text_range(), text: only.text().to_string() });
            }
        } else {
            let body = reindent(&inner.text().to_string(), &indent, &format!("{indent}    "));
            add.push(Edit { range: inner.text_range(), text: format!("{{\n{indent}    {body}\n{indent}}}") });
        }
    }
    let mut out = Vec::new();
    if !add.is_empty() {
        out.push(Fix::new(format!("Add braces to '{what}' statement"), add));
    }
    if !remove.is_empty() {
        out.push(Fix::new(format!("Remove braces from '{what}' statement"), remove));
    }
    out
}

// ------------------------------------------------------------ if ↔ when

/// The text of a branch body for a `when` entry, re-indented one level deeper.
fn branch_text(body: &SyntaxNode, from: &str, to: &str) -> Option<String> {
    Some(reindent(&body.first_child()?.text().to_string(), from, to))
}

fn if_to_when(file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::IF_KW {
        return None;
    }
    let mut top = tok.parent()?;
    while let Some(p) = top.parent().filter(|p| p.kind() == SyntaxKind::ELSE_BRANCH).and_then(|b| b.parent()).filter(|p| p.kind() == SyntaxKind::IF_EXPR) {
        top = p;
    }
    let indent = indent_at(&file.text, top.text_range().start());
    let inner = format!("{indent}    ");
    let mut entries = Vec::new();
    let mut cur = top.clone();
    loop {
        let cond = cur.children().find(|c| c.kind() == SyntaxKind::CONDITION)?;
        let then = cur.children().find(|c| c.kind() == SyntaxKind::THEN_BRANCH)?;
        entries.push(format!("{inner}{} -> {}", cond.text(), branch_text(&then, &indent, &inner)?));
        match cur.children().find(|c| c.kind() == SyntaxKind::ELSE_BRANCH) {
            Some(els) => match els.first_child() {
                Some(next) if next.kind() == SyntaxKind::IF_EXPR => cur = next,
                _ => {
                    entries.push(format!("{inner}else -> {}", branch_text(&els, &indent, &inner)?));
                    break;
                }
            },
            None => break,
        }
    }
    // A `when` used as a value must be exhaustive; an `if` value always has an `else`.
    if entries.len() < 2 {
        return None;
    }
    Some(Fix::replace("Replace 'if' with 'when'", top.text_range(), format!("when {{\n{}\n{indent}}}", entries.join("\n"))))
}

fn when_to_if(file: &FileData, tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::WHEN_KW {
        return None;
    }
    let when = tok.parent()?;
    // The subject is repeated in each condition: only a plain name or property chain.
    let subject = match when.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) {
        Some(s) => {
            let expr = s.first_child()?;
            let plain = expr.descendants().all(|d| matches!(d.kind(), SyntaxKind::NAME_REF | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::THIS_EXPR));
            if !plain {
                return None;
            }
            Some(expr.text().to_string())
        }
        None => None,
    };
    let indent = indent_at(&file.text, when.text_range().start());
    let inner = format!("{indent}    ");
    let entries: Vec<SyntaxNode> = when.children().filter(|c| c.kind() == SyntaxKind::WHEN_ENTRY).collect();
    let mut parts = Vec::new();
    let mut has_else = false;
    for (i, entry) in entries.iter().enumerate() {
        let body = entry.children().find(|c| c.kind() == SyntaxKind::BODY)?.first_child()?;
        let body_text = match body.kind() {
            SyntaxKind::BLOCK => {
                let content = body.text().to_string();
                let content = content.trim_start_matches('{').trim_end_matches('}').trim();
                reindent(content, &inner, &inner)
            }
            _ => reindent(&body.text().to_string(), &format!("{inner}    "), &inner),
        };
        if ast::token(entry, SyntaxKind::ELSE_KW).is_some() {
            if i + 1 != entries.len() {
                return None;
            }
            parts.push(format!(" else {{\n{inner}{body_text}\n{indent}}}"));
            has_else = true;
            continue;
        }
        let conds: Vec<String> = entry
            .children()
            .filter(|c| c.kind() == SyntaxKind::WHEN_CONDITION)
            .map(|c| {
                let t = c.text().to_string();
                match &subject {
                    None => t,
                    Some(s) if c.children().any(|x| x.kind() == SyntaxKind::OPERATION_REFERENCE) => format!("{s} {t}"),
                    Some(s) => format!("{s} == {t}"),
                }
            })
            .collect();
        let cond = conds.join(" || ");
        let head = if i == 0 { format!("if ({cond}) {{") } else { format!(" else if ({cond}) {{") };
        parts.push(format!("{head}\n{inner}{body_text}\n{indent}}}"));
    }
    if parts.is_empty() || !has_else && when.parent().is_some_and(|p| p.kind() != SyntaxKind::BLOCK) {
        return None;
    }
    Some(Fix::replace("Replace 'when' with 'if'", when.text_range(), parts.concat()))
}

// ------------------------------------------------------------ lambda arguments

/// `foo(a, { … })` → `foo(a) { … }`.
fn lambda_out_of_parentheses(tok: &SyntaxToken) -> Option<Fix> {
    let arg = tok.parent_ancestors().find(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT && a.children().any(|c| c.kind() == SyntaxKind::LAMBDA_EXPR))?;
    let lambda = arg.children().find(|c| c.kind() == SyntaxKind::LAMBDA_EXPR)?;
    if arg.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) {
        return None;
    }
    let list = arg.parent().filter(|l| l.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    let call = list.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR)?;
    if call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) || list.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).last().as_ref() != Some(&arg) {
        return None;
    }
    let args = list.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).count();
    let edits = if args == 1 {
        vec![Edit { range: list.text_range(), text: format!(" {}", lambda.text()) }]
    } else {
        let prev = arg.prev_sibling()?;
        vec![Edit { range: TextRange::new(prev.text_range().end(), list.text_range().end()), text: format!(") {}", lambda.text()) }]
    };
    Some(Fix::new("Move lambda argument out of parentheses", edits))
}

/// `foo(a) { … }` → `foo(a, { … })` (on the trailing lambda's `{`).
fn lambda_into_parentheses(tok: &SyntaxToken) -> Option<Fix> {
    if tok.kind() != SyntaxKind::L_CURLY {
        return None;
    }
    let lambda = tok.parent().filter(|l| l.kind() == SyntaxKind::LAMBDA_EXPR)?;
    let arg = lambda.parent().filter(|a| a.kind() == SyntaxKind::LAMBDA_ARGUMENT)?;
    let call = arg.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR)?;
    if call.children().filter(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT).count() > 1 {
        return None;
    }
    let list = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST);
    let edits = match list {
        Some(list) => {
            let close = list.last_token().filter(|t| t.kind() == SyntaxKind::R_PAREN)?;
            let sep = if list.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT) { ", " } else { "" };
            vec![Edit { range: TextRange::new(close.text_range().start(), arg.text_range().end()), text: format!("{sep}{})", lambda.text()) }]
        }
        None => {
            let callee = call.first_child()?;
            vec![Edit { range: TextRange::new(callee.text_range().end(), arg.text_range().end()), text: format!("({})", lambda.text()) }]
        }
    };
    Some(Fix::new("Move lambda argument into parentheses", edits))
}

// ------------------------------------------------------------ string templates

/// `"Hi " + name + "!"` → `"Hi $name!"`.
fn string_template(e: &Engine, tok: &SyntaxToken) -> Option<Fix> {
    let plus = |n: &SyntaxNode| n.kind() == SyntaxKind::BINARY_EXPR && n.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE).is_some_and(|o| o.text() == "+");
    let mut top = tok.parent_ancestors().find(|a| plus(a))?;
    while let Some(p) = top.parent().filter(|p| plus(p)) {
        top = p;
    }
    // Operands of the left-associated chain.
    let mut operands = Vec::new();
    let mut cur = top.clone();
    loop {
        let mut kids = cur.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE);
        let (l, r) = (kids.next()?, kids.next()?);
        operands.push(r);
        if plus(&l) {
            cur = l;
        } else {
            operands.push(l);
            break;
        }
    }
    operands.reverse();
    let raw = |n: &SyntaxNode| n.first_token().is_some_and(|t| t.text() == "\"\"\"");
    let template = |n: &SyntaxNode| n.kind() == SyntaxKind::STRING_TEMPLATE && !raw(n);
    // Concatenation from the start: the first operand is a String.
    let first_string = template(&operands[0]) || e.type_of(&operands[0]).is_some_and(|t| matches!(&t, crate::symbols::TypeRef::Named { fq: Some(f), nullable: false, .. } if &**f == "kotlin.String"));
    if !first_string || !operands.iter().any(template) || operands.iter().any(raw) {
        return None;
    }
    let mut out = String::new();
    for (i, op) in operands.iter().enumerate() {
        if template(op) {
            let t = op.text().to_string();
            out.push_str(&t[1..t.len() - 1]);
            continue;
        }
        let text = op.text().to_string();
        let next_ident = operands.get(i + 1).filter(|n| template(n)).and_then(|n| n.text().to_string().chars().nth(1)).is_some_and(|c| c.is_alphanumeric() || c == '_');
        if op.kind() == SyntaxKind::NAME_REF && !next_ident && !text.starts_with('`') {
            out.push_str(&format!("${text}"));
        } else {
            out.push_str(&format!("${{{text}}}"));
        }
    }
    Some(Fix::replace("Convert concatenation to template", top.text_range(), format!("\"{out}\"")))
}
