//! Request handlers. They run on worker threads against an immutable snapshot.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lsp_types::{DocumentSymbol, Location, WorkspaceSymbol};

use super::line_index::{Encoding, LineIndex};
use super::to_proto;
use crate::analysis::Snapshot;
use crate::analysis::diagnostics::{self, Coverage};
use crate::analysis::source::SourceDecl;
use crate::symbols::{Origin, SymbolKind};
use crate::util::path_to_uri;

/// Raised when a request was cancelled or the snapshot became stale.
#[derive(Debug)]
pub struct Cancelled;

pub type Result<T> = std::result::Result<T, Cancelled>;

/// How a request handler can fail.
#[derive(Debug)]
pub enum RequestError {
    Cancelled,
    /// Shown to the user (e.g. why a rename is not possible).
    Failed(String),
}

impl From<Cancelled> for RequestError {
    fn from(_: Cancelled) -> Self {
        RequestError::Cancelled
    }
}

pub struct Ctx<'a> {
    pub snap: &'a Snapshot,
    pub enc: Encoding,
    pub cancelled: &'a AtomicBool,
}

impl Ctx<'_> {
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) { Err(Cancelled) } else { Ok(()) }
    }
}

pub fn document_symbols(cx: &Ctx, path: &Path) -> Result<Vec<DocumentSymbol>> {
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let summary = file.summary();
    let index = LineIndex::new(&file.text);
    fn convert(d: &SourceDecl, text: &str, index: &LineIndex, enc: Encoding, member: bool) -> Option<DocumentSymbol> {
        let Origin::Source { range, name_range, .. } = &d.symbol.origin else { return None };
        let full = index.range(text, *range, enc);
        let mut sel = index.range(text, *name_range, enc);
        // The selection range must be inside the full range.
        if !range.contains_range(*name_range) {
            sel = lsp_types::Range { start: full.start, end: full.start };
        }
        let name = if d.symbol.kind == SymbolKind::Constructor { "constructor".to_string() } else { d.symbol.name.clone() };
        let children: Vec<DocumentSymbol> = d.children.iter().filter_map(|c| convert(c, text, index, enc, true)).collect();
        #[allow(deprecated)]
        Some(DocumentSymbol {
            name,
            detail: Some(d.symbol.signature()),
            kind: to_proto::symbol_kind(d.symbol.kind, d.symbol.modifiers, member),
            tags: d.symbol.modifiers.has(crate::symbols::Modifiers::DEPRECATED).then(|| vec![lsp_types::SymbolTag::DEPRECATED]),
            deprecated: None,
            range: full,
            selection_range: sel,
            children: (!children.is_empty()).then_some(children),
        })
    }
    cx.check()?;
    Ok(summary.decls.iter().filter_map(|d| convert(d, &file.text, &index, cx.enc, false)).collect())
}

/// Fuzzy match: query characters appear in order; camel-hump starts and prefix matches score higher.
pub fn match_score(name: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let name_chars: Vec<char> = name.chars().collect();
    let mut qi = query.chars().peekable();
    let mut score: i64 = 0;
    let mut prev_matched = false;
    for (i, &c) in name_chars.iter().enumerate() {
        let Some(&q) = qi.peek() else { break };
        if c.eq_ignore_ascii_case(&q) {
            let hump = i == 0 || c.is_uppercase() || name_chars[i - 1] == '_';
            score += if hump { 10 } else if prev_matched { 5 } else { 1 };
            if c == q {
                score += 1;
            }
            qi.next();
            prev_matched = true;
        } else {
            prev_matched = false;
        }
    }
    if qi.peek().is_some() {
        return None;
    }
    if name.eq_ignore_ascii_case(query) {
        score += 100;
    } else if name.to_lowercase().starts_with(&query.to_lowercase()) {
        score += 50;
    }
    Some(score - name.len() as i64 / 4)
}

pub fn workspace_symbols(cx: &Ctx, query: &str) -> Result<Vec<WorkspaceSymbol>> {
    let mut hits: Vec<(i64, crate::symbols::Symbol)> = Vec::new();
    let consider = |sym: &crate::symbols::Symbol, hits: &mut Vec<(i64, crate::symbols::Symbol)>| {
        if let Some(score) = match_score(&sym.name, query) {
            hits.push((score, sym.clone()));
        }
    };
    for (i, (_, locs)) in cx.snap.index.all_classifiers().enumerate() {
        if i % 256 == 0 {
            cx.check()?;
        }
        for loc in locs {
            if let Some(f) = cx.snap.file(&loc.path) {
                let s = f.summary();
                if let Some(d) = loc.resolve(&s) {
                    consider(&d.symbol, &mut hits);
                }
            }
        }
    }
    for (_, locs) in cx.snap.index.all_top_level() {
        cx.check()?;
        for loc in locs {
            if let Some(f) = cx.snap.file(&loc.path) {
                let s = f.summary();
                if let Some(d) = loc.resolve(&s)
                    && d.symbol.kind.is_callable() {
                        consider(&d.symbol, &mut hits);
                    }
            }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.fq_name.cmp(&b.1.fq_name)));
    hits.truncate(128);
    let mut out = Vec::new();
    for (_, sym) in hits {
        let Origin::Source { file, name_range, .. } = &sym.origin else { continue };
        let Some(f) = cx.snap.file(file) else { continue };
        let index = LineIndex::new(&f.text);
        let container = sym.fq_name.rsplit_once('.').map(|(c, _)| c.to_string());
        out.push(WorkspaceSymbol {
            name: sym.name.clone(),
            kind: to_proto::symbol_kind(sym.kind, sym.modifiers, false),
            tags: None,
            container_name: container,
            location: lsp_types::OneOf::Left(Location { uri: path_to_uri(file), range: index.range(&f.text, *name_range, cx.enc) }),
            data: None,
        });
    }
    if query.chars().count() >= 2 && out.len() < 128 {
        out.extend(library_symbols(cx, query, 128 - out.len())?);
    }
    Ok(out)
}

