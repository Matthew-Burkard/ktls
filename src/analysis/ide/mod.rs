//! IDE features built on the analysis: completion, definition, hover, references, rename
//! and signature help.

pub mod assists;
pub mod completion;
pub mod constant;
pub mod create;
pub mod extract;
pub mod file_rename;
pub mod format;
pub mod generate;
pub mod hierarchy;
pub mod imports;
pub mod infer;
pub mod inlay;
pub mod intentions;
pub mod kdoc;
pub mod library;
pub mod rename;
pub mod rewrites;
pub mod semantic;
pub mod signature;
pub mod signature_change;
pub mod structure;

use std::path::{Path, PathBuf};

use self::infer::{Def, Engine, LocalDef, LocalKind, substitute};
use super::Snapshot;
use super::diagnostics::{LocalsCache, scope_at};
use super::resolve::Resolved;
use super::source::Import;
use crate::symbols::{Origin, Symbol, SymbolKind};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

/// A navigation target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavTarget {
    pub path: PathBuf,
    pub range: TextRange,
}

#[derive(Clone, Debug)]
pub struct HoverInfo {
    pub markdown: String,
    /// The hovered identifier.
    pub range: TextRange,
}

/// Tokens that may stand for an operator-convention call.
const OPERATOR_TOKENS: &[SyntaxKind] = &[
    SyntaxKind::PLUS,
    SyntaxKind::MINUS,
    SyntaxKind::STAR,
    SyntaxKind::SLASH,
    SyntaxKind::PERCENT,
    SyntaxKind::RANGE,
    SyntaxKind::RANGE_UNTIL,
    SyntaxKind::PLUS_EQ,
    SyntaxKind::MINUS_EQ,
    SyntaxKind::STAR_EQ,
    SyntaxKind::SLASH_EQ,
    SyntaxKind::PERCENT_EQ,
    SyntaxKind::EQ2,
    SyntaxKind::NOT_EQ,
    SyntaxKind::LT,
    SyntaxKind::GT,
    SyntaxKind::LT_EQ,
    SyntaxKind::GT_EQ,
    SyntaxKind::IN_KW,
    SyntaxKind::NOT_IN_KW,
    SyntaxKind::EXCL,
    SyntaxKind::PLUS2,
    SyntaxKind::MINUS2,
    SyntaxKind::L_BRACK,
    SyntaxKind::L_PAREN,
];

/// The identifier (or `this`, or an operator) at `offset`, preferring identifiers and
/// the token to the right.
pub(super) fn token_at(root: &SyntaxNode, offset: TextSize) -> Option<SyntaxToken> {
    let tokens = root.token_at_offset(offset);
    let ident = |t: &SyntaxToken| matches!(t.kind(), SyntaxKind::IDENT | SyntaxKind::THIS_KW | SyntaxKind::SUPER_KW);
    let op = |t: &SyntaxToken| OPERATOR_TOKENS.contains(&t.kind());
    match tokens {
        rowan::TokenAtOffset::None => None,
        rowan::TokenAtOffset::Single(t) => (ident(&t) || op(&t)).then_some(t),
        // The character under a block cursor is the right token. `(` ranks last so that a
        // bar cursor right after `foo` still means `foo` (not an `invoke` call).
        rowan::TokenAtOffset::Between(l, r) => {
            let paren = |t: &SyntaxToken| t.kind() == SyntaxKind::L_PAREN;
            [(&r, ident(&r)), (&r, op(&r) && !paren(&r)), (&l, ident(&l)), (&r, op(&r)), (&l, op(&l))]
                .into_iter()
                .find(|(_, ok)| *ok)
                .map(|(t, _)| t.clone())
        }
    }
}

pub(super) const DECLARATIONS: &[SyntaxKind] = &[
    SyntaxKind::CLASS,
    SyntaxKind::OBJECT_DECLARATION,
    SyntaxKind::FUN,
    SyntaxKind::PROPERTY,
    SyntaxKind::TYPEALIAS,
    SyntaxKind::ENUM_ENTRY,
    SyntaxKind::VALUE_PARAMETER,
    SyntaxKind::TYPE_PARAMETER,
    SyntaxKind::DESTRUCTURING_ENTRY,
];

/// Resolve what the token at `offset` refers to.
pub(super) fn defs_at(e: &Engine, offset: TextSize) -> Option<(TextRange, Vec<Def>)> {
    if let Some(found) = kdoc::defs_at(e, offset) {
        return Some(found);
    }
    let tok = token_at(&e.root, offset)?;
    defs_of_token(e, &tok)
}

