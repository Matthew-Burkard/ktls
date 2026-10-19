//! Semantic analysis database.
//!
//! [`AnalysisHost`] owns the mutable state (workspace model + file contents).
//! [`Snapshot`]s are cheap immutable views handed to worker threads; derived
//! data (parses, summaries, the source index, classpath indexes) is computed
//! lazily and memoized.

pub mod diagnostics;
pub mod ide;
pub mod index;
pub mod java;
pub mod lints;
pub mod resolve;
pub mod source;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;

use crate::classpath::ClasspathIndex;
use crate::project::jdk::Jdk;
use crate::project::{Module, Project, SourceKind};
use crate::syntax::{self, Parse};
pub use index::SourceIndex;
pub use source::{FileSummary, Language};

/// Identifies a module: (project index, module index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleRef {
    pub project: u32,
    pub module: u32,
}

/// The source context of a file: its module and whether it is test code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileContext {
    pub module: ModuleRef,
    pub kind: SourceKind,
}

/// The imported build model plus lazily built classpath indexes.
pub struct Workspace {
    pub projects: Vec<Project>,
    pub jdk: Option<Jdk>,
    classpaths: Mutex<HashMap<Vec<PathBuf>, Arc<OnceLock<Arc<ClasspathIndex>>>>>,
    /// `classpath(ctx)` results (computing the entry list is not free).
    by_context: RwLock<HashMap<Option<FileContext>, Arc<ClasspathIndex>>>,
    /// Source roots sorted by depth (deepest first) for file → module lookup.
    roots: Vec<(PathBuf, FileContext)>,
    /// Where library sources / stubs for navigation are written.
    pub cache_dir: PathBuf,
    /// Never download (e.g. sources jars).
    pub offline: bool,
}

impl Workspace {
    pub fn new(projects: Vec<Project>, jdk: Option<Jdk>) -> Workspace {
        let mut roots = Vec::new();
        for (pi, p) in projects.iter().enumerate() {
            for (mi, m) in p.modules.iter().enumerate() {
                for r in &m.source_roots {
                    roots.push((r.path.clone(), FileContext { module: ModuleRef { project: pi as u32, module: mi as u32 }, kind: r.kind }));
                }
            }
        }
        roots.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        Workspace {
            projects,
            jdk,
            classpaths: Mutex::new(HashMap::new()),
            by_context: RwLock::new(HashMap::new()),
            roots,
            cache_dir: crate::util::default_cache_dir(),
            offline: false,
        }
    }

    /// Set the cache directory and network policy (from the server configuration).
    pub fn with_cache(mut self, cache_dir: PathBuf, offline: bool) -> Workspace {
        self.cache_dir = cache_dir;
        self.offline = offline;
        self
    }

    pub fn empty() -> Workspace {
        Workspace::new(Vec::new(), None)
    }

    pub fn module(&self, m: ModuleRef) -> &Module {
        &self.projects[m.project as usize].modules[m.module as usize]
    }

    pub fn modules(&self) -> impl Iterator<Item = (ModuleRef, &Module)> {
        self.projects.iter().enumerate().flat_map(|(pi, p)| {
            p.modules.iter().enumerate().map(move |(mi, m)| (ModuleRef { project: pi as u32, module: mi as u32 }, m))
        })
    }

    pub fn context_of(&self, path: &Path) -> Option<FileContext> {
        self.roots.iter().find(|(r, _)| path.starts_with(r)).map(|(_, c)| *c)
    }

    /// The (deepest) source root containing `path`.
    pub fn source_root_of(&self, path: &Path) -> Option<&Path> {
        self.roots.iter().find(|(r, _)| path.starts_with(r)).map(|(r, _)| r.as_path())
    }

    pub fn source_roots(&self) -> impl Iterator<Item = &(PathBuf, FileContext)> {
        self.roots.iter()
    }

