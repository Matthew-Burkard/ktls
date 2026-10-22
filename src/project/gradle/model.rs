//! Static interpretation of Gradle settings and build scripts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::catalog::{self, Catalog};
use super::dsl::{self, Arg, Expr, Stmt, StrPart};
use crate::project::coords::{Coordinate, ModuleId};
use crate::project::repo::Repository;

// ----------------------------------------------------------------- settings

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub root_dir: PathBuf,
    pub root_name: String,
    /// `(path, dir)`, including the root project `:`.
    pub projects: Vec<(String, PathBuf)>,
    pub included_builds: Vec<PathBuf>,
    pub plugin_builds: Vec<PathBuf>,
    pub repositories: Vec<Repository>,
    pub plugin_repositories: Vec<Repository>,
    /// `pluginManagement { plugins { id("x") version "y" } }`
    pub plugin_versions: HashMap<String, String>,
    pub catalogs: HashMap<String, Catalog>,
    pub properties: HashMap<String, String>,
    pub files: Vec<PathBuf>,
    pub problems: Vec<String>,
}

pub fn settings_file(dir: &Path) -> Option<PathBuf> {
    ["settings.gradle.kts", "settings.gradle"].iter().map(|f| dir.join(f)).find(|p| p.is_file())
}

pub fn build_file(dir: &Path) -> Option<PathBuf> {
    ["build.gradle.kts", "build.gradle"].iter().map(|f| dir.join(f)).find(|p| p.is_file())
}

/// `gradle.properties` of the user home, the root and (optionally) a project dir.
pub fn read_properties(files: &mut Vec<PathBuf>, dirs: &[&Path]) -> HashMap<String, String> {
    let mut props = HashMap::new();
    let mut paths: Vec<PathBuf> = dirs.iter().map(|d| d.join("gradle.properties")).collect();
    if let Some(home) = std::env::var_os("GRADLE_USER_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".gradle"))) {
        paths.push(home.join("gradle.properties"));
    }
    for p in paths {
        if let Ok(text) = std::fs::read_to_string(&p) {
            files.push(p);
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.starts_with('!') {
                    continue;
                }
                if let Some((k, v)) = line.split_once(['=', ':']) {
                    props.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
                }
            }
        }
    }
    props
}

pub fn read_settings(root: &Path) -> Settings {
    let mut s = Settings { root_dir: root.to_path_buf(), ..Default::default() };
    s.properties = read_properties(&mut s.files, &[root]);
    s.root_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    s.projects.push((":".into(), root.to_path_buf()));

    // Default catalog.
    let default_catalog = root.join("gradle/libs.versions.toml");
    if default_catalog.is_file() {
        load_catalog(&mut s, "libs", &default_catalog);
    }

    let Some(file) = settings_file(root) else { return s };
    s.files.push(file.clone());
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let stmts = dsl::parse(&text);
    let mut ev = Evaluator::new(root, root, s.properties.clone());
    settings_stmts(&mut s, &mut ev, &stmts, SettingsCtx::Top);
    // Ensure parents of nested includes exist (`:a:b` implies `:a`).
    let paths: Vec<String> = s.projects.iter().map(|(p, _)| p.clone()).collect();
    for p in paths {
        let mut parent = p.as_str();
        while let Some(i) = parent.rfind(':') {
            parent = &parent[..i];
            if parent.is_empty() {
                break;
            }
            if !s.projects.iter().any(|(q, _)| q == parent) {
                let dir = root.join(parent.trim_start_matches(':').replace(':', "/"));
                s.projects.push((parent.to_string(), dir));
            }
        }
    }
    s
}