pub(super) fn defs_of_token(e: &Engine, tok: &SyntaxToken) -> Option<(TextRange, Vec<Def>)> {
    let tok = tok.clone();
    let range = tok.text_range();
    let parent = tok.parent()?;
    if OPERATOR_TOKENS.contains(&tok.kind()) {
        return Some((range, e.operator_defs(&tok).unwrap_or_default()));
    }
    let defs = match parent.kind() {
        // `val x by delegate` → getValue/setValue
        SyntaxKind::PROPERTY_DELEGATE if tok.text() == "by" => e.operator_defs(&tok).unwrap_or_default(),
        SyntaxKind::NAME_REF => e.resolve_name_ref(&parent),
        SyntaxKind::THIS_EXPR | SyntaxKind::SUPER_EXPR => e.this_target(&parent).map(|s| vec![Def::Symbol { sym: s, subst: Default::default() }]).unwrap_or_default(),
        SyntaxKind::OPERATION_REFERENCE => e.resolve_infix(&parent),
        SyntaxKind::USER_TYPE_SEGMENT => type_segment(e, &parent),
        SyntaxKind::DOT_QUALIFIED_NAME => qualified_name_segment(e, &parent, &tok),
        SyntaxKind::VALUE_ARGUMENT_NAME => {
            // Named argument: the called function.
            let call = parent.ancestors().find(|a| a.kind() == SyntaxKind::CALL_EXPR)?;
            let callee = call.children().next()?;
            if callee.kind() == SyntaxKind::NAME_REF { e.resolve_name_ref(&callee) } else { Vec::new() }
        }
        k if DECLARATIONS.contains(&k) => declaration_def(e, &parent, &tok).into_iter().collect(),
        _ => Vec::new(),
    };
    Some((range, defs))
}

pub(super) fn type_segment(e: &Engine, seg: &SyntaxNode) -> Vec<Def> {
    let Some(ut) = seg.parent() else { return Vec::new() };
    let segs: Vec<SyntaxNode> = ut.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).collect();
    let Some(idx) = segs.iter().position(|s| s == seg) else { return Vec::new() };
    let names: Vec<String> = segs[..=idx].iter().filter_map(|s| ast::token(s, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let package = e.res.summary().map(|s| s.package.clone()).unwrap_or_default();
    let scope = scope_at(&ut, &package, &mut LocalsCache::default());
    match e.res.resolve_type_name(&refs, &scope) {
        Ok(Resolved::Classifier(c)) => vec![Def::Symbol { sym: c.symbol().clone(), subst: Default::default() }],
        Ok(Resolved::TypeParameter(name)) => type_parameter(&ut, &name).into_iter().map(Def::Local).collect(),
        Ok(Resolved::Local(name)) => local_class(&ut, &name).into_iter().map(Def::Local).collect(),
        Ok(Resolved::Package(p)) => vec![Def::Package(p)],
        _ => Vec::new(),
    }
}

fn type_parameter(at: &SyntaxNode, name: &str) -> Option<LocalDef> {
    for anc in at.ancestors() {
        for list in anc.children().filter(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST) {
            for tp in list.children() {
                if let Some(t) = ast::token(&tp, SyntaxKind::IDENT)
                    && ast::ident_text(&t) == name {
                        return Some(LocalDef { name: name.into(), kind: LocalKind::Class, name_range: t.text_range(), ty: None });
                    }
            }
        }
    }
    None
}

fn local_class(at: &SyntaxNode, name: &str) -> Option<LocalDef> {
    for anc in at.ancestors().filter(|a| a.kind() == SyntaxKind::BLOCK) {
        for c in anc.descendants().filter(|c| matches!(c.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS)) {
            if let Some(t) = ast::token(&c, SyntaxKind::IDENT)
                && ast::ident_text(&t) == name {
                    return Some(LocalDef { name: name.into(), kind: LocalKind::Class, name_range: t.text_range(), ty: None });
                }
        }
    }
    None
}

/// A segment of an `import` or `package` path.
pub(super) fn qualified_name_segment(e: &Engine, qn: &SyntaxNode, tok: &SyntaxToken) -> Vec<Def> {
    let segs: Vec<SyntaxToken> = qn.children_with_tokens().filter_map(|t| t.into_token()).filter(|t| t.kind() == SyntaxKind::IDENT).collect();
    let Some(idx) = segs.iter().position(|t| t == tok) else { return Vec::new() };
    let path: Vec<String> = segs[..=idx].iter().map(ast::ident_text).collect();
    if qn.parent().is_some_and(|p| p.kind() == SyntaxKind::PACKAGE_DIRECTIVE) {
        return vec![Def::Package(path.join("."))];
    }
    let imp = Import { segments: path.iter().map(|s| (s.clone(), TextRange::default())).collect(), star: false, alias: None, range: TextRange::default() };
    match e.res.resolve_import(&imp) {
        Ok(Resolved::Classifier(c)) => vec![Def::Symbol { sym: c.symbol().clone(), subst: Default::default() }],
        Ok(Resolved::Callables(cs)) => cs.into_iter().map(|sym| Def::Symbol { sym, subst: Default::default() }).collect(),
        _ if e.res.package_exists(&path.join(".")) => vec![Def::Package(path.join("."))],
        _ => Vec::new(),
    }
}

/// The declaration whose name is `tok`.
pub(super) fn declaration_def(e: &Engine, decl: &SyntaxNode, tok: &SyntaxToken) -> Option<Def> {
    // Indexed declarations carry full symbols.
    let summary = e.file.summary();
    fn walk(ds: &[super::source::SourceDecl], r: TextRange) -> Option<&Symbol> {
        for d in ds {
            if let Origin::Source { name_range, .. } = &d.symbol.origin
                && *name_range == r {
                    return Some(&d.symbol);
                }
            if let Some(s) = walk(&d.children, r) {
                return Some(s);
            }
        }
        None
    }
    if let Some(sym) = walk(&summary.decls, tok.text_range()) {
        return Some(Def::Symbol { sym: sym.clone(), subst: Default::default() });
    }
    // Members of anonymous objects.
    if let Some(sym) = e.anon_member_at(decl, tok.text_range()) {
        return Some(Def::Symbol { sym, subst: Default::default() });
    }
    // Locals and parameters: resolve the name just after the declaration.
    let name = ast::ident_text(tok);
    let kind = match decl.kind() {
        SyntaxKind::VALUE_PARAMETER => LocalKind::Parameter,
        SyntaxKind::FUN => LocalKind::Function,
        SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPE_PARAMETER => LocalKind::Class,
        _ if ast::token(decl, SyntaxKind::VAR_KW).is_some() => LocalKind::Var,
        _ => LocalKind::Val,
    };
    let ty = ast::child::<ast::TypeRef>(decl).map(|t| e.resolve_syntax_type(&t)).or_else(|| match decl.kind() {
        SyntaxKind::PROPERTY => e.initializer_type(decl),
        _ => {
            // Lambda / loop parameters and destructuring entries: resolve the name from inside
            // the scope that declares it.
            let scope_node = decl.ancestors().find(|a| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FOR_EXPR | SyntaxKind::BLOCK))?;
            let inner = match scope_node.kind() {
                SyntaxKind::LAMBDA_EXPR => scope_node.children().find(|c| c.kind() == SyntaxKind::BLOCK)?,
                SyntaxKind::FOR_EXPR => scope_node.children().find(|c| c.kind() == SyntaxKind::BODY)?,
                _ => scope_node.last_child()?,
            };
            let probe = inner.last_child().unwrap_or(inner);
            e.find_local(&probe, &name).filter(|l| l.name_range == tok.text_range()).and_then(|l| l.ty)
        }
    });
    Some(Def::Local(LocalDef { name, kind, name_range: tok.text_range(), ty }))
}