    /// Source contexts whose declarations are visible from `ctx`.
    pub fn visible_contexts(&self, ctx: FileContext) -> HashSet<FileContext> {
        let mut out = HashSet::new();
        out.insert(FileContext { module: ctx.module, kind: SourceKind::Main });
        if ctx.kind == SourceKind::Test {
            out.insert(ctx);
        }
        let project = &self.projects[ctx.module.project as usize];
        let mut queue: Vec<(String, SourceKind)> =
            self.module(ctx.module).module_deps(ctx.kind).map(|d| (d.module.clone(), SourceKind::Main)).collect();
        let mut seen = HashSet::new();
        while let Some((name, kind)) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(mi) = project.modules.iter().position(|m| m.name == name) {
                let r = ModuleRef { project: ctx.module.project, module: mi as u32 };
                out.insert(FileContext { module: r, kind });
                // Gradle exposes `api` deps transitively; including all module deps errs on the
                // side of fewer false "unresolved" reports.
                queue.extend(project.modules[mi].module_deps(SourceKind::Main).map(|d| (d.module.clone(), SourceKind::Main)));
            }
        }
        out
    }

    /// Classpath index for a file context (JDK + binary dependencies).
    pub fn classpath(&self, ctx: Option<FileContext>) -> Arc<ClasspathIndex> {
        if let Some(hit) = self.by_context.read().get(&ctx) {
            return hit.clone();
        }
        let index = self.build_classpath(ctx);
        self.by_context.write().insert(ctx, index.clone());
        index
    }

    fn build_classpath(&self, ctx: Option<FileContext>) -> Arc<ClasspathIndex> {
        let entries = match ctx {
            Some(c) => self.module(c.module).classpath(c.kind),
            None => Vec::new(),
        };
        let cell = self.classpaths.lock().entry(entries.clone()).or_default().clone();
        cell.get_or_init(|| Arc::new(ClasspathIndex::build(&entries, self.jdk.as_ref()))).clone()
    }

    /// Build the classpath indexes of every module up front (in parallel). Call this
    /// from a non-rayon thread before handing the workspace to request handlers.
    pub fn warm_classpaths(&self) {
        let mut contexts: Vec<Option<FileContext>> = vec![None];
        for (m, _) in self.modules() {
            contexts.push(Some(FileContext { module: m, kind: SourceKind::Main }));
            contexts.push(Some(FileContext { module: m, kind: SourceKind::Test }));
        }
        // Distinct classpaths first so that identical ones are built once.
        let mut seen = HashSet::new();
        let distinct: Vec<Option<FileContext>> = contexts
            .into_iter()
            .filter(|c| seen.insert(c.map(|c| self.module(c.module).classpath(c.kind)).unwrap_or_default()))
            .collect();
        distinct.par_iter().for_each(|c| {
            let entries = c.map(|c| self.module(c.module).classpath(c.kind)).unwrap_or_default();
            let cell = self.classpaths.lock().entry(entries.clone()).or_default().clone();
            cell.get_or_init(|| Arc::new(ClasspathIndex::build_with(&entries, self.jdk.as_ref(), false)));
        });
    }

    /// Whether semantic diagnostics for `ctx` can be trusted (all deps resolved).
    pub fn is_complete(&self, ctx: Option<FileContext>) -> bool {
        let Some(c) = ctx else { return false };
        self.jdk.is_some() && self.module(c.module).complete
    }
}

pub struct FileData {
    pub path: PathBuf,
    pub text: Arc<str>,
    pub language: Language,
    /// LSP document version if the file is open in the editor.
    pub version: Option<i32>,
    parse: OnceLock<Parse>,
    summary: OnceLock<Arc<FileSummary>>,
}

impl FileData {
    pub fn new(path: PathBuf, text: Arc<str>, version: Option<i32>) -> Option<FileData> {
        let language = Language::of(&path)?;
        Some(FileData { path, text, language, version, parse: OnceLock::new(), summary: OnceLock::new() })
    }

    /// Kotlin parse tree (Java files get an empty tree).
    pub fn parse(&self) -> &Parse {
        self.parse.get_or_init(|| match self.language {
            Language::Kotlin => syntax::parse(&self.text, false),
            Language::KotlinScript => syntax::parse(&self.text, true),
            Language::Java => syntax::parse("", false),
        })
    }

    pub fn summary(&self) -> Arc<FileSummary> {
        self.summary
            .get_or_init(|| {
                Arc::new(match self.language {
                    Language::Java => java::summarize(&self.path, &self.text),
                    lang => source::summarize(&self.path, self.parse(), lang),
                })
            })
            .clone()
    }
}

/// Immutable view of the analysis state.
#[derive(Clone)]
pub struct Snapshot {
    pub workspace: Arc<Workspace>,
    pub files: Arc<HashMap<PathBuf, Arc<FileData>>>,
    pub index: Arc<SourceIndex>,
    pub lints: Arc<lints::LintConfig>,
}

impl Snapshot {
    pub fn file(&self, path: &Path) -> Option<&Arc<FileData>> {
        self.files.get(path)
    }

    pub fn context_of(&self, path: &Path) -> Option<FileContext> {
        self.workspace.context_of(path)
    }

