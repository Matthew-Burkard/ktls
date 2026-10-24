//! Gradle builds: static interpretation of settings/build scripts plus native
//! dependency resolution, optionally refined by the Gradle tooling.

pub mod catalog;
pub mod dsl;
pub mod model;
pub mod tooling;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use self::model::{DeclaredDep, Notation, ScriptInterpreter, ScriptModel, Settings};
use super::coords::{ModuleId, Version};
use super::pom::{PomLoader, Scope};
use super::repo::Repository;
use super::resolver::{Request, Resolver, Strategy, TargetPlatform};
use super::{BuildSystem, GradleMode, KotlinSettings, LoadOptions, ModelSource, Module, ModuleDependency, Project, SourceKind, SourceRoot};

pub fn is_settings_dir(d: &Path) -> bool {
    model::settings_file(d).is_some()
}

pub fn is_build_dir(d: &Path) -> bool {
    model::build_file(d).is_some()
}

pub fn has_wrapper(root: &Path) -> bool {
    root.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" }).is_file()
}

pub fn load(root: &Path, opts: &LoadOptions) -> Project {
    let use_tooling = match opts.gradle_mode {
        GradleMode::Static => false,
        GradleMode::Tooling => true,
        GradleMode::Auto => opts.allow_tooling && has_wrapper(root),
    };
    if use_tooling {
        opts.report("Running Gradle to import the project");
        match tooling::load(root, opts) {
            Ok(p) => return p,
            Err(e) => {
                log::warn!("Gradle tooling import failed, falling back to static analysis: {e:#}");
                let mut p = load_static(root, opts);
                p.problems.push(format!("Gradle import failed ({e}); using static analysis of build scripts"));
                return p;
            }
        }
    }
    load_static(root, opts)
}

/// Precompiled script plugins in buildSrc / included plugin builds: id → script.
fn convention_plugins(root: &Path, settings: &Settings) -> HashMap<String, PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![root.join("buildSrc")];
    dirs.extend(settings.plugin_builds.iter().cloned());
    dirs.extend(settings.included_builds.iter().cloned());
    let mut out = HashMap::new();
    for d in dirs {
        if !d.is_dir() {
            continue;
        }
        for e in walkdir::WalkDir::new(&d).max_depth(8).into_iter().flatten() {
            let p = e.path();
            if p.components().any(|c| c.as_os_str() == "build" || c.as_os_str() == ".gradle") {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let is_src = p.to_string_lossy().contains("/src/main/");
            let stem = name.strip_suffix(".gradle.kts").or_else(|| name.strip_suffix(".gradle"));
            if let (true, Some(stem)) = (is_src, stem) {
                let text = std::fs::read_to_string(p).unwrap_or_default();
                let package = text.lines().find_map(|l| l.trim().strip_prefix("package ")).map(|p| p.trim().trim_end_matches(';').to_string());
                let id = match package {
                    Some(pkg) if !pkg.is_empty() => format!("{pkg}.{stem}"),
                    _ => stem.to_string(),
                };
                out.insert(id, p.to_path_buf());
            }
        }
    }
    out
}

struct ProjectInfo {
    path: String,
    dir: PathBuf,
    model: ScriptModel,
}