fn library_target(snap: &Snapshot, e: &Engine, sym: &Symbol, download: bool) -> Option<NavTarget> {
    let t = library::locate_in_sources(snap, &e.res.classpath, sym, download).or_else(|| library::stub(snap, &e.res.classpath, sym))?;
    Some(NavTarget { path: t.path, range: t.range })
}

pub(super) fn def_location(snap: &Snapshot, e: &Engine, d: &Def, download: bool) -> Option<NavTarget> {
    match d {
        Def::Local(l) => Some(NavTarget { path: e.path.clone(), range: l.name_range }),
        Def::Package(_) => None,
        Def::Symbol { sym, .. } => match &sym.origin {
            Origin::Source { file, name_range, .. } => Some(NavTarget { path: file.clone(), range: *name_range }),
            Origin::Library { .. } | Origin::Builtin => library_target(snap, e, sym, download),
        },
    }
}

/// Go to definition. `download` allows fetching `-sources.jar`s for libraries.
pub fn definition(snap: &Snapshot, path: &Path, offset: TextSize, download: bool) -> Vec<NavTarget> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let Some((_, defs)) = defs_at(&e, offset) else { return Vec::new() };
    let mut out: Vec<NavTarget> = Vec::new();
    for d in &defs {
        if let Some(t) = def_location(snap, &e, d, download)
            && !out.contains(&t) {
                out.push(t);
            }
    }
    out
}

/// Do two resolutions denote the same declaration?
fn same_def(a: &Def, b: &Def) -> bool {
    match (a, b) {
        (Def::Local(x), Def::Local(y)) => x.name_range == y.name_range && x.name == y.name,
        (Def::Package(x), Def::Package(y)) => x == y,
        (Def::Symbol { sym: x, .. }, Def::Symbol { sym: y, .. }) => match (&x.origin, &y.origin) {
            (Origin::Source { file: f1, name_range: r1, .. }, Origin::Source { file: f2, name_range: r2, .. }) => f1 == f2 && r1 == r2,
            (Origin::Source { .. }, _) | (_, Origin::Source { .. }) => false,
            // Library / built-in symbols: same declaration if FQ name, kind and signature agree.
            _ => x.fq_name == y.fq_name && x.kind == y.kind && x.signature() == y.signature(),
        },
        _ => false,
    }
}

/// Where the declaration itself lives (to include or exclude it from references).
fn declaration_target(e: &Engine, d: &Def) -> Option<NavTarget> {
    match d {
        Def::Local(l) => Some(NavTarget { path: e.path.clone(), range: l.name_range }),
        Def::Symbol { sym, .. } => match &sym.origin {
            Origin::Source { file, name_range, .. } => Some(NavTarget { path: file.clone(), range: *name_range }),
            _ => None,
        },
        Def::Package(_) => None,
    }
}

