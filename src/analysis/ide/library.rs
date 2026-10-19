//! Navigation targets for library symbols: the declaration in a `-sources.jar`
//! (downloaded on demand), or a generated stub when no sources exist.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::analysis::source::{self, FileSummary, Language, SourceDecl};
use crate::analysis::{Snapshot, java};
use crate::classpath::ClasspathIndex;
use crate::project::coords::Coordinate;
use crate::project::repo::{RepoConfig, RepoSystem, Repository};
use crate::symbols::{Origin, Symbol, SymbolKind};
use crate::syntax::{TextRange, TextSize};

/// A location in a materialized (extracted or generated) file.
pub struct Target {
    pub path: PathBuf,
    pub range: TextRange,
    /// KDoc / Javadoc of the declaration if the real sources were found.
    pub doc: Option<String>,
    pub from_sources: bool,
}

/// The binary archive and class (internal name) a library symbol comes from.
fn origin_of(sym: &Symbol, cp: &ClasspathIndex) -> Option<(PathBuf, String)> {
    match &sym.origin {
        Origin::Library { archive, class } => Some((archive.clone(), class.clone())),
        Origin::Builtin => {
            // Built-ins (kotlin.String...) have no class file; their sources live in the stdlib.
            let archive = cp.builtins_archive()?.path.clone();
            let fq = if sym.kind.is_classifier() { sym.fq_name.clone() } else { sym.fq_name.rsplit_once('.')?.0.to_string() };
            Some((archive, fq.replace('.', "/")))
        }
        Origin::Source { .. } => None,
    }
}