/// Library classes matching `query` (from the classpath of each module), located in their
/// sources jar or a generated stub. Only the best few are located: that reads archives.
fn library_symbols(cx: &Ctx, query: &str, room: usize) -> Result<Vec<WorkspaceSymbol>> {
    use crate::analysis::ide::library;
    const LOCATED: usize = 20;
    // One file per module gives that module's classpath.
    let mut seen_contexts = std::collections::HashSet::new();
    let mut classpaths: Vec<std::sync::Arc<crate::classpath::ClasspathIndex>> = Vec::new();
    for (path, f) in cx.snap.files.iter() {
        if f.language != crate::analysis::source::Language::Kotlin || !seen_contexts.insert(cx.snap.context_of(path)) {
            continue;
        }
        let cp = cx.snap.resolver(path).classpath.clone();
        if !classpaths.iter().any(|c| std::sync::Arc::ptr_eq(c, &cp)) {
            classpaths.push(cp);
        }
    }
    let mut hits: Vec<(i64, String, usize)> = Vec::new();
    let mut found = std::collections::HashSet::new();
    for (i, cp) in classpaths.iter().enumerate() {
        cx.check()?;
        for fq in cp.top_level_classes_where(&|simple| match_score(simple, query).is_some()) {
            if !crate::analysis::ide::completion::importable(&fq) || !found.insert(fq.clone()) {
                continue;
            }
            let simple = fq.rsplit('.').next().unwrap_or(&fq);
            if let Some(score) = match_score(simple, query) {
                hits.push((score, fq, i));
            }
        }
    }
    // Shorter names and Kotlin packages first among equal scores.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::new();
    for (_, fq, i) in hits.into_iter().take(room.min(LOCATED)) {
        cx.check()?;
        let cp = &classpaths[i];
        let Some(class) = cp.find_class(&fq) else { continue };
        let sym = class.symbol.clone();
        let Some(target) = library::locate_in_sources(cx.snap, cp, &sym, false).or_else(|| library::stub(cx.snap, cp, &sym)) else { continue };
        let Ok(text) = std::fs::read_to_string(&target.path) else { continue };
        let index = LineIndex::new(&text);
        out.push(WorkspaceSymbol {
            name: sym.name.clone(),
            kind: to_proto::symbol_kind(sym.kind, sym.modifiers, false),
            tags: None,
            container_name: fq.rsplit_once('.').map(|(c, _)| c.to_string()),
            location: lsp_types::OneOf::Left(Location { uri: path_to_uri(&target.path), range: index.range(&text, target.range, cx.enc) }),
            data: None,
        });
    }
    Ok(out)
}

pub fn file_diagnostics(cx: &Ctx, path: &Path) -> Result<(Vec<lsp_types::Diagnostic>, Coverage)> {
    let Some(file) = cx.snap.file(path) else { return Ok((Vec::new(), Coverage::SyntaxOnly("unknown file".into()))) };
    let (diags, coverage) = diagnostics::file_diagnostics(cx.snap, path);
    cx.check()?;
    let index = LineIndex::new(&file.text);
    Ok((diags.iter().map(|d| to_proto::diagnostic(&file.text, &index, d, cx.enc)).collect(), coverage))
}

