//! KDoc references: `[Name]`, `[Type.member]`, `[kotlin.collections.List]` links and the
//! subjects of `@param`, `@property`, `@throws`, `@exception`, `@see` and `@sample`,
//! resolved in the scope of the documented declaration.

use super::infer::{Def, Engine, LocalDef, LocalKind, class_type};
use crate::symbols::SymbolKind;
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

const TAGS: &[&str] = &["@param", "@property", "@throws", "@exception", "@see", "@sample"];

/// A reference in a KDoc comment: its dotted segments (with their ranges in the file),
/// up to and including the one at the cursor.
pub struct Link {
    pub segments: Vec<(String, TextRange)>,
    /// The KDoc's declaration.
    pub decl: SyntaxNode,
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '`'
}

/// The KDoc token covering `offset`.
pub fn kdoc_at(root: &SyntaxNode, offset: TextSize) -> Option<SyntaxToken> {
    root.token_at_offset(offset).find(|t| t.kind() == SyntaxKind::KDOC && t.text_range().start() < offset && offset < t.text_range().end())
}

/// The link at `offset` in `tok` (a KDOC token). `partial`: the reference may be
/// unfinished (completion), so only the text before the cursor counts.
pub fn link_at(tok: &SyntaxToken, offset: TextSize, partial: bool) -> Option<Link> {
    let text = tok.text();
    let base = tok.text_range().start();
    let rel = usize::from(offset - base);
    // The dotted name around the cursor.
    let in_name = |c: char| is_name_char(c) || c == '.';
    let start = text[..rel].char_indices().rev().take_while(|(_, c)| in_name(*c)).last().map_or(rel, |(i, _)| i);
    let end = if partial { rel } else { text[rel..].char_indices().find(|(_, c)| !in_name(*c)).map_or(text.len(), |(i, _)| rel + i) };
    let before = text[..start].trim_end_matches(' ');
    let linked = text[..start].ends_with('[') || TAGS.iter().any(|t| before.ends_with(t) && before.len() < start);
    if !linked {
        return None;
    }
    let name = &text[start..end];
    let mut segments = Vec::new();
    let mut at = start;
    for seg in name.split('.') {
        let r = TextRange::at(base + TextSize::from(at as u32), TextSize::from(seg.len() as u32));
        segments.push((seg.trim_matches('`').to_string(), r));
        at += seg.len() + 1;
        // Only up to the segment at the cursor.
        if at > rel {
            break;
        }
    }
    Some(Link { segments, decl: tok.parent()? })
}

/// A node inside `decl` from which its parameters and members are in scope.
pub fn anchor(decl: &SyntaxNode) -> SyntaxNode {
    let inner = decl.children().find(|c| matches!(c.kind(), SyntaxKind::BLOCK | SyntaxKind::CLASS_BODY));
    inner.and_then(|b| b.first_child()).or_else(|| decl.children().find(|c| matches!(c.kind(), SyntaxKind::BLOCK | SyntaxKind::CLASS_BODY))).unwrap_or_else(|| decl.clone())
}

/// Parameters of the documented function or primary constructor (and type parameters).
pub fn parameters(decl: &SyntaxNode) -> Vec<(String, TextRange)> {
    let lists = decl.children().filter(|c| matches!(c.kind(), SyntaxKind::VALUE_PARAMETER_LIST | SyntaxKind::PRIMARY_CONSTRUCTOR | SyntaxKind::TYPE_PARAMETER_LIST));
    lists
        .flat_map(|l| l.descendants().filter(|p| matches!(p.kind(), SyntaxKind::VALUE_PARAMETER | SyntaxKind::TYPE_PARAMETER)).collect::<Vec<_>>())
        .filter_map(|p| ast::token(&p, SyntaxKind::IDENT).map(|t| (ast::ident_text(&t), t.text_range())))
        .collect()
}

/// What the link's last segment refers to.
pub fn resolve(e: &Engine, link: &Link) -> Vec<Def> {
    let names: Vec<&str> = link.segments.iter().map(|(n, _)| n.as_str()).collect();
    // A fully qualified class name.
    if names.len() > 1
        && let Some(c) = e.res.find_classifier(&names.join("."))
    {
        return vec![Def::symbol(c.symbol().clone())];
    }
    let first = names[0];
    let mut defs: Vec<Def> = match parameters(&link.decl).into_iter().find(|(n, _)| n == first) {
        Some((name, name_range)) => vec![Def::Local(LocalDef { name, kind: LocalKind::Parameter, name_range, ty: None })],
        None => e.resolve_name_text(&anchor(&link.decl), first),
    };
    for (i, name) in names.iter().enumerate().skip(1) {
        defs = match defs.first() {
            Some(Def::Package(p)) => {
                let fq = format!("{p}.{name}");
                match e.res.find_classifier(&fq) {
                    Some(c) => vec![Def::symbol(c.symbol().clone())],
                    None if i + 1 < names.len() => vec![Def::Package(fq)],
                    None => e.top_level_in(p, name).into_iter().map(Def::symbol).collect(),
                }
            }
            Some(Def::Symbol { sym, .. }) if sym.kind.is_classifier() => {
                let Some(c) = e.classifier(&sym.fq_name) else { return Vec::new() };
                let (nested, statics) = e.all_static_members(&c);
                if let Some((_, fq)) = nested.iter().find(|(n, _)| n == name)
                    && let Some(n) = e.classifier(fq)
                {
                    vec![Def::symbol(n.symbol().clone())]
                } else {
                    let members = statics.into_iter().chain(e.all_members(&class_type(c.symbol())));
                    members.filter(|(m, _)| m.name == *name && m.kind != SymbolKind::Constructor).map(|(sym, subst)| Def::Symbol { sym, subst }).collect()
                }
            }
            _ => return Vec::new(),
        };
    }
    defs
}

/// The definitions of the KDoc reference at `offset`, with the range of its segment.
pub fn defs_at(e: &Engine, offset: TextSize) -> Option<(TextRange, Vec<Def>)> {
    let tok = kdoc_at(&e.root, offset)?;
    let link = link_at(&tok, offset, false)?;
    let range = link.segments.last()?.1;
    if link.segments.last()?.0.is_empty() {
        return None;
    }
    Some((range, resolve(e, &link)))
}
