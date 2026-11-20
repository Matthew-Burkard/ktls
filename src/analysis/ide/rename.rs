//! Rename: every reference found by [`super::references`] (including the override family),
//! named arguments at call sites, KDoc links, and the file of a top-level class.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::infer::{Def, Engine, LocalKind};
use super::{NavTarget, declaration_target, defs_of_token, references};
use crate::analysis::Snapshot;
use crate::analysis::source::Language;
use crate::symbols::{Modifiers, Origin, SymbolKind};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenameEdits {
    /// `(file, range, new text)`, sorted.
    pub edits: Vec<(PathBuf, TextRange, String)>,
    /// The file declaring a renamed top-level class, when it is named after it.
    pub file_rename: Option<(PathBuf, PathBuf)>,
}

/// What would be renamed at `offset`: the identifier's range and its current name.
pub fn prepare_rename(snap: &Snapshot, path: &Path, offset: TextSize) -> Result<(TextRange, String), String> {
    let t = target(snap, path, offset)?;
    Ok((t.range, t.name))
}

struct Target {
    /// The identifier under the cursor.
    range: TextRange,
    name: String,
    def: Def,
    /// Where the declaration's references are searched from (the cursor, or the parameter
    /// a named argument stands for).
    path: PathBuf,
    offset: TextSize,
}

/// Rename the declaration at `offset` to `new_name`. `Ok(None)` if cancelled.
pub fn rename(snap: &Snapshot, path: &Path, offset: TextSize, new_name: &str, cancelled: &(dyn Fn() -> bool + Sync)) -> Result<Option<RenameEdits>, String> {
    let Target { name: old, def, path: search_path, offset: search_offset, .. } = target(snap, path, offset)?;
    let (path, offset) = (search_path.as_path(), search_offset);
    let e = Engine::new(snap, path).ok_or("file is not loaded")?;
    let new = quote(new_name)?;
    if strip(&new) == old {
        return Ok(Some(RenameEdits::default()));
    }
    let Some(refs) = references(snap, path, offset, true, cancelled) else {
        return if cancelled() { Ok(None) } else { Err(format!("cannot find the references of '{old}'")) };
    };
    // Keyed by (file, start, end): sorted and deduplicated.
    let mut edits: BTreeSet<(PathBuf, TextSize, TextSize)> = BTreeSet::new();
    // Only occurrences spelled with the old name: aliased imports keep their alias, and
    // destructuring entries (`val (a, b) = p` calling `component1`) are other variables.
    for r in &refs {
        if text_at(snap, r).is_some_and(|t| strip(t) == old) {
            edits.insert((r.path.clone(), r.range.start(), r.range.end()));
        }
    }
    // (The references of a parameter include the named arguments at every call.)
    let decl = declaration_target(&e, &def);
    // KDoc. Parameters: `[old]`, `@param old` and `@property old` on their owner only.
    // Other declarations: `[old]` / `[Owner.old]` links in the files that use them.
    let param_owner = decl.as_ref().and_then(|d| parameter_owner(snap, d));
    match (&param_owner, &def) {
        (Some(owner), _) => {
            for r in kdoc_params(owner, &old) {
                edits.insert((owner.0.clone(), r.start(), r.end()));
            }
        }
        (None, Def::Local(_)) => {}
        (None, _) => {
            let files: BTreeSet<PathBuf> = edits.iter().map(|(p, ..)| p.clone()).collect();
            for f in &files {
                for r in kdoc_links(snap, f, &old) {
                    edits.insert((f.clone(), r.start(), r.end()));
                }
            }
        }
    }
    let file_rename = match (&def, &decl) {
        (Def::Symbol { sym, .. }, Some(d)) if sym.kind.is_classifier() && !new.starts_with('`') => {
            let top_level = snap.file(&d.path).is_some_and(|f| {
                let pkg = &f.summary().package;
                sym.fq_name == if pkg.is_empty() { old.clone() } else { format!("{pkg}.{old}") }
            });
            let named_after = d.path.file_stem().is_some_and(|s| s == old.as_str()) && d.path.extension().is_some_and(|x| x == "kt");
            let target = d.path.with_file_name(format!("{new}.kt"));
            (top_level && named_after && !target.exists() && snap.file(&target).is_none()).then(|| (d.path.clone(), target))
        }
        _ => None,
    };
    Ok(Some(RenameEdits { edits: edits.into_iter().map(|(p, a, b)| (p, TextRange::new(a, b), new.clone())).collect(), file_rename }))
}