/// Quick fixes and suppressions for lint findings in `range`, plus "fix all" for the file.
pub fn code_actions(cx: &Ctx, path: &Path, range: lsp_types::Range, context: &lsp_types::CodeActionContext) -> Result<Vec<lsp_types::CodeActionOrCommand>> {
    let only = context.only.as_deref();
    use crate::analysis::lints::{self, Category, Fix, Level};
    use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, TextEdit, WorkspaceEdit};
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let text = &file.text;
    let index = LineIndex::new(text);
    let wanted = crate::syntax::TextRange::new(index.offset(text, range.start, cx.enc), index.offset(text, range.end, cx.enc));
    let found = diagnostics::lints(cx.snap, path);
    cx.check()?;
    let wants = |k: &CodeActionKind| {
        only.is_none_or(|o| o.iter().any(|w| k.as_str() == w.as_str() || k.as_str().starts_with(&format!("{}.", w.as_str()))))
    };
    let uri = path_to_uri(path);
    let edit = |edits: Vec<&lints::Edit>| WorkspaceEdit {
        changes: Some(std::collections::HashMap::from([(
            uri.clone(),
            edits.into_iter().map(|e| TextEdit { range: index.range(text, e.range, cx.enc), new_text: e.text.clone() }).collect(),
        )])),
        ..Default::default()
    };
    let action = |title: &str, kind: CodeActionKind, diag: Option<lsp_types::Diagnostic>, fix: &Fix, preferred: bool| {
        let mut we = edit(fix.edits.iter().collect());
        if let Some(changes) = we.changes.as_mut() {
            for (p, e) in &fix.elsewhere {
                let Some(target) = cx.snap.file(p) else { continue };
                let li = LineIndex::new(&target.text);
                changes.entry(path_to_uri(p)).or_default().push(TextEdit { range: li.range(&target.text, e.range, cx.enc), new_text: e.text.clone() });
            }
            changes.retain(|_, v| !v.is_empty());
        }
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: Some(kind),
            diagnostics: diag.map(|d| vec![d]),
            edit: Some(we),
            is_preferred: Some(preferred),
            ..Default::default()
        })
    };
    let mut out = Vec::new();
    if wants(&CodeActionKind::QUICKFIX) {
        // Imports for unresolved names; attached to the client's matching diagnostic.
        let fixes = crate::analysis::ide::imports::import_fixes(cx.snap, path, wanted);
        cx.check()?;
        let single = fixes.len() == 1;
        for f in fixes {
            let diag = context.diagnostics.iter().find(|d| {
                matches!(&d.code, Some(lsp_types::NumberOrString::String(c)) if c == "UNRESOLVED_REFERENCE")
                    && crate::syntax::TextRange::new(index.offset(text, d.range.start, cx.enc), index.offset(text, d.range.end, cx.enc)).intersect(f.range).is_some()
            });
            let fix = Fix::replace(format!("Import '{}'", f.fq), f.edit.0, f.edit.1);
            out.push(action(&fix.title, CodeActionKind::QUICKFIX, diag.cloned(), &fix, single));
        }
        for f in crate::analysis::ide::create::create_fixes(cx.snap, path, wanted) {
            // `WorkspaceEdit::changes` is keyed by `Uri`.
            #[allow(clippy::mutable_key_type)]
            let mut changes: std::collections::HashMap<lsp_types::Uri, Vec<TextEdit>> = std::collections::HashMap::new();
            for (p, r, t) in &f.edits {
                let Some(target) = cx.snap.file(p) else { continue };
                let li = LineIndex::new(&target.text);
                changes.entry(path_to_uri(p)).or_default().push(TextEdit { range: li.range(&target.text, *r, cx.enc), new_text: t.clone() });
            }
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: f.title,
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                ..Default::default()
            }));
        }
        cx.check()?;
        let root = file.parse().syntax();
        for l in found.iter().filter(|l| l.range.intersect(wanted).is_some() && l.level != Level::Action) {
            let diag = to_proto::diagnostic(text, &index, &l.diagnostic(), cx.enc);
            if let Some(fix) = &l.fix {
                out.push(action(&fix.title, CodeActionKind::QUICKFIX, Some(diag.clone()), fix, true));
            }
            for fix in &l.alternatives {
                out.push(action(&fix.title, CodeActionKind::QUICKFIX, Some(diag.clone()), fix, false));
            }
            for s in lints::suppress_fixes(&root, text, l) {
                out.push(action(&s.title, CodeActionKind::QUICKFIX, Some(diag.clone()), &s, false));
            }
        }
    }
    // Rules at level `action`: a refactoring at the cursor, without a diagnostic.
    if wants(&CodeActionKind::REFACTOR_REWRITE) {
        for l in found.iter().filter(|l| l.level == Level::Action && l.range.intersect(wanted).is_some()) {
            if let Some(fix) = &l.fix {
                out.push(action(&fix.title, CodeActionKind::REFACTOR_REWRITE, None, fix, false));
            }
        }
    }
    for (kind, fix) in crate::analysis::ide::assists::assists(cx.snap, path, wanted) {
        let kind = CodeActionKind::from(kind.to_string());
        if wants(&kind) {
            out.push(action(&fix.title, kind, None, &fix, false));
        }
    }
    let generate = CodeActionKind::new("source.generate");
    if wants(&generate) {
        for g in crate::analysis::ide::generate::actions(cx.snap, path, wanted.start()) {
            out.push(action(&g.title, generate.clone(), None, &Fix::replace(g.title.clone(), g.range, g.text), false));
        }
    }
    if wants(&CodeActionKind::SOURCE_ORGANIZE_IMPORTS)
        && let Some((range, new_text)) = crate::analysis::ide::imports::organize_imports(cx.snap, path)
    {
        out.push(action("Organize imports", CodeActionKind::SOURCE_ORGANIZE_IMPORTS, None, &Fix::replace("Organize imports", range, new_text), false));
    }
    // Fix all unused/redundant code at once (non-overlapping fixes only).
    if wants(&CodeActionKind::SOURCE_FIX_ALL) {
        let mut edits: Vec<&lints::Edit> = Vec::new();
        let mut end = crate::syntax::TextSize::from(0);
        for l in &found {
            let Some(fix) = l.fix.as_ref().filter(|f| f.elsewhere.is_empty()) else { continue };
            if !matches!(l.rule.category, Category::Unused | Category::Redundancy) || matches!(l.level, Level::Hint | Level::Action) {
                continue;
            }
            let start = fix.edits.iter().map(|e| e.range.start()).min().unwrap_or_default();
            if start < end && !edits.is_empty() {
                continue;
            }
            end = fix.edits.iter().map(|e| e.range.end()).max().unwrap_or_default();
            edits.extend(fix.edits.iter());
        }
        if !edits.is_empty() && (only.is_some() || edits.len() > 1) {
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Fix all unused and redundant code".into(),
                kind: Some(CodeActionKind::SOURCE_FIX_ALL),
                edit: Some(edit(edits)),
                ..Default::default()
            }));
        }
    }
    Ok(out)
}

fn offset_of(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Option<(std::sync::Arc<crate::analysis::FileData>, crate::syntax::TextSize)> {
    let file = cx.snap.file(path)?.clone();
    let index = LineIndex::new(&file.text);
    let offset = index.offset(&file.text, pos, cx.enc);
    Some((file, offset))
}

pub fn definition(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Vec<Location>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(Vec::new()) };
    cx.check()?;
    let offline = cx.snap.workspace.offline;
    let targets = crate::analysis::ide::definition(cx.snap, path, offset, !offline);
    cx.check()?;
    Ok(locations(cx, targets))
}

