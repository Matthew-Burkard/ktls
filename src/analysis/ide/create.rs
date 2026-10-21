//! "Create from usage": declarations for unresolved names, shaped by how they are used.

use std::path::{Path, PathBuf};

use super::infer::{Engine, fq_of};
use crate::analysis::Snapshot;
use crate::symbols::{Origin, TypeRef};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateFix {
    pub title: String,
    /// (file, range, new text); the target may be another file.
    pub edits: Vec<(PathBuf, TextRange, String)>,
}

const TODO: &str = "TODO(\"Not yet implemented\")";

pub fn create_fixes(snap: &Snapshot, path: &Path, range: TextRange) -> Vec<CreateFix> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some(file) = snap.file(path) else { return Vec::new() };
    let text = &file.text;
    let tok = if range.is_empty() {
        e.root.token_at_offset(range.start()).find(|t| t.kind() == SyntaxKind::IDENT)
    } else {
        e.root.descendants_with_tokens().filter_map(|el| el.into_token()).find(|t| t.kind() == SyntaxKind::IDENT && t.text_range().intersect(range).is_some_and(|r| !r.is_empty()))
    };
    let Some(tok) = tok else { return Vec::new() };
    let Some(nr) = tok.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF) else { return Vec::new() };
    if !e.resolve_name_ref(&nr).is_empty() || nr.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_LIST | SyntaxKind::PACKAGE_DIRECTIVE)) {
        return Vec::new();
    }
    let name = ast::ident_text(&tok);
    let call = nr.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(&nr));
    let expr = call.clone().unwrap_or_else(|| nr.clone());
    let receiver = expr
        .parent()
        .filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(&expr))
        .and_then(|p| p.first_child());
    let expected = expected_type(&e, &expr);
    let mut out = Vec::new();
    match (&receiver, &call) {
        // `recv.name(…)` / `recv.name`: a member of a project class, or an extension.
        (Some(recv), _) => {
            let Some(t) = e.type_of(recv) else { return out };
            let Some(fq) = fq_of(&t).map(str::to_string) else { return out };
            let decl = match &call {
                Some(c) => function_text(&e, &name, c, expected.as_ref(), None),
                None => property_text(&name, expected.as_ref(), None),
            };
            let what = if call.is_some() { "function" } else { "property" };
            if let Some(c) = e.classifier(&fq)
                && let Origin::Source { file: target, .. } = &c.symbol().origin
                && let Some(edit) = member_insertion(snap, target, &c.symbol().fq_name, &decl)
            {
                out.push(CreateFix { title: format!("Create member {what} '{}.{name}'", c.symbol().name), edits: vec![edit] });
            }
            let shown = t.display().replace('!', "").trim_end_matches('?').to_string();
            let ext = match &call {
                Some(c) => function_text(&e, &name, c, expected.as_ref(), Some(&shown)),
                None => format!("val {shown}.{name}: {}\n    get() = {TODO}", expected.as_ref().map(type_text).unwrap_or_else(|| "Any".into())),
            };
            out.push(CreateFix { title: format!("Create extension {what} '{shown}.{name}'"), edits: vec![end_of_file(path, text, &ext)] });
        }
        // `Name(…)`: a class.
        (None, Some(c)) if name.starts_with(char::is_uppercase) => {
            let params = parameters(&e, c);
            let decl = if params.is_empty() { format!("class {name}") } else { format!("class {name}({})", params.iter().map(|p| format!("val {p}")).collect::<Vec<_>>().join(", ")) };
            out.push(CreateFix { title: format!("Create class '{name}'"), edits: vec![end_of_file(path, text, &decl)] });
        }
        // `name(…)`: a function in the enclosing class, or top-level.
        (None, Some(c)) => {
            let decl = function_text(&e, &name, c, expected.as_ref(), None);
            if let Some(class) = enclosing_class(&nr)
                && let Some(edit) = class_body_insertion(path, text, &class, &format!("private {decl}"))
            {
                out.push(CreateFix { title: format!("Create member function '{name}'"), edits: vec![edit] });
            }
            out.push(CreateFix { title: format!("Create function '{name}'"), edits: vec![end_of_file(path, text, &decl)] });
        }
        // `name`: a local variable, a property, or a parameter.
        (None, None) => {
            let ty = expected.as_ref().map(type_text).unwrap_or_else(|| "Any".into());
            if let Some(stmt) = statement_of(&nr)
                && !is_assignment_target(&nr)
            {
                let indent = crate::analysis::lints::indent_at(text, stmt.text_range().start());
                out.push(CreateFix {
                    title: format!("Create local variable '{name}'"),
                    edits: vec![(path.to_path_buf(), TextRange::empty(stmt.text_range().start()), format!("val {name}: {ty} = {TODO}\n{indent}"))],
                });
            }
            let decl = property_text(&name, expected.as_ref(), None);
            if let Some(class) = enclosing_class(&nr)
                && let Some(edit) = class_body_insertion(path, text, &class, &format!("private {decl}"))
            {
                out.push(CreateFix { title: format!("Create property '{name}'"), edits: vec![edit] });
            }
            if let Some(f) = nr.ancestors().find(|a| a.kind() == SyntaxKind::FUN)
                && let Some(list) = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)
                && let Some(close) = list.last_token().filter(|t| t.kind() == SyntaxKind::R_PAREN)
            {
                let sep = if list.children().any(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) { ", " } else { "" };
                out.push(CreateFix { title: format!("Create parameter '{name}'"), edits: vec![(path.to_path_buf(), TextRange::empty(close.text_range().start()), format!("{sep}{name}: {ty}"))] });
            }
        }
    }
    out
}