/// Find (or download) the sources archive for a binary archive.
fn sources_archive(snap: &Snapshot, archive: &Path, download: bool) -> Option<PathBuf> {
    // The JDK image: lib/src.zip
    if archive.file_name().is_some_and(|n| n == "modules") || archive.extension().is_some_and(|e| e == "jmod") {
        return snap.workspace.jdk.as_ref()?.sources.clone();
    }
    let name = archive.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jar")?;
    let sources_name = format!("{stem}-sources.jar");
    // Maven layout (~/.m2, our cache): next to the jar.
    let sibling = archive.with_file_name(&sources_name);
    if sibling.is_file() {
        return Some(sibling);
    }
    // Gradle cache: files-2.1/g/a/v/<sha1>/a-v.jar and .../<other sha1>/a-v-sources.jar
    if let Some(version_dir) = archive.parent().and_then(|p| p.parent())
        && let Ok(entries) = std::fs::read_dir(version_dir) {
            for e in entries.flatten() {
                let p = e.path().join(&sources_name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    if !download || snap.workspace.offline {
        return None;
    }
    // Download through the repository system, using the coordinate the resolver recorded.
    let (coord, repos) = snap.workspace.projects.iter().find_map(|p| {
        p.modules.iter().flat_map(|m| m.dependencies.iter()).find(|d| d.file.as_deref() == Some(archive)).map(|d| (d.coord.clone(), p.repositories.clone()))
    })?;
    let mut repos: Vec<Repository> = repos;
    if !repos.contains(&Repository::maven_central()) {
        repos.push(Repository::maven_central());
    }
    let repo = RepoSystem::new(RepoConfig::from_environment(snap.workspace.cache_dir.join("repository"), false), None);
    let plain = Coordinate { classifier: None, ext: "jar".into(), ..coord };
    repo.artifact(&plain, Some("sources"), "jar", &repos)
}

/// Extract the candidate source files of `class` (best first) from a sources archive.
/// Several files may share a name and package (multiplatform source sets).
fn extract_source(snap: &Snapshot, cp: &ClasspathIndex, sources: &Path, class: &str) -> Vec<(PathBuf, String)> {
    extract_candidates(snap, cp, sources, class).unwrap_or_default()
}

fn extract_candidates(snap: &Snapshot, cp: &ClasspathIndex, sources: &Path, class: &str) -> Option<Vec<(PathBuf, String)>> {
    let top = class.split('$').next().unwrap_or(class);
    let package_path = top.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let package = package_path.replace('/', ".");
    let source_file = cp.class_file(class).and_then(|cf| cf.source_file).unwrap_or_else(|| {
        let simple = top.rsplit('/').next().unwrap_or(top);
        format!("{simple}.kt")
    });
    let file = std::fs::File::open(sources).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let names: Vec<String> = zip.file_names().filter_map(Result::ok).map(|n| n.into_owned()).collect();
    let mut candidates: Vec<&String> = names.iter().filter(|n| n.rsplit('/').next() == Some(source_file.as_str())).collect();
    if candidates.is_empty() && source_file.ends_with(".kt") {
        let java = source_file.replace(".kt", ".java");
        candidates = names.iter().filter(|n| n.rsplit('/').next() == Some(java.as_str())).collect();
    }
    // Exact package directory first, then JVM-specific multiplatform source sets.
    candidates.sort_by_key(|n| {
        let exact = n.ends_with(&format!("{package_path}/{source_file}"));
        let jvm = n.contains("jvm") || n.contains("java.base");
        (!exact, !jvm, n.len())
    });
    let hash = crate::util::short_hash(&sources.display().to_string());
    let mut found = Vec::new();
    for name in candidates {
        let mut text = String::new();
        match zip.by_name(name) {
            Ok(mut f) => {
                if f.read_to_string(&mut text).is_err() {
                    continue;
                }
            }
            Err(_) => continue,
        }
        // Several files can share a name: the package must match.
        let declared = text.lines().find_map(|l| l.trim().strip_prefix("package ")).map(|p| p.trim().trim_end_matches(';').trim().to_string()).unwrap_or_default();
        if declared != package && !package.is_empty() {
            continue;
        }
        let out = snap.workspace.cache_dir.join("sources").join(&hash).join(name);
        if !out.is_file() {
            std::fs::create_dir_all(out.parent()?).ok()?;
            std::fs::write(&out, &text).ok()?;
        }
        found.push((out, text));
    }
    Some(found)
}

fn summarize(path: &Path, text: &str) -> Option<FileSummary> {
    match Language::of(path)? {
        Language::Java => Some(java::summarize(path, text)),
        lang => {
            let parse = crate::syntax::parse(text, lang == Language::KotlinScript);
            Some(source::summarize(path, &parse, lang))
        }
    }
}

/// Find the declaration matching `sym` in a summarized source file.
fn find_decl<'a>(summary: &'a FileSummary, sym: &Symbol) -> Option<&'a SourceDecl> {
    fn walk<'a>(ds: &'a [SourceDecl], out: &mut Vec<&'a SourceDecl>) {
        for d in ds {
            out.push(d);
            walk(&d.children, out);
        }
    }
    let mut all = Vec::new();
    walk(&summary.decls, &mut all);
    let want_fq = if sym.kind == SymbolKind::Constructor { sym.fq_name.trim_end_matches(".<init>").to_string() } else { sym.fq_name.clone() };
    let kind_ok = |d: &SourceDecl| {
        let k = d.symbol.kind;
        match sym.kind {
            SymbolKind::Constructor => k.is_classifier(),
            SymbolKind::Function => matches!(k, SymbolKind::Function),
            SymbolKind::Property | SymbolKind::Field => matches!(k, SymbolKind::Property | SymbolKind::Field),
            k2 if k2.is_classifier() => k.is_classifier(),
            _ => true,
        }
    };
    let mut matches: Vec<&SourceDecl> = all.iter().copied().filter(|d| d.symbol.fq_name == want_fq && kind_ok(d)).collect();
    if matches.is_empty() {
        // Java getters used as Kotlin properties, `expect`/`actual` duplicates, etc: match by name.
        let name = want_fq.rsplit('.').next().unwrap_or(&want_fq);
        matches = all.iter().copied().filter(|d| d.symbol.name == name && kind_ok(d)).collect();
    }
    if matches.len() > 1 && sym.kind == SymbolKind::Function {
        // Overloads: same number of parameters, then same receiver presence.
        if let Some(m) = matches.iter().find(|d| d.symbol.params.len() == sym.params.len() && d.symbol.receiver.is_some() == sym.receiver.is_some()) {
            return Some(m);
        }
    }
    // Prefer `actual` over `expect` declarations.
    matches.sort_by_key(|d| d.symbol.modifiers.has(crate::symbols::Modifiers::EXPECT));
    matches.first().copied()
}