pub fn implementation(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Vec<Location>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(Vec::new()) };
    let cancelled = || cx.cancelled.load(Ordering::Relaxed);
    let targets = crate::analysis::ide::hierarchy::implementations(cx.snap, path, offset, &cancelled);
    cx.check()?;
    Ok(locations(cx, targets))
}

pub fn type_definition(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Vec<Location>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(Vec::new()) };
    let targets = crate::analysis::ide::hierarchy::type_definition(cx.snap, path, offset);
    cx.check()?;
    Ok(locations(cx, targets))
}

pub fn declaration(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Vec<Location>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(Vec::new()) };
    let targets = crate::analysis::ide::hierarchy::declaration(cx.snap, path, offset);
    cx.check()?;
    Ok(locations(cx, targets))
}

fn anchor_data(a: &crate::analysis::ide::hierarchy::Anchor) -> serde_json::Value {
    use crate::analysis::ide::hierarchy::Anchor;
    match a {
        Anchor::Token(p, o) => serde_json::json!({ "path": p, "offset": u32::from(*o) }),
        Anchor::Type(p, fq) => serde_json::json!({ "path": p, "fq": fq }),
    }
}

fn anchor_of(data: Option<&serde_json::Value>) -> Option<crate::analysis::ide::hierarchy::Anchor> {
    use crate::analysis::ide::hierarchy::Anchor;
    let d = data?;
    let path = std::path::PathBuf::from(d.get("path")?.as_str()?);
    match (d.get("offset").and_then(|o| o.as_u64()), d.get("fq").and_then(|f| f.as_str())) {
        (Some(o), _) => Some(Anchor::Token(path, crate::syntax::TextSize::from(o as u32))),
        (_, Some(fq)) => Some(Anchor::Type(path, fq.to_string())),
        _ => None,
    }
}

/// A hierarchy item's file text (library stubs and sources live outside the snapshot).
fn item_text(cx: &Ctx, path: &Path) -> Option<std::sync::Arc<str>> {
    match cx.snap.file(path) {
        Some(f) => Some(f.text.clone()),
        None => std::fs::read_to_string(path).ok().map(Into::into),
    }
}

fn call_item(cx: &Ctx, i: &crate::analysis::ide::hierarchy::HierItem) -> Option<lsp_types::CallHierarchyItem> {
    let text = item_text(cx, &i.path)?;
    let index = LineIndex::new(&text);
    Some(lsp_types::CallHierarchyItem {
        name: i.name.clone(),
        kind: to_proto::symbol_kind(i.kind, Default::default(), i.detail.is_some()),
        tags: i.deprecated.then(|| vec![lsp_types::SymbolTag::DEPRECATED]),
        detail: i.detail.clone(),
        uri: path_to_uri(&i.path),
        range: index.range(&text, i.range, cx.enc),
        selection_range: index.range(&text, i.selection, cx.enc),
        data: Some(anchor_data(&i.anchor)),
    })
}

pub fn prepare_call_hierarchy(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Option<Vec<lsp_types::CallHierarchyItem>>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let item = crate::analysis::ide::hierarchy::prepare_call_hierarchy(cx.snap, path, offset);
    cx.check()?;
    Ok(item.and_then(|i| call_item(cx, &i)).map(|i| vec![i]))
}

/// Ranges of calls in `path`.
fn ranges_in(cx: &Ctx, path: &Path, ranges: &[crate::syntax::TextRange]) -> Vec<lsp_types::Range> {
    let Some(text) = item_text(cx, path) else { return Vec::new() };
    let index = LineIndex::new(&text);
    ranges.iter().map(|r| index.range(&text, *r, cx.enc)).collect()
}

pub fn incoming_calls(cx: &Ctx, item: &lsp_types::CallHierarchyItem) -> Result<Vec<lsp_types::CallHierarchyIncomingCall>> {
    let Some(anchor) = anchor_of(item.data.as_ref()) else { return Ok(Vec::new()) };
    let cancelled = || cx.cancelled.load(Ordering::Relaxed);
    let calls = crate::analysis::ide::hierarchy::incoming_calls(cx.snap, &anchor, &cancelled);
    cx.check()?;
    Ok(calls
        .into_iter()
        .filter_map(|(from, ranges)| Some(lsp_types::CallHierarchyIncomingCall { from_ranges: ranges_in(cx, &from.path, &ranges), from: call_item(cx, &from)? }))
        .collect())
}

pub fn outgoing_calls(cx: &Ctx, item: &lsp_types::CallHierarchyItem) -> Result<Vec<lsp_types::CallHierarchyOutgoingCall>> {
    use crate::analysis::ide::hierarchy::Anchor;
    let Some(anchor) = anchor_of(item.data.as_ref()) else { return Ok(Vec::new()) };
    let calls = crate::analysis::ide::hierarchy::outgoing_calls(cx.snap, &anchor);
    cx.check()?;
    // `fromRanges` are in the caller's file, where the call sites (anchors) are.
    Ok(calls
        .into_iter()
        .filter_map(|(to, ranges)| {
            let Anchor::Token(site, _) = &to.anchor else { return None };
            Some(lsp_types::CallHierarchyOutgoingCall { from_ranges: ranges_in(cx, site, &ranges), to: call_item(cx, &to)? })
        })
        .collect())
}

fn type_item(cx: &Ctx, i: &crate::analysis::ide::hierarchy::HierItem) -> Option<lsp_types::TypeHierarchyItem> {
    let c = call_item(cx, i)?;
    Some(lsp_types::TypeHierarchyItem {
        name: c.name,
        kind: c.kind,
        tags: i.deprecated.then_some(lsp_types::SymbolTag::DEPRECATED),
        detail: c.detail,
        uri: c.uri,
        range: c.range,
        selection_range: c.selection_range,
        data: c.data,
    })
}