fn load_catalog(s: &mut Settings, name: &str, path: &Path) {
    match std::fs::read_to_string(path).map_err(anyhow::Error::from).and_then(|t| Catalog::parse(&t)) {
        Ok(c) => {
            s.catalogs.insert(name.to_string(), c);
            s.files.push(path.to_path_buf());
        }
        Err(e) => s.problems.push(format!("{}: {e}", path.display())),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsCtx {
    Top,
    PluginManagement,
    Plugins,
    DependencyResolution,
    Catalogs,
}

fn settings_stmts(s: &mut Settings, ev: &mut Evaluator, stmts: &[Stmt], ctx: SettingsCtx) {
    for stmt in stmts {
        match stmt {
            Stmt::Var { name, value: Some(v), .. } => {
                if let Some(val) = ev.string(v) {
                    ev.vars.insert(name.clone(), val);
                }
            }
            Stmt::Assign { target, value, .. } => {
                let path = target.path().unwrap_or_default();
                if path == "rootProject.name" {
                    if let Some(n) = ev.string(value) {
                        s.root_name = n;
                    }
                } else if let Expr::Member(obj, field) = target {
                    // project(":x").projectDir = file("dir")
                    if field == "name"
                        && let Expr::Call { target: callee, args, .. } = &**obj
                            && callee.path().as_deref() == Some("project")
                                && let (Some(p), Some(new_name)) = (args.first().and_then(|a| ev.string(&a.value)), ev.string(value))
                                    && let Some(entry) = s.projects.iter_mut().find(|(q, _)| *q == p) {
                                        let parent = p.rsplit_once(':').map(|(a, _)| a).unwrap_or("");
                                        entry.0 = format!("{parent}:{new_name}");
                                    }
                    if field == "projectDir"
                        && let Expr::Call { target: callee, args, .. } = &**obj
                            && callee.path().as_deref() == Some("project")
                                && let (Some(p), Some(dir)) = (args.first().and_then(|a| ev.string(&a.value)), ev.file(value))
                                    && let Some(entry) = s.projects.iter_mut().find(|(q, _)| *q == p) {
                                        entry.1 = dir;
                                    }
                }
            }
            Stmt::If { then, otherwise, .. } => {
                settings_stmts(s, ev, then, ctx);
                settings_stmts(s, ev, otherwise, ctx);
            }
            Stmt::Expr(e) => settings_expr(s, ev, e, ctx),
            _ => {}
        }
    }
}

fn settings_expr(s: &mut Settings, ev: &mut Evaluator, e: &Expr, ctx: SettingsCtx) {
    // `id("x") version "y"` inside pluginManagement.plugins
    if let Expr::Infix(lhs, op, rhs) = e {
        if ctx == SettingsCtx::Plugins && op == "version" {
            if let Some((id, _, _)) = plugin_request(ev, lhs, &s.catalogs)
                && let Some(v) = ev.string(rhs) {
                    s.plugin_versions.insert(id, v);
                }
            return;
        }
        if op == "apply" {
            settings_expr(s, ev, lhs, ctx);
        }
        return;
    }
    let Expr::Call { target, args, closure } = e else { return };
    let name = target.path().unwrap_or_default();
    let body = closure.as_deref().unwrap_or(&[]);
    match (ctx, name.as_str()) {
        (_, "include") | (_, "includeFlat") => {
            for a in args {
                let values = match &a.value {
                    Expr::List(items) => items.iter().filter_map(|i| ev.string(i)).collect(),
                    v => ev.string(v).into_iter().collect::<Vec<_>>(),
                };
                for v in values {
                    let path = if v.starts_with(':') { v.clone() } else { format!(":{v}") };
                    let dir = if name == "includeFlat" {
                        s.root_dir.parent().unwrap_or(&s.root_dir).join(path.trim_start_matches(':'))
                    } else {
                        s.root_dir.join(path.trim_start_matches(':').replace(':', "/"))
                    };
                    if !s.projects.iter().any(|(p, _)| *p == path) {
                        s.projects.push((path, dir));
                    }
                }
            }
        }
        (SettingsCtx::Top, "includeBuild") => {
            if let Some(dir) = args.first().and_then(|a| ev.file(&a.value)) {
                s.included_builds.push(dir);
            }
        }
        (SettingsCtx::PluginManagement, "includeBuild") => {
            if let Some(dir) = args.first().and_then(|a| ev.file(&a.value)) {
                s.plugin_builds.push(dir);
            }
        }
        (SettingsCtx::Top, "pluginManagement") => settings_stmts(s, ev, body, SettingsCtx::PluginManagement),
        (SettingsCtx::PluginManagement, "plugins") => settings_stmts(s, ev, body, SettingsCtx::Plugins),
        (SettingsCtx::PluginManagement, "repositories") => {
            let repos = repositories(ev, body);
            s.plugin_repositories.extend(repos);
        }
        (SettingsCtx::Top, "dependencyResolutionManagement") => settings_stmts(s, ev, body, SettingsCtx::DependencyResolution),
        (SettingsCtx::DependencyResolution, "repositories") => {
            let repos = repositories(ev, body);
            s.repositories.extend(repos);
        }
        (SettingsCtx::DependencyResolution, "versionCatalogs") => settings_stmts(s, ev, body, SettingsCtx::Catalogs),
        (SettingsCtx::Catalogs, "create") | (SettingsCtx::Catalogs, "register") => {
            let Some(cat_name) = args.first().and_then(|a| ev.string(&a.value)) else { return };
            for st in body {
                if let Stmt::Expr(Expr::Call { target, args, .. }) = st
                    && target.path().as_deref() == Some("from") {
                        // from(files("x.toml")) / from("g:a:v") (published catalogs are not supported)
                        if let Some(Arg { value: Expr::Call { args: inner, .. }, .. }) = args.first() {
                            if let Some(file) = inner.first().and_then(|a| ev.file(&a.value)) {
                                load_catalog(s, &cat_name, &file);
                            }
                        } else {
                            s.problems.push(format!("published version catalog '{cat_name}' is not supported"));
                        }
                    }
            }
        }
        (_, "plugins") if ctx == SettingsCtx::Top => {}
        _ => {}
    }
}

// --------------------------------------------------------- build scripts

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notation {
    Module { module: ModuleId, version: String, classifier: Option<String>, ext: String },
    Project(String),
    Platform { inner: Box<Notation>, enforced: bool },
    Files(Vec<PathBuf>),
    Gradle(String),
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredDep {
    pub configuration: String,
    pub notation: Notation,
    pub exclusions: Vec<ModuleId>,
    pub transitive: bool,
    pub strict: bool,
    /// Declared inside a `constraints { }` block.
    pub constraint: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Plugin {
    pub id: String,
    pub version: Option<String>,
    pub apply: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptModel {
    pub plugins: Vec<Plugin>,
    pub repositories: Vec<Repository>,
    pub deps: Vec<DeclaredDep>,
    pub buildscript_classpath: Vec<Notation>,
    pub group: Option<String>,
    pub version: Option<String>,
    pub jvm_toolchain: Option<String>,
    pub jvm_target: Option<String>,
    pub language_version: Option<String>,
    pub api_version: Option<String>,
    pub opt_in: Vec<String>,
    pub explicit_api: bool,
    /// Extra source directories per source set.
    pub source_dirs: Vec<(String, PathBuf)>,
    /// Source sets whose default directories were replaced (`setSrcDirs`, `srcDirs = ...`).
    pub replaced_source_dirs: Vec<String>,
    /// Source sets declared explicitly (KMP `val x by creating`, java `create("x")`).
    pub source_sets: Vec<String>,
    pub kmp_targets: Vec<String>,
    pub android_compile_sdk: Option<String>,
    pub files: Vec<PathBuf>,
    pub problems: Vec<String>,
}

impl ScriptModel {
    pub fn has_plugin(&self, id: &str) -> bool {
        self.plugins.iter().any(|p| p.apply && p.id == id)
    }

    pub fn has_plugin_prefix(&self, prefix: &str) -> bool {
        self.plugins.iter().any(|p| p.apply && p.id.starts_with(prefix))
    }
}

/// Expression evaluator with variable scope.
#[derive(Clone)]
pub struct Evaluator {
    pub vars: HashMap<String, String>,
    pub script_dir: PathBuf,
    pub project_dir: PathBuf,
    pub catalogs: HashMap<String, Catalog>,
    pub group: Option<String>,
    pub version: Option<String>,
    pub root_name: String,
}

impl Evaluator {
    pub fn new(script_dir: &Path, project_dir: &Path, vars: HashMap<String, String>) -> Self {
        Evaluator {
            vars,
            script_dir: script_dir.to_path_buf(),
            project_dir: project_dir.to_path_buf(),
            catalogs: HashMap::new(),
            group: None,
            version: None,
            root_name: String::new(),
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        let mut key = name;
        for prefix in ["project.", "rootProject.", "ext.", "extra.", "properties.", "project.ext.", "rootProject.ext.", "project.extra.", "rootProject.extra."] {
            if let Some(rest) = key.strip_prefix(prefix) {
                key = rest;
            }
        }
        for prefix in ["ext.", "extra."] {
            if let Some(rest) = key.strip_prefix(prefix) {
                key = rest;
            }
        }
        match key {
            "version" if self.version.is_some() => return self.version.clone(),
            "group" if self.group.is_some() => return self.group.clone(),
            "name" if name.starts_with("rootProject") => return Some(self.root_name.clone()),
            "rootDir" | "projectDir" => return Some(self.project_dir.display().to_string()),
            _ => {}
        }
        if let Some(v) = self.vars.get(key) {
            return Some(v.clone());
        }
        // catalog versions: libs.versions.kotlin(.get())
        let key = key.strip_suffix(".get").unwrap_or(key);
        if let Some((cat, rest)) = key.split_once('.')
            && let Some(c) = self.catalogs.get(cat)
                && let Some(v) = rest.strip_prefix("versions.") {
                    return c.version(v).cloned();
                }
        // Enum-like constants
        if let Some(v) = key.strip_prefix("JavaVersion.VERSION_") {
            return Some(v.replace('_', ".").trim_start_matches("1.").to_string());
        }
        if let Some(v) = key.strip_prefix("JvmTarget.JVM_") {
            return Some(v.replace('_', "."));
        }
        if let Some(v) = key.strip_prefix("KotlinVersion.KOTLIN_") {
            return Some(v.replace('_', "."));
        }
        None
    }

    /// Evaluate an expression to a string, if statically known.
    pub fn string(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Str(parts) => {
                let mut out = String::new();
                for p in parts {
                    match p {
                        StrPart::Lit(l) => out.push_str(l),
                        StrPart::Ref(r) => out.push_str(&self.lookup(r)?),
                        StrPart::Expr(e) => out.push_str(&self.string(e)?),
                    }
                }
                Some(out)
            }
            Expr::Num(n) => Some(n.trim_end_matches(['L', 'f']).to_string()),
            Expr::Bool(b) => Some(b.to_string()),
            Expr::Ident(_) | Expr::Member(..) => self.lookup(&e.path()?),
            Expr::Infix(l, op, r) => match op.as_str() {
                "+" => Some(format!("{}{}", self.string(l)?, self.string(r)?)),
                "?:" => self.string(l).or_else(|| self.string(r)),
                _ => None,
            },
            Expr::Index(target, key) => {
                let k = self.string(key)?;
                let base = target.path().unwrap_or_default();
                if base.ends_with("extra") || base.ends_with("ext") || base.ends_with("properties") || base == "project" {
                    self.lookup(&k)
                } else {
                    None
                }
            }
            Expr::Call { target, args, .. } => {
                let name = target.path().unwrap_or_default();
                let first = || args.first().and_then(|a| self.string(&a.value));
                match name.rsplit('.').next().unwrap_or("") {
                    "get" | "getOrNull" | "orNull" => {
                        // providers.gradleProperty("x").get(), libs.versions.x.get(), extra.get("x")
                        if let Expr::Member(inner, _) = &**target
                            && let Some(v) = self.string(inner) {
                                return Some(v);
                            }
                        first().and_then(|k| self.lookup(&k))
                    }
                    "getOrElse" => {
                        if let Expr::Member(inner, _) = &**target {
                            return self.string(inner).or_else(first);
                        }
                        None
                    }
                    "property" | "findProperty" | "gradleProperty" | "getProperty" => first().and_then(|k| self.lookup(&k)),
                    "getenv" => first().and_then(|k| std::env::var(k).ok()),
                    "uri" | "file" | "of" | "toString" | "trim" => {
                        if (name.ends_with("toString") || name.ends_with("trim"))
                            && let Expr::Member(inner, _) = &**target {
                                return self.string(inner);
                            }
                        first()
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// A path argument (`file("x")`, `"x"`, `File(rootDir, "x")`) relative to the script dir.
    pub fn file(&self, e: &Expr) -> Option<PathBuf> {
        if let Expr::Call { target, args, .. } = e
            && target.path().as_deref() == Some("File") && args.len() == 2 {
                let base = self.string(&args[0].value)?;
                let rel = self.string(&args[1].value)?;
                return Some(PathBuf::from(base).join(rel));
            }
        let s = self.string(e)?;
        let p = PathBuf::from(&s);
        Some(if p.is_absolute() { p } else { normalize(&self.script_dir.join(p)) })
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Plugin id (+version, apply) from a `plugins {}` entry.
fn plugin_request(ev: &Evaluator, e: &Expr, catalogs: &HashMap<String, Catalog>) -> Option<(String, Option<String>, bool)> {
    match e {
        Expr::Infix(lhs, op, rhs) => {
            let (id, mut version, mut apply) = plugin_request(ev, lhs, catalogs)?;
            match op.as_str() {
                "version" => version = ev.string(rhs),
                "apply" => apply = !matches!(**rhs, Expr::Bool(false)),
                _ => {}
            }
            Some((id, version, apply))
        }
        Expr::Call { target, args, .. } => {
            let name = target.path()?;
            let first = args.first().map(|a| &a.value);
            match name.as_str() {
                "id" => Some((ev.string(first?)?, None, true)),
                "kotlin" => Some((format!("org.jetbrains.kotlin.{}", ev.string(first?)?), None, true)),
                "alias" => {
                    let path = first?.path()?;
                    let (cat, rest) = path.split_once('.')?;
                    let rest = rest.strip_prefix("plugins.")?;
                    let (id, v) = catalogs.get(cat)?.plugin(rest)?;
                    Some((id.clone(), v.clone(), true))
                }
                _ => None,
            }
        }
        Expr::Ident(id) => Some((id.clone(), None, true)),
        Expr::Member(..) => {
            // `java`, `application`, `id.x` style — rare; treat path as id
            Some((e.path()?, None, true))
        }
        _ => None,
    }
}

pub fn repositories(ev: &Evaluator, stmts: &[Stmt]) -> Vec<Repository> {
    let mut out = Vec::new();
    for st in stmts {
        let e = match st {
            Stmt::Expr(e) => e,
            _ => continue,
        };
        let Expr::Call { target, args, closure } = e else { continue };
        let name = target.path().unwrap_or_default();
        match name.as_str() {
            "mavenCentral" => out.push(Repository::maven_central()),
            "google" => out.push(Repository::google()),
            "gradlePluginPortal" => out.push(Repository::gradle_plugin_portal()),
            "jcenter" => out.push(Repository::new("jcenter", "https://jcenter.bintray.com/")),
            "mavenLocal" => {
                if let Some(h) = dirs::home_dir() {
                    out.push(Repository::new("mavenLocal", &format!("file://{}", h.join(".m2/repository").display())));
                }
            }
            "maven" | "ivy" if name == "maven" => {
                let mut url = args.iter().find(|a| a.name.is_none() || a.name.as_deref() == Some("url")).and_then(|a| ev.string(&a.value));
                if let Some(body) = closure {
                    for s in body {
                        match s {
                            Stmt::Assign { target, value, .. } if target.path().as_deref() == Some("url") => url = ev.string(value),
                            Stmt::Expr(Expr::Call { target, args, .. })
                                if matches!(target.path().as_deref(), Some("url") | Some("setUrl")) =>
                            {
                                url = args.first().and_then(|a| ev.string(&a.value));
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(u) = url {
                    let u = if u.starts_with("http") || u.starts_with("file:") { u } else { format!("file://{}", ev.file(&Expr::Str(vec![StrPart::Lit(u)])).unwrap().display()) };
                    out.push(Repository::new(u.clone(), &u));
                }
            }
            "exclusiveContent" | "forRepository" | "forRepositories" => {
                if let Some(body) = closure {
                    out.extend(repositories(ev, body));
                }
                for a in args {
                    if let Expr::Closure(b) = &a.value {
                        out.extend(repositories(ev, b));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, PartialEq, Debug)]
enum Ctx {
    Top,
    Plugins,
    Buildscript,
    BuildscriptDeps,
    Dependencies { source_set: Option<String>, constraint: bool },
    Kotlin,
    KotlinSourceSets,
    KotlinSourceSet(String),
    Java,
    SourceSets,
    SourceSet(String),
    Ext,
    Android,
    CompilerOptions,
}

/// Interprets one project's scripts.
pub struct ScriptInterpreter<'s> {
    pub settings: &'s Settings,
    pub ev: Evaluator,
    pub model: ScriptModel,
    /// plugin id → precompiled convention script
    pub conventions: &'s HashMap<String, PathBuf>,
    applied: HashSet<PathBuf>,
    /// KMP: local variable name → source set name (`val jvmMain by getting`)
    source_set_vars: HashMap<String, String>,
    /// Closures of `allprojects {}` / `subprojects {}` found in this script.
    pub allprojects: Vec<Vec<Stmt>>,
    pub subprojects: Vec<Vec<Stmt>>,
    /// `project(":x") { }` blocks: path → closure.
    pub project_blocks: Vec<(String, Vec<Stmt>)>,
    /// `pluginManager.withPlugin(id) { }` / `plugins.withId(id) { }` waiting for `id`.
    deferred: Vec<(String, Vec<Stmt>)>,
}

impl<'s> ScriptInterpreter<'s> {
    pub fn new(settings: &'s Settings, project_dir: &Path, vars: HashMap<String, String>, conventions: &'s HashMap<String, PathBuf>) -> Self {
        let mut ev = Evaluator::new(project_dir, project_dir, vars);
        ev.catalogs = settings.catalogs.clone();
        ev.root_name = settings.root_name.clone();
        ScriptInterpreter {
            settings,
            ev,
            model: ScriptModel::default(),
            conventions,
            applied: HashSet::new(),
            source_set_vars: HashMap::new(),
            allprojects: Vec::new(),
            subprojects: Vec::new(),
            project_blocks: Vec::new(),
            deferred: Vec::new(),
        }
    }

    pub fn run_file(&mut self, path: &Path) {
        if !self.applied.insert(path.to_path_buf()) {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            self.model.problems.push(format!("cannot read {}", path.display()));
            return;
        };
        self.model.files.push(path.to_path_buf());
        let old_dir = std::mem::replace(&mut self.ev.script_dir, path.parent().unwrap_or(Path::new(".")).to_path_buf());
        let stmts = dsl::parse(&text);
        self.stmts(&stmts, &Ctx::Top);
        self.ev.script_dir = old_dir;
    }

    pub fn run_stmts(&mut self, stmts: &[Stmt]) {
        self.stmts(stmts, &Ctx::Top);
    }

    fn stmts(&mut self, stmts: &[Stmt], ctx: &Ctx) {
        for s in stmts {
            self.stmt(s, ctx);
        }
    }

    fn stmt(&mut self, s: &Stmt, ctx: &Ctx) {
        match s {
            Stmt::Var { name, value, delegate } => {
                if let Some(v) = value.as_ref().and_then(|v| self.ev.string(v)) {
                    self.ev.vars.insert(name.clone(), v);
                }
                if let Some(d) = delegate {
                    match d {
                        // val x: String by project / by extra / by rootProject.extra
                        Expr::Ident(_) | Expr::Member(..) => {}
                        Expr::Call { target, args, closure } => {
                            let callee = target.path().unwrap_or_default();
                            if callee.ends_with("extra") {
                                if let Some(v) = args.first().and_then(|a| self.ev.string(&a.value)) {
                                    self.ev.vars.insert(name.clone(), v);
                                }
                            } else if matches!(callee.as_str(), "getting" | "creating" | "registering" | "existing") {
                                // KMP / java source sets
                                let ss = name.clone();
                                self.source_set_vars.insert(name.clone(), ss.clone());
                                if callee == "creating" || callee == "registering" {
                                    self.model.source_sets.push(ss.clone());
                                }
                                if let Some(body) = closure {
                                    self.stmts(body, &Ctx::KotlinSourceSet(ss));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Stmt::Assign { target, value, .. } => self.assign(target, value, ctx),
            Stmt::If { then, otherwise, .. } => {
                self.stmts(then, ctx);
                self.stmts(otherwise, ctx);
            }
            Stmt::Expr(e) => self.expr(e, ctx),
        }
    }

    fn assign(&mut self, target: &Expr, value: &Expr, ctx: &Ctx) {
        let path = target.path().unwrap_or_default();
        let v = self.ev.string(value);
        match (ctx, path.as_str()) {
            (_, "group") | (_, "project.group") => {
                self.model.group = v.clone();
                self.ev.group = v;
            }
            (_, "version") | (_, "project.version") => {
                self.model.version = v.clone();
                self.ev.version = v;
            }
            (Ctx::Ext, key) => {
                if let Some(v) = v {
                    self.ev.vars.insert(key.to_string(), v);
                }
            }
            (_, p) if p.starts_with("ext.") || p.starts_with("project.ext.") || p.starts_with("rootProject.ext.") => {
                if let Some(v) = v {
                    self.ev.vars.insert(p.rsplit('.').next().unwrap().to_string(), v);
                }
            }
            (_, "sourceCompatibility") | (_, "targetCompatibility") | (_, "java.sourceCompatibility") => {
                if self.model.jvm_toolchain.is_none() {
                    self.model.jvm_toolchain = v;
                }
            }
            (_, p) if p.ends_with("jvmTarget") => self.model.jvm_target = v,
            (_, p) if p.ends_with("languageVersion") && *ctx != Ctx::Java => self.model.language_version = v,
            (_, p) if p.ends_with("apiVersion") => self.model.api_version = v,
            (Ctx::Android, "compileSdk") | (Ctx::Android, "compileSdkVersion") => self.model.android_compile_sdk = v,
            (Ctx::Java, p) if p.ends_with("languageVersion") => self.model.jvm_toolchain = v,
            (Ctx::SourceSet(ss), p) | (Ctx::KotlinSourceSet(ss), p) if p.ends_with("srcDirs") => {
                let ss = ss.clone();
                self.set_src_dirs(&ss, value)
            }
            _ => {
                if let Expr::Index(t, key) = target {
                    let base = t.path().unwrap_or_default();
                    if (base.ends_with("extra") || base.ends_with("ext"))
                        && let (Some(k), Some(v)) = (self.ev.string(key), v) {
                            self.ev.vars.insert(k, v);
                        }
                }
            }
        }
    }

    fn set_src_dirs(&mut self, ss: &str, value: &Expr) {
        self.model.source_dirs.retain(|(s, _)| s != ss);
        if !self.model.replaced_source_dirs.iter().any(|s| s == ss) {
            self.model.replaced_source_dirs.push(ss.to_string());
        }
        self.add_src_dirs(ss, value);
    }

    /// Known source set names so far (for `sourceSets.all { }` templates).
    fn known_source_sets(&self, kotlin: bool) -> Vec<String> {
        let mut out: Vec<String> = if kotlin { vec!["commonMain".into(), "commonTest".into()] } else { vec!["main".into(), "test".into()] };
        for t in &self.model.kmp_targets {
            let base = if t == "androidTarget" { "android" } else { t.as_str() };
            out.push(format!("{base}Main"));
            out.push(format!("{base}Test"));
        }
        out.extend(self.model.source_sets.iter().cloned());
        out.extend(self.source_set_vars.values().cloned());
        out.sort();
        out.dedup();
        out
    }

    fn add_src_dirs(&mut self, ss: &str, value: &Expr) {
        fn items(e: &Expr) -> Vec<&Expr> {
            match e {
                Expr::List(items) => items.iter().collect(),
                // srcDirs(...), listOf(...), files(...)
                Expr::Call { args, .. } => args.iter().flat_map(|a| items(&a.value)).collect(),
                other => vec![other],
            }
        }
        let items = items(value);
        for i in items {
            if let Some(p) = self.ev.file(i) {
                self.model.source_dirs.push((ss.to_string(), p));
            }
        }
    }

    fn expr(&mut self, e: &Expr, ctx: &Ctx) {
        match ctx {
            Ctx::Plugins => {
                if let Some((id, version, apply)) = plugin_request(&self.ev, e, &self.settings.catalogs) {
                    let version = version.or_else(|| self.settings.plugin_versions.get(&id).cloned());
                    self.add_plugin(id, version, apply);
                }
                return;
            }
            Ctx::Dependencies { source_set, constraint } => {
                self.dependency(e, source_set.as_deref(), *constraint);
                return;
            }
            Ctx::BuildscriptDeps => {
                if let Expr::Call { target, args, .. } = e
                    && target.path().as_deref() == Some("classpath")
                        && let Some(n) = args.first().and_then(|a| self.notation(&a.value, args)) {
                            self.model.buildscript_classpath.push(n);
                        }
                return;
            }
            _ => {}
        }
        let Expr::Call { target, args, closure } = e else {
            if let Expr::Infix(l, _, _) = e {
                self.expr(l, ctx);
            }
            return;
        };
        let name = target.path().unwrap_or_default();
        let body: &[Stmt] = closure.as_deref().unwrap_or(&[]);
        let first_str = || args.first().and_then(|a| self.ev.string(&a.value));
        // Kotlin compile task configuration: `tasks.withType<KotlinCompile>().configureEach { }`,
        // `tasks.compileTestKotlin { }`, `tasks.named("compileKotlin") { }`.
        if *ctx == Ctx::Top && closure.is_some() && (name.starts_with("tasks.") || name.is_empty() && chain_root(target).as_deref() == Some("tasks")) {
            return self.compiler_options(&name, args, body);
        }

        // Resource directories are not source roots.
        if name.split('.').any(|p| p == "resources" || p == "res" || p == "assets") {
            return;
        }
        // Source-set receivers: `jvmMain.dependencies { }`, `sourceSets["main"].java.srcDir(...)`
        if let Some((recv, method)) = name.rsplit_once('.') {
            let ss_name = self.source_set_vars.get(recv).cloned().or_else(|| match ctx {
                Ctx::KotlinSourceSets | Ctx::SourceSets => Some(recv.to_string()),
                _ => recv.strip_prefix("sourceSets.").or_else(|| recv.strip_prefix("kotlin.sourceSets.")).map(str::to_string),
            });
            if let Some(ss) = ss_name.filter(|s| !s.contains('.')) {
                match method {
                    "dependencies" => return self.stmts(body, &Ctx::Dependencies { source_set: Some(ss), constraint: false }),
                    "srcDir" | "srcDirs" | "setSrcDirs" => return self.add_src_dirs(&ss, e),
                    _ => {}
                }
            }
            if method == "setSrcDirs" {
                let ss = match ctx {
                    Ctx::SourceSet(ss) | Ctx::KotlinSourceSet(ss) => Some(ss.clone()),
                    _ => None,
                };
                if let Some(ss) = ss {
                    return self.set_src_dirs(&ss, e);
                }
            }
            if method == "srcDir" || method == "srcDirs" {
                // sourceSets.main.java.srcDir("x") / kotlin.srcDir("x") inside a source set block
                let ss = match ctx {
                    Ctx::SourceSet(ss) | Ctx::KotlinSourceSet(ss) => Some(ss.clone()),
                    _ => recv.split('.').find(|p| *p != "sourceSets" && *p != "java" && *p != "kotlin").map(str::to_string),
                };
                if let Some(ss) = ss {
                    return self.add_src_dirs(&ss, e);
                }
            }
        }

        match (ctx, name.as_str()) {
            (Ctx::Kotlin, "sourceSets.all") | (Ctx::Kotlin, "sourceSets.configureEach") | (Ctx::Top, "kotlin.sourceSets.all") => {
                let call = Expr::Call { target: Box::new(Expr::Ident("all".into())), args: Vec::new(), closure: closure.clone() };
                self.expr(&call, &Ctx::KotlinSourceSets)
            }
            (Ctx::Top, "sourceSets.all") | (Ctx::Top, "sourceSets.configureEach") => {
                let call = Expr::Call { target: Box::new(Expr::Ident("all".into())), args: Vec::new(), closure: closure.clone() };
                self.expr(&call, &Ctx::SourceSets)
            }
            // io.spring.dependency-management: dependencyManagement { imports { mavenBom("g:a:v") } }
            (Ctx::Top, "dependencyManagement") => self.spring_dependency_management(body),
            (_, "plugins") => self.stmts(body, &Ctx::Plugins),
            (_, "buildscript") => self.stmts(body, &Ctx::Buildscript),
            (Ctx::Buildscript, "dependencies") => self.stmts(body, &Ctx::BuildscriptDeps),
            (Ctx::Buildscript, "repositories") => {}
            (Ctx::Buildscript, "ext") => self.stmts(body, &Ctx::Ext),
            (Ctx::Top, "repositories") => {
                let repos = repositories(&self.ev, body);
                self.model.repositories.extend(repos);
            }
            (Ctx::Top, "dependencies") => self.stmts(body, &Ctx::Dependencies { source_set: None, constraint: false }),
            (Ctx::KotlinSourceSet(ss), "dependencies") | (Ctx::SourceSet(ss), "dependencies") => {
                self.stmts(body, &Ctx::Dependencies { source_set: Some(ss.clone()), constraint: false })
            }
            (_, "allprojects") => self.allprojects.push(body.to_vec()),
            (_, "subprojects") => self.subprojects.push(body.to_vec()),
            (Ctx::Top, "project") => {
                if let Some(p) = first_str() {
                    self.project_blocks.push((p, body.to_vec()));
                }
            }
            (_, "ext") | (_, "extra") => {
                self.stmts(body, &Ctx::Ext);
            }
            (Ctx::Ext, "set") => {
                if let (Some(k), Some(v)) = (first_str(), args.get(1).and_then(|a| self.ev.string(&a.value))) {
                    self.ev.vars.insert(k, v);
                }
            }
            (_, "apply") | (_, "project.apply") => {
                for a in args {
                    match a.name.as_deref() {
                        Some("plugin") => {
                            if let Some(id) = self.ev.string(&a.value) {
                                self.add_plugin(id, None, true);
                            }
                        }
                        Some("from") => {
                            if let Some(f) = self.ev.file(&a.value)
                                && f.is_file() {
                                    self.run_file(&f);
                                }
                        }
                        _ => {}
                    }
                }
                for s in body {
                    if let Stmt::Expr(Expr::Call { target, args, .. }) = s
                        && target.path().as_deref() == Some("plugin")
                            && let Some(id) = args.first().and_then(|a| self.ev.string(&a.value)) {
                                self.add_plugin(id, None, true);
                            }
                }
            }
            (_, "pluginManager.apply") => {
                if let Some(id) = first_str() {
                    self.add_plugin(id, None, true);
                }
            }
            (_, "afterEvaluate") | (_, "configure") | (_, "with") => self.stmts(body, ctx),
            // Runs when (or once) the plugin is applied.
            (_, "pluginManager.withPlugin" | "plugins.withId" | "project.pluginManager.withPlugin" | "project.plugins.withId") => {
                if let Some(id) = first_str() {
                    let id = canonical_plugin_id(&id);
                    if self.model.plugins.iter().any(|p| p.id == id && p.apply) {
                        self.stmts(body, &Ctx::Top);
                    } else {
                        self.deferred.push((id, body.to_vec()));
                    }
                }
            }
            (Ctx::Top, "kotlin") => self.stmts(body, &Ctx::Kotlin),
            (Ctx::Top, "java") => self.stmts(body, &Ctx::Java),
            (Ctx::Java, "toolchain") => self.stmts(body, &Ctx::Java),
            (Ctx::Top, "android") => self.stmts(body, &Ctx::Android),
            (Ctx::Android, "compileSdk") | (Ctx::Android, "compileSdkVersion") => self.model.android_compile_sdk = first_str(),
            (Ctx::Top, "sourceSets") | (Ctx::Android, "sourceSets") => self.stmts(body, &Ctx::SourceSets),
            (Ctx::SourceSets, "create") | (Ctx::SourceSets, "register") | (Ctx::SourceSets, "named") | (Ctx::SourceSets, "getByName") | (Ctx::SourceSets, "maybeCreate") => {
                if let Some(ss) = first_str() {
                    if name != "named" && name != "getByName" {
                        self.model.source_sets.push(ss.clone());
                    }
                    self.stmts(body, &Ctx::SourceSet(ss));
                }
            }
            (Ctx::SourceSets, ss) if !ss.contains('.') => {
                if !matches!(ss, "main" | "test") && !self.model.source_sets.iter().any(|s| s == ss) {
                    self.model.source_sets.push(ss.to_string());
                }
                self.stmts(body, &Ctx::SourceSet(ss.to_string()));
            }
            (Ctx::SourceSet(_), "java") | (Ctx::SourceSet(_), "kotlin") => self.stmts(body, ctx),
            (Ctx::SourceSet(ss), "srcDir") | (Ctx::SourceSet(ss), "srcDirs") => {
                let ss = ss.clone();
                self.add_src_dirs(&ss, e)
            }
            (Ctx::Kotlin, "jvmToolchain") => self.model.jvm_toolchain = first_str(),
            // Also inside `configure<KotlinProjectExtension> { }` (type arguments are not kept).
            (_, "explicitApi" | "explicitApiWarning" | "kotlin.explicitApi" | "kotlin.explicitApiWarning") => self.model.explicit_api = true,
            (Ctx::Kotlin, "sourceSets") => self.stmts(body, &Ctx::KotlinSourceSets),
            (Ctx::Kotlin, "compilerOptions") | (Ctx::Kotlin, "kotlinOptions") | (Ctx::CompilerOptions, _) | (Ctx::Top, "kotlinOptions") => {
                self.compiler_options(&name, args, body);
            }
            (Ctx::Kotlin, "targets") => self.stmts(body, &Ctx::Kotlin),
            (Ctx::Kotlin, target) if is_kmp_target(target) => {
                let t = if target == "android" { "androidTarget" } else { target };
                if !self.model.kmp_targets.iter().any(|x| x == t) {
                    self.model.kmp_targets.push(t.to_string());
                }
                // jvm { compilerOptions { } }
                self.stmts(body, &Ctx::Kotlin);
            }
            (Ctx::KotlinSourceSets, "getByName") | (Ctx::KotlinSourceSets, "named") | (Ctx::KotlinSourceSets, "create") | (Ctx::KotlinSourceSets, "register") | (Ctx::KotlinSourceSets, "maybeCreate") => {
                if let Some(ss) = first_str() {
                    if name == "create" || name == "register" || name == "maybeCreate" {
                        self.model.source_sets.push(ss.clone());
                    }
                    self.stmts(body, &Ctx::KotlinSourceSet(ss));
                }
            }
            (Ctx::KotlinSourceSets, "all") | (Ctx::KotlinSourceSets, "configureEach") | (Ctx::SourceSets, "all") | (Ctx::SourceSets, "configureEach") => {
                // Applies to every source set; `name` is the source set's name.
                let kotlin = *ctx == Ctx::KotlinSourceSets;
                let saved = self.ev.vars.get("name").cloned();
                for ss in self.known_source_sets(kotlin) {
                    self.ev.vars.insert("name".into(), ss.clone());
                    let c = if kotlin { Ctx::KotlinSourceSet(ss) } else { Ctx::SourceSet(ss) };
                    self.stmts(body, &c);
                }
                match saved {
                    Some(v) => self.ev.vars.insert("name".into(), v),
                    None => self.ev.vars.remove("name"),
                };
            }
            (Ctx::KotlinSourceSets, ss) if !ss.contains('.') && !matches!(ss, "all" | "configureEach" | "matching") => {
                self.stmts(body, &Ctx::KotlinSourceSet(ss.to_string()));
            }
            (Ctx::KotlinSourceSet(_), "kotlin") => self.stmts(body, ctx),
            (Ctx::KotlinSourceSet(_), "languageSettings" | "languageSettings.apply") => self.compiler_options(&name, args, body),
            (Ctx::Dependencies { .. }, _) => {}
            (Ctx::Top, "dependencyLocking") | (Ctx::Top, "tasks") => {}
            _ => {}
        }
    }

    fn spring_dependency_management(&mut self, body: &[Stmt]) {
        for st in body {
            let Stmt::Expr(Expr::Call { target, args, closure }) = st else { continue };
            match target.path().as_deref() {
                Some("imports") => self.spring_dependency_management(closure.as_deref().unwrap_or(&[])),
                Some("dependencies") => {
                    for d in closure.iter().flatten() {
                        if let Stmt::Expr(Expr::Call { target, args, .. }) = d
                            && target.path().as_deref() == Some("dependency")
                                && let Some(Notation::Module { module, version, classifier, ext }) = args.first().and_then(|a| self.notation(&a.value, args)) {
                                    self.model.deps.push(DeclaredDep {
                                        configuration: "implementation".into(),
                                        notation: Notation::Module { module, version, classifier, ext },
                                        exclusions: vec![],
                                        transitive: true,
                                        strict: false,
                                        constraint: true,
                                    });
                                }
                    }
                }
                Some("mavenBom") => {
                    if let Some(n) = args.first().and_then(|a| self.notation(&a.value, args)) {
                        self.model.deps.push(DeclaredDep {
                            configuration: "implementation".into(),
                            notation: Notation::Platform { inner: Box::new(n), enforced: false },
                            exclusions: vec![],
                            transitive: true,
                            strict: false,
                            constraint: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn compiler_options(&mut self, _name: &str, _args: &[Arg], body: &[Stmt]) {
        for s in body {
            match s {
                Stmt::Assign { target, value, .. } => self.assign(target, value, &Ctx::CompilerOptions),
                Stmt::Expr(Expr::Call { target, args, closure }) => {
                    let n = target.path().unwrap_or_default();
                    let v = args.first().and_then(|a| self.ev.string(&a.value));
                    if n.ends_with("jvmTarget.set") {
                        self.model.jvm_target = v;
                    } else if n.ends_with("languageVersion.set") {
                        self.model.language_version = v;
                    } else if n.ends_with("apiVersion.set") {
                        self.model.api_version = v;
                    } else if n.ends_with("optIn.add") || n.ends_with("optIn") {
                        if let Some(v) = v.filter(|v| !self.model.opt_in.contains(v)) {
                            self.model.opt_in.push(v);
                        }
                    } else if n.ends_with("freeCompilerArgs.add") || n.ends_with("freeCompilerArgs.addAll") {
                        for a in args {
                            if let Some(arg) = self.ev.string(&a.value)
                                && let Some(o) = arg.strip_prefix("-opt-in=")
                                && !self.model.opt_in.iter().any(|x| x == o) {
                                    self.model.opt_in.push(o.into());
                                }
                        }
                    } else if let Some(body) = closure {
                        self.compiler_options(&n, args, body);
                    }
                }
                _ => {}
            }
        }
    }

    fn add_plugin(&mut self, id: String, version: Option<String>, apply: bool) {
        let id = canonical_plugin_id(&id);
        if let Some(existing) = self.model.plugins.iter_mut().find(|p| p.id == id) {
            let newly_applied = apply && !existing.apply;
            existing.apply |= apply;
            if version.is_some() {
                existing.version = version;
            }
            if newly_applied {
                self.run_deferred(&id);
            }
            return;
        }
        self.model.plugins.push(Plugin { id: id.clone(), version, apply });
        if apply
            && let Some(script) = self.conventions.get(&id).cloned() {
                self.run_file(&script);
            }
        if apply {
            self.run_deferred(&id);
        }
    }

    fn run_deferred(&mut self, id: &str) {
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred).into_iter().partition(|(d, _)| *d == id);
        self.deferred = waiting;
        for (_, body) in ready {
            self.stmts(&body, &Ctx::Top);
        }
    }

    fn notation(&self, e: &Expr, args: &[Arg]) -> Option<Notation> {
        match e {
            Expr::Str(_) | Expr::Infix(..) | Expr::Index(..) => {
                let s = self.ev.string(e)?;
                let c = Coordinate::parse(&s)?;
                Some(Notation::Module { module: c.module(), version: c.version, classifier: c.classifier, ext: c.ext })
            }
            Expr::Ident(_) | Expr::Member(..) => {
                let full = e.path()?;
                let path = full.strip_prefix("rootProject.").or_else(|| full.strip_prefix("project.")).unwrap_or(&full).to_string();
                if let Some((cat, rest)) = path.split_once('.') {
                    if let Some(c) = self.settings.catalogs.get(cat) {
                        let rest = rest.strip_suffix(".get").unwrap_or(rest);
                        if let Some(lib) = c.library(rest) {
                            return Some(Notation::Module { module: lib.module.clone(), version: lib.version.clone().unwrap_or_default(), classifier: None, ext: "jar".into() });
                        }
                        return None;
                    }
                    if cat == "projects" {
                        return self.project_accessor(rest).map(Notation::Project);
                    }
                }
                // a variable holding a coordinate string
                let s = self.ev.string(e)?;
                let c = Coordinate::parse(&s)?;
                Some(Notation::Module { module: c.module(), version: c.version, classifier: c.classifier, ext: c.ext })
            }
            Expr::Call { target, args: inner, .. } => {
                let name = target.path().unwrap_or_default();
                let first = inner.first().map(|a| &a.value);
                match name.as_str() {
                    "project" => {
                        let p = inner.iter().find(|a| a.name.is_none() || a.name.as_deref() == Some("path")).and_then(|a| self.ev.string(&a.value))?;
                        Some(Notation::Project(p))
                    }
                    "platform" | "enforcedPlatform" => Some(Notation::Platform { inner: Box::new(self.notation(first?, inner)?), enforced: name == "enforcedPlatform" }),
                    "kotlin" => {
                        let module = self.ev.string(first?)?;
                        let version = inner.get(1).and_then(|a| self.ev.string(&a.value)).unwrap_or_default();
                        Some(Notation::Module { module: ModuleId::new("org.jetbrains.kotlin", format!("kotlin-{module}")), version, classifier: None, ext: "jar".into() })
                    }
                    "files" | "fileTree" | "project.files" => Some(Notation::Files(self.files(&name, inner))),
                    "gradleApi" | "gradleTestKit" | "localGroovy" => Some(Notation::Gradle(name)),
                    "testFixtures" => {
                        let mut n = self.notation(first?, inner)?;
                        if let Notation::Module { classifier, .. } = &mut n {
                            *classifier = Some("test-fixtures".into());
                        }
                        Some(n)
                    }
                    "get" | "getOrNull" => match &**target {
                        Expr::Member(recv, _) => self.notation(recv, inner),
                        _ => None,
                    },
                    _ => Some(Notation::Unknown(name)),
                }
            }
            Expr::Map(_) | Expr::Unknown | Expr::Num(_) | Expr::Bool(_) | Expr::Null | Expr::List(_) | Expr::Closure(_) => {
                // Groovy/Kotlin named arguments: group = "", name = "", version = ""
                let get = |k: &str| args.iter().find(|a| a.name.as_deref() == Some(k)).and_then(|a| self.ev.string(&a.value));
                let group = get("group")?;
                let name = get("name")?;
                Some(Notation::Module {
                    module: ModuleId::new(group, name),
                    version: get("version").unwrap_or_default(),
                    classifier: get("classifier"),
                    ext: get("ext").unwrap_or_else(|| "jar".into()),
                })
            }
        }
    }

    /// `projects.featureA.api` → `:feature-a:api` (matching by normalized name).
    fn project_accessor(&self, rest: &str) -> Option<String> {
        let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        let wanted: Vec<String> = rest.split('.').map(norm).collect();
        self.settings
            .projects
            .iter()
            .find(|(p, _)| {
                let segs: Vec<String> = p.trim_start_matches(':').split(':').map(norm).collect();
                segs == wanted
            })
            .map(|(p, _)| p.clone())
    }

    fn files(&self, kind: &str, args: &[Arg]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if kind == "fileTree" {
            let dir = args
                .iter()
                .find(|a| a.name.is_none() || a.name.as_deref() == Some("dir"))
                .and_then(|a| match &a.value {
                    Expr::Map(m) => m.iter().find(|(k, _)| k == "dir").and_then(|(_, v)| self.ev.file(v)),
                    v => self.ev.file(v),
                });
            if let Some(dir) = dir {
                for e in walkdir::WalkDir::new(&dir).into_iter().flatten() {
                    if e.path().extension().is_some_and(|x| x == "jar" || x == "aar") {
                        out.push(e.path().to_path_buf());
                    }
                }
            }
        } else {
            for a in args {
                if let Some(f) = self.ev.file(&a.value) {
                    out.push(f);
                }
            }
        }
        out
    }

    fn dependency(&mut self, e: &Expr, source_set: Option<&str>, constraint: bool) {
        let Expr::Call { target, args, closure } = e else { return };
        let mut config = target.path().unwrap_or_default();
        if config == "constraints" {
            if let Some(body) = closure {
                self.stmts(body, &Ctx::Dependencies { source_set: source_set.map(str::to_string), constraint: true });
            }
            return;
        }
        let mut args: &[Arg] = args;
        // `when (x) { ... }`, `run { }`, `if` bodies, helper functions...: walk nested blocks.
        if !is_configuration_name(&config) && config != "add" {
            let ctx = Ctx::Dependencies { source_set: source_set.map(str::to_string), constraint };
            if let Some(body) = closure {
                self.stmts(body, &ctx);
            }
            for a in args {
                if let Expr::Closure(body) = &a.value {
                    self.stmts(body, &ctx);
                }
            }
            return;
        }
        if config == "add" && args.len() >= 2 {
            config = self.ev.string(&args[0].value).unwrap_or_default();
            args = &args[1..];
        }
        if let Some(ss) = source_set {
            // KMP: `commonMain { dependencies { implementation(...) } }`
            if !config.is_empty() {
                let mut c = config.chars();
                let first = c.next().unwrap().to_uppercase().collect::<String>();
                config = format!("{ss}{first}{}", c.as_str());
            }
        }
        let Some(first) = args.first() else { return };
        let first_path = first.value.path().map(|p| p.strip_prefix("rootProject.").map(str::to_string).unwrap_or(p));
        let notations: Vec<Notation> = match &first.value {
            // libs.bundles.x
            _ if first_path.as_deref().is_some_and(|p| p.split('.').nth(1) == Some("bundles")) => {
                let path = first_path.clone().unwrap();
                let (cat, rest) = path.split_once('.').unwrap();
                let rest = rest.strip_prefix("bundles.").unwrap();
                self.settings
                    .catalogs
                    .get(cat)
                    .and_then(|c| c.bundle(&catalog::normalize(rest)))
                    .map(|libs| {
                        libs.into_iter()
                            .map(|l| Notation::Module { module: l.module.clone(), version: l.version.clone().unwrap_or_default(), classifier: None, ext: "jar".into() })
                            .collect()
                    })
                    .unwrap_or_default()
            }
            _ if first.name.is_some() => self.notation(&Expr::Unknown, args).into_iter().collect(),
            v => self.notation(v, args).into_iter().collect(),
        };
        let mut exclusions = Vec::new();
        let mut transitive = true;
        let mut strict = false;
        if let Some(body) = closure {
            for s in body {
                match s {
                    Stmt::Expr(Expr::Call { target, args, closure }) => match target.path().as_deref() {
                        Some("exclude") => {
                            let get = |k: &str| args.iter().find(|a| a.name.as_deref() == Some(k)).and_then(|a| self.ev.string(&a.value));
                            let group = get("group").unwrap_or_else(|| "*".into());
                            let module = get("module").unwrap_or_else(|| "*".into());
                            exclusions.push(ModuleId::new(group, module));
                        }
                        Some("version") => {
                            for vs in closure.iter().flatten() {
                                if let Stmt::Expr(Expr::Call { target, args, .. }) = vs
                                    && target.path().as_deref() == Some("strictly") {
                                        strict = true;
                                        let _ = args;
                                    }
                            }
                        }
                        _ => {}
                    },
                    Stmt::Assign { target, value, .. }
                        if matches!(target.path().as_deref(), Some("isTransitive") | Some("transitive")) =>
                    {
                        transitive = !matches!(value, Expr::Bool(false));
                    }
                    _ => {}
                }
            }
        }
        if notations.is_empty() {
            // Reported later, and only if the configuration feeds a compile classpath.
            if !matches!(first.value, Expr::Closure(_)) {
                let what = first.value.path().unwrap_or_else(|| "expression".into());
                self.model.deps.push(DeclaredDep {
                    configuration: config,
                    notation: Notation::Unknown(what),
                    exclusions,
                    transitive,
                    strict,
                    constraint,
                });
            }
            return;
        }
        for n in notations {
            let n = match n {
                // `!!` strict shorthand
                Notation::Module { module, version, classifier, ext } if version.ends_with("!!") => {
                    strict = true;
                    Notation::Module { module, version: version.trim_end_matches("!!").to_string(), classifier, ext }
                }
                n => n,
            };
            self.model.deps.push(DeclaredDep { configuration: config.clone(), notation: n, exclusions: exclusions.clone(), transitive, strict, constraint });
        }
    }
}

/// Plausible Gradle configuration names (`implementation`, `testApi`, `kapt`, `jvmMainImplementation`...).
fn is_configuration_name(name: &str) -> bool {
    const BASE: &[&str] = &[
        "implementation", "api", "compileOnly", "compileOnlyApi", "runtimeOnly", "compile", "runtime", "provided", "kapt", "ksp",
        "annotationProcessor", "classpath", "detektPlugins", "lintChecks", "coreLibraryDesugaring", "testFixturesImplementation",
        "testFixturesApi",
    ];
    const SUFFIXES: &[&str] = &["Implementation", "Api", "CompileOnly", "CompileOnlyApi", "RuntimeOnly", "Compile", "Runtime", "AnnotationProcessor"];
    BASE.contains(&name)
        || SUFFIXES.iter().any(|s| name.len() > s.len() && name.ends_with(s))
        || (name.starts_with("kapt") || name.starts_with("ksp")) && name[3..].chars().next().is_some_and(|c| c.is_uppercase())
}

pub fn is_kmp_target(name: &str) -> bool {
    matches!(
        name,
        "jvm" | "android" | "androidTarget" | "js" | "wasmJs" | "wasmWasi" | "iosX64" | "iosArm64" | "iosSimulatorArm64" | "macosX64"
            | "macosArm64" | "linuxX64" | "linuxArm64" | "mingwX64" | "tvosArm64" | "tvosX64" | "tvosSimulatorArm64" | "watchosArm64"
            | "watchosArm32" | "watchosX64" | "watchosSimulatorArm64" | "androidNativeArm64" | "androidNativeX64" | "ios" | "androidLibrary"
    )
}

/// Normalize legacy plugin ids (`kotlin`, `kotlin-android`...) to their full ids.
pub fn canonical_plugin_id(id: &str) -> String {
    match id {
        "kotlin" => "org.jetbrains.kotlin.jvm",
        "kotlin-android" => "org.jetbrains.kotlin.android",
        "kotlin-multiplatform" => "org.jetbrains.kotlin.multiplatform",
        "kotlin-kapt" => "org.jetbrains.kotlin.kapt",
        "kotlin-allopen" => "org.jetbrains.kotlin.plugin.allopen",
        "kotlin-noarg" => "org.jetbrains.kotlin.plugin.noarg",
        "kotlin-spring" => "org.jetbrains.kotlin.plugin.spring",
        "kotlin-jpa" => "org.jetbrains.kotlin.plugin.jpa",
        "kotlinx-serialization" => "org.jetbrains.kotlin.plugin.serialization",
        "kotlin-parcelize" => "org.jetbrains.kotlin.plugin.parcelize",
        other => other,
    }
    .to_string()
}

/// The leftmost name of a call chain (`tasks.withType<T>().configureEach` → `tasks`).
fn chain_root(e: &Expr) -> Option<String> {
    match e {
        Expr::Ident(s) => Some(s.clone()),
        Expr::Member(t, _) => chain_root(t),
        Expr::Call { target, .. } => chain_root(target),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn settings_with_includes_and_catalogs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "settings.gradle.kts", r#"
pluginManagement {
    repositories { gradlePluginPortal() }
    plugins { kotlin("jvm") version "1.9.20" }
}
dependencyResolutionManagement {
    repositories { mavenCentral(); google() }
    versionCatalogs { create("deps") { from(files("gradle/deps.toml")) } }
}
rootProject.name = "demo"
include(":app", ":libs:core")
project(":libs:core").projectDir = file("core")
"#);
        write(d, "gradle/libs.versions.toml", "[libraries]\nfoo = \"a:b:1\"\n");
        write(d, "gradle/deps.toml", "[libraries]\nbar = \"c:d:2\"\n");
        let s = read_settings(d);
        assert_eq!(s.root_name, "demo");
        let paths: Vec<_> = s.projects.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec![":", ":app", ":libs:core", ":libs"]);
        assert_eq!(s.projects[2].1, d.join("core"));
        assert_eq!(s.repositories.len(), 2);
        assert_eq!(s.plugin_versions.get("org.jetbrains.kotlin.jvm").map(String::as_str), Some("1.9.20"));
        assert!(s.catalogs.contains_key("libs") && s.catalogs.contains_key("deps"));
    }

    #[test]
    fn build_script_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "settings.gradle.kts", "include(\":core\", \":feature-a\")\n");
        write(d, "gradle/libs.versions.toml", r#"
[versions]
ktor = "2.3.7"
[libraries]
ktor-core = { module = "io.ktor:ktor-server-core", version.ref = "ktor" }
ktor-netty = { module = "io.ktor:ktor-server-netty", version.ref = "ktor" }
[bundles]
ktor = ["ktor-core", "ktor-netty"]
[plugins]
kotlin-jvm = { id = "org.jetbrains.kotlin.jvm", version = "2.0.0" }
"#);
        write(d, "gradle.properties", "coroutinesVersion=1.8.0\n");
        let build = write(d, "build.gradle.kts", r#"
plugins {
    alias(libs.plugins.kotlin.jvm)
    `java-library`
}
val coroutinesVersion: String by project
val localVersion = "4.0"
group = "com.example"
repositories { mavenCentral() }
dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:$coroutinesVersion")
    implementation(libs.bundles.ktor)
    api(platform("org.springframework:spring-framework-bom:6.1.0"))
    implementation("com.x:y:${localVersion}") { exclude(group = "org.slf4j") }
    testImplementation(kotlin("test"))
    implementation(projects.core)
    implementation(project(":feature-a"))
    compileOnly(files("libs/a.jar"))
    implementation(group = "g", name = "n", version = "1")
}
kotlin {
    jvmToolchain(17)
    compilerOptions { optIn.add("kotlin.RequiresOptIn") }
}
"#);
        let settings = read_settings(d);
        let conventions = HashMap::new();
        let mut it = ScriptInterpreter::new(&settings, d, settings.properties.clone(), &conventions);
        it.run_file(&build);
        let m = it.model;
        assert!(m.has_plugin("org.jetbrains.kotlin.jvm"));
        assert_eq!(m.plugins[0].version.as_deref(), Some("2.0.0"));
        assert!(m.has_plugin("java-library"));
        assert_eq!(m.group.as_deref(), Some("com.example"));
        assert_eq!(m.jvm_toolchain.as_deref(), Some("17"));
        assert_eq!(m.opt_in, vec!["kotlin.RequiresOptIn"]);
        let summary: Vec<String> = m
            .deps
            .iter()
            .map(|d| match &d.notation {
                Notation::Module { module, version, .. } => format!("{}:{module}:{version}", d.configuration),
                Notation::Project(p) => format!("{}:project{p}", d.configuration),
                Notation::Platform { inner, .. } => format!("{}:platform:{inner:?}", d.configuration).chars().take(40).collect(),
                Notation::Files(f) => format!("{}:files:{}", d.configuration, f.len()),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                "implementation:org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.0",
                "implementation:io.ktor:ktor-server-core:2.3.7",
                "implementation:io.ktor:ktor-server-netty:2.3.7",
                "api:platform:Module { module: ModuleId {",
                "implementation:com.x:y:4.0",
                "testImplementation:org.jetbrains.kotlin:kotlin-test:",
                "implementation:project:core",
                "implementation:project:feature-a",
                "compileOnly:files:1",
                "implementation:g:n:1",
            ]
        );
        assert_eq!(m.deps[4].exclusions, vec![ModuleId::new("org.slf4j", "*")]);
    }

    #[test]
    fn multiplatform_source_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let build = write(d, "build.gradle.kts", r#"
plugins { kotlin("multiplatform") version "2.0.0" }
kotlin {
    jvm()
    iosArm64()
    sourceSets {
        commonMain.dependencies {
            implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.0")
        }
        val jvmMain by getting {
            dependencies { implementation("io.ktor:ktor-client-cio:2.3.7") }
        }
        commonTest {
            dependencies { implementation(kotlin("test")) }
        }
    }
}
"#);
        let settings = read_settings(d);
        let conventions = HashMap::new();
        let mut it = ScriptInterpreter::new(&settings, d, HashMap::new(), &conventions);
        it.run_file(&build);
        let m = it.model;
        assert_eq!(m.kmp_targets, vec!["jvm", "iosArm64"]);
        let configs: Vec<_> = m.deps.iter().map(|d| d.configuration.as_str()).collect();
        assert_eq!(configs, vec!["commonMainImplementation", "jvmMainImplementation", "commonTestImplementation"]);
    }

    #[test]
    fn opt_in_from_tasks_and_language_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let build = write(d, "build.gradle.kts", r#"
plugins { kotlin("multiplatform") version "2.0.0" }
kotlin {
    jvm()
    sourceSets.all {
        languageSettings.apply { optIn("a.Experimental") }
    }
}
tasks.withType<KotlinCompile>().configureEach {
    compilerOptions { optIn.add("a.Internal") }
}
tasks.compileTestKotlin {
    compilerOptions { freeCompilerArgs.add("-opt-in=a.Internal") }
}
"#);
        let settings = read_settings(d);
        let conventions = HashMap::new();
        let mut it = ScriptInterpreter::new(&settings, d, HashMap::new(), &conventions);
        it.run_file(&build);
        assert_eq!(it.model.opt_in, vec!["a.Experimental", "a.Internal"]);
    }

    #[test]
    fn groovy_build_with_ext_and_apply_from() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "versions.gradle", "ext { guava_version = '31.1-jre' }\n");
        let build = write(d, "build.gradle", r#"
apply plugin: 'kotlin'
apply from: 'versions.gradle'
ext.kotlin_version = '1.8.0'
dependencies {
    implementation "org.jetbrains.kotlin:kotlin-stdlib:$kotlin_version"
    implementation "com.google.guava:guava:${guava_version}"
    implementation group: 'com.squareup', name: 'okio', version: '3.0.0'
}
"#);
        let settings = read_settings(d);
        let conventions = HashMap::new();
        let mut it = ScriptInterpreter::new(&settings, d, HashMap::new(), &conventions);
        it.run_file(&build);
        let m = it.model;
        assert!(m.has_plugin("org.jetbrains.kotlin.jvm"));
        let versions: Vec<_> = m
            .deps
            .iter()
            .filter_map(|d| match &d.notation {
                Notation::Module { module, version, .. } => Some(format!("{}:{version}", module.artifact)),
                _ => None,
            })
            .collect();
        assert_eq!(versions, vec!["kotlin-stdlib:1.8.0", "guava:31.1-jre", "okio:3.0.0"]);
    }
}

