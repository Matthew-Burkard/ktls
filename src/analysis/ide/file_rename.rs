//! Edits for moving a Kotlin file to another directory (`workspace/willRenameFiles`): its
//! `package` follows the directory, and imports follow its declarations.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::completion::import_edit;
use crate::analysis::Snapshot;
use crate::analysis::source::Language;
use crate::syntax::{SyntaxKind, TextRange, TextSize};

/// The package for a file in `dir`, given that `pkg` was the package for `old_dir`: the
/// part of the package not mirrored by directories (`com.acme` in `com.acme.util` under
/// `util/`) is kept.
fn package_for(snap: &Snapshot, pkg: &str, old_dir: &Path, new_dir: &Path) -> Option<String> {
    let dirs = |d: &Path| -> Option<Vec<String>> {
        let root = snap.workspace.source_root_of(d).or_else(|| snap.workspace.source_roots().map(|(r, _)| r.as_path()).find(|r| d == *r))?;
        Some(d.strip_prefix(root).ok()?.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
    };
    let (old, new) = (dirs(old_dir)?, dirs(new_dir)?);
    let segments: Vec<&str> = if pkg.is_empty() { Vec::new() } else { pkg.split('.').collect() };
    if !segments.ends_with(&old.iter().map(String::as_str).collect::<Vec<_>>()) {
        return None;
    }
    let prefix = &segments[..segments.len() - old.len()];
    let all: Vec<String> = prefix.iter().map(|s| s.to_string()).chain(new).collect();
    all.iter().all(|s| s.chars().all(|c| c.is_alphanumeric() || c == '_') && !s.is_empty()).then(|| all.join("."))
}

/// The edits for moving `old` to `new`, by file (at their current paths).
pub fn rename_file_edits(snap: &Snapshot, old: &Path, new: &Path) -> Vec<(PathBuf, TextRange, String)> {
    let mut out = Vec::new();
    let Some(file) = snap.file(old) else { return out };
    if file.language != Language::Kotlin || old.parent() == new.parent() {
        return out;
    }
    let summary = file.summary();
    let old_pkg = summary.package.clone();
    let (Some(od), Some(nd)) = (old.parent(), new.parent()) else { return out };
    let Some(new_pkg) = package_for(snap, &old_pkg, od, nd) else { return out };
    if new_pkg == old_pkg {
        return out;
    }
    // The moved file's `package`.
    let root = file.parse().syntax();
    let directive = root.children().find(|n| n.kind() == SyntaxKind::PACKAGE_DIRECTIVE).and_then(|d| d.children().find(|c| c.kind() == SyntaxKind::DOT_QUALIFIED_NAME));
    match directive {
        Some(name) => out.push((old.to_path_buf(), name.text_range(), new_pkg.clone())),
        None => out.push((old.to_path_buf(), TextRange::empty(TextSize::from(0)), format!("package {new_pkg}\n\n"))),
    }
    let moved: Vec<String> = summary.decls.iter().map(|d| d.symbol.name.clone()).filter(|n| !n.is_empty()).collect();
    let idents = |root: &crate::syntax::SyntaxNode| -> HashSet<String> {
        root.descendants_with_tokens().filter_map(|t| t.into_token()).filter(|t| t.kind() == SyntaxKind::IDENT && !t.parent_ancestors().any(|a| a.kind() == SyntaxKind::IMPORT_DIRECTIVE)).map(|t| t.text().to_string()).collect()
    };
    let mut own_needs: Vec<String> = Vec::new();
    let own_idents = idents(&root);
    for (path, other) in snap.files.iter() {
        if path == old || other.language != Language::Kotlin {
            continue;
        }
        let os = other.summary();
        let same_package = os.package == old_pkg;
        // The moved file used this file's declarations without importing them.
        if same_package {
            for d in &os.decls {
                if own_idents.contains(&d.symbol.name) && !moved.contains(&d.symbol.name) {
                    own_needs.push(format!("{old_pkg}.{}", d.symbol.name).trim_start_matches('.').to_string());
                }
            }
        }
        if !moved.iter().any(|m| other.text.contains(m.as_str())) {
            continue;
        }
        let mut imported: HashSet<String> = HashSet::new();
        // Explicit imports of the moved declarations (and their members).
        for imp in &os.imports {
            let path_text = imp.path();
            let Some((first, last)) = imp.segments.first().zip(imp.segments.last()) else { continue };
            for m in &moved {
                let fq = if old_pkg.is_empty() { m.clone() } else { format!("{old_pkg}.{m}") };
                if path_text == fq || path_text.starts_with(&format!("{fq}.")) {
                    let rest = &path_text[fq.len()..];
                    out.push((path.clone(), TextRange::new(first.1.start(), last.1.end()), format!("{new_pkg}.{m}{rest}")));
                    imported.insert(m.clone());
                }
            }
        }
        // Same-package users (and star importers) now need an import.
        let star = os.imports.iter().any(|i| i.star && i.path() == old_pkg);
        if same_package || star {
            let used = idents(&other.parse().syntax());
            for m in moved.iter().filter(|m| used.contains(*m) && !imported.contains(*m)) {
                let (range, text) = import_edit(other, &format!("{new_pkg}.{m}"));
                out.push((path.clone(), range, text));
            }
        }
    }
    own_needs.sort();
    own_needs.dedup();
    for fq in own_needs {
        if !summary.imports.iter().any(|i| i.path() == fq) {
            let (range, text) = import_edit(file, &fq);
            out.push((old.to_path_buf(), range, text));
        }
    }
    out
}