fn type_text(t: &TypeRef) -> String {
    t.display().replace('!', "")
}

/// What the context requires: a declared property type, a parameter type, or a return type.
fn expected_type(e: &Engine, expr: &SyntaxNode) -> Option<TypeRef> {
    let mut node = expr.clone();
    while let Some(p) = node.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.last_child().as_ref() == Some(&node)) {
        node = p;
    }
    let parent = node.parent()?;
    let t = match parent.kind() {
        SyntaxKind::PROPERTY => ast::child::<ast::TypeRef>(&parent).map(|t| e.resolve_syntax_type(&t)),
        SyntaxKind::RETURN_EXPR => {
            let f = parent.ancestors().find(|a| a.kind() == SyntaxKind::FUN)?;
            ast::FunDecl::cast(f).and_then(|d| d.return_type()).map(|t| e.resolve_syntax_type(&t))
        }
        SyntaxKind::FUN => ast::FunDecl::cast(parent).and_then(|d| d.return_type()).map(|t| e.resolve_syntax_type(&t)),
        SyntaxKind::CONDITION => Some(TypeRef::named("kotlin.Boolean")),
        _ => None,
    }?;
    (!t.display().contains("<error>")).then_some(t)
}

/// `p: T` for each argument: named arguments keep their name, references their identifier.
fn parameters(e: &Engine, call: &SyntaxNode) -> Vec<String> {
    let Some(list) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return Vec::new() };
    let mut used: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (i, a) in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).enumerate() {
        let value = a.children().last();
        let named = a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
        let ty = value.as_ref().and_then(|v| e.type_of(v)).map(|t| type_text(&t)).filter(|t| !t.contains("<error>")).unwrap_or_else(|| "Any".into());
        let mut name = named
            .or_else(|| value.as_ref().filter(|v| v.kind() == SyntaxKind::NAME_REF).map(|v| v.text().to_string()))
            .or_else(|| value.as_ref().filter(|v| v.kind() == SyntaxKind::DOT_QUALIFIED_EXPR).and_then(|v| v.last_child()).filter(|l| l.kind() == SyntaxKind::NAME_REF).map(|l| l.text().to_string()))
            .unwrap_or_else(|| format!("p{}", i + 1));
        if used.contains(&name) {
            name = format!("{name}{}", i + 1);
        }
        used.push(name.clone());
        out.push(format!("{name}: {ty}"));
    }
    // A trailing lambda becomes a function-typed parameter.
    if call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
        out.push("block: () -> Unit".into());
    }
    out
}