/// Interpret every project of a build (parents first so inherited state flows down).
fn interpret_build(root: &Path, settings: &Settings, conventions: &HashMap<String, PathBuf>) -> Vec<ProjectInfo> {
    let mut projects: Vec<(String, PathBuf)> = settings.projects.clone();
    projects.sort_by_key(|(p, _)| if p == ":" { 0 } else { p.matches(':').count() });

    let mut results: Vec<ProjectInfo> = Vec::new();
    // Inherited state per project path: (vars, allprojects closures, subprojects closures, project blocks)
    let mut vars_of: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut allprojects: Vec<Vec<dsl::Stmt>> = Vec::new();
    let mut subprojects: Vec<(String, Vec<dsl::Stmt>)> = Vec::new();
    let mut blocks: Vec<(String, Vec<dsl::Stmt>)> = Vec::new();

    for (path, dir) in projects {
        let parent_path = parent_of(&path);
        let mut vars = settings.properties.clone();
        if let Some(pv) = parent_path.as_ref().and_then(|p| vars_of.get(p)) {
            for (k, v) in pv {
                vars.insert(k.clone(), v.clone());
            }
        }
        let mut files = Vec::new();
        for (k, v) in model::read_properties(&mut files, &[&dir]) {
            vars.insert(k, v);
        }
        let mut it = ScriptInterpreter::new(settings, &dir, vars, conventions);
        it.ev.root_name = settings.root_name.clone();
        it.ev.project_name = Some(if path == ":" { settings.root_name.clone() } else { path.rsplit(':').next().unwrap_or_default().to_string() });
        it.model.files.extend(files.into_iter().filter(|f| f.starts_with(root)));
        for c in &allprojects {
            it.run_stmts(c);
        }
        for (owner, c) in &subprojects {
            if is_ancestor(owner, &path) {
                it.run_stmts(c);
            }
        }
        for (target, c) in &blocks {
            if *target == path {
                it.run_stmts(c);
            }
        }
        if let Some(bf) = model::build_file(&dir) {
            it.run_file(&bf);
        }
        // The root's own allprojects closures apply to itself too.
        if path == ":" {
            for c in it.allprojects.clone() {
                it.run_stmts(&c);
            }
        }
        allprojects.append(&mut it.allprojects);
        subprojects.extend(it.subprojects.drain(..).map(|c| (path.clone(), c)));
        blocks.append(&mut it.project_blocks);
        vars_of.insert(path.clone(), it.ev.vars.clone());
        results.push(ProjectInfo { path, dir, model: it.model });
    }
    results
}

fn parent_of(path: &str) -> Option<String> {
    if path == ":" {
        return None;
    }
    match path.rfind(':') {
        Some(0) => Some(":".into()),
        Some(i) => Some(path[..i].to_string()),
        None => None,
    }
}