pub fn prepare_type_hierarchy(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Option<Vec<lsp_types::TypeHierarchyItem>>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let item = crate::analysis::ide::hierarchy::prepare_type_hierarchy(cx.snap, path, offset);
    cx.check()?;
    Ok(item.and_then(|i| type_item(cx, &i)).map(|i| vec![i]))
}

/// Direct supertypes (`sub == false`) or direct project subtypes of a type hierarchy item.
pub fn type_hierarchy(cx: &Ctx, item: &lsp_types::TypeHierarchyItem, sub: bool) -> Result<Vec<lsp_types::TypeHierarchyItem>> {
    use crate::analysis::ide::hierarchy;
    let Some(anchor) = anchor_of(item.data.as_ref()) else { return Ok(Vec::new()) };
    let items = if sub { hierarchy::subtypes(cx.snap, &anchor) } else { hierarchy::supertypes(cx.snap, &anchor) };
    cx.check()?;
    Ok(items.iter().filter_map(|i| type_item(cx, i)).collect())
}

/// LSP locations of targets, which may be extracted library sources outside the snapshot.
fn locations(cx: &Ctx, targets: Vec<crate::analysis::ide::NavTarget>) -> Vec<Location> {
    let mut out = Vec::new();
    for t in targets {
        // Targets may be extracted library sources that are not part of the snapshot.
        let text: std::sync::Arc<str> = match cx.snap.file(&t.path) {
            Some(f) => f.text.clone(),
            None => match std::fs::read_to_string(&t.path) {
                Ok(s) => s.into(),
                Err(_) => continue,
            },
        };
        let index = LineIndex::new(&text);
        out.push(Location { uri: path_to_uri(&t.path), range: index.range(&text, t.range, cx.enc) });
    }
    out
}

pub fn references(cx: &Ctx, path: &Path, pos: lsp_types::Position, include_declaration: bool) -> Result<Vec<Location>> {
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(Vec::new()) };
    let cancelled = || cx.cancelled.load(Ordering::Relaxed);
    let Some(targets) = crate::analysis::ide::references(cx.snap, path, offset, include_declaration, &cancelled) else {
        cx.check()?;
        return Ok(Vec::new());
    };
    cx.check()?;
    let mut out = Vec::with_capacity(targets.len());
    let mut index_cache: std::collections::HashMap<std::path::PathBuf, LineIndex> = std::collections::HashMap::new();
    for t in targets {
        let Some(f) = cx.snap.file(&t.path) else { continue };
        let index = index_cache.entry(t.path.clone()).or_insert_with(|| LineIndex::new(&f.text));
        out.push(Location { uri: path_to_uri(&t.path), range: index.range(&f.text, t.range, cx.enc) });
    }
    Ok(out)
}

pub fn completion(cx: &Ctx, path: &Path, pos: lsp_types::Position, snippets: bool) -> Result<Option<lsp_types::CompletionResponse>> {
    use crate::analysis::ide::completion::{self, ItemKind};
    use lsp_types::{CompletionItem, CompletionItemKind as K, CompletionItemLabelDetails, CompletionTextEdit, InsertTextFormat, TextEdit};
    let Some((file, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let Some(c) = completion::complete(cx.snap, path, offset, completion::Options { snippets }) else { return Ok(None) };
    cx.check()?;
    let text = &file.text;
    let index = LineIndex::new(text);
    let range = index.range(text, c.range, cx.enc);
    let items = c
        .items
        .into_iter()
        .enumerate()
        .map(|(i, it)| {
            let kind = match it.kind {
                ItemKind::Variable => K::VARIABLE,
                ItemKind::Function => K::FUNCTION,
                ItemKind::Method => K::METHOD,
                ItemKind::Property => K::PROPERTY,
                ItemKind::Field => K::FIELD,
                ItemKind::Constructor => K::CONSTRUCTOR,
                ItemKind::Class => K::CLASS,
                ItemKind::Interface => K::INTERFACE,
                ItemKind::Object => K::MODULE,
                ItemKind::Enum => K::ENUM,
                ItemKind::EnumMember => K::ENUM_MEMBER,
                ItemKind::TypeParameter => K::TYPE_PARAMETER,
                ItemKind::Keyword => K::KEYWORD,
                ItemKind::Package => K::MODULE,
                ItemKind::Snippet => K::SNIPPET,
            };
            let edit_range = it.range.map(|r| index.range(text, r, cx.enc)).unwrap_or(range);
            CompletionItem {
                label: it.label.clone(),
                label_details: (it.label_detail.is_some() || it.description.is_some())
                    .then(|| CompletionItemLabelDetails { detail: it.label_detail.clone(), description: it.description.clone() }),
                kind: Some(kind),
                detail: it.detail,
                documentation: it.doc.map(|d| lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent { kind: lsp_types::MarkupKind::Markdown, value: d })),
                tags: it.deprecated.then(|| vec![lsp_types::CompletionItemTag::DEPRECATED]),
                // Our order: rank, then name; the index keeps it stable.
                sort_text: Some(format!("{}{:05}", it.rank, i)),
                filter_text: it.filter,
                insert_text_format: Some(if it.snippet { InsertTextFormat::SNIPPET } else { InsertTextFormat::PLAIN_TEXT }),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range: edit_range, new_text: it.insert.unwrap_or(it.label) })),
                additional_text_edits: (!it.additional.is_empty())
                    .then(|| it.additional.iter().map(|(r, t)| TextEdit { range: index.range(text, *r, cx.enc), new_text: t.clone() }).collect()),
                ..Default::default()
            }
        })
        .collect();
    Ok(Some(lsp_types::CompletionResponse::List(lsp_types::CompletionList { is_incomplete: c.incomplete, items })))
}