/// The renameable declaration at `offset`.
fn target(snap: &Snapshot, path: &Path, offset: TextSize) -> Result<Target, String> {
    let e = Engine::new(snap, path).ok_or("file is not loaded")?;
    // Only identifiers are renamed: prefer the one right of the cursor, then left.
    let tok = e.root.token_at_offset(offset).collect::<Vec<_>>().into_iter().rev().find(|t| t.kind() == SyntaxKind::IDENT).ok_or("no symbol to rename here")?;
    // A named argument stands for the parameter of the called function.
    if tok.parent().is_some_and(|p| p.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) {
        let param = parameter_of_named_argument(&e, &tok)?;
        let t = target(snap, &param.path, param.range.start())?;
        return Ok(Target { range: tok.text_range(), ..t });
    }
    let (range, defs) = defs_of_token(&e, &tok).ok_or("no symbol to rename here")?;
    let def = defs.into_iter().next().ok_or_else(|| format!("cannot resolve '{}'", ast::ident_text(&tok)))?;
    let name = match &def {
        Def::Local(l) if l.kind == LocalKind::It => return Err("'it' is an implicit parameter; name it explicitly first".into()),
        Def::Local(l) => l.name.clone(),
        Def::Package(_) => return Err("renaming packages is not supported".into()),
        Def::Symbol { sym, .. } => {
            let Origin::Source { file, .. } = &sym.origin else { return Err(format!("'{}' is declared in a library", sym.name)) };
            if e.snap.file(file).is_none_or(|f| f.language == Language::Java) {
                return Err(format!("'{}' is declared in Java", sym.name));
            }
            if sym.modifiers.has(Modifiers::OPERATOR) {
                return Err(format!("'{}' is an operator function; renaming it would break operator calls", sym.name));
            }
            // Overrides of library members must keep their name.
            if sym.modifiers.has(Modifiers::OVERRIDE) {
                match e.override_root(sym) {
                    Some((top, _)) if !top.modifiers.has(Modifiers::OVERRIDE) => {}
                    _ => return Err(format!("'{}' overrides a library declaration", sym.name)),
                }
            }
            match sym.kind {
                SymbolKind::Constructor => sym.fq_name.trim_end_matches(".<init>").rsplit('.').next().unwrap_or_default().to_string(),
                _ => sym.name.clone(),
            }
        }
    };
    if ast::ident_text(&tok) != name {
        return Err(format!("'{}' is an alias; rename the import alias instead", ast::ident_text(&tok)));
    }
    Ok(Target { range, name, def, path: path.to_path_buf(), offset: range.start() })
}

/// The declaration of the parameter that named argument `tok` refers to.
pub(super) fn parameter_of_named_argument(e: &Engine, tok: &crate::syntax::SyntaxToken) -> Result<NavTarget, String> {
    let name = ast::ident_text(tok);
    let (_, callees) = defs_of_token(e, tok).ok_or("cannot resolve the call")?;
    for d in callees {
        let Def::Symbol { sym, .. } = &d else { continue };
        let Origin::Source { file, name_range, .. } = &sym.origin else { return Err(format!("'{}' is declared in a library", sym.name)) };
        let Some(root) = tree(e.snap, file) else { continue };
        let decl = crate::syntax::covering(&root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::CLASS));
        let Some(decl) = decl else { continue };
        let list = match decl.kind() {
            SyntaxKind::CLASS => decl.children().find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).and_then(|c| c.children().find(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST)),
            _ => decl.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST),
        };
        let param = list
            .into_iter()
            .flat_map(|l| l.children().filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER).collect::<Vec<_>>())
            .find_map(|p| ast::token(&p, SyntaxKind::IDENT).filter(|t| ast::ident_text(t) == name));
        if let Some(p) = param {
            return Ok(NavTarget { path: file.clone(), range: p.text_range() });
        }
    }
    Err(format!("cannot find parameter '{name}'"))
}