fn decl_target(path: PathBuf, decl: &SourceDecl) -> Target {
    let range = match &decl.symbol.origin {
        Origin::Source { name_range, .. } => *name_range,
        _ => TextRange::default(),
    };
    Target { path, range, doc: decl.symbol.doc.clone(), from_sources: true }
}

/// Locate a library symbol in its sources. `download` allows fetching sources jars.
pub fn locate_in_sources(snap: &Snapshot, cp: &ClasspathIndex, sym: &Symbol, download: bool) -> Option<Target> {
    let (archive, class) = origin_of(sym, cp)?;
    let sources = sources_archive(snap, &archive, download)?;
    let mut files = extract_source(snap, cp, &sources, &class);
    if files.is_empty() {
        // Built-ins and facades: also try the class name file of the symbol's container.
        if let Some(alt) = sym.fq_name.rsplit_once('.').map(|(c, _)| c.replace('.', "/")) {
            files = extract_source(snap, cp, &sources, &alt);
        }
    }
    for (path, text) in &files {
        let Some(summary) = summarize(path, text) else { continue };
        if let Some(d) = find_decl(&summary, sym) {
            return Some(decl_target(path.clone(), d));
        }
    }
    // Not declared in any candidate (e.g. built-ins): let the caller generate a stub.
    None
}

/// Generate a readable stub for a library class (when no sources are available).
pub fn stub(snap: &Snapshot, cp: &ClasspathIndex, sym: &Symbol) -> Option<Target> {
    let (archive, class) = origin_of(sym, cp)?;
    let owner_fq = if sym.kind.is_classifier() { sym.fq_name.clone() } else { sym.fq_name.rsplit_once('.').map(|(c, _)| c.to_string()).unwrap_or_default() };
    let mut text = format!("// Generated from {} — no sources available.\n", archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
    let mut target_offset = 0usize;
    let mut name_len = sym.name.len();
    let push_symbol = |text: &mut String, s: &Symbol, indent: &str, target: &mut usize| {
        let sig = s.signature();
        if s.fq_name == sym.fq_name && s.kind == sym.kind && s.params.len() == sym.params.len() && *target == 0 {
            let name_at = sig.find(&s.name).unwrap_or(0);
            *target = text.len() + indent.len() + name_at;
        }
        text.push_str(indent);
        text.push_str(&sig);
        text.push('\n');
    };
    match cp.find_class(&owner_fq) {
        Some(c) => {
            let package = c.symbol.fq_name.strip_suffix(&format!(".{}", c.symbol.name)).unwrap_or("");
            if !package.is_empty() {
                text.push_str(&format!("package {package}\n\n"));
            }
            if let Some(doc) = &c.symbol.doc {
                text.push_str(doc);
                text.push('\n');
            }
            push_symbol(&mut text, &c.symbol, "", &mut target_offset);
            text.pop();
            text.push_str(" {\n");
            for m in &c.members {
                let mut m = m.clone();
                if m.kind == SymbolKind::Constructor {
                    m.name = "constructor".into();
                }
                push_symbol(&mut text, &m, "    ", &mut target_offset);
            }
            for n in &c.nested {
                text.push_str(&format!("    class {n} {{ /* … */ }}\n"));
            }
            text.push_str("}\n");
            if sym.kind == SymbolKind::Constructor {
                name_len = c.symbol.name.len();
                target_offset = text.find(&format!("class {}", c.symbol.name)).map(|i| i + 6).unwrap_or(0);
            }
        }
        None => {
            // Top-level declarations of a package (file facade).
            let package = owner_fq.clone();
            text.push_str(&format!("package {package}\n\n"));
            for m in cp.package_members(&package).iter().filter(|m| m.name == sym.name) {
                push_symbol(&mut text, m, "", &mut target_offset);
            }
        }
    }
    let _ = class;
    let path = snap.workspace.cache_dir.join("stubs").join(format!("{}.kt", owner_fq.replace(['<', '>'], "_")));
    std::fs::create_dir_all(path.parent()?).ok()?;
    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&path, &text).ok()?;
    }
    let start = TextSize::from(target_offset as u32);
    Some(Target { path, range: TextRange::at(start, TextSize::from(name_len as u32)), doc: None, from_sources: false })
}

