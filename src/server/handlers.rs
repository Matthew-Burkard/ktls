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
    Ok(out)
}

pub fn file_diagnostics(cx: &Ctx, path: &Path) -> Result<(Vec<lsp_types::Diagnostic>, Coverage)> {
    let Some(file) = cx.snap.file(path) else { return Ok((Vec::new(), Coverage::SyntaxOnly("unknown file".into()))) };
    let (diags, coverage) = diagnostics::file_diagnostics(cx.snap, path);
    cx.check()?;
    let index = LineIndex::new(&file.text);
    Ok((diags.iter().map(|d| to_proto::diagnostic(&file.text, &index, d, cx.enc)).collect(), coverage))
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
    Ok(out)
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