/// The name as it must be written: plain, or in backticks (keywords, spaces...).
fn quote(new: &str) -> Result<String, String> {
    let n = new.trim();
    let n = n.strip_prefix('`').and_then(|s| s.strip_suffix('`')).unwrap_or(n);
    if n.is_empty() || n.chars().all(|c| c == '_') || n.chars().any(|c| "`.;[]/<>:\\\n\r\t".contains(c)) {
        return Err(format!("'{n}' is not a valid Kotlin name"));
    }
    let mut chars = n.chars();
    let plain = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_');
    Ok(if plain && SyntaxKind::from_keyword(n).is_none() { n.to_string() } else { format!("`{n}`") })
}

fn strip(t: &str) -> &str {
    t.strip_prefix('`').and_then(|t| t.strip_suffix('`')).unwrap_or(t)
}

fn text_at<'a>(snap: &'a Snapshot, t: &NavTarget) -> Option<&'a str> {
    snap.file(&t.path)?.text.get(usize::from(t.range.start())..usize::from(t.range.end()))
}

fn tree(snap: &Snapshot, path: &Path) -> Option<SyntaxNode> {
    Some(snap.file(path)?.parse().syntax())
}

/// `f(old = 1)` / `Owner(old = 1)` / `@Ann(old = 1)` for a parameter declared at `decl`.
pub(super) fn named_arguments(snap: &Snapshot, decl: &NavTarget, old: &str, cancelled: &(dyn Fn() -> bool + Sync)) -> Result<Vec<NavTarget>, String> {
    let Some(root) = tree(snap, &decl.path) else { return Ok(Vec::new()) };
    let Some(tok) = crate::syntax::covering(&root, decl.range) else { return Ok(Vec::new()) };
    let Some(param) = tok.ancestors().find(|a| a.kind() == SyntaxKind::VALUE_PARAMETER) else { return Ok(Vec::new()) };
    let Some(owner) = param.parent().filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST).and_then(|l| l.parent()) else { return Ok(Vec::new()) };
    // The callable whose calls pass this parameter: a function, or a class (constructors).
    let owner_name = match owner.kind() {
        SyntaxKind::FUN => ast::token(&owner, SyntaxKind::IDENT),
        SyntaxKind::PRIMARY_CONSTRUCTOR | SyntaxKind::SECONDARY_CONSTRUCTOR => {
            owner.ancestors().find(|a| a.kind() == SyntaxKind::CLASS).and_then(|c| ast::token(&c, SyntaxKind::IDENT))
        }
        _ => None, // lambdas, catch clauses, setters: no named arguments
    };
    let Some(owner_name) = owner_name else { return Ok(Vec::new()) };
    let Some(calls) = references(snap, &decl.path, owner_name.text_range().start(), false, cancelled) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for call in calls {
        let Some(root) = tree(snap, &call.path) else { continue };
        let Some(el) = crate::syntax::covering(&root, call.range) else { continue };
        // The nearest call-like node (call, annotation, super/delegation call) must have
        // this occurrence as its callee, not inside a type or value argument.
        let Some(node) = el.ancestors().find(|a| a.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)) else { continue };
        let Some(args) = node.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { continue };
        if args.text_range().contains_range(call.range) || !node.first_child().is_some_and(|c| c.text_range().contains_range(call.range)) {
            continue;
        }
        for name in args.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).filter_map(|a| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME)) {
            if let Some(t) = ast::token(&name, SyntaxKind::IDENT).filter(|t| ast::ident_text(t) == old) {
                out.push(NavTarget { path: call.path.clone(), range: t.text_range() });
            }
        }
    }
    Ok(out)
}