/// Find references to the declaration at `offset` across the project's Kotlin sources.
///
/// Candidate files are found by name, then every occurrence is resolved and kept only
/// if it denotes the same declaration. `cancelled` is polled between files.
pub fn references(snap: &Snapshot, path: &Path, offset: TextSize, include_declaration: bool, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<Vec<NavTarget>> {
    use rayon::prelude::*;
    let e = Engine::new(snap, path)?;
    let (_, defs) = kdoc::defs_at(&e, offset).or_else(|| defs_of_token(&e, &token_at(&e.root, offset)?))?;
    let target = defs.into_iter().next()?;
    // `it` and similar implicit names have no written declaration.
    let name = match &target {
        Def::Local(l) => l.name.clone(),
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Constructor => sym.fq_name.trim_end_matches(".<init>").rsplit('.').next()?.to_string(),
        Def::Symbol { sym, .. } => sym.name.clone(),
        Def::Package(p) => p.rsplit('.').next()?.to_string(),
    };
    // Operator conventions: which tokens may call the target, and a cheap per-file filter.
    let op_name = match &target {
        Def::Symbol { sym, .. } if sym.kind == SymbolKind::Function => sym.name.clone(),
        _ => String::new(),
    };
    let (op_kinds, lexemes) = operator_tokens_for(&op_name);
    // Destructuring calls componentN, which for source data classes is a constructor property.
    let destructuring = op_name.starts_with("component") && op_name[9..].parse::<u32>().is_ok()
        || matches!(&target, Def::Symbol { sym, .. } if sym.kind == SymbolKind::Property && is_data_class_property(&e, sym));
    // Members: the override family is the topmost declaration in the project plus every
    // override of it; each occurrence is checked in its own file's context (its module may
    // see subclasses the declaring module cannot).
    let root = match &target {
        Def::Symbol { sym, .. } => e.override_root(sym),
        _ => None,
    };
    let family: Vec<Def> = match &root {
        Some((top, _)) => vec![target.clone(), Def::Symbol { sym: top.clone(), subst: Default::default() }],
        None => vec![target.clone()],
    };
    let decls: Vec<NavTarget> = family.iter().filter_map(|d| declaration_target(&e, d)).collect();
    let in_family = |fe: &Engine, d: &Def| {
        family.iter().any(|f| same_def(d, f))
            || match (d, &root) {
                (Def::Symbol { sym, .. }, Some((top, top_class))) => fe.overrides(sym, top, top_class),
                _ => false,
            }
    };
    // Search scope: locals and private declarations stay in their file.
    let files: Vec<PathBuf> = match &target {
        Def::Local(_) => vec![path.to_path_buf()],
        Def::Symbol { sym, .. } if sym.visibility == crate::symbols::Visibility::Private => match &sym.origin {
            Origin::Source { file, .. } => vec![file.clone()],
            _ => vec![path.to_path_buf()],
        },
        _ => snap
            .files
            .values()
            .filter(|f| f.language != super::Language::Java)
            .filter(|f| {
                f.text.contains(name.as_str())
                    || lexemes.iter().any(|l| f.text.contains(l))
                    || destructuring && (f.text.contains("val (") || f.text.contains("var (") || f.text.contains("(("))
            })
            .map(|f| f.path.clone())
            .collect(),
    };
    let found: Vec<Vec<NavTarget>> = files
        .par_iter()
        .map(|file| {
            if cancelled() {
                return Vec::new();
            }
            // Engines are per thread (they cache with interior mutability).
            let Some(fe) = Engine::new(snap, file) else { return Vec::new() };
            let mut out = Vec::new();
            for t in fe.root.descendants_with_tokens().filter_map(|el| el.into_token()) {
                // KDoc links: each `name` in the comment that is a link resolving here.
                if t.kind() == SyntaxKind::KDOC {
                    for (i, _) in t.text().match_indices(name.as_str()) {
                        let at = t.text_range().start() + TextSize::from(i as u32);
                        let Some(link) = kdoc::link_at(&t, at + TextSize::from(1), false) else { continue };
                        let Some((last, range)) = link.segments.last() else { continue };
                        if *last == name && range.start() == at && kdoc::resolve(&fe, &link).iter().any(|d| in_family(&fe, d)) {
                            out.push(NavTarget { path: file.clone(), range: *range });
                        }
                    }
                    continue;
                }
                let by = t.kind() == SyntaxKind::IDENT && t.text() == "by" && op_kinds.contains(&SyntaxKind::IDENT);
                let entry = destructuring && t.kind() == SyntaxKind::IDENT && t.parent().is_some_and(|p| p.kind() == SyntaxKind::DESTRUCTURING_ENTRY);
                let matches_name = t.kind() == SyntaxKind::IDENT && ast::ident_text(&t) == name;
                if !(matches_name || by || entry || op_kinds.contains(&t.kind()) && t.kind() != SyntaxKind::IDENT) {
                    continue;
                }
                let resolved = if entry {
                    t.parent().and_then(|p| fe.component_defs(&p)).map(|d| (t.text_range(), d))
                } else {
                    defs_of_token(&fe, &t)
                };
                let Some((range, defs)) = resolved else { continue };
                if defs.iter().any(|d| in_family(&fe, d)) {
                    // Override declarations count as declarations, not usages (destructuring
                    // entries are usages of componentN / the data class property).
                    let is_decl = !entry && t.parent().is_some_and(|p| DECLARATIONS.contains(&p.kind()));
                    if include_declaration || !is_decl {
                        out.push(NavTarget { path: file.clone(), range });
                    }
                }
            }
            out
        })
        .collect();
    if cancelled() {
        return None;
    }
    let mut out: Vec<NavTarget> = found.into_iter().flatten().collect();
    if include_declaration {
        for d in decls {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    } else {
        out.retain(|t| !decls.contains(t));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start().cmp(&b.range.start())));
    out.dedup();
    Some(out)
}

/// Occurrences of the declaration at `offset` in its file, each marked as a write
/// (declaration, assignment target, `++`/`--` operand) or a read.
pub fn document_highlight(snap: &Snapshot, path: &Path, offset: TextSize) -> Option<Vec<(TextRange, bool)>> {
    if let Some(found) = snap.file(path).and_then(|f| keyword_highlight(&f.parse().syntax(), offset)) {
        return Some(found);
    }
    let e = Engine::new(snap, path)?;
    let tok = token_at(&e.root, offset).filter(|t| t.kind() == SyntaxKind::IDENT)?;
    let (_, defs) = defs_of_token(&e, &tok)?;
    let target = defs.into_iter().next()?;
    let root = match &target {
        Def::Symbol { sym, .. } => e.override_root(sym),
        _ => None,
    };
    let name = ast::ident_text(&tok);
    let mut out = Vec::new();
    for t in e.root.descendants_with_tokens().filter_map(|el| el.into_token()) {
        if t.kind() != SyntaxKind::IDENT || ast::ident_text(&t) != name {
            continue;
        }
        let Some((range, defs)) = defs_of_token(&e, &t) else { continue };
        let hit = defs.iter().any(|d| {
            same_def(d, &target)
                || match (d, &root) {
                    (Def::Symbol { sym, .. }, Some((top, top_class))) => same_def(d, &Def::Symbol { sym: top.clone(), subst: Default::default() }) || e.overrides(sym, top, top_class),
                    _ => false,
                }
        });
        if hit {
            out.push((range, is_write(&t)));
        }
    }
    Some(out)
}

/// On `return`/`throw`/`fun`: the function's keyword and its exit points (its own
/// `return`s, including non-local ones from lambdas, and `throw`s). On a loop keyword or
/// `break`/`continue`: the loop's keyword and the jumps out of it.
fn keyword_highlight(root: &SyntaxNode, offset: TextSize) -> Option<Vec<(TextRange, bool)>> {
    use SyntaxKind as K;
    let kw = |t: &SyntaxToken| matches!(t.kind(), K::RETURN_KW | K::THROW_KW | K::FUN_KW | K::BREAK_KW | K::CONTINUE_KW | K::FOR_KW | K::WHILE_KW | K::DO_KW);
    let tok = root.token_at_offset(offset).find(kw)?;
    let node = tok.parent()?;
    let label = |n: &SyntaxNode| n.children().find(|c| c.kind() == K::LABEL_QUALIFIER).and_then(|q| ast::token(&q, K::IDENT)).map(|t| ast::ident_text(&t));
    let keyword = |n: &SyntaxNode, k: K| ast::token(n, k).map(|t| (t.text_range(), false));
    match tok.kind() {
        K::RETURN_KW | K::THROW_KW | K::FUN_KW => {
            let owner = |n: &SyntaxNode| n.ancestors().find(|a| matches!(a.kind(), K::FUN | K::ANONYMOUS_FUNCTION | K::PROPERTY_ACCESSOR | K::CLASS_BODY));
            let f = if tok.kind() == K::FUN_KW { Some(node.clone()).filter(|n| matches!(n.kind(), K::FUN | K::ANONYMOUS_FUNCTION))? } else {
                // `return@label` belongs to a lambda: not handled here.
                if label(&node).is_some() {
                    return None;
                }
                owner(&node).filter(|o| o.kind() != K::CLASS_BODY)?
            };
            let mut out: Vec<(TextRange, bool)> = keyword(&f, K::FUN_KW).into_iter().collect();
            for x in f.descendants().filter(|d| matches!(d.kind(), K::RETURN_EXPR | K::THROW_EXPR)) {
                if label(&x).is_none() && owner(&x).as_ref() == Some(&f) {
                    out.extend(keyword(&x, if x.kind() == K::RETURN_EXPR { K::RETURN_KW } else { K::THROW_KW }));
                }
            }
            Some(out)
        }
        _ => {
            let is_loop = |n: &SyntaxNode| matches!(n.kind(), K::FOR_EXPR | K::WHILE_EXPR | K::DO_WHILE_EXPR);
            let loop_label = |n: &SyntaxNode| {
                let l = n.parent().filter(|p| p.kind() == K::LABELED_EXPR)?.children().find(|c| c.kind() == K::LABEL)?;
                ast::token(&l, K::IDENT).map(|t| ast::ident_text(&t))
            };
            // The loop a jump leaves: the labeled one, or the innermost.
            let target = |jump: &SyntaxNode| match label(jump) {
                Some(l) => jump.ancestors().find(|a| is_loop(a) && loop_label(a).as_deref() == Some(l.as_str())),
                None => jump.ancestors().skip(1).take_while(|a| !matches!(a.kind(), K::FUN | K::CLASS_BODY)).find(is_loop),
            };
            let lp = if matches!(tok.kind(), K::BREAK_KW | K::CONTINUE_KW) { target(&node)? } else { Some(node.clone()).filter(is_loop)? };
            let mut out: Vec<(TextRange, bool)> = Vec::new();
            out.extend(keyword(&lp, if lp.kind() == K::FOR_EXPR { K::FOR_KW } else { K::WHILE_KW }));
            if lp.kind() == K::DO_WHILE_EXPR {
                out.extend(keyword(&lp, K::DO_KW));
            }
            for j in lp.descendants().filter(|d| matches!(d.kind(), K::BREAK_EXPR | K::CONTINUE_EXPR)) {
                if target(&j).as_ref() == Some(&lp) {
                    out.extend(j.first_token().map(|t| (t.text_range(), false)));
                }
            }
            out.sort_by_key(|(r, _)| r.start());
            Some(out)
        }
    }
}

/// The token names a declaration or the target of an assignment or increment.
fn is_write(tok: &SyntaxToken) -> bool {
    let Some(parent) = tok.parent() else { return false };
    if DECLARATIONS.contains(&parent.kind()) {
        return true;
    }
    if parent.kind() != SyntaxKind::NAME_REF {
        return false;
    }
    // `x = …`, `obj.x += …`, `x++`: the reference is (the selector of) the left operand.
    let mut expr = parent;
    while let Some(p) = expr.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::PAREN_EXPR) && p.last_child().as_ref() == Some(&expr)) {
        expr = p;
    }
    let Some(p) = expr.parent() else { return false };
    match p.kind() {
        SyntaxKind::BINARY_EXPR => {
            p.first_child().as_ref() == Some(&expr)
                && p.children_with_tokens().filter_map(|t| t.into_token()).any(|t| {
                    matches!(t.kind(), SyntaxKind::EQ | SyntaxKind::PLUS_EQ | SyntaxKind::MINUS_EQ | SyntaxKind::STAR_EQ | SyntaxKind::SLASH_EQ | SyntaxKind::PERCENT_EQ)
                })
        }
        SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
            p.descendants_with_tokens().filter_map(|t| t.into_token()).any(|t| matches!(t.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2))
        }
        _ => false,
    }
}

