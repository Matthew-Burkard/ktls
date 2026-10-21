//! Extract function: selected statements (or one expression) become a private function
//! after the enclosing one; locals they read become parameters, and one local they
//! declare that is used afterwards becomes the return value.

use super::assists::{extraction_site, type_text};
use super::infer::{Def, Engine, LocalKind};
use super::inlay::value_type;
use crate::analysis::FileData;
use crate::analysis::lints::{Edit, Fix, has_modifier, indent_at};
use crate::symbols::TypeRef;
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

/// What is being extracted.
enum Selection {
    Statements(Vec<SyntaxNode>),
    Expression(SyntaxNode),
}

pub fn extract_function(e: &Engine, file: &FileData, range: TextRange) -> Option<Fix> {
    let text = &file.text;
    let sel = &text[usize::from(range.start())..usize::from(range.end())];
    let lead = sel.len() - sel.trim_start().len();
    let range = TextRange::new(range.start() + TextSize::from(lead as u32), range.start() + TextSize::from(sel.trim_end().len() as u32));
    let selection = match extraction_site(e, file, range) {
        Some((expr, _)) => Selection::Expression(expr),
        None => {
            // Whole statements of one block.
            let block = e.root.covering_element(range).ancestors().find(|a| a.kind() == SyntaxKind::BLOCK)?;
            let stmts: Vec<SyntaxNode> = block.children().filter(|s| range.contains_range(s.text_range())).collect();
            let (first, last) = (stmts.first()?, stmts.last()?);
            if first.text_range().start() != range.start() || last.text_range().end() != range.end() {
                return None;
            }
            Selection::Statements(stmts)
        }
    };
    let nodes: Vec<SyntaxNode> = match &selection {
        Selection::Statements(s) => s.clone(),
        Selection::Expression(x) => vec![x.clone()],
    };
    // The enclosing named function: the new one goes after it, with the same receivers.
    let host = nodes[0].ancestors().find(|a| a.kind() == SyntaxKind::FUN && ast::token(a, SyntaxKind::IDENT).is_some())?;
    if host.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) || host.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::FUN) {
        return None;
    }
    // Lambdas with receivers between the host and the selection give implicit `this`.
    if nodes[0].ancestors().take_while(|a| *a != host).any(|a| a.kind() == SyntaxKind::LAMBDA_EXPR && e.lambda_signature(&a).is_none_or(|(r, _)| r.is_some())) {
        return None;
    }
    // Jumps out of the selection cannot move into another function.
    let jumps = nodes.iter().flat_map(|n| n.descendants_with_tokens()).filter_map(|t| t.into_token()).any(|t| {
        matches!(t.kind(), SyntaxKind::RETURN_KW | SyntaxKind::BREAK_KW | SyntaxKind::CONTINUE_KW)
            && !t.parent_ancestors().take_while(|a| !nodes.contains(a)).any(|a| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FUN | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::OBJECT_LITERAL))
    });
    if jumps {
        return None;
    }
    // Parameters: locals declared before the selection that it reads.
    let inside = |r: TextRange| range.contains_range(r);
    let mut params: Vec<(String, TextRange, TypeRef)> = Vec::new();
    for r in nodes.iter().flat_map(|n| n.descendants()).filter(|d| d.kind() == SyntaxKind::NAME_REF) {
        let defs = e.resolve_name_ref(&r);
        let [Def::Local(l)] = defs.as_slice() else { continue };
        if inside(l.name_range) || params.iter().any(|(_, nr, _)| *nr == l.name_range) {
            continue;
        }
        if matches!(l.kind, LocalKind::It | LocalKind::Function | LocalKind::Class) {
            return None;
        }
        // Assigning a local of the caller cannot be done from another function.
        let assigned = r.parent().is_some_and(|p| p.kind() == SyntaxKind::BINARY_EXPR && p.first_child().as_ref() == Some(&r) && p.children().any(|o| o.kind() == SyntaxKind::OPERATION_REFERENCE && o.text().to_string().ends_with('=') && !matches!(o.text().to_string().as_str(), "==" | "!=" | "<=" | ">=")))
            || r.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR) && (p.text().to_string().contains("++") || p.text().to_string().contains("--")));
        if assigned {
            return None;
        }
        let t = l.ty.clone().or_else(|| e.type_of(&r))?;
        params.push((l.name.clone(), l.name_range, t));
    }
    // The result: the expression's value, or one local declared inside and used after.
    let (ret, out_decl): (Option<TypeRef>, Option<(String, String)>) = match &selection {
        Selection::Expression(x) => (Some(value_type(e, x)?), None),
        Selection::Statements(stmts) => {
            let block = stmts[0].parent()?;
            let declared: Vec<SyntaxNode> = stmts.iter().filter(|s| s.kind() == SyntaxKind::PROPERTY).cloned().collect();
            let mut used_after = Vec::new();
            for d in &declared {
                let Some(name) = ast::token(d, SyntaxKind::IDENT) else { continue };
                let after = block.descendants().filter(|r| r.kind() == SyntaxKind::NAME_REF && r.text_range().start() >= range.end()).any(|r| matches!(e.resolve_name_ref(&r).as_slice(), [Def::Local(l)] if l.name_range == name.text_range()));
                if after {
                    used_after.push(d.clone());
                }
            }
            match used_after.as_slice() {
                [] => (None, None),
                [d] => {
                    let name = ast::ident_text(&ast::token(d, SyntaxKind::IDENT)?);
                    let t = match ast::child::<ast::TypeRef>(d) {
                        Some(t) => e.resolve_syntax_type(&t),
                        None => value_type(e, &crate::analysis::lints::initializer(d)?)?,
                    };
                    let kw = if ast::token(d, SyntaxKind::VAR_KW).is_some() { "var" } else { "val" };
                    (Some(t), Some((kw.to_string(), name)))
                }
                _ => return None,
            }
        }
    };
    // Signature text (types must be writable here; imports come along).
    let anchor = host.first_token()?;
    let mut edits: Vec<Edit> = Vec::new();
    let mut sig = Vec::new();
    for (name, _, t) in &params {
        let (tt, imports) = type_text(e, file, t, &anchor)?;
        edits.extend(imports);
        sig.push(format!("{name}: {tt}"));
    }
    let ret_text = match &ret {
        Some(t) if !matches!(t.fq_name(), Some("kotlin.Unit")) => {
            let (tt, imports) = type_text(e, file, t, &anchor)?;
            edits.extend(imports);
            Some(tt)
        }
        _ => None,
    };
    let fun_indent = indent_at(text, host.text_range().start());
    let body_indent = format!("{fun_indent}    ");
    let stmt_indent = indent_at(text, nodes[0].text_range().start());
    let reindent = |s: &str| {
        s.split('\n').enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { l.strip_prefix(stmt_indent.as_str()).map_or(l.to_string(), |r| format!("{body_indent}{r}")) }).collect::<Vec<_>>().join("\n")
    };
    let existing: Vec<String> = host.parent().into_iter().flat_map(|p| p.children()).filter(|c| c.kind() == SyntaxKind::FUN).filter_map(|f| ast::token(&f, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))).collect();
    let mut name = "extracted".to_string();
    let mut i = 2;
    while existing.contains(&name) {
        name = format!("extracted{i}");
        i += 1;
    }
    let suspend = if has_modifier(&host, "suspend") { "suspend " } else { "" };
    let args: Vec<&str> = params.iter().map(|(n, _, _)| n.as_str()).collect();
    let call = format!("{name}({})", args.join(", "));
    let selected = &text[usize::from(range.start())..usize::from(range.end())];
    let ret_sig = ret_text.as_ref().map(|t| format!(": {t}")).unwrap_or_default();
    let new_fun = match (&selection, &out_decl) {
        (Selection::Expression(_), _) => format!("private {suspend}fun {name}({}){ret_sig} = {}", sig.join(", "), reindent(selected)),
        (Selection::Statements(_), Some((_, out))) => format!("private {suspend}fun {name}({}){ret_sig} {{\n{body_indent}{}\n{body_indent}return {out}\n{fun_indent}}}", sig.join(", "), reindent(selected)),
        (Selection::Statements(_), None) => format!("private {suspend}fun {name}({}) {{\n{body_indent}{}\n{fun_indent}}}", sig.join(", "), reindent(selected)),
    };
    let replacement = match &out_decl {
        Some((kw, out)) => format!("{kw} {out} = {call}"),
        None => call,
    };
    edits.push(Edit { range, text: replacement });
    edits.push(Edit { range: TextRange::empty(host.text_range().end()), text: format!("\n\n{fun_indent}{new_fun}") });
    Some(Fix::new("Extract function", edits))
}