fn is_ancestor(ancestor: &str, path: &str) -> bool {
    ancestor != path && (ancestor == ":" || path.starts_with(&format!("{ancestor}:")))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConfigKind {
    Api,
    Implementation,
    CompileOnly,
    CompileOnlyApi,
    Runtime,
    Processor,
}

/// Split a configuration name into (source set, kind).
fn classify(config: &str) -> Option<(String, ConfigKind)> {
    const SUFFIXES: &[(&str, ConfigKind)] = &[
        ("CompileOnlyApi", ConfigKind::CompileOnlyApi),
        ("Implementation", ConfigKind::Implementation),
        ("CompileOnly", ConfigKind::CompileOnly),
        ("RuntimeOnly", ConfigKind::Runtime),
        ("AnnotationProcessor", ConfigKind::Processor),
        ("Compile", ConfigKind::Implementation),
        ("Runtime", ConfigKind::Runtime),
        ("Api", ConfigKind::Api),
    ];
    match config {
        "api" => return Some(("main".into(), ConfigKind::Api)),
        "implementation" | "compile" => return Some(("main".into(), ConfigKind::Implementation)),
        "compileOnly" | "provided" => return Some(("main".into(), ConfigKind::CompileOnly)),
        "compileOnlyApi" => return Some(("main".into(), ConfigKind::CompileOnlyApi)),
        "runtimeOnly" | "runtime" => return Some(("main".into(), ConfigKind::Runtime)),
        "kapt" | "ksp" | "annotationProcessor" => return Some(("main".into(), ConfigKind::Processor)),
        _ => {}
    }
    if let Some(rest) = config.strip_prefix("kapt").or_else(|| config.strip_prefix("ksp"))
        && rest.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Some((lower_first(rest), ConfigKind::Processor));
        }
    for (suffix, kind) in SUFFIXES {
        if let Some(ss) = config.strip_suffix(suffix)
            && !ss.is_empty() {
                return Some((ss.to_string(), *kind));
            }
    }
    None
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn is_test_source_set(ss: &str) -> bool {
    ss == "test" || ss.ends_with("Test") || ss.starts_with("test") || ss.contains("androidTest") || ss.ends_with("UnitTest")
}

/// Source sets whose dependencies are visible when compiling JVM code of this module.
fn jvm_visible(ss: &str, android: bool) -> bool {
    const NON_JVM: &[&str] =
        &["ios", "macos", "tvos", "watchos", "linux", "mingw", "native", "apple", "js", "wasm", "androidNative", "posix", "darwin", "unix"];
    if NON_JVM.iter().any(|p| ss.starts_with(p)) {
        return false;
    }
    !ss.starts_with("android") || android
}

fn kotlin_version(projects: &[ProjectInfo], settings: &Settings, build_src: Option<&ScriptModel>) -> Option<String> {
    let from_plugins = projects.iter().flat_map(|p| p.model.plugins.iter()).find(|p| p.id.starts_with("org.jetbrains.kotlin.") && p.version.is_some()).and_then(|p| p.version.clone());
    let from_settings = settings.plugin_versions.iter().find(|(k, _)| k.starts_with("org.jetbrains.kotlin.")).map(|(_, v)| v.clone());
    let kgp = |n: &Notation| match n {
        Notation::Module { module, version, .. } if module.group == "org.jetbrains.kotlin" && module.artifact.starts_with("kotlin-gradle-plugin") && !version.is_empty() => Some(version.clone()),
        _ => None,
    };
    let from_classpath = projects.iter().flat_map(|p| p.model.buildscript_classpath.iter()).find_map(kgp);
    let from_build_src = build_src.and_then(|m| m.deps.iter().find_map(|d| kgp(&d.notation)));
    let from_catalog = settings
        .catalogs
        .values()
        .flat_map(|c| c.plugins.values())
        .find(|(id, v)| id.starts_with("org.jetbrains.kotlin.") && v.is_some())
        .and_then(|(_, v)| v.clone());
    from_plugins.or(from_settings).or(from_classpath).or(from_build_src).or(from_catalog)
}

fn android_sdk(root: &Path) -> Option<PathBuf> {
    let local = std::fs::read_to_string(root.join("local.properties")).ok().and_then(|t| {
        t.lines().find_map(|l| l.trim().strip_prefix("sdk.dir=")).map(|p| PathBuf::from(p.trim().replace("\\:", ":").replace("\\\\", "\\")))
    });
    local
        .or_else(|| std::env::var_os("ANDROID_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join("Android/Sdk")))
        .filter(|p| p.is_dir())
}

pub fn load_static(root: &Path, opts: &LoadOptions) -> Project {
    opts.report("Reading Gradle build scripts");
    let settings = model::read_settings(root);
    let conventions = convention_plugins(root, &settings);
    let infos = interpret_build(root, &settings, &conventions);

    let build_src = root.join("buildSrc");
    let build_src_model = model::build_file(&build_src).map(|bf| {
        let s = model::read_settings(&build_src);
        let empty = HashMap::new();
        let mut it = ScriptInterpreter::new(&s, &build_src, HashMap::new(), &empty);
        it.run_file(&bf);
        it.model
    });

    let mut problems = settings.problems.clone();
    let kotlin_version = kotlin_version(&infos, &settings, build_src_model.as_ref());

    // Repositories: settings first, then project-declared ones.
    let mut repos: Vec<Repository> = settings.repositories.clone();
    for info in &infos {
        for r in &info.model.repositories {
            if !repos.contains(r) {
                repos.push(r.clone());
            }
        }
    }
    let any_android = infos.iter().any(|i| i.model.has_plugin_prefix("com.android."));
    if repos.is_empty() {
        repos.push(Repository::maven_central());
        if any_android {
            repos.push(Repository::google());
        }
    }
    repos.extend(opts.extra_repositories.iter().cloned());

    let repo = opts.repo_system();
    let loader = Arc::new(PomLoader::new(repo, repos.clone()));

    // Exported (api) declarations per project, for project dependencies.
    let by_path: HashMap<&str, &ProjectInfo> = infos.iter().map(|i| (i.path.as_str(), i)).collect();
    let coordinate_of: HashMap<ModuleId, String> = infos
        .iter()
        .filter_map(|i| {
            let group = i.model.group.clone()?;
            let name = i.path.rsplit(':').next().filter(|n| !n.is_empty())?.to_string();
            Some((ModuleId::new(group, name), i.path.clone()))
        })
        .collect();

    let mut build_files: Vec<PathBuf> = settings.files.clone();
    let mut modules = Vec::new();
    for info in &infos {
        build_files.extend(info.model.files.iter().cloned());
        let m = &info.model;
        let is_kmp = m.has_plugin("org.jetbrains.kotlin.multiplatform");
        let is_android = m.has_plugin_prefix("com.android.");
        let is_kotlin = m.has_plugin_prefix("org.jetbrains.kotlin.") && !m.plugins.iter().all(|p| p.id.starts_with("org.jetbrains.kotlin.plugin."));
        let has_java = m.has_plugin("java") || m.has_plugin("java-library") || m.has_plugin("application") || m.has_plugin("org.springframework.boot");
        let applies_language = is_kotlin || is_android || has_java || is_kmp || !m.deps.is_empty();
        let has_sources = info.dir.join("src").is_dir();
        if !applies_language && !has_sources {
            continue; // aggregator-only project (e.g. root without code)
        }
        opts.report(&format!("Resolving dependencies of {}", if info.path == ":" { settings.root_name.as_str() } else { &info.path }));

        let mut module_problems: Vec<String> = m.problems.clone();
        let mut requests: Vec<Request> = Vec::new();
        let mut managed: HashMap<ModuleId, String> = HashMap::new();
        let mut module_deps: Vec<ModuleDependency> = Vec::new();
        let mut local: Vec<(PathBuf, Scope)> = Vec::new();
        let mut source_sets: HashSet<String> = HashSet::new();

        let kv = kotlin_version.clone();
        let add_declared = |d: &DeclaredDep, scope_override: Option<Scope>, requests: &mut Vec<Request>, module_deps: &mut Vec<ModuleDependency>, local: &mut Vec<(PathBuf, Scope)>, problems: &mut Vec<String>, source_sets: &mut HashSet<String>| {
            let Some((ss, kind)) = classify(&d.configuration) else { return };
            if matches!(kind, ConfigKind::Runtime | ConfigKind::Processor) || !jvm_visible(&ss, is_android) {
                return;
            }
            source_sets.insert(ss.clone());
            let scope = scope_override.unwrap_or(if is_test_source_set(&ss) {
                Scope::Test
            } else if matches!(kind, ConfigKind::CompileOnly | ConfigKind::CompileOnlyApi) {
                Scope::Provided
            } else {
                Scope::Compile
            });
            match &d.notation {
                Notation::Module { module, version, classifier, ext } => {
                    if let Some(path) = coordinate_of.get(module) {
                        module_deps.push(ModuleDependency { module: path.clone(), scope });
                        return;
                    }
                    let mut version = version.clone();
                    if version.is_empty() && module.group == "org.jetbrains.kotlin" {
                        version = kv.clone().unwrap_or_default();
                    }
                    let mut r = Request::new(module.clone(), version, scope);
                    r.classifier = classifier.clone();
                    r.ext = ext.clone();
                    r.exclusions = d.exclusions.clone();
                    r.transitive = d.transitive;
                    r.strict = d.strict;
                    requests.push(r);
                }
                Notation::Platform { inner, .. } => {
                    if let Notation::Module { module, version, .. } = &**inner {
                        let mut r = Request::new(module.clone(), version.clone(), scope);
                        r.platform = true;
                        requests.push(r);
                    }
                }
                Notation::Project(p) => module_deps.push(ModuleDependency { module: p.clone(), scope }),
                Notation::Files(files) => local.extend(files.iter().map(|f| (f.clone(), scope))),
                Notation::Gradle(api) => problems.push(format!("{api}() dependencies are only available with the Gradle import")),
                Notation::Unknown(n) => problems.push(format!("Could not evaluate dependency '{n}' in '{}'", d.configuration)),
            }
        };

        // The Spring Boot plugin + io.spring.dependency-management import Boot's BOM.
        if m.has_plugin("io.spring.dependency-management") {
            let boot = infos.iter().flat_map(|i| i.model.plugins.iter()).find(|p| p.id == "org.springframework.boot" && p.version.is_some());
            if let Some(v) = boot.and_then(|p| p.version.clone()).filter(|_| m.plugins.iter().any(|p| p.id == "org.springframework.boot")) {
                let mut r = Request::new(ModuleId::new("org.springframework.boot", "spring-boot-dependencies"), v, Scope::Compile);
                r.platform = true;
                requests.push(r);
            }
        }
        for d in &m.deps {
            if d.constraint {
                if let Notation::Module { module, version, .. } = &d.notation
                    && !version.is_empty() {
                        managed.insert(module.clone(), version.clone());
                    }
                continue;
            }
            add_declared(d, None, &mut requests, &mut module_deps, &mut local, &mut module_problems, &mut source_sets);
        }

        // Project dependencies: their api/compileOnlyApi declarations are exposed to us.
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<(String, Scope)> = module_deps.iter().map(|d| (d.module.clone(), d.scope)).collect();
        while let Some((path, scope)) = queue.pop() {
            if !seen.insert(path.clone()) || path == info.path {
                continue;
            }
            let Some(dep) = by_path.get(path.as_str()) else {
                module_problems.push(format!("Project {path} not found in settings"));
                continue;
            };
            if !module_deps.iter().any(|d| d.module == path) {
                module_deps.push(ModuleDependency { module: path.clone(), scope });
            }
            for d in &dep.model.deps {
                let Some((ss, kind)) = classify(&d.configuration) else { continue };
                let exported = matches!(kind, ConfigKind::Api | ConfigKind::CompileOnlyApi) && !is_test_source_set(&ss);
                if !exported || d.constraint {
                    continue;
                }
                match &d.notation {
                    Notation::Project(p) => queue.push((p.clone(), scope)),
                    _ => {
                        let mut ignored = HashSet::new();
                        add_declared(d, Some(scope), &mut requests, &mut module_deps, &mut local, &mut module_problems, &mut ignored);
                    }
                }
            }
            // A Kotlin library always brings the stdlib.
            if dep.model.has_plugin_prefix("org.jetbrains.kotlin.")
                && let Some(v) = &kotlin_version {
                    requests.push(Request::new(ModuleId::new("org.jetbrains.kotlin", "kotlin-stdlib"), v.clone(), scope));
                }
        }

        // Kotlin stdlib from the plugin.
        let stdlib_default = settings.properties.get("kotlin.stdlib.default.dependency").is_none_or(|v| v != "false");
        let mut kotlin = KotlinSettings {
            version: kotlin_version.clone(),
            language_version: m.language_version.clone(),
            api_version: m.api_version.clone(),
            jvm_target: m.jvm_target.clone().or_else(|| m.jvm_toolchain.clone()),
            multiplatform: is_kmp,
            compiler_plugins: compiler_plugins(m),
            explicit_api: m.explicit_api,
            opt_in: m.opt_in.clone(),
        };
        if is_kotlin || is_kmp || (is_android && m.has_plugin("org.jetbrains.kotlin.android")) {
            match &kotlin_version {
                Some(v) if stdlib_default => {
                    if !requests.iter().any(|r| r.module.group == "org.jetbrains.kotlin" && r.module.artifact == "kotlin-stdlib") {
                        requests.push(Request::new(ModuleId::new("org.jetbrains.kotlin", "kotlin-stdlib"), v.clone(), Scope::Compile));
                    }
                }
                Some(_) => {}
                None => {
                    module_problems.push("Could not determine the Kotlin Gradle plugin version".into());
                    if let Some(v) = newest_cached_stdlib(&loader) {
                        kotlin.version = Some(v.clone());
                        requests.push(Request::new(ModuleId::new("org.jetbrains.kotlin", "kotlin-stdlib"), v, Scope::Compile));
                    }
                }
            }
        }
        // kotlin("test") without a version
        for r in &mut requests {
            if r.version.is_empty() && r.module.group == "org.jetbrains.kotlin"
                && let Some(v) = &kotlin.version {
                    r.version = v.clone();
                }
        }
        // The Kotlin Gradle plugin turns kotlin-test into the flavour matching the test framework.
        if let Some(kt) = requests.iter().find(|r| r.module.group == "org.jetbrains.kotlin" && r.module.artifact == "kotlin-test").cloned() {
            let script = m.files.iter().filter_map(|f| std::fs::read_to_string(f).ok()).collect::<String>();
            let flavour = if script.contains("useJUnitPlatform") {
                "kotlin-test-junit5"
            } else if script.contains("useTestNG") {
                "kotlin-test-testng"
            } else {
                "kotlin-test-junit"
            };
            if !requests.iter().any(|r| r.module.artifact.starts_with("kotlin-test-")) {
                let mut r = Request::new(ModuleId::new("org.jetbrains.kotlin", flavour), kt.version.clone(), kt.scope);
                r.exclusions = kt.exclusions.clone();
                requests.push(r);
            }
        }

        let mut resolver = Resolver::new(loader.clone(), Strategy::Highest);
        resolver.platform = if is_android { TargetPlatform::Android } else { TargetPlatform::Jvm };
        resolver.managed = managed;
        let resolution = resolver.resolve(&requests);
        module_problems.extend(resolution.problems.iter().cloned());

        // Android SDK
        if is_android {
            match (android_sdk(root), &m.android_compile_sdk) {
                (Some(sdk), Some(level)) => {
                    let level = level.trim_start_matches("android-");
                    let jar = sdk.join(format!("platforms/android-{level}/android.jar"));
                    if jar.is_file() {
                        local.push((jar, Scope::Provided));
                    } else {
                        module_problems.push(format!("Android platform {level} is not installed ({})", jar.display()));
                    }
                }
                (None, _) => module_problems.push("Android SDK not found (set sdk.dir in local.properties or ANDROID_HOME)".into()),
                (_, None) => module_problems.push("Android compileSdk could not be determined".into()),
            }
        }

        let roots = gradle_source_roots(&info.dir, m, is_kmp, is_android, &source_sets);
        let complete = module_problems.is_empty();
        modules.push(Module {
            name: info.path.clone(),
            dir: info.dir.clone(),
            source_roots: roots,
            dependencies: resolution.artifacts,
            module_dependencies: module_deps,
            local_classpath: local,
            kotlin,
            java_release: m.jvm_toolchain.clone(),
            problems: module_problems,
            complete,
        });
    }

    // buildSrc is a build of its own; index its sources with whatever we know.
    if let Some(bm) = build_src_model {
        build_files.extend(bm.files.iter().cloned());
        let mut roots = super::conventional_roots(&build_src, "main", SourceKind::Main, &[]);
        roots.retain(|r| r.path.is_dir());
        modules.push(Module {
            name: ":buildSrc".into(),
            dir: build_src.clone(),
            source_roots: roots,
            problems: vec!["buildSrc requires the Gradle API, which is only available with the Gradle import".into()],
            complete: false,
            ..Default::default()
        });
    }

    // Included (composite) builds become part of this project.
    for inc in &settings.included_builds {
        if inc.join("settings.gradle.kts").is_file() || inc.join("settings.gradle").is_file() || is_build_dir(inc) {
            let sub = load_static(inc, opts);
            let prefix = inc.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            for mut m in sub.modules {
                m.name = format!(":{prefix}{}", if m.name == ":" { String::new() } else { m.name.clone() });
                modules.push(m);
            }
            build_files.extend(sub.build_files);
            problems.extend(sub.problems);
        }
    }

    build_files.sort();
    build_files.dedup();
    Project {
        root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        modules,
        problems,
        source: ModelSource::Static,
        build_files,
        repositories: repos,
    }
}

fn compiler_plugins(m: &ScriptModel) -> Vec<String> {
    let mut out = Vec::new();
    for p in &m.plugins {
        let short = match p.id.as_str() {
            "org.jetbrains.kotlin.plugin.serialization" => "serialization",
            "org.jetbrains.kotlin.plugin.spring" => "spring",
            "org.jetbrains.kotlin.plugin.allopen" => "allopen",
            "org.jetbrains.kotlin.plugin.noarg" => "noarg",
            "org.jetbrains.kotlin.plugin.jpa" => "jpa",
            "org.jetbrains.kotlin.plugin.parcelize" => "parcelize",
            "org.jetbrains.kotlin.plugin.compose" | "org.jetbrains.compose" => "compose",
            "org.jetbrains.kotlin.plugin.power-assert" => "power-assert",
            "org.jetbrains.kotlin.kapt" => "kapt",
            "com.google.devtools.ksp" => "ksp",
            _ => continue,
        };
        if !out.iter().any(|o| o == short) {
            out.push(short.to_string());
        }
    }
    out
}

fn newest_cached_stdlib(loader: &PomLoader) -> Option<String> {
    let versions = loader.repo.list_versions(&ModuleId::new("org.jetbrains.kotlin", "kotlin-stdlib"), &[]);
    versions.into_iter().filter(|v| Version::parse(v).is_stable()).max_by(|a, b| Version::parse(a).cmp(&Version::parse(b)))
}

fn gradle_source_roots(dir: &Path, m: &ScriptModel, is_kmp: bool, is_android: bool, used: &HashSet<String>) -> Vec<SourceRoot> {
    let mut sets: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        if !sets.iter().any(|x| x == s) {
            sets.push(s.to_string());
        }
    };
    if is_kmp {
        add("commonMain");
        add("commonTest");
        for t in &m.kmp_targets {
            let base = if t == "androidTarget" || t == "androidLibrary" { "android" } else { t.as_str() };
            add(&format!("{base}Main"));
            add(&format!("{base}Test"));
            if base.starts_with("ios") {
                add("iosMain");
                add("iosTest");
                add("appleMain");
                add("nativeMain");
            } else if base.starts_with("macos") || base.starts_with("tvos") || base.starts_with("watchos") {
                add("appleMain");
                add("nativeMain");
            } else if base.starts_with("linux") || base.starts_with("mingw") || base.starts_with("androidNative") {
                add("nativeMain");
            }
        }
        if m.kmp_targets.iter().any(|t| t == "jvm") {
            add("jvmMain");
        }
    } else {
        add("main");
        add("test");
        if is_android {
            add("androidTest");
            add("debug");
            add("release");
        }
    }
    for s in m.source_sets.iter().chain(used.iter()) {
        add(s);
    }
    let mut roots = Vec::new();
    for ss in &sets {
        let kind = if is_test_source_set(ss) { SourceKind::Test } else { SourceKind::Main };
        let extra: Vec<PathBuf> = m.source_dirs.iter().filter(|(s, _)| s == ss).map(|(_, p)| p.clone()).collect();
        let mut r = super::conventional_roots(dir, ss, kind, &extra);
        if m.replaced_source_dirs.iter().any(|s| s == ss) {
            r.retain(|root| root.generated || extra.contains(&root.path));
        }
        // Keep the primary JVM roots even if missing (new files may be created there).
        r.retain(|root| root.path.is_dir() || (matches!(ss.as_str(), "main" | "test" | "commonMain") && root.path.ends_with("kotlin")));
        roots.extend(r);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn fake_artifact(m2: &Path, g: &str, a: &str, v: &str, deps: &[(&str, &str, &str)]) {
        let dir = m2.join(g.replace('.', "/")).join(a).join(v);
        std::fs::create_dir_all(&dir).unwrap();
        let mut xml = format!("<project><groupId>{g}</groupId><artifactId>{a}</artifactId><version>{v}</version><dependencies>");
        for (dg, da, dv) in deps {
            xml.push_str(&format!("<dependency><groupId>{dg}</groupId><artifactId>{da}</artifactId><version>{dv}</version></dependency>"));
        }
        xml.push_str("</dependencies></project>");
        std::fs::write(dir.join(format!("{a}-{v}.pom")), xml).unwrap();
        std::fs::write(dir.join(format!("{a}-{v}.jar")), "jar").unwrap();
    }

    #[test]
    fn classify_configurations() {
        assert_eq!(classify("implementation"), Some(("main".into(), ConfigKind::Implementation)));
        assert_eq!(classify("testImplementation"), Some(("test".into(), ConfigKind::Implementation)));
        assert_eq!(classify("commonMainApi"), Some(("commonMain".into(), ConfigKind::Api)));
        assert_eq!(classify("jvmTestCompileOnly"), Some(("jvmTest".into(), ConfigKind::CompileOnly)));
        assert_eq!(classify("kspTest"), Some(("test".into(), ConfigKind::Processor)));
        assert_eq!(classify("detektPlugins"), None);
    }

    #[test]
    fn project_name_conditions() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "settings.gradle.kts", "rootProject.name = \"demo\"\ninclude(\":lib\", \":examples\")\n");
        write(d, "build.gradle.kts", "subprojects {\n  if (project.name != \"examples\") {\n    kotlin { explicitApi() }\n  }\n}\n");
        for m in ["lib", "examples"] {
            write(d, &format!("{m}/build.gradle.kts"), "plugins { kotlin(\"jvm\") }\n");
            std::fs::create_dir_all(d.join(m).join("src/main/kotlin")).unwrap();
        }
        let project = load_static(d, &LoadOptions::isolated(d.join("cache")));
        let explicit: Vec<(&str, bool)> = project.modules.iter().map(|m| (m.name.as_str(), m.kotlin.explicit_api)).collect();
        assert_eq!(explicit, vec![(":lib", true), (":examples", false)]);
    }

    #[test]
    fn multi_project_static_import() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let m2 = d.join("m2");
        fake_artifact(&m2, "org.jetbrains.kotlin", "kotlin-stdlib", "2.0.0", &[("org.jetbrains", "annotations", "13.0")]);
        fake_artifact(&m2, "org.jetbrains", "annotations", "13.0", &[]);
        fake_artifact(&m2, "com.squareup.okio", "okio", "3.6.0", &[]);
        fake_artifact(&m2, "com.google.guava", "guava", "32.0.0-jre", &[]);
        fake_artifact(&m2, "junit", "junit", "4.13.2", &[]);

        write(d, "settings.gradle.kts", "rootProject.name = \"demo\"\ninclude(\":core\", \":app\")\n");
        write(
            d,
            "build.gradle.kts",
            &format!(
                "plugins {{ kotlin(\"jvm\") version \"2.0.0\" apply false }}\nallprojects {{ repositories {{ maven(\"file://{}\") }} }}\n",
                m2.display()
            ),
        );
        write(
            d,
            "core/build.gradle.kts",
            "plugins { kotlin(\"jvm\"); `java-library` }\ndependencies {\n  api(\"com.squareup.okio:okio:3.6.0\")\n  implementation(\"com.google.guava:guava:32.0.0-jre\")\n}\n",
        );
        write(
            d,
            "app/build.gradle.kts",
            "plugins { kotlin(\"jvm\") }\ndependencies {\n  implementation(project(\":core\"))\n  testImplementation(\"junit:junit:4.13.2\")\n}\n",
        );
        std::fs::create_dir_all(d.join("app/src/main/kotlin")).unwrap();
        std::fs::create_dir_all(d.join("core/src/main/kotlin")).unwrap();

        let opts = LoadOptions::isolated(d.join("cache"));
        let project = load_static(d, &opts);
        let names: Vec<_> = project.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec![":core", ":app"], "root has no sources and no plugins applied");

        let core = project.module(":core").unwrap();
        assert!(core.complete, "{:?}", core.problems);
        let mut core_deps: Vec<_> = core.dependencies.iter().map(|a| a.coord.artifact.as_str()).collect();
        core_deps.sort();
        assert_eq!(core_deps, vec!["annotations", "guava", "kotlin-stdlib", "okio"]);

        let app = project.module(":app").unwrap();
        assert!(app.complete, "{:?}", app.problems);
        assert_eq!(app.module_dependencies[0].module, ":core");
        let mut app_deps: Vec<_> = app.dependencies.iter().map(|a| format!("{}:{:?}", a.coord.artifact, a.scope)).collect();
        app_deps.sort();
        // okio is `api` in core → visible; guava is `implementation` → not visible.
        assert_eq!(app_deps, vec!["annotations:Compile", "junit:Test", "kotlin-stdlib:Compile", "okio:Compile"]);
        assert_eq!(app.kotlin.version.as_deref(), Some("2.0.0"));
        let main_cp = app.classpath(SourceKind::Main);
        assert_eq!(main_cp.len(), 3);
        assert_eq!(app.classpath(SourceKind::Test).len(), 4);
        assert!(project.build_files.iter().any(|f| f.ends_with("app/build.gradle.kts")));
    }
}