pub fn prepare_rename(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> std::result::Result<Option<lsp_types::PrepareRenameResponse>, RequestError> {
    let Some((file, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let (range, placeholder) = crate::analysis::ide::rename::prepare_rename(cx.snap, path, offset).map_err(RequestError::Failed)?;
    cx.check()?;
    let index = LineIndex::new(&file.text);
    Ok(Some(lsp_types::PrepareRenameResponse::RangeWithPlaceholder { range: index.range(&file.text, range, cx.enc), placeholder }))
}

/// `rename_files`: the client accepts `RenameFile` operations.
pub fn rename(cx: &Ctx, path: &Path, pos: lsp_types::Position, new_name: &str, rename_files: bool) -> std::result::Result<lsp_types::WorkspaceEdit, RequestError> {
    use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, ResourceOp, TextDocumentEdit, TextEdit};
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Err(RequestError::Failed("file is not loaded".into())) };
    let cancelled = || cx.cancelled.load(Ordering::Relaxed);
    let result = crate::analysis::ide::rename::rename(cx.snap, path, offset, new_name, &cancelled).map_err(RequestError::Failed)?;
    cx.check()?;
    let Some(r) = result else { return Err(RequestError::Cancelled) };
    // Group edits per file, in order.
    let mut files: Vec<(std::path::PathBuf, Vec<TextEdit>)> = Vec::new();
    let mut index: Option<(std::path::PathBuf, LineIndex)> = None;
    for (p, range, text) in &r.edits {
        let Some(f) = cx.snap.file(p) else { continue };
        if index.as_ref().is_none_or(|(ip, _)| ip != p) {
            index = Some((p.clone(), LineIndex::new(&f.text)));
            files.push((p.clone(), Vec::new()));
        }
        let li = &index.as_ref().unwrap().1;
        files.last_mut().unwrap().1.push(TextEdit { range: li.range(&f.text, *range, cx.enc), new_text: text.clone() });
    }
    match r.file_rename.filter(|_| rename_files) {
        None => Ok(lsp_types::WorkspaceEdit { changes: Some(files.into_iter().map(|(p, e)| (path_to_uri(&p), e)).collect()), ..Default::default() }),
        Some((from, to)) => {
            // Text edits address the old file name, so they go first.
            let mut ops: Vec<DocumentChangeOperation> = files
                .into_iter()
                .map(|(p, e)| {
                    let version = cx.snap.file(&p).and_then(|f| f.version);
                    DocumentChangeOperation::Edit(TextDocumentEdit {
                        text_document: OptionalVersionedTextDocumentIdentifier { uri: path_to_uri(&p), version },
                        edits: e.into_iter().map(OneOf::Left).collect(),
                    })
                })
                .collect();
            ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(lsp_types::RenameFile {
                old_uri: path_to_uri(&from),
                new_uri: path_to_uri(&to),
                options: None,
                annotation_id: None,
            })));
            Ok(lsp_types::WorkspaceEdit { document_changes: Some(DocumentChanges::Operations(ops)), ..Default::default() })
        }
    }
}