/// Token kinds that can invoke operator function `name`, and source lexemes to look for.
fn operator_tokens_for(name: &str) -> (Vec<SyntaxKind>, Vec<&'static str>) {
    use SyntaxKind::*;
    let (kinds, lexemes): (&[SyntaxKind], &[&str]) = match name {
        "plus" => (&[PLUS, PLUS_EQ], &["+"]),
        "minus" => (&[MINUS, MINUS_EQ], &["-"]),
        "times" => (&[STAR, STAR_EQ], &["*"]),
        "div" => (&[SLASH, SLASH_EQ], &["/"]),
        "rem" => (&[PERCENT, PERCENT_EQ], &["%"]),
        "plusAssign" => (&[PLUS_EQ], &["+="]),
        "minusAssign" => (&[MINUS_EQ], &["-="]),
        "timesAssign" => (&[STAR_EQ], &["*="]),
        "divAssign" => (&[SLASH_EQ], &["/="]),
        "remAssign" => (&[PERCENT_EQ], &["%="]),
        "unaryPlus" => (&[PLUS], &["+"]),
        "unaryMinus" => (&[MINUS], &["-"]),
        "not" => (&[EXCL], &["!"]),
        "inc" => (&[PLUS2], &["++"]),
        "dec" => (&[MINUS2], &["--"]),
        "rangeTo" => (&[RANGE], &[".."]),
        "rangeUntil" => (&[RANGE_UNTIL], &["..<"]),
        "contains" => (&[IN_KW, NOT_IN_KW], &["in "]),
        "iterator" => (&[IN_KW], &["for"]),
        "get" | "set" => (&[L_BRACK], &["["]),
        "equals" => (&[EQ2, NOT_EQ], &["==", "!="]),
        "compareTo" => (&[LT, GT, LT_EQ, GT_EQ], &["<", ">"]),
        "invoke" => (&[L_PAREN], &["("]),
        "getValue" | "setValue" | "provideDelegate" => (&[IDENT], &[" by "]),
        _ => (&[], &[]),
    };
    (kinds.to_vec(), lexemes.to_vec())
}