    pub fn resolver(&self, path: &Path) -> resolve::Resolver<'_> {
        resolve::Resolver::new(self, path)
    }
}

/// Owner of the mutable analysis inputs.
pub struct AnalysisHost {
    workspace: Arc<Workspace>,
    files: HashMap<PathBuf, Arc<FileData>>,
    index: Option<Arc<SourceIndex>>,
    /// Files changed since the index was last built.
    dirty: HashSet<PathBuf>,
    lints: Arc<lints::LintConfig>,
}

impl Default for AnalysisHost {
    fn default() -> Self {
        AnalysisHost { workspace: Arc::new(Workspace::empty()), files: HashMap::new(), index: None, dirty: HashSet::new(), lints: Arc::default() }
    }
}

impl AnalysisHost {
    pub fn workspace(&self) -> &Arc<Workspace> {
        &self.workspace
    }

    pub fn set_lint_config(&mut self, config: lints::LintConfig) {
        self.lints = Arc::new(config);
    }

    /// Replace the workspace model. Files read from disk are dropped (the caller
    /// re-adds them); open documents (with a version) are kept.
    pub fn set_workspace(&mut self, ws: Workspace) {
        self.workspace = Arc::new(ws);
        self.files.retain(|_, f| f.version.is_some());
        self.index = None;
    }

    /// Add a prepared file (summary possibly already computed on another thread).
    pub fn insert_file(&mut self, file: Arc<FileData>) {
        self.dirty.insert(file.path.clone());
        self.index = None;
        self.files.insert(file.path.clone(), file);
    }

    pub fn file(&self, path: &Path) -> Option<&Arc<FileData>> {
        self.files.get(path)
    }

    pub fn files(&self) -> impl Iterator<Item = &Arc<FileData>> {
        self.files.values()
    }

    /// Add or replace a file's contents.
    pub fn set_file(&mut self, path: PathBuf, text: Arc<str>, version: Option<i32>) {
        if let Some(existing) = self.files.get(&path)
            && existing.text == text && existing.version == version {
                return;
            }
        if let Some(fd) = FileData::new(path.clone(), text, version) {
            self.files.insert(path.clone(), Arc::new(fd));
            self.dirty.insert(path);
        }
    }

    pub fn remove_file(&mut self, path: &Path) {
        if self.files.remove(path).is_some() {
            self.index = None;
        }
    }

    /// Load every Kotlin/Java source under the workspace's source roots (that is not
    /// already known, e.g. open in the editor).
    pub fn load_sources_from_disk(&mut self) -> usize {
        let roots: Vec<PathBuf> = self.workspace.source_roots().map(|(p, _)| p.clone()).collect();
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut seen = HashSet::new();
        for root in roots {
            for e in walkdir::WalkDir::new(&root).into_iter().filter_entry(|e| e.file_name() != "build" || e.depth() == 0).flatten() {
                let p = e.path();
                if e.file_type().is_file() && Language::of(p).is_some() && seen.insert(p.to_path_buf()) && !self.files.contains_key(p) {
                    paths.push(p.to_path_buf());
                }
            }
        }
        let loaded: Vec<(PathBuf, String)> = paths.par_iter().filter_map(|p| Some((p.clone(), std::fs::read_to_string(p).ok()?))).collect();
        let n = loaded.len();
        for (p, text) in loaded {
            if let Some(fd) = FileData::new(p.clone(), text.into(), None) {
                self.files.insert(p, Arc::new(fd));
            }
        }
        if n > 0 {
            self.index = None;
        }
        n
    }

    pub fn snapshot(&mut self) -> Snapshot {
        let reuse = match &self.index {
            Some(idx) if !self.dirty.is_empty() => {
                // Unchanged declaration shapes mean the index is still valid.
                let dirty: Vec<&PathBuf> = self.dirty.iter().collect();
                dirty.par_iter().all(|p| match (self.files.get(*p), idx.shape(p)) {
                    (Some(f), Some(old)) => f.summary().shape_hash() == old,
                    _ => false,
                })
            }
            Some(_) => true,
            None => false,
        };
        if !reuse {
            let files: Vec<Arc<FileData>> = self.files.values().cloned().collect();
            self.index = Some(Arc::new(SourceIndex::build(&files, &self.workspace)));
        }
        self.dirty.clear();
        Snapshot {
            workspace: self.workspace.clone(),
            files: Arc::new(self.files.clone()),
            index: self.index.clone().unwrap(),
            lints: self.lints.clone(),
        }
    }
}
