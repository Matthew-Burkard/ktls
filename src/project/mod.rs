//! Project model: build system import (Gradle / Maven), dependency resolution
//! and the resulting per-module source roots and classpaths.

pub mod coords;
pub mod gradle;
pub mod jdk;
pub mod maven;
pub mod pom;
pub mod repo;
pub mod resolver;
pub mod xml;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use self::pom::Scope;
use self::repo::{ProgressFn, RepoConfig, RepoSystem, Repository};
use self::resolver::ResolvedArtifact;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BuildSystem {
    Gradle,
    Maven,
    /// Plain directory without a recognised build file.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Main,
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRoot {
    pub path: PathBuf,
    pub kind: SourceKind,
    /// Generated sources (kapt/ksp/annotation processors).
    pub generated: bool,
    /// Kotlin Multiplatform source set name (`commonMain`, `jvmMain`...), or `main`/`test`.
    pub source_set: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KotlinSettings {
    /// Kotlin (plugin / stdlib) version.
    pub version: Option<String>,
    pub language_version: Option<String>,
    pub api_version: Option<String>,
    pub jvm_target: Option<String>,
    pub multiplatform: bool,
    /// Compiler plugins by short name: `serialization`, `allopen`, `noarg`, `compose`, `kapt`, `ksp`, `spring`...
    pub compiler_plugins: Vec<String>,
    pub explicit_api: bool,
    pub opt_in: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDependency {
    /// `Module::name` of the depended-upon module in the same project.
    pub module: String,
    pub scope: Scope,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    /// Unique within the project: Gradle path (`:app`) or Maven `groupId:artifactId`.
    pub name: String,
    pub dir: PathBuf,
    pub source_roots: Vec<SourceRoot>,
    /// External (binary) dependencies, transitively resolved.
    pub dependencies: Vec<ResolvedArtifact>,
    pub module_dependencies: Vec<ModuleDependency>,
    /// Local jars / class directories (`files(...)`, `fileTree`, Maven system scope).
    pub local_classpath: Vec<(PathBuf, Scope)>,
    pub kotlin: KotlinSettings,
    /// Requested Java release (toolchain / `maven.compiler.release`).
    pub java_release: Option<String>,
    pub problems: Vec<String>,
    /// Dependency resolution fully succeeded (safe to report unresolved references).
    pub complete: bool,
}

impl Module {
    fn scopes(kind: SourceKind) -> &'static [Scope] {
        match kind {
            SourceKind::Main => &[Scope::Compile, Scope::Provided, Scope::System],
            SourceKind::Test => &[Scope::Compile, Scope::Provided, Scope::System, Scope::Test, Scope::Runtime],
        }
    }

    /// Binary classpath for compiling sources of `kind` (excluding other modules' sources).
    pub fn classpath(&self, kind: SourceKind) -> Vec<PathBuf> {
        let scopes = Self::scopes(kind);
        let mut out: Vec<PathBuf> = self
            .dependencies
            .iter()
            .filter(|a| scopes.contains(&a.scope))
            .filter_map(|a| a.file.clone())
            .collect();
        out.extend(self.local_classpath.iter().filter(|(_, s)| scopes.contains(s)).map(|(p, _)| p.clone()));
        out.dedup();
        out
    }

    pub fn module_deps(&self, kind: SourceKind) -> impl Iterator<Item = &ModuleDependency> {
        let scopes = Self::scopes(kind);
        self.module_dependencies.iter().filter(move |d| scopes.contains(&d.scope))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSource {
    /// Built by reading build files and resolving dependencies natively.
    Static,
    /// Reported by the build tool itself (Gradle init script / Maven).
    Tooling,
    /// Loaded from the on-disk model cache.
    Cached,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub root: PathBuf,
    pub build_system: BuildSystem,
    pub modules: Vec<Module>,
    pub problems: Vec<String>,
    pub source: ModelSource,
    /// Build files the model was derived from (watched for changes, cache fingerprint).
    pub build_files: Vec<PathBuf>,
    pub repositories: Vec<Repository>,
}

impl Project {
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.name == name)
    }

    /// The module owning a source file (deepest matching source root).
    pub fn module_for_file(&self, path: &Path) -> Option<(&Module, &SourceRoot)> {
        self.modules
            .iter()
            .flat_map(|m| m.source_roots.iter().map(move |r| (m, r)))
            .filter(|(_, r)| path.starts_with(&r.path))
            .max_by_key(|(_, r)| r.path.components().count())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GradleMode {
    /// Static analysis first; refine with the Gradle tooling when a wrapper exists.
    #[default]
    Auto,
    Static,
    Tooling,
}

#[derive(Clone)]
pub struct LoadOptions {
    pub cache_dir: PathBuf,
    pub offline: bool,
    pub gradle_mode: GradleMode,
    /// Allow running build tools (`gradlew`, `mvn`) to obtain the model.
    pub allow_tooling: bool,
    pub progress: Option<ProgressFn>,
    pub extra_repositories: Vec<Repository>,
    /// Override the environment-derived repository configuration (tests).
    pub repo_config: Option<RepoConfig>,
}

impl LoadOptions {
    /// Options that ignore the user's ~/.m2 and ~/.gradle (hermetic tests).
    pub fn isolated(cache_dir: PathBuf) -> Self {
        let mut o = LoadOptions::new(cache_dir.clone());
        o.offline = true;
        o.repo_config = Some(RepoConfig {
            cache_dir: cache_dir.join("repository"),
            maven_local: None,
            gradle_user_home: None,
            offline: true,
            mirrors: Vec::new(),
            servers: Default::default(),
            negative_ttl: std::time::Duration::from_secs(60),
        });
        o
    }

    pub fn new(cache_dir: PathBuf) -> Self {
        LoadOptions {
            cache_dir,
            offline: false,
            gradle_mode: GradleMode::Auto,
            allow_tooling: false,
            progress: None,
            extra_repositories: Vec::new(),
            repo_config: None,
        }
    }

    pub fn repo_system(&self) -> Arc<RepoSystem> {
        let config = self.repo_config.clone().unwrap_or_else(|| RepoConfig::from_environment(self.cache_dir.join("repository"), self.offline));
        Arc::new(RepoSystem::new(config, self.progress.clone()))
    }

    pub fn report(&self, msg: &str) {
        if let Some(p) = &self.progress {
            p(msg);
        }
    }
}

const SKIP_DIRS: &[&str] = &["node_modules", "build", "target", "out", ".gradle", ".git", ".idea", "buildSrc", ".kotlin"];

/// Find project roots in workspace folders.
pub fn discover(folders: &[PathBuf]) -> Vec<(PathBuf, BuildSystem)> {
    let mut found: Vec<(PathBuf, BuildSystem)> = Vec::new();
    let push = |found: &mut Vec<(PathBuf, BuildSystem)>, p: PathBuf, bs| {
        if !found.iter().any(|(f, _)| p.starts_with(f)) {
            found.retain(|(f, _)| !f.starts_with(&p));
            found.push((p, bs));
        }
    };
    for folder in folders {
        // A folder inside a Gradle build: use the build root (nearest settings file).
        if let Some(root) = folder.ancestors().take(6).find(|d| gradle::is_settings_dir(d)) {
            push(&mut found, root.to_path_buf(), BuildSystem::Gradle);
            continue;
        }
        if gradle::is_build_dir(folder) {
            push(&mut found, folder.clone(), BuildSystem::Gradle);
            continue;
        }
        if folder.join("pom.xml").is_file() {
            // Walk up while the parent aggregates us.
            let mut root = folder.clone();
            while let Some(parent) = root.parent() {
                if parent.join("pom.xml").is_file() && maven::aggregates(&parent.join("pom.xml"), &root) {
                    root = parent.to_path_buf();
                } else {
                    break;
                }
            }
            push(&mut found, root, BuildSystem::Maven);
            continue;
        }
        // Look a couple of levels down for nested projects.
        let mut nested = false;
        let walker = walkdir::WalkDir::new(folder).min_depth(1).max_depth(3).into_iter().filter_entry(|e| {
            !e.file_type().is_dir() || !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
        });
        for e in walker.flatten() {
            if !e.file_type().is_dir() {
                continue;
            }
            let d = e.path();
            if gradle::is_settings_dir(d) || gradle::is_build_dir(d) {
                push(&mut found, d.to_path_buf(), BuildSystem::Gradle);
                nested = true;
            } else if d.join("pom.xml").is_file() {
                push(&mut found, d.to_path_buf(), BuildSystem::Maven);
                nested = true;
            }
        }
        if !nested {
            push(&mut found, folder.clone(), BuildSystem::None);
        }
    }
    found
}

/// Load one project (static analysis + native dependency resolution).
pub fn load(root: &Path, build_system: BuildSystem, opts: &LoadOptions) -> Project {
    let project = match build_system {
        BuildSystem::Gradle => gradle::load(root, opts),
        BuildSystem::Maven => maven::load(root, opts),
        BuildSystem::None => plain_project(root),
    };
    if project.build_system != BuildSystem::None {
        save_cached(&project, opts);
    }
    project
}

/// A directory without a build system: every Kotlin/Java file is a source.
fn plain_project(root: &Path) -> Project {
    let mut roots = Vec::new();
    for candidate in ["src/main/kotlin", "src/main/java", "src"] {
        let p = root.join(candidate);
        if p.is_dir() {
            roots.push(SourceRoot { path: p, kind: SourceKind::Main, generated: false, source_set: "main".into() });
            break;
        }
    }
    if roots.is_empty() {
        roots.push(SourceRoot { path: root.to_path_buf(), kind: SourceKind::Main, generated: false, source_set: "main".into() });
    }
    for candidate in ["src/test/kotlin", "src/test/java"] {
        let p = root.join(candidate);
        if p.is_dir() {
            roots.push(SourceRoot { path: p, kind: SourceKind::Test, generated: false, source_set: "test".into() });
        }
    }
    Project {
        root: root.to_path_buf(),
        build_system: BuildSystem::None,
        modules: vec![Module {
            name: root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into()),
            dir: root.to_path_buf(),
            source_roots: roots,
            complete: false,
            problems: vec!["No build file found: library dependencies are unknown".into()],
            ..Default::default()
        }],
        problems: Vec::new(),
        source: ModelSource::Static,
        build_files: Vec::new(),
        repositories: Vec::new(),
    }
}

// ------------------------------------------------------------ model cache

#[derive(Serialize, Deserialize)]
struct CachedModel {
    fingerprint: String,
    project: Project,
}

fn cache_path(root: &Path, opts: &LoadOptions) -> PathBuf {
    opts.cache_dir.join("models").join(format!("{}.json", crate::util::short_hash(&root.display().to_string())))
}

/// Bump when the importer's output changes, so cached models are recomputed.
const IMPORTER_REVISION: &str = "2";

fn fingerprint(files: &[PathBuf]) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(env!("CARGO_PKG_VERSION").as_bytes());
    h.update(IMPORTER_REVISION.as_bytes());
    for f in files {
        h.update(f.display().to_string().as_bytes());
        if let Ok(bytes) = std::fs::read(f) {
            h.update(&bytes);
        }
    }
    h.digest().to_string()
}

fn save_cached(project: &Project, opts: &LoadOptions) {
    let path = cache_path(&project.root, opts);
    let model = CachedModel { fingerprint: fingerprint(&project.build_files), project: project.clone() };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(&model) {
        let _ = std::fs::write(path, json);
    }
}

/// A previously computed model, if none of its build files changed since.
pub fn load_cached(root: &Path, opts: &LoadOptions) -> Option<Project> {
    let bytes = std::fs::read(cache_path(root, opts)).ok()?;
    let model: CachedModel = serde_json::from_slice(&bytes).ok()?;
    if model.fingerprint != fingerprint(&model.project.build_files) {
        return None;
    }
    // Jars may have been deleted from caches since.
    let all_present = model.project.modules.iter().all(|m| m.dependencies.iter().all(|d| d.file.as_ref().is_none_or(|f| f.exists())));
    if !all_present {
        return None;
    }
    let mut project = model.project;
    project.source = ModelSource::Cached;
    Some(project)
}

/// Standard source directories for a Gradle/Maven style module.
pub(crate) fn conventional_roots(dir: &Path, source_set: &str, kind: SourceKind, extra: &[PathBuf]) -> Vec<SourceRoot> {
    let mut roots = Vec::new();
    let mut add = |p: PathBuf, generated: bool| {
        if !roots.iter().any(|r: &SourceRoot| r.path == p) {
            roots.push(SourceRoot { path: p, kind, generated, source_set: source_set.to_string() });
        }
    };
    for lang in ["kotlin", "java"] {
        add(dir.join("src").join(source_set).join(lang), false);
    }
    for p in extra {
        add(p.clone(), false);
    }
    // Generated sources (only if present on disk).
    for g in [
        format!("build/generated/ksp/{source_set}/kotlin"),
        format!("build/generated/ksp/{source_set}/java"),
        format!("build/generated/source/kapt/{source_set}"),
        format!("build/generated/source/kaptKotlin/{source_set}"),
        format!("build/generated/sources/annotationProcessor/java/{source_set}"),
        format!("build/generated/source/buildConfig/{source_set}"),
    ] {
        let p = dir.join(g);
        if p.is_dir() {
            add(p, true);
        }
    }
    roots
}
