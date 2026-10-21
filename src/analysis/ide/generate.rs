//! "Generate" source actions at the cursor: `equals()`/`hashCode()` and `toString()` for
//! a class from its properties, and a KDoc stub for a declaration.

use std::path::Path;

use super::infer::Engine;
use crate::analysis::Snapshot;
use crate::analysis::lints::{has_modifier, indent_at, initializer};
use crate::symbols::TypeRef;
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

#[derive(Clone, Debug)]
pub struct Generated {
    pub title: String,
    pub range: TextRange,
    pub text: String,
}

/// A property that takes part in `equals`/`hashCode`/`toString`.
struct Prop {
    name: String,
    nullable: bool,
    array: bool,
}

pub fn actions(snap: &Snapshot, path: &Path, offset: TextSize) -> Vec<Generated> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some(tok) = e.root.token_at_offset(offset).right_biased() else { return Vec::new() };
    let Some(file) = snap.file(path) else { return Vec::new() };
    let text = &file.text;
    let mut out = Vec::new();
    // On a declaration's header (not inside its body).
    let header = tok.parent_ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::PROPERTY | SyntaxKind::BLOCK | SyntaxKind::CLASS_BODY));
    if let Some(decl) = header.filter(|d| !matches!(d.kind(), SyntaxKind::BLOCK | SyntaxKind::CLASS_BODY))
        && let Some(g) = kdoc_stub(&e, text, &decl)
    {
        out.push(g);
    }
    // In the class, but not in a function or accessor body.
    let Some(class) = tok.parent_ancestors().find(|a| a.kind() == SyntaxKind::CLASS) else { return out };
    if tok.parent_ancestors().take_while(|a| *a != class).any(|a| matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::CLASS)) {
        return out;
    }
    if ["data", "enum", "annotation", "value", "inline"].iter().any(|m| has_modifier(&class, m)) || class.children_with_tokens().any(|t| t.kind() == SyntaxKind::INTERFACE_KW) {
        return out;
    }
    let props = properties(&e, &class);
    let Some(name) = ast::token(&class, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)) else { return out };
    let generic = class.children().any(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST);
    let declared = |n: &str| class.children().filter(|c| c.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children()).any(|m| m.kind() == SyntaxKind::FUN && ast::token(&m, SyntaxKind::IDENT).is_some_and(|t| t.text() == n));
    let indent = format!("{}    ", indent_at(text, class.text_range().start()));
    if !props.is_empty() && !declared("equals") && !declared("hashCode") {
        let ty = if generic { format!("{name}<*>") } else { name.clone() };
        let mut eq = format!("override fun equals(other: Any?): Boolean {{\n{indent}    if (this === other) return true\n{indent}    if (other !is {ty}) return false\n");
        for p in &props {
            let n = &p.name;
            let cond = match (p.array, p.nullable) {
                (true, true) => format!("!({n}?.contentEquals(other.{n}) ?: (other.{n} == null))"),
                (true, false) => format!("!{n}.contentEquals(other.{n})"),
                _ => format!("{n} != other.{n}"),
            };
            eq.push_str(&format!("{indent}    if ({cond}) return false\n"));
        }
        eq.push_str(&format!("{indent}    return true\n{indent}}}"));
        let hash_of = |p: &Prop| match (p.array, p.nullable) {
            (true, true) => format!("({}?.contentHashCode() ?: 0)", p.name),
            (true, false) => format!("{}.contentHashCode()", p.name),
            (false, true) => format!("({}?.hashCode() ?: 0)", p.name),
            (false, false) => format!("{}.hashCode()", p.name),
        };
        let hash = if props.len() == 1 {
            format!("override fun hashCode(): Int = {}", hash_of(&props[0]))
        } else {
            let mut h = format!("override fun hashCode(): Int {{\n{indent}    var result = {}\n", hash_of(&props[0]));
            for p in &props[1..] {
                h.push_str(&format!("{indent}    result = 31 * result + {}\n", hash_of(p)));
            }
            h.push_str(&format!("{indent}    return result\n{indent}}}"));
            h
        };
        if let Some(g) = insert_member(text, &class, &[eq, hash], "Generate equals() and hashCode()") {
            out.push(g);
        }
    }
    if !declared("toString") {
        let fields: Vec<String> = props.iter().map(|p| if p.array { format!("{0}=${{{0}.contentToString()}}", p.name) } else { format!("{0}=${0}", p.name) }).collect();
        let body = format!("override fun toString(): String = \"{name}({})\"", fields.join(", "));
        if let Some(g) = insert_member(text, &class, &[body], "Generate toString()") {
            out.push(g);
        }
    }
    out
}

