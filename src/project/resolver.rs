//! Transitive dependency resolution.
//!
//! Supports Maven semantics (nearest wins, scope mediation, exclusions,
//! dependencyManagement) and Gradle semantics (highest version wins, platforms,
//! Gradle Module Metadata variants with `available-at` redirection).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::coords::{Coordinate, ModuleId, Version, VersionReq};
use super::pom::{PomLoader, Scope};
use super::repo::Repository;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Nearest declaration wins (Maven).
    Nearest,
    /// Highest requested version wins (Gradle).
    Highest,
}

/// A requested (direct) dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub module: ModuleId,
    pub version: String,
    pub scope: Scope,
    pub classifier: Option<String>,
    pub ext: String,
    pub exclusions: Vec<ModuleId>,
    pub transitive: bool,
    /// Gradle `strictly` / `!!`: this exact version, no conflict resolution upwards.
    pub strict: bool,
    /// `platform(...)` / Maven `import` BOM: contributes version constraints only.
    pub platform: bool,
}

impl Request {
    pub fn new(module: ModuleId, version: impl Into<String>, scope: Scope) -> Self {
        Request {
            module,
            version: version.into(),
            scope,
            classifier: None,
            ext: "jar".into(),
            exclusions: Vec::new(),
            transitive: true,
            strict: false,
            platform: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedArtifact {
    pub coord: Coordinate,
    pub scope: Scope,
    /// The binary (jar/klib/extracted aar classes) if found.
    pub file: Option<PathBuf>,
    pub direct: bool,
    pub depth: u32,
    /// Module that pulled this one in (None for direct dependencies).
    pub via: Option<ModuleId>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Resolution {
    pub artifacts: Vec<ResolvedArtifact>,
    pub problems: Vec<String>,
}

impl Resolution {
    pub fn files(&self, scopes: &[Scope]) -> Vec<PathBuf> {
        self.artifacts.iter().filter(|a| scopes.contains(&a.scope)).filter_map(|a| a.file.clone()).collect()
    }
}

/// Target platform used to pick Gradle Module Metadata variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetPlatform {
    Jvm,
    Android,
}

// ------------------------------------------------------------- descriptors

#[derive(Clone, Debug, Default)]
struct Edge {
    module: ModuleId,
    version: String,
    scope: Scope,
    classifier: Option<String>,
    ext: String,
    optional: bool,
    exclusions: Vec<ModuleId>,
    platform: bool,
}

#[derive(Clone, Debug, Default)]
struct Descriptor {
    edges: Vec<Edge>,
    /// Version constraints contributed by this component when used as a platform / BOM.
    constraints: Vec<(ModuleId, String)>,
    /// Gradle `dependencyConstraints`: apply whenever this component is in the graph.
    graph_constraints: Vec<(ModuleId, String)>,
    /// Explicit files from Gradle metadata: (repository-relative path).
    files: Vec<String>,
    /// The coordinate whose artifact files should be used (after `available-at` / relocation).
    artifact_coord: Option<Coordinate>,
    packaging: String,
    /// Gradle metadata says the selected variant has no files (e.g. kotlin-stdlib-common 2.x).
    no_files: bool,
    missing: bool,
    problems: Vec<String>,
}

pub struct Resolver {
    pub loader: Arc<PomLoader>,
    pub strategy: Strategy,
    pub platform: TargetPlatform,
    /// Use Gradle Module Metadata (`.module` files) when published.
    pub gradle_metadata: bool,
    /// Versions forced by the consumer (Maven dependencyManagement of the root).
    pub managed: HashMap<ModuleId, String>,
    descriptors: Mutex<HashMap<(ModuleId, String), Arc<Descriptor>>>,
}

#[derive(Clone, Debug)]
struct Node {
    module: ModuleId,
    version: String,
    scope: Scope,
    classifier: Option<String>,
    ext: String,
    exclusions: Arc<Vec<ModuleId>>,
    depth: u32,
    via: Option<ModuleId>,
    transitive: bool,
}

fn excluded(exclusions: &[ModuleId], m: &ModuleId) -> bool {
    exclusions.iter().any(|e| (e.group == "*" || e.group == m.group) && (e.artifact == "*" || e.artifact == m.artifact))
}

fn scope_strength(s: Scope) -> u8 {
    match s {
        Scope::Compile | Scope::System | Scope::Import => 4,
        Scope::Provided => 3,
        Scope::Runtime => 2,
        Scope::Test => 1,
    }
}

impl Resolver {
    pub fn new(loader: Arc<PomLoader>, strategy: Strategy) -> Self {
        Resolver {
            loader,
            strategy,
            platform: TargetPlatform::Jvm,
            gradle_metadata: strategy == Strategy::Highest,
            managed: HashMap::new(),
            descriptors: Mutex::new(HashMap::new()),
        }
    }

    fn repos(&self) -> &[Repository] {
        &self.loader.repos
    }

    pub fn resolve(&self, requests: &[Request]) -> Resolution {
        let mut problems = Vec::new();
        // Constraints from platforms / BOMs.
        let mut constraints: HashMap<ModuleId, String> = self.managed.clone();
        for r in requests.iter().filter(|r| r.platform) {
            let version = self.select_version(&r.module, &r.version, &constraints);
            let Some(version) = version else {
                problems.push(format!("No version for platform {}", r.module));
                continue;
            };
            let d = self.descriptor(&r.module, &version);
            if d.missing {
                problems.push(format!("Could not find platform {}:{version}", r.module));
            }
            for (m, v) in &d.constraints {
                constraints.entry(m.clone()).or_insert_with(|| v.clone());
            }
        }

        let roots: Vec<&Request> = requests.iter().filter(|r| !r.platform).collect();
        let (selected, nodes) = match self.strategy {
            Strategy::Nearest => self.traverse(&roots, &constraints, &HashMap::new(), &mut problems),
            Strategy::Highest => {
                let mut forced: HashMap<ModuleId, String> = HashMap::new();
                let mut result = None;
                for _ in 0..8 {
                    let mut pass_problems = Vec::new();
                    let (requested, nodes) = self.traverse(&roots, &constraints, &forced, &mut pass_problems);
                    // requested: module -> all requested versions; pick the highest.
                    let next: HashMap<ModuleId, String> = requested;
                    let done = next == forced;
                    forced = next;
                    result = Some((forced.clone(), nodes, pass_problems));
                    if done {
                        break;
                    }
                }
                let (sel, nodes, p) = result.unwrap();
                problems.extend(p);
                (sel, nodes)
            }
        };

        // Fetch artifact files in parallel.
        let artifacts: Vec<ResolvedArtifact> = nodes
            .par_iter()
            .filter_map(|n| {
                let version = selected.get(&n.module).cloned().unwrap_or_else(|| n.version.clone());
                let desc = self.descriptor(&n.module, &version);
                if desc.no_files && n.classifier.is_none() {
                    return None; // e.g. kotlin-stdlib-common 2.x has no JVM artifact
                }
                // Report the coordinate that actually provides the files (`available-at` target).
                let mut coord = match &desc.artifact_coord {
                    Some(c) if n.classifier.is_none() => c.clone(),
                    _ => Coordinate::new(&n.module.group, &n.module.artifact, &version),
                };
                coord.classifier = n.classifier.clone();
                coord.ext = n.ext.clone();
                let file = self.artifact_file(&n.module, &version, n.classifier.as_deref(), &n.ext);
                Some(ResolvedArtifact { coord, scope: n.scope, file, direct: n.depth == 0, depth: n.depth, via: n.via.clone() })
            })
            .collect();
        for (a, n) in artifacts.iter().zip(nodes.iter().filter(|n| {
            let v = selected.get(&n.module).cloned().unwrap_or_else(|| n.version.clone());
            !(self.descriptor(&n.module, &v).no_files && n.classifier.is_none())
        })) {
            if a.file.is_none() && a.coord.ext != "pom" {
                let v = selected.get(&n.module).cloned().unwrap_or_else(|| n.version.clone());
                let d = self.descriptor(&n.module, &v);
                if !d.missing && d.packaging != "pom" && !d.no_files {
                    problems.push(format!("Artifact file not found: {}", a.coord));
                }
            }
        }
        problems.sort();
        problems.dedup();
        Resolution { artifacts, problems }
    }

    fn select_version(&self, module: &ModuleId, requested: &str, constraints: &HashMap<ModuleId, String>) -> Option<String> {
        let req = if requested.is_empty() {
            {
                let v = constraints.get(module)?;
                VersionReq::parse(v)
            }
        } else {
            VersionReq::parse(requested.trim_end_matches("!!"))
        };
        match &req {
            VersionReq::None => None,
            VersionReq::Exact(v) => Some(v.clone()),
            _ => {
                let available = self.loader.repo.list_versions(module, self.repos());
                req.select(available.iter().map(String::as_str)).or_else(|| req.fallback())
            }
        }
    }

    /// Breadth first traversal. Returns (selected versions, nodes in visit order).
    fn traverse(
        &self,
        roots: &[&Request],
        constraints: &HashMap<ModuleId, String>,
        forced: &HashMap<ModuleId, String>,
        problems: &mut Vec<String>,
    ) -> (HashMap<ModuleId, String>, Vec<Node>) {
        let mut queue: VecDeque<Node> = VecDeque::new();
        for r in roots {
            let version = if self.strategy == Strategy::Nearest {
                // Direct declarations win; management applies only when no version is given.
                self.select_version(&r.module, &r.version, constraints)
            } else {
                self.select_version(&r.module, &r.version, constraints)
            };
            let Some(version) = version else {
                problems.push(format!("No version specified for {}", r.module));
                continue;
            };
            queue.push_back(Node {
                module: r.module.clone(),
                version,
                scope: r.scope,
                classifier: r.classifier.clone(),
                ext: r.ext.clone(),
                exclusions: Arc::new(r.exclusions.clone()),
                depth: 0,
                via: None,
                transitive: r.transitive,
            });
        }

        let mut order: Vec<Node> = Vec::new();
        let mut index: HashMap<(ModuleId, Option<String>), usize> = HashMap::new();
        let mut requested: HashMap<ModuleId, String> = HashMap::new();
        let mut graph_constraints: HashMap<ModuleId, String> = HashMap::new();
        let strict: HashSet<ModuleId> = roots.iter().filter(|r| r.strict).map(|r| r.module.clone()).collect();

        while !queue.is_empty() {
            // Prefetch descriptors of this BFS level in parallel.
            let level: Vec<(ModuleId, String)> = queue
                .iter()
                .map(|n| (n.module.clone(), forced.get(&n.module).cloned().unwrap_or_else(|| n.version.clone())))
                .collect();
            level.par_iter().for_each(|(m, v)| {
                self.descriptor(m, v);
            });
            let mut next: VecDeque<Node> = VecDeque::new();
            while let Some(mut node) = queue.pop_front() {
                // Record the requested version for Gradle's conflict resolution.
                let entry = requested.entry(node.module.clone()).or_insert_with(|| node.version.clone());
                if !strict.contains(&node.module) && Version::parse(&node.version) > Version::parse(entry) {
                    *entry = node.version.clone();
                }
                if let Some(v) = constraints.get(&node.module)
                    && self.strategy == Strategy::Highest && Version::parse(v) > Version::parse(entry) && !strict.contains(&node.module) {
                        *entry = v.clone();
                    }
                if let Some(v) = forced.get(&node.module) {
                    node.version = v.clone();
                }
                let key = (node.module.clone(), node.classifier.clone());
                if let Some(&i) = index.get(&key) {
                    // Already selected (nearest wins). A broader scope widens the selected
                    // node and must be propagated to its subtree again.
                    if scope_strength(node.scope) <= scope_strength(order[i].scope) {
                        continue;
                    }
                    order[i].scope = node.scope;
                    node.version = order[i].version.clone();
                    node.exclusions = order[i].exclusions.clone();
                    node.depth = order[i].depth;
                } else {
                    index.insert(key, order.len());
                    order.push(node.clone());
                }
                if !node.transitive {
                    continue;
                }
                let desc = self.descriptor(&node.module, &node.version);
                if desc.missing {
                    problems.push(format!("Could not resolve {}:{}", node.module, node.version));
                    continue;
                }
                problems.extend(desc.problems.iter().cloned());
                if self.strategy == Strategy::Highest {
                    for (m, v) in &desc.graph_constraints {
                        let c = graph_constraints.entry(m.clone()).or_insert_with(|| v.clone());
                        if Version::parse(v) > Version::parse(c) {
                            *c = v.clone();
                        }
                    }
                }
                for edge in &desc.edges {
                    if edge.optional || excluded(&node.exclusions, &edge.module) {
                        continue;
                    }
                    let Some(scope) = node.scope.transitive(edge.scope) else { continue };
                    if edge.platform {
                        continue;
                    }
                    // Root-level management overrides transitive versions (Maven) /
                    // participates in conflict resolution (Gradle).
                    let version = match (self.strategy, self.managed.get(&edge.module)) {
                        (Strategy::Nearest, Some(v)) => v.clone(),
                        _ => {
                            let v = if edge.version.is_empty() { constraints.get(&edge.module).cloned().unwrap_or_default() } else { edge.version.clone() };
                            match self.select_version(&edge.module, &v, constraints) {
                                Some(v) => v,
                                None => {
                                    problems.push(format!("No version for {} (required by {})", edge.module, node.module));
                                    continue;
                                }
                            }
                        }
                    };
                    let mut exclusions = (*node.exclusions).clone();
                    exclusions.extend(edge.exclusions.iter().cloned());
                    next.push_back(Node {
                        module: edge.module.clone(),
                        version,
                        scope,
                        classifier: edge.classifier.clone(),
                        ext: edge.ext.clone(),
                        exclusions: Arc::new(exclusions),
                        depth: node.depth + 1,
                        via: Some(node.module.clone()),
                        transitive: true,
                    });
                }
            }
            queue = next;
        }

        let selected = match self.strategy {
            Strategy::Nearest => order.iter().map(|n| (n.module.clone(), n.version.clone())).collect(),
            Strategy::Highest => {
                // Constraints only matter for modules that are actually in the graph.
                for (m, v) in graph_constraints {
                    if let Some(cur) = requested.get_mut(&m)
                        && !strict.contains(&m) && Version::parse(&v) > Version::parse(cur) {
                            *cur = v;
                        }
                }
                requested
            }
        };
        (selected, order)
    }

    fn descriptor(&self, module: &ModuleId, version: &str) -> Arc<Descriptor> {
        let key = (module.clone(), version.to_string());
        if let Some(d) = self.descriptors.lock().get(&key) {
            return d.clone();
        }
        let d = Arc::new(self.load_descriptor(module, version, 0));
        self.descriptors.lock().insert(key, d.clone());
        d
    }

    fn load_descriptor(&self, module: &ModuleId, version: &str, depth: u32) -> Descriptor {
        if self.gradle_metadata && depth < 4 {
            let coord = Coordinate::new(&module.group, &module.artifact, version);
            if let Some(path) = self.loader.repo.artifact(&coord, None, "module", self.repos())
                && let Some(d) = std::fs::read_to_string(&path).ok().and_then(|t| self.gradle_metadata_descriptor(&coord, &t, depth)) {
                    return d;
                }
        }
        self.pom_descriptor(module, version)
    }

    fn pom_descriptor(&self, module: &ModuleId, version: &str) -> Descriptor {
        let Some(pom) = self.loader.load(&module.group, &module.artifact, version) else {
            return Descriptor { missing: true, ..Default::default() };
        };
        if let Some(reloc) = &pom.relocation
            && (reloc.module() != *module || reloc.version != version) {
                return Descriptor {
                    edges: vec![Edge {
                        module: reloc.module(),
                        version: reloc.version.clone(),
                        ext: "jar".into(),
                        ..Default::default()
                    }],
                    packaging: "pom".into(),
                    ..Default::default()
                };
            }
        let edges = pom
            .dependencies
            .iter()
            .map(|d| {
                let (ext, classifier) = d.artifact_kind();
                Edge {
                    module: d.module.clone(),
                    version: d.version.clone().unwrap_or_default(),
                    scope: d.scope.unwrap_or(Scope::Compile),
                    classifier,
                    ext,
                    optional: d.optional,
                    exclusions: d.exclusions.clone(),
                    platform: false,
                }
            })
            .collect();
        Descriptor {
            edges,
            constraints: pom.managed.iter().filter_map(|d| Some((d.module.clone(), d.version.clone()?))).collect(),
            packaging: pom.packaging.clone(),
            problems: pom.problems.clone(),
            ..Default::default()
        }
    }

    fn gradle_metadata_descriptor(&self, coord: &Coordinate, text: &str, depth: u32) -> Option<Descriptor> {
        let json: serde_json::Value = serde_json::from_str(text).ok()?;
        let variants = json.get("variants")?.as_array()?;
        let variant = select_variant(variants, self.platform)?;
        let attrs = variant.get("attributes");
        let category = attrs.and_then(|a| a.get("org.gradle.category")).and_then(|c| c.as_str()).unwrap_or("library");

        // Redirect to another module (e.g. kotlinx-coroutines-core -> kotlinx-coroutines-core-jvm).
        if let Some(at) = variant.get("available-at") {
            let g = at.get("group")?.as_str()?;
            let m = at.get("module")?.as_str()?;
            let v = at.get("version")?.as_str()?;
            let target = ModuleId::new(g, m);
            let mut d = self.load_descriptor(&target, v, depth + 1);
            if d.artifact_coord.is_none() {
                d.artifact_coord = Some(Coordinate::new(g, m, v));
            }
            return Some(d);
        }

        let parse_dep = |dep: &serde_json::Value| -> Option<Edge> {
            let group = dep.get("group")?.as_str()?;
            let module = dep.get("module")?.as_str()?;
            let v = dep.get("version");
            let strictly = v.and_then(|v| v.get("strictly")).and_then(|s| s.as_str());
            let version = strictly
                .or_else(|| v.and_then(|v| v.get("requires")).and_then(|s| s.as_str()))
                .or_else(|| v.and_then(|v| v.get("prefers")).and_then(|s| s.as_str()))
                .unwrap_or("")
                .to_string();
            let platform = dep
                .get("attributes")
                .and_then(|a| a.get("org.gradle.category"))
                .and_then(|c| c.as_str())
                .is_some_and(|c| c == "platform" || c == "enforced-platform");
            let exclusions = dep
                .get("excludes")
                .and_then(|e| e.as_array())
                .map(|ex| {
                    ex.iter()
                        .map(|e| {
                            ModuleId::new(
                                e.get("group").and_then(|g| g.as_str()).unwrap_or("*"),
                                e.get("module").and_then(|g| g.as_str()).unwrap_or("*"),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Edge {
                module: ModuleId::new(group, module),
                version,
                scope: Scope::Compile,
                classifier: None,
                ext: "jar".into(),
                optional: false,
                exclusions,
                platform,
            })
        };

        let edges: Vec<Edge> = variant.get("dependencies").and_then(|d| d.as_array()).map(|a| a.iter().filter_map(parse_dep).collect()).unwrap_or_default();
        let mut constraints: Vec<(ModuleId, String)> = variant
            .get("dependencyConstraints")
            .and_then(|d| d.as_array())
            .map(|a| a.iter().filter_map(parse_dep).filter(|e| !e.version.is_empty()).map(|e| (e.module, e.version)).collect())
            .unwrap_or_default();
        // Platforms depended upon contribute their constraints too.
        for e in edges.iter().filter(|e| e.platform) {
            if !e.version.is_empty() {
                let d = self.descriptor(&e.module, &e.version);
                constraints.extend(d.constraints.iter().cloned());
            }
        }
        let base = coord.repo_path(&coord.version, None, "module");
        let dir = &base[..base.rfind('/').unwrap() + 1];
        let files: Vec<String> = variant
            .get("files")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|f| f.get("url")?.as_str()).map(|u| normalize_rel(&format!("{dir}{u}"))).collect())
            .unwrap_or_default();
        let no_files = files.is_empty() && category != "platform";
        Some(Descriptor {
            edges,
            no_files,
            graph_constraints: constraints.clone(),
            constraints,
            files,
            artifact_coord: Some(coord.clone()),
            packaging: if category == "platform" { "pom".into() } else { "jar".into() },
            ..Default::default()
        })
    }

    /// Locate the binary for a selected module version.
    fn artifact_file(&self, module: &ModuleId, version: &str, classifier: Option<&str>, ext: &str) -> Option<PathBuf> {
        if ext == "pom" {
            return None;
        }
        let desc = self.descriptor(module, version);
        if (desc.packaging == "pom" || desc.no_files) && classifier.is_none() {
            return None;
        }
        let repos = self.repos();
        if classifier.is_none() {
            for rel in &desc.files {
                let file_name = rel.rsplit('/').next().unwrap_or(rel).to_string();
                let gradle_coord = desc.artifact_coord.clone().unwrap_or_else(|| Coordinate::new(&module.group, &module.artifact, version));
                if let Some(p) = self.loader.repo.file(rel, Some((&gradle_coord, &[file_name])), repos) {
                    return self.unpack(p);
                }
            }
        }
        let coord = desc.artifact_coord.clone().unwrap_or_else(|| Coordinate::new(&module.group, &module.artifact, version));
        let ext = if desc.packaging == "aar" && ext == "jar" { "aar" } else { ext };
        if let Some(p) = self.loader.repo.artifact(&coord, classifier, ext, repos) {
            return self.unpack(p);
        }
        // Some POMs declare `bundle`/`aar` packaging inconsistently: try the other one.
        let alt = if ext == "jar" { "aar" } else { "jar" };
        self.loader.repo.artifact(&coord, classifier, alt, repos).and_then(|p| self.unpack(p))
    }

    /// Android archives: extract `classes.jar` next to our cache.
    fn unpack(&self, path: PathBuf) -> Option<PathBuf> {
        if path.extension().is_some_and(|e| e == "aar") {
            return extract_aar_classes(&path, &self.loader.repo.config.cache_dir);
        }
        Some(path)
    }
}

fn normalize_rel(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Pick the best variant for compiling JVM code.
fn select_variant(variants: &[serde_json::Value], platform: TargetPlatform) -> Option<&serde_json::Value> {
    variants
        .iter()
        .filter_map(|v| {
            let attrs = v.get("attributes")?.as_object()?;
            let get = |k: &str| attrs.get(k).and_then(|x| x.as_str());
            let category = get("org.gradle.category").unwrap_or("library");
            if category == "documentation" || category == "verification" {
                return None;
            }
            let mut score: i32 = 0;
            match get("org.gradle.usage") {
                Some("java-api") => score += 30,
                Some("java-runtime") => score += 20,
                Some("kotlin-api") => score += 10,
                Some("kotlin-runtime") => score += 5,
                Some(_) => return None,
                None => {}
            }
            match get("org.jetbrains.kotlin.platform.type") {
                Some("jvm") => score += if platform == TargetPlatform::Jvm { 100 } else { 80 },
                Some("androidJvm") => score += if platform == TargetPlatform::Android { 100 } else { 60 },
                Some("common") => score += 1,
                Some(_) => return None,
                None => score += 50,
            }
            match get("org.gradle.jvm.environment") {
                Some("standard-jvm") if platform == TargetPlatform::Jvm => score += 5,
                Some("android") if platform == TargetPlatform::Jvm => score -= 5,
                _ => {}
            }
            if get("org.gradle.libraryelements").is_some_and(|e| e == "jar" || e == "classes") {
                score += 1;
            }
            if category == "platform" {
                score += 1;
            } else if category == "enforced-platform" {
                score -= 1;
            }
            // Variants providing an extra capability (kotlin-test's junit/testng flavours,
            // test fixtures...) are only chosen when explicitly requested.
            if v.get("capabilities").and_then(|c| c.as_array()).is_some_and(|c| !c.is_empty()) {
                score -= 1000;
            }
            Some((score, v))
        })
        .max_by_key(|(s, _)| *s)
        .map(|(_, v)| v)
}

fn extract_aar_classes(aar: &std::path::Path, cache: &std::path::Path) -> Option<PathBuf> {
    let stem = aar.file_stem()?.to_string_lossy().to_string();
    let hash = sha1_smol::Sha1::from(aar.display().to_string()).digest().to_string();
    let out = cache.join("aar").join(format!("{stem}-{}", &hash[..8])).join("classes.jar");
    if out.is_file() {
        return Some(out);
    }
    let file = std::fs::File::open(aar).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut entry = zip.by_name("classes.jar").ok()?;
    std::fs::create_dir_all(out.parent()?).ok()?;
    let tmp = out.with_extension("jar.part");
    let mut w = std::fs::File::create(&tmp).ok()?;
    std::io::copy(&mut entry, &mut w).ok()?;
    std::fs::rename(&tmp, &out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::repo::{RepoConfig, RepoSystem};
    use std::path::Path;

    fn pom(dir: &Path, g: &str, a: &str, v: &str, deps: &[(&str, &str, &str, &str)], extra: &str) {
        let mut xml = format!("<project><groupId>{g}</groupId><artifactId>{a}</artifactId><version>{v}</version>{extra}<dependencies>");
        for (dg, da, dv, scope) in deps {
            xml.push_str(&format!(
                "<dependency><groupId>{dg}</groupId><artifactId>{da}</artifactId><version>{dv}</version><scope>{scope}</scope></dependency>"
            ));
        }
        xml.push_str("</dependencies></project>");
        let base = dir.join("m2").join(g.replace('.', "/")).join(a).join(v);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join(format!("{a}-{v}.pom")), xml).unwrap();
        std::fs::write(base.join(format!("{a}-{v}.jar")), "jar").unwrap();
    }

    fn resolver(dir: &Path, strategy: Strategy) -> Resolver {
        let config = RepoConfig {
            cache_dir: dir.join("cache"),
            maven_local: Some(dir.join("m2")),
            gradle_user_home: None,
            offline: true,
            mirrors: vec![],
            negative_ttl: std::time::Duration::from_secs(1),
        };
        let loader = Arc::new(PomLoader::new(Arc::new(RepoSystem::new(config, None)), vec![]));
        Resolver::new(loader, strategy)
    }

    fn versions(r: &Resolution) -> Vec<String> {
        let mut v: Vec<String> = r.artifacts.iter().map(|a| format!("{}:{}:{:?}", a.coord.artifact, a.coord.version, a.scope)).collect();
        v.sort();
        v
    }

    fn graph(dir: &Path) {
        // app -> a:1 -> c:1 ; app -> b:1 -> d:1 -> c:2 ; a:1 -> t:1 (test, not transitive) ; b -> r (runtime)
        pom(dir, "g", "a", "1", &[("g", "c", "1", "compile"), ("g", "t", "1", "test")], "");
        pom(dir, "g", "b", "1", &[("g", "d", "1", "compile"), ("g", "r", "1", "runtime")], "");
        pom(dir, "g", "d", "1", &[("g", "c", "2", "compile")], "");
        pom(dir, "g", "c", "1", &[], "");
        pom(dir, "g", "c", "2", &[], "");
        pom(dir, "g", "r", "1", &[], "");
        pom(dir, "g", "t", "1", &[], "");
    }

    #[test]
    fn maven_nearest_wins() {
        let tmp = tempfile::tempdir().unwrap();
        graph(tmp.path());
        let r = resolver(tmp.path(), Strategy::Nearest);
        let res = r.resolve(&[
            Request::new(ModuleId::new("g", "a"), "1", Scope::Compile),
            Request::new(ModuleId::new("g", "b"), "1", Scope::Compile),
        ]);
        assert!(res.problems.is_empty(), "{:?}", res.problems);
        assert_eq!(versions(&res), vec!["a:1:Compile", "b:1:Compile", "c:1:Compile", "d:1:Compile", "r:1:Runtime"]);
        assert!(res.artifacts.iter().all(|a| a.file.is_some()));
    }

    #[test]
    fn gradle_highest_wins() {
        let tmp = tempfile::tempdir().unwrap();
        graph(tmp.path());
        let r = resolver(tmp.path(), Strategy::Highest);
        let res = r.resolve(&[
            Request::new(ModuleId::new("g", "a"), "1", Scope::Compile),
            Request::new(ModuleId::new("g", "b"), "1", Scope::Compile),
        ]);
        assert!(res.problems.is_empty(), "{:?}", res.problems);
        assert_eq!(versions(&res), vec!["a:1:Compile", "b:1:Compile", "c:2:Compile", "d:1:Compile", "r:1:Runtime"]);
    }

    #[test]
    fn exclusions_and_management() {
        let tmp = tempfile::tempdir().unwrap();
        graph(tmp.path());
        let mut r = resolver(tmp.path(), Strategy::Nearest);
        r.managed.insert(ModuleId::new("g", "c"), "2".into());
        let mut req = Request::new(ModuleId::new("g", "b"), "1", Scope::Compile);
        req.exclusions.push(ModuleId::new("g", "r"));
        let res = r.resolve(&[Request::new(ModuleId::new("g", "a"), "1", Scope::Compile), req]);
        assert_eq!(versions(&res), vec!["a:1:Compile", "b:1:Compile", "c:2:Compile", "d:1:Compile"]);
    }

    #[test]
    fn gradle_module_metadata_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let root = d.join("m2/org/k/core/1.0");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("core-1.0.pom"), "<project><groupId>org.k</groupId><artifactId>core</artifactId><version>1.0</version></project>").unwrap();
        std::fs::write(
            root.join("core-1.0.module"),
            r#"{"formatVersion":"1.1","component":{"group":"org.k","module":"core","version":"1.0"},
            "variants":[
              {"name":"metadataApiElements","attributes":{"org.gradle.category":"library","org.gradle.usage":"kotlin-metadata","org.jetbrains.kotlin.platform.type":"common"}},
              {"name":"jvmApiElements-published","attributes":{"org.gradle.category":"library","org.gradle.usage":"java-api","org.jetbrains.kotlin.platform.type":"jvm"},
               "available-at":{"url":"../../core-jvm/1.0/core-jvm-1.0.module","group":"org.k","module":"core-jvm","version":"1.0"}},
              {"name":"jsApiElements-published","attributes":{"org.gradle.usage":"kotlin-api","org.jetbrains.kotlin.platform.type":"js"}}
            ]}"#,
        )
        .unwrap();
        let jvm = d.join("m2/org/k/core-jvm/1.0");
        std::fs::create_dir_all(&jvm).unwrap();
        std::fs::write(
            jvm.join("core-jvm-1.0.module"),
            r#"{"formatVersion":"1.1","variants":[
              {"name":"jvmApiElements","attributes":{"org.gradle.category":"library","org.gradle.usage":"java-api","org.jetbrains.kotlin.platform.type":"jvm"},
               "dependencies":[{"group":"g","module":"c","version":{"requires":"1"}}],
               "files":[{"name":"core-jvm-1.0.jar","url":"core-jvm-1.0.jar"}]},
              {"name":"jvmRuntimeElements","attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime","org.jetbrains.kotlin.platform.type":"jvm"},
               "dependencies":[{"group":"g","module":"r","version":{"requires":"1"}}]}
            ]}"#,
        )
        .unwrap();
        std::fs::write(jvm.join("core-jvm-1.0.jar"), "jar").unwrap();
        pom(d, "g", "c", "1", &[], "");
        pom(d, "g", "r", "1", &[], "");

        let r = resolver(d, Strategy::Highest);
        let res = r.resolve(&[Request::new(ModuleId::new("org.k", "core"), "1.0", Scope::Compile)]);
        assert!(res.problems.is_empty(), "{:?}", res.problems);
        // The artifact is reported under the module that provides the files.
        assert_eq!(versions(&res), vec!["c:1:Compile", "core-jvm:1.0:Compile"]);
        let core = res.artifacts.iter().find(|a| a.coord.artifact == "core-jvm").unwrap();
        assert!(core.file.as_ref().unwrap().ends_with("core-jvm-1.0.jar"), "{:?}", core.file);
    }
}