/// `[old]` and `[Qualifier.old]` in the KDoc of `path`.
fn kdoc_links(snap: &Snapshot, path: &Path, old: &str) -> Vec<TextRange> {
    let Some(root) = tree(snap, path) else { return Vec::new() };
    let mut out = Vec::new();
    for t in root.descendants_with_tokens().filter_map(|e| e.into_token()).filter(|t| t.kind() == SyntaxKind::KDOC) {
        // `[old]` in the KDoc of something with its own parameter `old` means that parameter.
        if t.parent().is_some_and(|d| has_parameter(&d, old)) {
            continue;
        }
        let text = t.text();
        let base = t.text_range().start();
        for (i, _) in text.match_indices(old) {
            // A whole segment of a link: `[old]`, `[a.old]`, `[old.member]`.
            let end = i + old.len();
            let Some(close) = text[end..].find(']').map(|c| end + c) else { continue };
            let rest = &text[end..close];
            if !(rest.is_empty() || rest.starts_with('.') && rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')) {
                continue;
            }
            // Back to the `[`, through a qualifier of identifier characters and dots.
            let link_start = text[..i].rfind('[');
            let qualifier_ok = link_start.is_some_and(|s| {
                let q = &text[s + 1..i];
                (q.is_empty() || q.ends_with('.')) && q.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
            });
            if qualifier_ok {
                out.push(TextRange::at(base + TextSize::from(i as u32), TextSize::from(old.len() as u32)));
            }
        }
    }
    out
}

/// The declaration whose parameter list declares `decl`, and its file.
fn parameter_owner(snap: &Snapshot, decl: &NavTarget) -> Option<(PathBuf, SyntaxNode)> {
    let root = tree(snap, &decl.path)?;
    let el = crate::syntax::covering(&root, decl.range)?;
    let param = el.ancestors().find(|a| a.kind() == SyntaxKind::VALUE_PARAMETER)?;
    let owner = param.ancestors().find(|a| matches!(a.kind(), SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::LAMBDA_EXPR | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CATCH_CLAUSE | SyntaxKind::FOR_EXPR))?;
    Some((decl.path.clone(), owner))
}

fn has_parameter(decl: &SyntaxNode, name: &str) -> bool {
    let lists = match decl.kind() {
        SyntaxKind::CLASS => decl.children().filter(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR).flat_map(|c| c.children().collect::<Vec<_>>()).collect::<Vec<_>>(),
        SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR => decl.children().collect(),
        _ => return false,
    };
    lists
        .iter()
        .filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST)
        .flat_map(|l| l.children().collect::<Vec<_>>())
        .any(|p| ast::token(&p, SyntaxKind::IDENT).is_some_and(|t| ast::ident_text(&t) == name))
}

/// `[old]`, `@param old` and `@property old` in the KDoc of a parameter's owner.
fn kdoc_params((_, owner): &(PathBuf, SyntaxNode), old: &str) -> Vec<TextRange> {
    let mut out = Vec::new();
    for t in owner.children_with_tokens().filter_map(|e| e.into_token()).filter(|t| t.kind() == SyntaxKind::KDOC) {
        let text = t.text();
        let at = |i: usize, len: usize| TextRange::at(t.text_range().start() + TextSize::from(i as u32), TextSize::from(len as u32));
        for tag in ["@param ", "@property "] {
            for (i, _) in text.match_indices(tag) {
                let start = i + tag.len();
                let word: &str = text[start..].split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '`')).next().unwrap_or("");
                if strip(word) == old {
                    out.push(at(start, word.len()));
                }
            }
        }
        let link = format!("[{old}]");
        for (i, _) in text.match_indices(&link) {
            out.push(at(i + 1, old.len()));
        }
    }
    out
}