/// Constructor `val`/`var` parameters and body properties with a backing field.
fn properties(e: &Engine, class: &SyntaxNode) -> Vec<Prop> {
    let mut out = Vec::new();
    let shape = |ty: Option<ast::TypeRef>, value: Option<SyntaxNode>| -> (bool, bool) {
        let t = match ty {
            Some(t) => e.resolve_syntax_type(&t),
            None => value.and_then(|v| e.type_of(&v)).unwrap_or(TypeRef::Error),
        };
        let nullable = matches!(super::infer::strip(&t), TypeRef::Named { nullable: true, .. } | TypeRef::Function { nullable: true, .. } | TypeRef::TypeParam { .. }) || matches!(t, TypeRef::Platform(_));
        let array = t.fq_name().is_some_and(|f| f == "kotlin.Array" || f.starts_with("kotlin.") && f.ends_with("Array") && !f[7..].contains('.'));
        (nullable, array)
    };
    let ctor = class.children().filter(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|p| p.descendants()).filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER);
    for p in ctor {
        if !p.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::VAL_KW | SyntaxKind::VAR_KW)) {
            continue;
        }
        let Some(name) = ast::token(&p, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)) else { continue };
        let (nullable, array) = shape(ast::child(&p), None);
        out.push(Prop { name, nullable, array });
    }
    let body = class.children().filter(|c| c.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children());
    for m in body.filter(|m| m.kind() == SyntaxKind::PROPERTY) {
        let delegated = m.children().any(|c| c.kind() == SyntaxKind::PROPERTY_DELEGATE);
        let getter = m.children().any(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR && c.text().to_string().trim_start().starts_with("get"));
        let extension = m.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE);
        if delegated || getter && initializer(&m).is_none() || extension || has_modifier(&m, "const") {
            continue;
        }
        let Some(name) = ast::token(&m, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)) else { continue };
        let (nullable, array) = shape(ast::child(&m), initializer(&m));
        out.push(Prop { name, nullable, array });
    }
    out
}

/// Members appended before the class body's closing brace (or a new body).
fn insert_member(text: &str, class: &SyntaxNode, members: &[String], title: &str) -> Option<Generated> {
    let base = indent_at(text, class.text_range().start());
    let indent = format!("{base}    ");
    let joined: Vec<String> = members.iter().map(|m| format!("{indent}{m}")).collect();
    match class.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY) {
        Some(b) => {
            let close = b.last_token().filter(|t| t.kind() == SyntaxKind::R_CURLY)?;
            let mut start = close.text_range().start();
            let mut prev = close.prev_token();
            while let Some(p) = prev.clone().filter(|p| matches!(p.kind(), SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE)) {
                start = p.text_range().start();
                prev = p.prev_token();
            }
            let empty = prev.is_some_and(|p| p.kind() == SyntaxKind::L_CURLY);
            let sep = if empty { "\n" } else { "\n\n" };
            Some(Generated { title: title.into(), range: TextRange::new(start, close.text_range().start()), text: format!("{sep}{}\n{base}", joined.join("\n\n")) })
        }
        None => Some(Generated { title: title.into(), range: TextRange::empty(class.text_range().end()), text: format!(" {{\n{}\n{base}}}", joined.join("\n\n")) }),
    }
}

/// `/** ... @param ... @return */` above a declaration without KDoc.
fn kdoc_stub(e: &Engine, text: &str, decl: &SyntaxNode) -> Option<Generated> {
    if decl.children_with_tokens().any(|t| t.kind() == SyntaxKind::KDOC) {
        return None;
    }
    // The declaration's first non-trivia token (after leading comments that belong to it).
    let first = decl.children_with_tokens().find(|t| !t.kind().is_trivia())?;
    let start = first.text_range().start();
    let indent = indent_at(text, start);
    let mut lines = vec!["/**".to_string(), " * ".to_string()];
    let params: Vec<String> = match decl.kind() {
        SyntaxKind::FUN => decl.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children()).filter_map(|p| ast::token(&p, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))).collect(),
        SyntaxKind::CLASS => decl
            .children()
            .filter(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR)
            .flat_map(|p| p.descendants())
            .filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER)
            .filter_map(|p| {
                let prop = p.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::VAL_KW | SyntaxKind::VAR_KW));
                ast::token(&p, SyntaxKind::IDENT).map(|t| format!("{} {}", if prop { "@property" } else { "@param" }, ast::ident_text(&t)))
            })
            .collect(),
        _ => Vec::new(),
    };
    let params: Vec<String> = params.into_iter().map(|p| if p.starts_with('@') { p } else { format!("@param {p}") }).collect();
    if !params.is_empty() {
        lines.push(" *".into());
    }
    lines.extend(params.iter().map(|p| format!(" * {p} ")));
    if decl.kind() == SyntaxKind::FUN
        && let Some(f) = ast::FunDecl::cast(decl.clone())
    {
        let returns = match f.return_type() {
            Some(t) => !matches!(e.resolve_syntax_type(&t).fq_name(), Some("kotlin.Unit" | "kotlin.Nothing")),
            None => f.has_expression_body(),
        };
        if returns {
            lines.push(" * @return ".into());
        }
    }
    lines.push(" */".into());
    let body: Vec<String> = lines.iter().enumerate().map(|(i, l)| if i == 0 { l.clone() } else { format!("{indent}{l}") }).collect();
    Some(Generated { title: "Add KDoc".into(), range: TextRange::empty(start), text: format!("{}\n{indent}", body.join("\n")) })
}