fn is_data_class_property(e: &Engine, sym: &Symbol) -> bool {
    let Some((container, _)) = sym.fq_name.rsplit_once('.') else { return false };
    e.res.find_classifier(container).is_some_and(|c| c.symbol().modifiers.has(crate::symbols::Modifiers::DATA))
}

/// Hover: signature, resolved type in context and documentation.
pub fn hover(snap: &Snapshot, path: &Path, offset: TextSize) -> Option<HoverInfo> {
    let e = Engine::new(snap, path)?;
    let (range, defs) = defs_at(&e, offset)?;
    let d = defs.first()?;
    let mut md = String::new();
    match d {
        Def::Symbol { sym, subst } => {
            // Show the inferred type of undeclared returns / properties (`fun f() = 0`).
            let mut shown = sym.clone();
            if shown.ty.is_none() && matches!(shown.kind, SymbolKind::Function | SymbolKind::Property) {
                shown.ty = e.symbol_type(sym);
            }
            md.push_str("```kotlin\n");
            md.push_str(&shown.signature());
            // The value of constants (`const val TIMEOUT = 60 * 1000` → `= 60000`).
            if sym.kind == SymbolKind::Property
                && !sym.modifiers.has(crate::symbols::Modifiers::VAR)
                && let Origin::Source { file, name_range, .. } = &sym.origin
            {
                let other = if *file == e.path { None } else { Engine::new(snap, file) };
                let owner = other.as_ref().unwrap_or(&e);
                let decl = crate::syntax::covering(&owner.root, *name_range).and_then(|c| c.ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY));
                if let Some(v) = decl.filter(|d| !d.children().any(|c| matches!(c.kind(), SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::PROPERTY_DELEGATE))).and_then(|d| constant::shown_value(owner, &d, sym.modifiers.has(crate::symbols::Modifiers::CONST))) {
                    md.push_str(&format!(" = {v}"));
                }
            }
            md.push_str("\n```\n");
            // The type in this context, when generics were substituted (`list.first()` → `User`).
            if !subst.is_empty()
                && matches!(sym.kind, SymbolKind::Function | SymbolKind::Property | SymbolKind::Field)
                && let Some(t) = e.symbol_type(sym) {
                    let t = substitute(&t, subst);
                    let shown = t.display();
                    if sym.ty.as_ref().map(|d| d.display()) != Some(shown.clone()) && !shown.contains("<error>") {
                        md.push_str(&format!("\n`{}: {shown}` here\n", if sym.kind == SymbolKind::Function { "returns" } else { "type" }));
                    }
                }
            let container = match sym.kind {
                k if k.is_classifier() => sym.fq_name.rsplit_once('.').map(|(p, _)| p.to_string()),
                _ => sym.fq_name.rsplit_once('.').map(|(c, _)| c.to_string()),
            };
            let origin = match &sym.origin {
                Origin::Library { archive, .. } => archive.file_name().map(|n| format!(" · *{}*", n.to_string_lossy())),
                Origin::Builtin => Some(" · *Kotlin built-in*".into()),
                Origin::Source { .. } => None,
            };
            if let Some(c) = container.filter(|c| !c.is_empty()) {
                md.push_str(&format!("\n`{c}`{}\n", origin.unwrap_or_default()));
            }
            let doc = match &sym.origin {
                Origin::Source { .. } => sym.doc.clone(),
                _ => library::locate_in_sources(snap, &e.res.classpath, sym, false).and_then(|t| t.doc),
            };
            if let Some(doc) = doc.map(|d| render_doc(&d)).filter(|d| !d.trim().is_empty()) {
                md.push_str("\n---\n\n");
                md.push_str(&doc);
                md.push('\n');
            }
        }
        Def::Local(l) => {
            let ty = l.ty.as_ref().map(|t| t.display()).unwrap_or_else(|| "?".into());
            let sig = match l.kind {
                LocalKind::Val => format!("val {}: {ty}", l.name),
                LocalKind::Var => format!("var {}: {ty}", l.name),
                LocalKind::Parameter | LocalKind::LambdaParameter | LocalKind::It => format!("{}: {ty}", l.name),
                LocalKind::Function => format!("fun {}(…): {ty}", l.name),
                LocalKind::Class => format!("class {}", l.name),
            };
            let value = (l.kind == LocalKind::Val)
                .then(|| crate::syntax::covering(&e.root, l.name_range).and_then(|c| c.ancestors().find(|a| a.kind() == SyntaxKind::PROPERTY)))
                .flatten()
                .and_then(|d| constant::shown_value(&e, &d, false))
                .map(|v| format!(" = {v}"))
                .unwrap_or_default();
            md.push_str(&format!("```kotlin\n{sig}{value}\n```\n"));
            if l.kind == LocalKind::It {
                md.push_str("\nimplicit lambda parameter\n");
            }
            // The type where it is used, when narrowed by a check (`if (x is String)`).
            let here = token_at(&e.root, offset).and_then(|t| t.parent()).filter(|p| p.kind() == SyntaxKind::NAME_REF).and_then(|n| e.type_of(&n));
            if let (Some(here), Some(declared)) = (here, &l.ty)
                && here.display() != declared.display()
                && !here.display().contains("<error>")
            {
                md.push_str(&format!("\nSmart cast to `{}`\n", here.display()));
            }
        }
        Def::Package(p) => md.push_str(&format!("```kotlin\npackage {p}\n```\n")),
    }
    Some(HoverInfo { markdown: md, range })
}

