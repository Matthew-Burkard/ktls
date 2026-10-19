//! Maven POM model and effective-model building (parents, properties, profiles,
//! dependency management and BOM imports).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use super::coords::{Coordinate, ModuleId};
use super::repo::{RepoSystem, Repository};
use super::xml::{self, Element};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Scope {
    #[default]
    Compile,
    Provided,
    Runtime,
    Test,
    System,
    Import,
}

impl Scope {
    pub fn parse(s: Option<&str>) -> Scope {
        match s.map(str::trim) {
            Some("provided") => Scope::Provided,
            Some("runtime") => Scope::Runtime,
            Some("test") => Scope::Test,
            Some("system") => Scope::System,
            Some("import") => Scope::Import,
            _ => Scope::Compile,
        }
    }

    /// Scope of a transitive dependency with scope `child` reached through `self`
    /// (Maven's scope table); `None` if it is not inherited at all.
    pub fn transitive(self, child: Scope) -> Option<Scope> {
        use Scope::*;
        match child {
            Provided | Test | System | Import => None,
            Compile => Some(match self {
                Compile => Compile,
                Provided => Provided,
                Runtime => Runtime,
                Test => Test,
                System | Import => Compile,
            }),
            Runtime => Some(match self {
                Compile | Runtime | System | Import => Runtime,
                Provided => Provided,
                Test => Test,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub module: ModuleId,
    pub version: Option<String>,
    pub scope: Option<Scope>,
    pub classifier: Option<String>,
    /// Maven `<type>` (jar, pom, test-jar, aar, ...).
    pub type_: String,
    pub optional: bool,
    /// Excluded modules; `*` wildcards allowed in either part.
    pub exclusions: Vec<ModuleId>,
    pub system_path: Option<String>,
}

impl Dependency {
    fn key(&self) -> (ModuleId, String, Option<String>) {
        (self.module.clone(), self.type_.clone(), self.classifier.clone())
    }

    /// File extension and classifier of the artifact this dependency refers to.
    pub fn artifact_kind(&self) -> (String, Option<String>) {
        match self.type_.as_str() {
            "test-jar" => ("jar".into(), Some(self.classifier.clone().unwrap_or_else(|| "tests".into()))),
            "maven-plugin" | "ejb" | "bundle" | "jar" | "" => ("jar".into(), self.classifier.clone()),
            other => (other.into(), self.classifier.clone()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plugin {
    pub group: String,
    pub artifact: String,
    pub version: Option<String>,
    /// `<configuration>` of the plugin and all of its executions, merged.
    pub configuration: Vec<Element>,
}

impl Plugin {
    /// All values of a (possibly list valued) configuration parameter.
    pub fn config_values(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.configuration {
            if let Some(el) = c.child(name) {
                if el.children.is_empty() {
                    if !el.text.is_empty() {
                        out.push(el.text.clone());
                    }
                } else {
                    out.extend(el.children.iter().map(|c| c.text.clone()).filter(|t| !t.is_empty()));
                }
            }
        }
        out
    }
}

/// A POM as written, before inheritance and interpolation.
#[derive(Clone, Debug, Default)]
pub struct RawPom {
    pub group_id: Option<String>,
    pub artifact_id: String,
    pub version: Option<String>,
    pub packaging: Option<String>,
    pub parent: Option<ParentRef>,
    pub properties: Vec<(String, String)>,
    pub dependencies: Vec<Dependency>,
    pub dependency_management: Vec<Dependency>,
    pub modules: Vec<String>,
    pub repositories: Vec<Repository>,
    pub source_directory: Option<String>,
    pub test_source_directory: Option<String>,
    pub plugins: Vec<Plugin>,
    pub profiles: Vec<Profile>,
    pub relocation: Option<(Option<String>, Option<String>, Option<String>)>,
    /// `<!-- do_not_remove: published-with-gradle-metadata -->`
    pub gradle_metadata_marker: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ParentRef {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub relative_path: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub id: String,
    pub active_by_default: bool,
    /// `<activation><property><name>` — `!name` means "active when absent".
    pub activation_property: Option<(String, Option<String>)>,
    pub activation_jdk: Option<String>,
    pub properties: Vec<(String, String)>,
    pub dependencies: Vec<Dependency>,
    pub dependency_management: Vec<Dependency>,
    pub modules: Vec<String>,
    pub repositories: Vec<Repository>,
    pub plugins: Vec<Plugin>,
}

pub fn parse_pom(text: &str) -> Option<RawPom> {
    let root = xml::parse(text)?;
    if root.name != "project" {
        return None;
    }
    let mut pom = RawPom {
        group_id: root.child_text("groupId").map(str::to_string),
        artifact_id: root.child_text("artifactId").unwrap_or_default().to_string(),
        version: root.child_text("version").map(str::to_string),
        packaging: root.child_text("packaging").map(str::to_string),
        gradle_metadata_marker: text.contains("published-with-gradle-metadata"),
        ..Default::default()
    };
    if let Some(p) = root.child("parent") {
        pom.parent = Some(ParentRef {
            group_id: p.child_text("groupId").unwrap_or_default().into(),
            artifact_id: p.child_text("artifactId").unwrap_or_default().into(),
            version: p.child_text("version").unwrap_or_default().into(),
            relative_path: p.child("relativePath").map(|r| r.text.clone()),
        });
    }
    pom.properties = parse_properties(&root);
    pom.dependencies = parse_deps(root.child("dependencies"));
    pom.dependency_management = parse_deps(root.path(&["dependencyManagement", "dependencies"]));
    pom.modules = parse_modules(&root);
    pom.repositories = parse_repositories(&root);
    if let Some(build) = root.child("build") {
        pom.source_directory = build.child_text("sourceDirectory").map(str::to_string);
        pom.test_source_directory = build.child_text("testSourceDirectory").map(str::to_string);
        pom.plugins = parse_plugins(build);
    }
    if let Some(r) = root.path(&["distributionManagement", "relocation"]) {
        pom.relocation = Some((
            r.child_text("groupId").map(str::to_string),
            r.child_text("artifactId").map(str::to_string),
            r.child_text("version").map(str::to_string),
        ));
    }
    if let Some(profiles) = root.child("profiles") {
        for p in profiles.children_named("profile") {
            let act = p.child("activation");
            pom.profiles.push(Profile {
                id: p.child_text("id").unwrap_or_default().into(),
                active_by_default: act.and_then(|a| a.child_text("activeByDefault")) == Some("true"),
                activation_property: act.and_then(|a| a.child("property")).and_then(|prop| {
                    Some((prop.child_text("name")?.to_string(), prop.child_text("value").map(str::to_string)))
                }),
                activation_jdk: act.and_then(|a| a.child_text("jdk")).map(str::to_string),
                properties: parse_properties(p),
                dependencies: parse_deps(p.child("dependencies")),
                dependency_management: parse_deps(p.path(&["dependencyManagement", "dependencies"])),
                modules: parse_modules(p),
                repositories: parse_repositories(p),
                plugins: p.child("build").map(parse_plugins).unwrap_or_default(),
            });
        }
    }
    Some(pom)
}

fn parse_properties(el: &Element) -> Vec<(String, String)> {
    el.child("properties").map(|p| p.children.iter().map(|c| (c.name.clone(), c.text.clone())).collect()).unwrap_or_default()
}

fn parse_modules(el: &Element) -> Vec<String> {
    el.child("modules").map(|m| m.children_named("module").map(|m| m.text.clone()).collect()).unwrap_or_default()
}

fn parse_repositories(el: &Element) -> Vec<Repository> {
    el.child("repositories")
        .map(|r| {
            r.children_named("repository")
                .filter_map(|r| {
                    let url = r.child_text("url")?;
                    Some(Repository::new(r.child_text("id").unwrap_or(url), url))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_deps(el: Option<&Element>) -> Vec<Dependency> {
    let Some(el) = el else { return Vec::new() };
    el.children_named("dependency")
        .map(|d| Dependency {
            module: ModuleId::new(d.child_text("groupId").unwrap_or_default(), d.child_text("artifactId").unwrap_or_default()),
            version: d.child_text("version").map(str::to_string),
            scope: d.child_text("scope").map(|s| Scope::parse(Some(s))),
            classifier: d.child_text("classifier").map(str::to_string),
            type_: d.child_text("type").unwrap_or("jar").to_string(),
            optional: d.child_text("optional") == Some("true"),
            exclusions: d
                .child("exclusions")
                .map(|e| {
                    e.children_named("exclusion")
                        .map(|x| ModuleId::new(x.child_text("groupId").unwrap_or("*"), x.child_text("artifactId").unwrap_or("*")))
                        .collect()
                })
                .unwrap_or_default(),
            system_path: d.child_text("systemPath").map(str::to_string),
        })
        .collect()
}

fn parse_plugins(build: &Element) -> Vec<Plugin> {
    let mut out = Vec::new();
    for section in [build.child("plugins"), build.path(&["pluginManagement", "plugins"])].into_iter().flatten() {
        for p in section.children_named("plugin") {
            let mut configuration: Vec<Element> = p.child("configuration").into_iter().cloned().collect();
            if let Some(execs) = p.child("executions") {
                for e in execs.children_named("execution") {
                    configuration.extend(e.child("configuration").cloned());
                }
            }
            out.push(Plugin {
                group: p.child_text("groupId").unwrap_or("org.apache.maven.plugins").into(),
                artifact: p.child_text("artifactId").unwrap_or_default().into(),
                version: p.child_text("version").map(str::to_string),
                configuration,
            });
        }
    }
    out
}

// --------------------------------------------------------- effective model

#[derive(Clone, Debug, Default)]
pub struct EffectivePom {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub packaging: String,
    pub properties: HashMap<String, String>,
    /// Dependencies with managed versions/scopes applied.
    pub dependencies: Vec<Dependency>,
    /// `dependencyManagement`, including imported BOMs.
    pub managed: Vec<Dependency>,
    pub repositories: Vec<Repository>,
    pub modules: Vec<String>,
    pub source_directory: Option<String>,
    pub test_source_directory: Option<String>,
    pub plugins: Vec<Plugin>,
    pub relocation: Option<Coordinate>,
    pub gradle_metadata_marker: bool,
    /// Problems encountered while building the model (missing parents/BOMs...).
    pub problems: Vec<String>,
    pub file: Option<PathBuf>,
}

impl EffectivePom {
    pub fn coordinate(&self) -> Coordinate {
        let mut c = Coordinate::new(&self.group_id, &self.artifact_id, &self.version);
        c.ext = self.packaging.clone();
        c
    }

    pub fn managed_version(&self, module: &ModuleId) -> Option<&Dependency> {
        self.managed.iter().find(|d| &d.module == module)
    }

    pub fn plugin(&self, artifact: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.artifact == artifact)
    }
}

/// Loads and caches effective POMs from a repository system.
pub struct PomLoader {
    pub repo: Arc<RepoSystem>,
    pub repos: Vec<Repository>,
    cache: Mutex<HashMap<(String, String, String), Option<Arc<EffectivePom>>>>,
    raw_cache: Mutex<HashMap<(String, String, String), Option<(Arc<RawPom>, PathBuf)>>>,
    /// System properties visible to profile activation and interpolation.
    pub system_properties: HashMap<String, String>,
}

/// One POM of an inheritance chain.
struct ChainLink {
    raw: RawPom,
    dir: Option<PathBuf>,
}

impl PomLoader {
    pub fn new(repo: Arc<RepoSystem>, repos: Vec<Repository>) -> Self {
        let mut system_properties = HashMap::new();
        system_properties.insert("java.version".to_string(), "17".to_string());
        PomLoader { repo, repos, cache: Mutex::new(HashMap::new()), raw_cache: Mutex::new(HashMap::new()), system_properties }
    }

    /// Register an already built model (reactor modules) so lookups find it.
    pub fn insert(&self, eff: EffectivePom) {
        let key = (eff.group_id.clone(), eff.artifact_id.clone(), eff.version.clone());
        self.cache.lock().insert(key, Some(Arc::new(eff)));
    }

    /// Effective POM of an artifact in a repository.
    pub fn load(&self, group: &str, artifact: &str, version: &str) -> Option<Arc<EffectivePom>> {
        self.load_with_repos(group, artifact, version, &self.repos)
    }

    fn load_raw(&self, group: &str, artifact: &str, version: &str, repos: &[Repository]) -> Option<(Arc<RawPom>, PathBuf)> {
        let key = (group.to_string(), artifact.to_string(), version.to_string());
        if let Some(hit) = self.raw_cache.lock().get(&key) {
            return hit.clone();
        }
        let result = (|| {
            let coord = Coordinate::new(group, artifact, version);
            let path = self.repo.artifact(&coord, None, "pom", repos)?;
            let text = std::fs::read_to_string(&path).ok()?;
            Some((Arc::new(parse_pom(&text)?), path))
        })();
        self.raw_cache.lock().insert(key, result.clone());
        result
    }

    pub fn load_with_repos(&self, group: &str, artifact: &str, version: &str, repos: &[Repository]) -> Option<Arc<EffectivePom>> {
        let key = (group.to_string(), artifact.to_string(), version.to_string());
        if let Some(hit) = self.cache.lock().get(&key) {
            return hit.clone();
        }
        let result = (|| {
            let (raw, path) = self.load_raw(group, artifact, version, repos)?;
            let mut all_repos = repos.to_vec();
            for r in &raw.repositories {
                if !all_repos.contains(r) && r.url.starts_with("https://") {
                    all_repos.push(r.clone());
                }
            }
            let mut eff = self.build((*raw).clone(), None, &all_repos, 0);
            eff.file = Some(path);
            Some(Arc::new(eff))
        })();
        self.cache.lock().insert(key, result.clone());
        result
    }

    /// Effective POM of a project file on disk (parents found via relativePath).
    pub fn load_file(&self, path: &Path) -> anyhow::Result<EffectivePom> {
        let text = std::fs::read_to_string(path)?;
        let raw = parse_pom(&text).ok_or_else(|| anyhow::anyhow!("not a valid POM: {}", path.display()))?;
        let mut repos = self.repos.clone();
        for r in &raw.repositories {
            if !repos.contains(r) {
                repos.push(r.clone());
            }
        }
        let mut eff = self.build(raw, path.parent(), &repos, 0);
        eff.file = Some(path.to_path_buf());
        Ok(eff)
    }

    /// Collect the inheritance chain, child first.
    fn chain(&self, raw: RawPom, dir: Option<&Path>, repos: &[Repository], problems: &mut Vec<String>) -> Vec<ChainLink> {
        let mut chain = vec![ChainLink { raw, dir: dir.map(Path::to_path_buf) }];
        while chain.len() < 32 {
            let last = chain.last().unwrap();
            let Some(p) = last.raw.parent.clone() else { break };
            // 1. relativePath on disk (projects only)
            let mut found = None;
            if let Some(dir) = &last.dir {
                let rel = p.relative_path.clone().unwrap_or_else(|| "../pom.xml".into());
                if !rel.is_empty() {
                    let mut candidate = dir.join(&rel);
                    if candidate.is_dir() {
                        candidate = candidate.join("pom.xml");
                    }
                    if let Some(parent_raw) = std::fs::read_to_string(&candidate).ok().and_then(|t| parse_pom(&t))
                        && parent_raw.artifact_id == p.artifact_id {
                            found = Some(ChainLink { raw: parent_raw, dir: candidate.parent().map(Path::to_path_buf) });
                        }
                }
            }
            // 2. repository
            if found.is_none() {
                match self.load_raw(&p.group_id, &p.artifact_id, &p.version, repos) {
                    Some((raw, _)) => found = Some(ChainLink { raw: (*raw).clone(), dir: None }),
                    None => problems.push(format!("parent POM not found: {}:{}:{}", p.group_id, p.artifact_id, p.version)),
                }
            }
            match found {
                Some(link) => chain.push(link),
                None => break,
            }
        }
        chain
    }

    /// Profiles of one POM that are active.
    fn active_profiles<'p>(&self, raw: &'p RawPom, props: &HashMap<String, String>) -> Vec<&'p Profile> {
        let explicit: Vec<&Profile> = raw
            .profiles
            .iter()
            .filter(|p| match &p.activation_property {
                Some((name, value)) => {
                    if let Some(neg) = name.strip_prefix('!') {
                        !props.contains_key(neg) && !self.system_properties.contains_key(neg)
                    } else {
                        let v = props.get(name).or_else(|| self.system_properties.get(name));
                        match (v, value) {
                            (Some(_), None) => true,
                            (Some(v), Some(want)) => v == want,
                            _ => false,
                        }
                    }
                }
                None => p.activation_jdk.as_deref().is_some_and(|jdk| jdk_matches(jdk, "17")),
            })
            .collect();
        if explicit.is_empty() { raw.profiles.iter().filter(|p| p.active_by_default).collect() } else { explicit }
    }

    fn build(&self, raw: RawPom, dir: Option<&Path>, repos: &[Repository], _depth: usize) -> EffectivePom {
        let mut problems = Vec::new();
        let child_parent = raw.parent.clone();
        let child_artifact = raw.artifact_id.clone();
        let child_packaging = raw.packaging.clone();
        let child_modules = raw.modules.clone();
        let child_relocation = raw.relocation.clone();
        let marker = raw.gradle_metadata_marker;
        let chain = self.chain(raw, dir, repos, &mut problems);

        // ---- inheritance assembly (raw values, top-most ancestor first) ----
        let mut group_id = String::new();
        let mut version = String::new();
        let mut props: HashMap<String, String> = HashMap::new();
        let mut deps: Vec<Dependency> = Vec::new();
        let mut managed_raw: Vec<Dependency> = Vec::new();
        let mut repositories: Vec<Repository> = Vec::new();
        let mut plugins: Vec<Plugin> = Vec::new();
        let mut source_directory = None;
        let mut test_source_directory = None;
        let mut modules = Vec::new();
        for link in chain.iter().rev() {
            let raw = &link.raw;
            if let Some(g) = raw.group_id.clone().or_else(|| raw.parent.as_ref().map(|p| p.group_id.clone())) {
                group_id = g;
            }
            if let Some(v) = raw.version.clone().or_else(|| raw.parent.as_ref().map(|p| p.version.clone())) {
                version = v;
            }
            for (k, v) in &raw.properties {
                props.insert(k.clone(), v.clone());
            }
            let active = self.active_profiles(raw, &props);
            for p in &active {
                for (k, v) in &p.properties {
                    props.insert(k.clone(), v.clone());
                }
            }
            // Child declarations override inherited ones with the same management key.
            let mut own_deps: Vec<Dependency> = raw.dependencies.clone();
            own_deps.extend(active.iter().flat_map(|p| p.dependencies.iter().cloned()));
            deps.retain(|d| !own_deps.iter().any(|o| o.key() == d.key()));
            deps.extend(own_deps);
            let mut own_managed: Vec<Dependency> = raw.dependency_management.clone();
            own_managed.extend(active.iter().flat_map(|p| p.dependency_management.iter().cloned()));
            managed_raw.retain(|d| !own_managed.iter().any(|o| o.key() == d.key()));
            managed_raw.extend(own_managed);
            for r in raw.repositories.iter().chain(active.iter().flat_map(|p| p.repositories.iter())) {
                if !repositories.contains(r) {
                    repositories.push(r.clone());
                }
            }
            for p in raw.plugins.iter().chain(active.iter().flat_map(|p| p.plugins.iter())) {
                if let Some(existing) = plugins.iter_mut().find(|e| e.artifact == p.artifact && e.group == p.group) {
                    // Child configuration first so its values win lookups.
                    let mut cfg = p.configuration.clone();
                    cfg.append(&mut existing.configuration);
                    existing.configuration = cfg;
                    if p.version.is_some() {
                        existing.version = p.version.clone();
                    }
                } else {
                    plugins.push(p.clone());
                }
            }
            if raw.source_directory.is_some() {
                source_directory = raw.source_directory.clone();
            }
            if raw.test_source_directory.is_some() {
                test_source_directory = raw.test_source_directory.clone();
            }
            if std::ptr::eq(link, &chain[0]) {
                modules = child_modules.clone();
                modules.extend(active.iter().flat_map(|p| p.modules.iter().cloned()));
            }
        }

        // ---- interpolation with the merged properties ----
        let mut builtins: HashMap<String, String> = HashMap::new();
        for prefix in ["project.", "pom."] {
            builtins.insert(format!("{prefix}groupId"), group_id.clone());
            builtins.insert(format!("{prefix}artifactId"), child_artifact.clone());
            builtins.insert(format!("{prefix}version"), version.clone());
            builtins.insert(format!("{prefix}packaging"), child_packaging.clone().unwrap_or_else(|| "jar".into()));
        }
        builtins.insert("version".into(), version.clone());
        builtins.insert("groupId".into(), group_id.clone());
        builtins.insert("artifactId".into(), child_artifact.clone());
        if let Some(p) = &child_parent {
            for prefix in ["project.parent.", "parent."] {
                builtins.insert(format!("{prefix}version"), p.version.clone());
                builtins.insert(format!("{prefix}groupId"), p.group_id.clone());
                builtins.insert(format!("{prefix}artifactId"), p.artifact_id.clone());
            }
        }
        if let Some(dir) = dir {
            builtins.insert("project.basedir".into(), dir.display().to_string());
            builtins.insert("basedir".into(), dir.display().to_string());
        }
        for (k, v) in builtins {
            props.insert(k, v);
        }
        for (k, v) in &self.system_properties {
            props.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let interp = Interpolator { props: &props };
        let interp_dep = |d: &Dependency| Dependency {
            module: ModuleId::new(interp.apply(&d.module.group), interp.apply(&d.module.artifact)),
            version: d.version.as_deref().map(|v| interp.apply(v)),
            scope: d.scope,
            classifier: d.classifier.as_deref().map(|c| interp.apply(c)),
            type_: interp.apply(&d.type_),
            optional: d.optional,
            exclusions: d.exclusions.iter().map(|e| ModuleId::new(interp.apply(&e.group), interp.apply(&e.artifact))).collect(),
            system_path: d.system_path.as_deref().map(|v| interp.apply(v)),
        };

        // ---- dependency management + BOM imports ----
        let mut managed: Vec<Dependency> = Vec::new();
        let mut imports = Vec::new();
        for d in managed_raw.iter().map(interp_dep) {
            if d.scope == Some(Scope::Import) && d.type_ == "pom" {
                imports.push(d);
            } else if !managed.iter().any(|m: &Dependency| m.key() == d.key()) {
                managed.push(d);
            }
        }
        for bom in imports {
            let Some(v) = bom.version.clone() else {
                problems.push(format!("BOM without version: {}", bom.module));
                continue;
            };
            match self.load_with_repos(&bom.module.group, &bom.module.artifact, &v, repos) {
                Some(b) => {
                    for d in &b.managed {
                        if !managed.iter().any(|m| m.key() == d.key()) {
                            managed.push(d.clone());
                        }
                    }
                }
                None => problems.push(format!("BOM not found: {}:{v}", bom.module)),
            }
        }

        let mut deps: Vec<Dependency> = deps.iter().map(interp_dep).collect();
        for d in &mut deps {
            if let Some(m) = managed.iter().find(|m| m.key() == d.key()).or_else(|| managed.iter().find(|m| m.module == d.module)) {
                if d.version.is_none() {
                    d.version = m.version.clone();
                }
                if d.scope.is_none() {
                    d.scope = m.scope;
                }
                for e in &m.exclusions {
                    if !d.exclusions.contains(e) {
                        d.exclusions.push(e.clone());
                    }
                }
            }
        }

        let repositories = repositories.into_iter().map(|r| Repository::new(r.id.clone(), &interp.apply(&r.url))).collect();
        let plugins = plugins
            .into_iter()
            .map(|p| Plugin {
                group: interp.apply(&p.group),
                artifact: interp.apply(&p.artifact),
                version: p.version.as_deref().map(|v| interp.apply(v)),
                configuration: p.configuration.iter().map(|c| interp_element(c, &interp)).collect(),
            })
            .collect();
        let relocation = child_relocation.as_ref().map(|(g, a, v)| {
            Coordinate::new(
                &g.as_deref().map(|g| interp.apply(g)).unwrap_or_else(|| interp.apply(&group_id)),
                &a.as_deref().map(|a| interp.apply(a)).unwrap_or_else(|| child_artifact.clone()),
                &v.as_deref().map(|v| interp.apply(v)).unwrap_or_else(|| interp.apply(&version)),
            )
        });

        EffectivePom {
            group_id: interp.apply(&group_id),
            artifact_id: interp.apply(&child_artifact),
            version: interp.apply(&version),
            packaging: child_packaging.as_deref().map(|p| interp.apply(p)).unwrap_or_else(|| "jar".into()),
            source_directory: source_directory.as_deref().map(|s| interp.apply(s)),
            test_source_directory: test_source_directory.as_deref().map(|s| interp.apply(s)),
            dependencies: deps,
            managed,
            repositories,
            modules: modules.iter().map(|m| interp.apply(m)).collect(),
            plugins,
            relocation,
            gradle_metadata_marker: marker,
            problems,
            file: None,
            properties: props,
        }
    }
}

fn interp_element(el: &Element, interp: &Interpolator) -> Element {
    Element {
        name: el.name.clone(),
        attrs: el.attrs.clone(),
        text: interp.apply(&el.text),
        children: el.children.iter().map(|c| interp_element(c, interp)).collect(),
    }
}

/// Does a profile `<jdk>` activation (`1.8`, `[11,)`, `!1.8`) match `jdk`?
fn jdk_matches(spec: &str, jdk: &str) -> bool {
    let spec = spec.trim();
    if let Some(neg) = spec.strip_prefix('!') {
        return !jdk_matches(neg, jdk);
    }
    if spec.starts_with('[') || spec.starts_with('(') {
        return super::coords::VersionReq::parse(spec).matches(jdk);
    }
    jdk.starts_with(spec)
}

struct Interpolator<'a> {
    props: &'a HashMap<String, String>,
}

impl Interpolator<'_> {
    fn apply(&self, s: &str) -> String {
        self.apply_depth(s, 0)
    }

    fn apply_depth(&self, s: &str, depth: usize) -> String {
        if !s.contains("${") || depth > 10 {
            return s.to_string();
        }
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let key = &rest[start + 2..start + end];
            let value = if let Some(env) = key.strip_prefix("env.") {
                std::env::var(env).ok()
            } else {
                self.props.get(key).cloned()
            };
            match value {
                Some(v) => out.push_str(&self.apply_depth(&v, depth + 1)),
                None => out.push_str(&rest[start..start + end + 1]),
            }
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::repo::RepoConfig;

    pub(crate) fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn loader(dir: &Path) -> PomLoader {
        let config = RepoConfig {
            cache_dir: dir.join("cache"),
            maven_local: Some(dir.join("m2")),
            gradle_user_home: None,
            offline: true,
            mirrors: vec![],
            negative_ttl: std::time::Duration::from_secs(1),
        };
        PomLoader::new(Arc::new(RepoSystem::new(config, None)), vec![])
    }

    #[test]
    fn effective_pom_with_parent_bom_and_properties() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        // A BOM in the local repository.
        write(
            d,
            "m2/org/bom/bom/1.0/bom-1.0.pom",
            r#"<project><groupId>org.bom</groupId><artifactId>bom</artifactId><version>1.0</version><packaging>pom</packaging>
            <dependencyManagement><dependencies>
              <dependency><groupId>org.lib</groupId><artifactId>managed</artifactId><version>3.3</version></dependency>
              <dependency><groupId>org.lib</groupId><artifactId>other</artifactId><version>9.9</version></dependency>
            </dependencies></dependencyManagement></project>"#,
        );
        write(
            d,
            "proj/pom.xml",
            r#"<project><groupId>com.acme</groupId><artifactId>parent</artifactId><version>2.0</version><packaging>pom</packaging>
            <properties><lib.version>1.5</lib.version><kotlin.version>1.9.0</kotlin.version></properties>
            <modules><module>app</module></modules>
            <dependencyManagement><dependencies>
              <dependency><groupId>org.lib</groupId><artifactId>other</artifactId><version>${lib.version}</version></dependency>
              <dependency><groupId>org.bom</groupId><artifactId>bom</artifactId><version>1.0</version><type>pom</type><scope>import</scope></dependency>
            </dependencies></dependencyManagement>
            <dependencies><dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>${kotlin.version}</version></dependency></dependencies>
            </project>"#,
        );
        let app = write(
            d,
            "proj/app/pom.xml",
            r#"<project><parent><groupId>com.acme</groupId><artifactId>parent</artifactId><version>2.0</version></parent>
            <artifactId>app</artifactId>
            <dependencies>
              <dependency><groupId>org.lib</groupId><artifactId>managed</artifactId></dependency>
              <dependency><groupId>org.lib</groupId><artifactId>other</artifactId></dependency>
              <dependency><groupId>${project.groupId}</groupId><artifactId>sibling</artifactId><version>${project.version}</version><scope>test</scope></dependency>
            </dependencies>
            <profiles><profile><id>p</id><activation><activeByDefault>true</activeByDefault></activation>
              <dependencies><dependency><groupId>org.p</groupId><artifactId>prof</artifactId><version>1</version></dependency></dependencies>
            </profile></profiles>
            </project>"#,
        );
        let eff = loader(d).load_file(&app).unwrap();
        assert!(eff.problems.is_empty(), "{:?}", eff.problems);
        assert_eq!(eff.group_id, "com.acme");
        assert_eq!(eff.version, "2.0");
        let v = |a: &str| eff.dependencies.iter().find(|x| x.module.artifact == a).and_then(|x| x.version.clone());
        assert_eq!(v("kotlin-stdlib").as_deref(), Some("1.9.0"), "inherited dependency");
        assert_eq!(v("managed").as_deref(), Some("3.3"), "version from imported BOM");
        assert_eq!(v("other").as_deref(), Some("1.5"), "explicit management wins over BOM");
        assert_eq!(v("sibling").as_deref(), Some("2.0"));
        assert_eq!(v("prof").as_deref(), Some("1"), "activeByDefault profile");
        let sib = eff.dependencies.iter().find(|x| x.module.artifact == "sibling").unwrap();
        assert_eq!(sib.module.group, "com.acme");
        assert_eq!(sib.scope, Some(Scope::Test));
    }

    #[test]
    fn scope_table() {
        assert_eq!(Scope::Compile.transitive(Scope::Compile), Some(Scope::Compile));
        assert_eq!(Scope::Compile.transitive(Scope::Runtime), Some(Scope::Runtime));
        assert_eq!(Scope::Test.transitive(Scope::Compile), Some(Scope::Test));
        assert_eq!(Scope::Compile.transitive(Scope::Test), None);
        assert_eq!(Scope::Compile.transitive(Scope::Provided), None);
    }
}