fn function_text(e: &Engine, name: &str, call: &SyntaxNode, ret: Option<&TypeRef>, receiver: Option<&str>) -> String {
    let params = parameters(e, call).join(", ");
    let recv = receiver.map(|r| format!("{r}.")).unwrap_or_default();
    let ret = ret.map(|t| format!(": {}", type_text(t))).unwrap_or_default();
    format!("fun {recv}{name}({params}){ret} {{\n    {TODO}\n}}")
}

fn property_text(name: &str, ty: Option<&TypeRef>, receiver: Option<&str>) -> String {
    let recv = receiver.map(|r| format!("{r}.")).unwrap_or_default();
    format!("val {recv}{name}: {} = {TODO}", ty.map(type_text).unwrap_or_else(|| "Any".into()))
}

fn indent(decl: &str, by: &str) -> String {
    decl.lines().map(|l| if l.is_empty() { String::new() } else { format!("{by}{l}") }).collect::<Vec<_>>().join("\n")
}

fn end_of_file(path: &Path, text: &str, decl: &str) -> (PathBuf, TextRange, String) {
    let end = TextSize::of(text);
    let sep = if text.ends_with("\n\n") || text.is_empty() { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
    (path.to_path_buf(), TextRange::empty(end), format!("{sep}{decl}\n"))
}

fn enclosing_class(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.ancestors().find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION))
}

/// Insert `decl` as the last member of `class` (which needs a `{ }` body).
fn class_body_insertion(path: &Path, text: &str, class: &SyntaxNode, decl: &str) -> Option<(PathBuf, TextRange, String)> {
    let body = class.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY)?;
    let close = body.last_token().filter(|t| t.kind() == SyntaxKind::R_CURLY)?;
    let base = crate::analysis::lints::indent_at(text, class.text_range().start());
    let member = format!("{base}    ");
    // After the last non-blank content before `}`.
    let mut start = close.text_range().start();
    let mut prev: Option<SyntaxToken> = close.prev_token();
    while let Some(p) = prev.clone().filter(|p| matches!(p.kind(), SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE)) {
        start = p.text_range().start();
        prev = p.prev_token();
    }
    let empty = prev.is_some_and(|p| p.kind() == SyntaxKind::L_CURLY);
    let gap = if empty { "\n" } else { "\n\n" };
    Some((path.to_path_buf(), TextRange::new(start, close.text_range().start()), format!("{gap}{}\n{base}", indent(decl, &member))))
}

/// Insert into the project class `fq` (possibly in another file).
fn member_insertion(snap: &Snapshot, file: &Path, fq: &str, decl: &str) -> Option<(PathBuf, TextRange, String)> {
    let f = snap.file(file)?;
    let root = f.parse().syntax();
    let class = root.descendants().filter(|n| matches!(n.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)).find(|n| {
        let names: Vec<String> = n.ancestors().filter(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)).filter_map(|a| ast::token(&a, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t)).collect();
        let dotted = names.into_iter().rev().collect::<Vec<_>>().join(".");
        fq == dotted || fq.ends_with(&format!(".{dotted}"))
    })?;
    class_body_insertion(file, &f.text, &class, decl)
}

/// The block statement containing `n`.
fn statement_of(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.ancestors().find(|a| a.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK))
}

fn is_assignment_target(n: &SyntaxNode) -> bool {
    n.parent().is_some_and(|p| p.kind() == SyntaxKind::BINARY_EXPR && p.first_child().as_ref() == Some(n) && p.children_with_tokens().any(|t| t.kind() == SyntaxKind::EQ))
}