/// Turn KDoc / Javadoc into Markdown.
pub fn render_doc(raw: &str) -> String {
    let body = raw.trim().trim_start_matches("/**").trim_end_matches("*/");
    let mut text_lines = Vec::new();
    let mut params = Vec::new();
    let mut tags = Vec::new();
    let mut in_code = false;
    for line in body.lines() {
        let l = line.trim_start();
        let l = l.strip_prefix('*').map(|r| r.strip_prefix(' ').unwrap_or(r)).unwrap_or(l);
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
            text_lines.push(l.to_string());
            continue;
        }
        if in_code {
            text_lines.push(l.to_string());
            continue;
        }
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("@param ") {
            let (name, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            params.push(format!("- `{}` — {}", name.trim_matches(['[', ']']), inline(desc.trim())));
        } else if let Some(rest) = t.strip_prefix("@return ").or_else(|| t.strip_prefix("@returns ")) {
            tags.push(format!("**Returns** {}", inline(rest)));
        } else if let Some(rest) = t.strip_prefix("@throws ").or_else(|| t.strip_prefix("@exception ")) {
            let (name, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            tags.push(format!("**Throws** `{}` {}", name.trim_matches(['[', ']']), inline(desc.trim())));
        } else if let Some(rest) = t.strip_prefix("@see ") {
            tags.push(format!("**See** {}", inline(rest)));
        } else if let Some(rest) = t.strip_prefix("@since ") {
            tags.push(format!("**Since** {rest}"));
        } else if t.starts_with('@') && params.is_empty() && tags.is_empty() && t.split_whitespace().next().is_some_and(|w| ["@author", "@suppress", "@sample", "@hide", "@apiNote", "@implNote", "@implSpec", "@serial"].contains(&w)) {
            // dropped
        } else if !params.is_empty() && !t.is_empty() && !t.starts_with('@') && tags.is_empty() {
            // continuation of the last @param
            if let Some(last) = params.last_mut() {
                last.push(' ');
                last.push_str(&inline(t));
            }
        } else {
            text_lines.push(inline(l));
        }
    }
    let mut out = text_lines.join("\n").trim().to_string();
    if !params.is_empty() {
        out.push_str("\n\n**Parameters**\n");
        out.push_str(&params.join("\n"));
    }
    if !tags.is_empty() {
        out.push_str("\n\n");
        out.push_str(&tags.join("  \n"));
    }
    out
}

/// Inline KDoc/Javadoc markup: `[Foo]`, `{@code x}`, `{@link X}`, simple HTML.
fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("{@code ").or_else(|| rest.strip_prefix("{@literal ")) {
            let end = r.find('}').unwrap_or(r.len());
            out.push('`');
            out.push_str(&r[..end]);
            out.push('`');
            rest = r.get(end + 1..).unwrap_or("");
        } else if let Some(r) = rest.strip_prefix("{@link ").or_else(|| rest.strip_prefix("{@linkplain ")) {
            let end = r.find('}').unwrap_or(r.len());
            let target = r[..end].split_whitespace().next().unwrap_or("").replace('#', ".");
            out.push_str(&format!("`{}`", target.trim_start_matches('.')));
            rest = r.get(end + 1..).unwrap_or("");
        } else if rest.starts_with('[') && !rest.starts_with("[]") {
            match rest.find(']') {
                Some(end) if !rest[1..end].contains(' ') && rest.as_bytes().get(end + 1) != Some(&b'(') => {
                    out.push_str(&format!("`{}`", &rest[1..end]));
                    rest = &rest[end + 1..];
                }
                _ => {
                    out.push('[');
                    rest = &rest[1..];
                }
            }
        } else if rest.starts_with('<') {
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            let tag = rest[..end].to_lowercase();
            match tag.trim_matches(['<', '>', '/']).split_whitespace().next().unwrap_or("") {
                "p" | "br" => out.push_str("\n\n"),
                "code" | "tt" => out.push('`'),
                "b" | "strong" => out.push_str("**"),
                "i" | "em" => out.push('*'),
                "li" if !tag.starts_with("</") => out.push_str("\n- "),
                "pre" => out.push_str("\n```\n"),
                _ => {}
            }
            rest = &rest[end..];
        } else if let Some(r) = rest.strip_prefix("&lt;") {
            out.push('<');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("&gt;") {
            out.push('>');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("&amp;") {
            out.push('&');
            rest = r;
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

#[cfg(test)]
mod tests;

/// Name ranges of the file's declarations that get a reference-count code lens: classes,
/// functions and properties, top-level and members (not locals or constructors).
pub fn lens_targets(snap: &Snapshot, path: &Path) -> Vec<TextRange> {
    fn walk(ds: &[super::source::SourceDecl], out: &mut Vec<TextRange>) {
        for d in ds {
            let s = &d.symbol;
            if matches!(s.kind, SymbolKind::Class | SymbolKind::Interface | SymbolKind::Object | SymbolKind::Enum | SymbolKind::Annotation | SymbolKind::TypeAlias | SymbolKind::Function | SymbolKind::Property)
                && let Origin::Source { name_range, .. } = &s.origin
                && !name_range.is_empty()
            {
                out.push(*name_range);
            }
            walk(&d.children, out);
        }
    }
    let Some(file) = snap.file(path) else { return Vec::new() };
    let mut out = Vec::new();
    walk(&file.summary().decls, &mut out);
    out.sort_by_key(|r| r.start());
    out.dedup();
    out
}

/// An expression that needs no parentheses as an operand.
pub(super) fn rewrites_atomic(n: &SyntaxNode) -> bool {
    matches!(
        n.kind(),
        SyntaxKind::NAME_REF | SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::LITERAL | SyntaxKind::STRING_TEMPLATE | SyntaxKind::PAREN_EXPR | SyntaxKind::INDEX_EXPR | SyntaxKind::THIS_EXPR | SyntaxKind::COLLECTION_LITERAL | SyntaxKind::CALLABLE_REFERENCE
    )
}