pub fn hover(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Option<lsp_types::Hover>> {
    let Some((file, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    cx.check()?;
    let Some(h) = crate::analysis::ide::hover(cx.snap, path, offset) else { return Ok(None) };
    cx.check()?;
    let index = LineIndex::new(&file.text);
    Ok(Some(lsp_types::Hover {
        contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent { kind: lsp_types::MarkupKind::Markdown, value: h.markdown }),
        range: Some(index.range(&file.text, h.range, cx.enc)),
    }))
}

pub fn document_highlight(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Option<Vec<lsp_types::DocumentHighlight>>> {
    use lsp_types::{DocumentHighlight, DocumentHighlightKind};
    let Some((file, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let Some(found) = crate::analysis::ide::document_highlight(cx.snap, path, offset) else { return Ok(None) };
    cx.check()?;
    let index = LineIndex::new(&file.text);
    Ok(Some(
        found
            .into_iter()
            .map(|(r, write)| DocumentHighlight { range: index.range(&file.text, r, cx.enc), kind: Some(if write { DocumentHighlightKind::WRITE } else { DocumentHighlightKind::READ }) })
            .collect(),
    ))
}

pub fn inlay_hints(cx: &Ctx, path: &Path, range: lsp_types::Range, config: &crate::analysis::ide::inlay::InlayConfig) -> Result<Vec<lsp_types::InlayHint>> {
    use crate::analysis::ide::inlay::{self, HintKind};
    use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel};
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let text = &file.text;
    let index = LineIndex::new(text);
    let wanted = crate::syntax::TextRange::new(index.offset(text, range.start, cx.enc), index.offset(text, range.end, cx.enc));
    let hints = inlay::inlay_hints(cx.snap, path, wanted, config);
    cx.check()?;
    Ok(hints
        .into_iter()
        .map(|h| InlayHint {
            position: index.position(text, h.offset, cx.enc),
            label: InlayHintLabel::String(h.label),
            kind: Some(match h.kind {
                HintKind::Type | HintKind::Chain => InlayHintKind::TYPE,
                HintKind::Parameter | HintKind::Return => InlayHintKind::PARAMETER,
            }),
            text_edits: None,
            tooltip: None,
            padding_left: Some(h.kind == HintKind::Chain),
            padding_right: Some(matches!(h.kind, HintKind::Parameter | HintKind::Return)),
            data: None,
        })
        .collect())
}

/// Semantic tokens of the file (or of `range`), in the LSP relative encoding. Tokens that
/// span lines (block comments, raw strings) are split per line.
pub fn semantic_tokens(cx: &Ctx, path: &Path, range: Option<lsp_types::Range>) -> Result<Vec<lsp_types::SemanticToken>> {
    use crate::analysis::ide::semantic;
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let text = &file.text;
    let index = LineIndex::new(text);
    let wanted = range.map(|r| crate::syntax::TextRange::new(index.offset(text, r.start, cx.enc), index.offset(text, r.end, cx.enc)));
    let tokens = semantic::semantic_tokens(cx.snap, path, wanted);
    cx.check()?;
    let len = |s: &str| match cx.enc {
        super::line_index::Encoding::Utf8 => s.len() as u32,
        super::line_index::Encoding::Utf16 => s.encode_utf16().count() as u32,
    };
    let mut out = Vec::with_capacity(tokens.len());
    let (mut line, mut col) = (0u32, 0u32);
    for t in tokens {
        let mut start = usize::from(t.range.start());
        for piece in text[start..usize::from(t.range.end())].split_inclusive('\n') {
            let body = piece.trim_end_matches(['\n', '\r']);
            if !body.is_empty() {
                let pos = index.position(text, crate::syntax::TextSize::from(start as u32), cx.enc);
                let delta_line = pos.line - line;
                let delta_start = if delta_line == 0 { pos.character - col } else { pos.character };
                out.push(lsp_types::SemanticToken { delta_line, delta_start, length: len(body), token_type: t.ty as u32, token_modifiers_bitset: t.modifiers });
                (line, col) = (pos.line, pos.character);
            }
            start += piece.len();
        }
    }
    Ok(out)
}

pub fn folding_ranges(cx: &Ctx, path: &Path, line_only: bool) -> Result<Vec<lsp_types::FoldingRange>> {
    use crate::analysis::ide::structure::{self, FoldKind};
    use lsp_types::FoldingRangeKind;
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let text = &file.text;
    let index = LineIndex::new(text);
    let folds = structure::folding_ranges(&file.parse().syntax(), text);
    cx.check()?;
    let mut out: Vec<lsp_types::FoldingRange> = Vec::new();
    for f in folds {
        let start = index.position(text, f.start, cx.enc);
        let end = index.position(text, f.end, cx.enc);
        let end_line = if f.keep_end_line { end.line.saturating_sub(1) } else { end.line };
        if end_line <= start.line || out.iter().any(|o| o.start_line == start.line) {
            continue;
        }
        out.push(lsp_types::FoldingRange {
            start_line: start.line,
            start_character: (!line_only).then_some(start.character),
            end_line,
            end_character: None,
            kind: match f.kind {
                FoldKind::Imports => Some(FoldingRangeKind::Imports),
                FoldKind::Comment => Some(FoldingRangeKind::Comment),
                FoldKind::Region => Some(FoldingRangeKind::Region),
                FoldKind::Code => None,
            },
            collapsed_text: None,
        });
    }
    Ok(out)
}

pub fn selection_ranges(cx: &Ctx, path: &Path, positions: &[lsp_types::Position]) -> Result<Vec<lsp_types::SelectionRange>> {
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let text = &file.text;
    let index = LineIndex::new(text);
    let root = file.parse().syntax();
    let mut out = Vec::new();
    for pos in positions {
        let offset = index.offset(text, *pos, cx.enc);
        let ranges = crate::analysis::ide::structure::selection_ranges(&root, text, offset);
        // Outermost first, each wrapping the next.
        let mut sel: Option<lsp_types::SelectionRange> = None;
        for r in ranges.iter().rev() {
            sel = Some(lsp_types::SelectionRange { range: index.range(text, *r, cx.enc), parent: sel.map(Box::new) });
        }
        out.push(sel.unwrap_or(lsp_types::SelectionRange { range: lsp_types::Range { start: *pos, end: *pos }, parent: None }));
    }
    cx.check()?;
    Ok(out)
}

pub fn signature_help(cx: &Ctx, path: &Path, pos: lsp_types::Position) -> Result<Option<lsp_types::SignatureHelp>> {
    use lsp_types::{Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureInformation};
    let Some((_, offset)) = offset_of(cx, path, pos) else { return Ok(None) };
    let Some(h) = crate::analysis::ide::signature::signature_help(cx.snap, path, offset) else { return Ok(None) };
    cx.check()?;
    let md = |v: String| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: v });
    // Parameter labels are offsets into the label, in UTF-16 code units.
    let utf16 = |s: &str| s.encode_utf16().count() as u32;
    let signatures = h
        .signatures
        .into_iter()
        .map(|s| SignatureInformation {
            parameters: Some(
                s.params
                    .iter()
                    .map(|(r, doc)| {
                        let (start, end) = (usize::from(r.start()), usize::from(r.end()));
                        let at = utf16(&s.label[..start]);
                        ParameterInformation { label: ParameterLabel::LabelOffsets([at, at + utf16(&s.label[start..end])]), documentation: doc.clone().map(md) }
                    })
                    .collect(),
            ),
            documentation: s.doc.map(md),
            active_parameter: s.active_param,
            label: s.label,
        })
        .collect();
    Ok(Some(lsp_types::SignatureHelp { signatures, active_signature: Some(h.active), active_parameter: None }))
}

/// JSON summary of the imported project model (for `kotlin.projectModel`).
pub fn project_model(snap: &Snapshot) -> serde_json::Value {
    let ws = &snap.workspace;
    let projects: Vec<serde_json::Value> = ws
        .projects
        .iter()
        .map(|p| {
            serde_json::json!({
                "root": p.root,
                "buildSystem": format!("{:?}", p.build_system),
                "source": format!("{:?}", p.source),
                "problems": p.problems,
                "modules": p.modules.iter().map(|m| serde_json::json!({
                    "name": m.name,
                    "dir": m.dir,
                    "complete": m.complete,
                    "kotlin": m.kotlin,
                    "sourceRoots": m.source_roots,
                    "moduleDependencies": m.module_dependencies,
                    "dependencies": m.dependencies.iter().map(|d| serde_json::json!({
                        "coordinate": d.coord.to_string(),
                        "scope": format!("{:?}", d.scope),
                        "file": d.file,
                        "direct": d.direct,
                    })).collect::<Vec<_>>(),
                    "problems": m.problems,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({
        "jdk": ws.jdk.as_ref().map(|j| serde_json::json!({ "home": j.home, "version": j.version })),
        "files": snap.files.len(),
        "projects": projects,
    })
}

/// A fresh cancellation flag.
pub fn new_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Indentation and trailing whitespace for the file, or the lines starting in `range`.
pub fn format(cx: &Ctx, path: &Path, options: &lsp_types::FormattingOptions, range: Option<lsp_types::Range>) -> Result<Option<Vec<lsp_types::TextEdit>>> {
    let Some(file) = cx.snap.file(path) else { return Ok(None) };
    if file.language != crate::analysis::source::Language::Kotlin {
        return Ok(None);
    }
    let text = &file.text;
    let index = LineIndex::new(text);
    let unit = if options.insert_spaces { " ".repeat(options.tab_size.max(1) as usize) } else { "\t".to_string() };
    let range = range.map(|r| crate::syntax::TextRange::new(index.offset(text, r.start, cx.enc), index.offset(text, r.end, cx.enc)));
    let root = file.parse().syntax();
    let edits = crate::analysis::ide::format::format(&root, text, &unit, range);
    cx.check()?;
    Ok(Some(edits.into_iter().map(|(r, t)| lsp_types::TextEdit { range: index.range(text, r, cx.enc), new_text: t }).collect()))
}

/// Package and import updates for moved Kotlin files (folders: every file inside).
pub fn will_rename_files(cx: &Ctx, params: &lsp_types::RenameFilesParams) -> Result<Option<lsp_types::WorkspaceEdit>> {
    let mut moves: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for f in &params.files {
        let (Some(old), Some(new)) = (f.old_uri.parse::<lsp_types::Uri>().ok().and_then(|u| crate::util::uri_to_path(&u)), f.new_uri.parse::<lsp_types::Uri>().ok().and_then(|u| crate::util::uri_to_path(&u))) else { continue };
        if cx.snap.file(&old).is_some() {
            moves.push((old, new));
        } else {
            for p in cx.snap.files.keys().filter(|p| p.starts_with(&old)) {
                if let Ok(rest) = p.strip_prefix(&old) {
                    moves.push((p.clone(), new.join(rest)));
                }
            }
        }
    }
    // `WorkspaceEdit::changes` is keyed by `Uri`.
    #[allow(clippy::mutable_key_type)]
    let mut changes: std::collections::HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> = std::collections::HashMap::new();
    for (old, new) in &moves {
        for (p, r, t) in crate::analysis::ide::file_rename::rename_file_edits(cx.snap, old, new) {
            let Some(f) = cx.snap.file(&p) else { continue };
            let li = LineIndex::new(&f.text);
            changes.entry(path_to_uri(&p)).or_default().push(lsp_types::TextEdit { range: li.range(&f.text, r, cx.enc), new_text: t });
        }
        cx.check()?;
    }
    Ok((!changes.is_empty()).then(|| lsp_types::WorkspaceEdit { changes: Some(changes), ..Default::default() }))
}

pub fn code_lenses(cx: &Ctx, path: &Path) -> Result<Vec<lsp_types::CodeLens>> {
    let Some(file) = cx.snap.file(path) else { return Ok(Vec::new()) };
    let index = LineIndex::new(&file.text);
    let uri = path_to_uri(path);
    let lenses = crate::analysis::ide::lens_targets(cx.snap, path)
        .into_iter()
        .map(|r| {
            let range = index.range(&file.text, r, cx.enc);
            lsp_types::CodeLens { range, command: None, data: Some(serde_json::json!({ "uri": uri.as_str(), "position": range.start })) }
        })
        .collect();
    cx.check()?;
    Ok(lenses)
}

/// Count the references of the lens's declaration (`kotlin.showReferences` with the
/// editor.action.showReferences arguments: uri, position, locations).
pub fn resolve_code_lens(cx: &Ctx, mut lens: lsp_types::CodeLens) -> Result<lsp_types::CodeLens> {
    let data = lens.data.clone().unwrap_or_default();
    let (Some(uri), Ok(pos)) = (data["uri"].as_str(), serde_json::from_value::<lsp_types::Position>(data["position"].clone())) else { return Ok(lens) };
    let Some(path) = uri.parse::<lsp_types::Uri>().ok().and_then(|u| crate::util::uri_to_path(&u)) else { return Ok(lens) };
    let locations = references(cx, &path, pos, false)?;
    let title = match locations.len() {
        1 => "1 reference".to_string(),
        n => format!("{n} references"),
    };
    lens.command = Some(lsp_types::Command { title, command: "kotlin.showReferences".into(), arguments: Some(vec![serde_json::json!(uri), serde_json::json!(pos), serde_json::json!(locations)]) });
    Ok(lens)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching() {
        assert!(match_score("InMemoryRepository", "imr").is_some());
        assert!(match_score("InMemoryRepository", "xyz").is_none());
        assert!(match_score("User", "user").unwrap() > match_score("UserRepository", "user").unwrap());
        assert!(match_score("HashMap", "HM").unwrap() > match_score("hashMapOf", "HM").unwrap());
    }
}
