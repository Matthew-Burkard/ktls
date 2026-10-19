//! Maven projects: reactor discovery, effective POMs and native resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::coords::ModuleId;
use super::pom::{EffectivePom, PomLoader, Scope, parse_pom};
use super::repo::Repository;
use super::resolver::{Request, Resolver, Strategy};
use super::{BuildSystem, KotlinSettings, LoadOptions, ModelSource, Module, ModuleDependency, Project, SourceKind, SourceRoot};

/// Does the POM at `pom` list `child` as one of its modules?
pub fn aggregates(pom: &Path, child: &Path) -> bool {
    let Some(raw) = std::fs::read_to_string(pom).ok().and_then(|t| parse_pom(&t)) else { return false };
    let dir = pom.parent().unwrap();
    raw.modules
        .iter()
        .chain(raw.profiles.iter().flat_map(|p| p.modules.iter()))
        .any(|m| normalize(&dir.join(m)) == normalize(child) || normalize(&dir.join(m)) == normalize(&child.join("pom.xml")))
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

pub fn load(root: &Path, opts: &LoadOptions) -> Project {
    opts.report("Reading Maven project");
    let repo = opts.repo_system();
    let mut repos = vec![Repository::maven_central()];
    repos.extend(opts.extra_repositories.iter().cloned());
    let loader = Arc::new(PomLoader::new(repo, repos.clone()));

    // Collect reactor POMs.
    let mut poms: Vec<(PathBuf, EffectivePom)> = Vec::new();
    let mut problems = Vec::new();
    let mut stack = vec![root.join("pom.xml")];
    let mut seen = std::collections::HashSet::new();
    while let Some(pom_path) = stack.pop() {
        let pom_path = normalize(&pom_path);
        if !seen.insert(pom_path.clone()) {
            continue;
        }
        match loader.load_file(&pom_path) {
            Ok(eff) => {
                let dir = pom_path.parent().unwrap().to_path_buf();
                for m in eff.modules.iter().rev() {
                    let p = dir.join(m);
                    stack.push(if p.is_dir() { p.join("pom.xml") } else { p });
                }
                poms.push((pom_path, eff));
            }
            Err(e) => problems.push(format!("{}: {e}", pom_path.display())),
        }
    }
    let mut build_files: Vec<PathBuf> = poms.iter().map(|(p, _)| p.clone()).collect();
    // Parent POMs outside the reactor that are on disk also affect the model.
    for (_, eff) in &poms {
        if let Some(f) = &eff.file
            && !build_files.contains(f) {
                build_files.push(f.clone());
            }
    }
    let settings = dirs::home_dir().map(|h| h.join(".m2/settings.xml"));
    if let Some(s) = settings.filter(|s| s.is_file()) {
        build_files.push(s);
    }

    // Reactor modules are resolvable "artifacts" whose deps come from their effective POM.
    let reactor: HashMap<ModuleId, String> = poms
        .iter()
        .map(|(_, e)| (ModuleId::new(&e.group_id, &e.artifact_id), format!("{}:{}", e.group_id, e.artifact_id)))
        .collect();
    for (_, eff) in &poms {
        loader.insert(eff.clone());
    }

    let mut modules = Vec::new();
    let mut all_repos = repos.clone();
    for (pom_path, eff) in &poms {
        opts.report(&format!("Resolving dependencies of {}", eff.artifact_id));
        let dir = pom_path.parent().unwrap().to_path_buf();
        let mut module_repos = repos.clone();
        for r in &eff.repositories {
            if !module_repos.contains(r) {
                module_repos.push(r.clone());
            }
            if !all_repos.contains(r) {
                all_repos.push(r.clone());
            }
        }
        let module_loader = if module_repos.len() == repos.len() {
            loader.clone()
        } else {
            let l = Arc::new(PomLoader::new(loader.repo.clone(), module_repos.clone()));
            for (_, e) in &poms {
                l.insert(e.clone());
            }
            l
        };
        let mut resolver = Resolver::new(module_loader, Strategy::Nearest);
        resolver.managed = eff.managed.iter().filter_map(|d| Some((d.module.clone(), d.version.clone()?))).collect();

        let mut requests = Vec::new();
        let mut module_deps = Vec::new();
        let mut local = Vec::new();
        for d in &eff.dependencies {
            let scope = d.scope.unwrap_or(Scope::Compile);
            if scope == Scope::System {
                if let Some(p) = &d.system_path {
                    local.push((PathBuf::from(p), Scope::System));
                }
                continue;
            }
            if let Some(name) = reactor.get(&d.module) {
                module_deps.push(ModuleDependency { module: name.clone(), scope });
            }
            let (ext, classifier) = d.artifact_kind();
            let mut r = Request::new(d.module.clone(), d.version.clone().unwrap_or_default(), scope);
            r.ext = ext;
            r.classifier = classifier;
            r.exclusions = d.exclusions.clone();
            requests.push(r);
        }
        let mut resolution = resolver.resolve(&requests);
        // Reactor modules are sources, not binaries; keep their transitive deps.
        resolution.artifacts.retain(|a| !reactor.contains_key(&a.coord.module()));
        // Transitively reached reactor modules are module dependencies too.
        let mut module_problems: Vec<String> = eff.problems.clone();
        module_problems.extend(resolution.problems.iter().filter(|p| !reactor.keys().any(|m| p.contains(&m.to_string()))).cloned());

        let kotlin = kotlin_settings(eff);
        let mut roots = source_roots(&dir, eff);
        roots.retain(|r| r.path.is_dir() || !r.generated);
        let complete = module_problems.is_empty();
        modules.push(Module {
            name: format!("{}:{}", eff.group_id, eff.artifact_id),
            dir,
            source_roots: if eff.packaging == "pom" && roots.iter().all(|r| !r.path.exists()) { Vec::new() } else { roots },
            dependencies: resolution.artifacts,
            module_dependencies: module_deps,
            local_classpath: local,
            kotlin,
            java_release: eff
                .properties
                .get("maven.compiler.release")
                .or_else(|| eff.properties.get("maven.compiler.target"))
                .or_else(|| eff.properties.get("java.version"))
                .cloned()
                .or_else(|| eff.plugin("maven-compiler-plugin").and_then(|p| p.config_values("release").into_iter().next())),
            problems: module_problems,
            complete,
        });
    }

    Project {
        root: root.to_path_buf(),
        build_system: BuildSystem::Maven,
        modules,
        problems,
        source: ModelSource::Static,
        build_files,
        repositories: all_repos,
    }
}

fn source_roots(dir: &Path, eff: &EffectivePom) -> Vec<SourceRoot> {
    let resolve = |s: &str| {
        let p = PathBuf::from(s);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let mut main_extra: Vec<PathBuf> = Vec::new();
    let mut test_extra: Vec<PathBuf> = Vec::new();
    if let Some(s) = &eff.source_directory {
        main_extra.push(resolve(s));
    }
    if let Some(s) = &eff.test_source_directory {
        test_extra.push(resolve(s));
    }
    if let Some(k) = eff.plugin("kotlin-maven-plugin") {
        // <sourceDirs> appears in both compile and test-compile executions; split by name.
        for s in k.config_values("sourceDirs") {
            let p = resolve(&s);
            if s.contains("test") { test_extra.push(p) } else { main_extra.push(p) }
        }
    }
    if let Some(b) = eff.plugin("build-helper-maven-plugin") {
        for s in b.config_values("sources") {
            let p = resolve(&s);
            if s.contains("test") { test_extra.push(p) } else { main_extra.push(p) }
        }
    }
    let mut roots = super::conventional_roots(dir, "main", SourceKind::Main, &main_extra);
    roots.extend(super::conventional_roots(dir, "test", SourceKind::Test, &test_extra));
    for (gen_dir, kind) in [("target/generated-sources", SourceKind::Main), ("target/generated-test-sources", SourceKind::Test)] {
        if let Ok(entries) = std::fs::read_dir(dir.join(gen_dir)) {
            for e in entries.flatten().filter(|e| e.path().is_dir()) {
                roots.push(SourceRoot {
                    path: e.path(),
                    kind,
                    generated: true,
                    source_set: if kind == SourceKind::Main { "main".into() } else { "test".into() },
                });
            }
        }
    }
    roots
}

fn kotlin_settings(eff: &EffectivePom) -> KotlinSettings {
    let mut k = KotlinSettings::default();
    let plugin = eff.plugin("kotlin-maven-plugin");
    k.version = plugin
        .and_then(|p| p.version.clone())
        .or_else(|| eff.properties.get("kotlin.version").cloned())
        .or_else(|| {
            eff.dependencies
                .iter()
                .find(|d| d.module.group == "org.jetbrains.kotlin" && d.module.artifact.starts_with("kotlin-stdlib"))
                .and_then(|d| d.version.clone())
        });
    if let Some(p) = plugin {
        k.jvm_target = p.config_values("jvmTarget").into_iter().next();
        k.language_version = p.config_values("languageVersion").into_iter().next();
        k.api_version = p.config_values("apiVersion").into_iter().next();
        k.compiler_plugins = p.config_values("compilerPlugins");
        for arg in p.config_values("args") {
            if let Some(o) = arg.strip_prefix("-opt-in=") {
                k.opt_in.push(o.to_string());
            }
            if arg.starts_with("-Xexplicit-api") {
                k.explicit_api = true;
            }
        }
    }
    k.jvm_target = k.jvm_target.or_else(|| eff.properties.get("kotlin.compiler.jvmTarget").cloned());
    k.language_version = k.language_version.or_else(|| eff.properties.get("kotlin.compiler.languageVersion").cloned());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn multi_module_reactor() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "home/m2/org/lib/a/1/a-1.pom", "<project><groupId>org.lib</groupId><artifactId>a</artifactId><version>1</version></project>");
        write(d, "home/m2/org/lib/a/1/a-1.jar", "x");
        write(
            d,
            "proj/pom.xml",
            r#"<project><groupId>com.x</groupId><artifactId>root</artifactId><version>1.0</version><packaging>pom</packaging>
               <modules><module>core</module><module>app</module></modules></project>"#,
        );
        write(
            d,
            "proj/core/pom.xml",
            r#"<project><parent><groupId>com.x</groupId><artifactId>root</artifactId><version>1.0</version></parent><artifactId>core</artifactId>
               <dependencies><dependency><groupId>org.lib</groupId><artifactId>a</artifactId><version>1</version></dependency></dependencies>
               <build><plugins><plugin><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-maven-plugin</artifactId><version>1.9.0</version>
               <configuration><jvmTarget>17</jvmTarget><compilerPlugins><plugin>spring</plugin></compilerPlugins></configuration></plugin></plugins></build></project>"#,
        );
        write(
            d,
            "proj/app/pom.xml",
            r#"<project><parent><groupId>com.x</groupId><artifactId>root</artifactId><version>1.0</version></parent><artifactId>app</artifactId>
               <dependencies><dependency><groupId>com.x</groupId><artifactId>core</artifactId><version>${project.version}</version></dependency></dependencies></project>"#,
        );
        std::fs::create_dir_all(d.join("proj/app/src/main/kotlin")).unwrap();
        assert!(aggregates(&d.join("proj/pom.xml"), &d.join("proj/core")));

        // Point the repository system at our fake ~/.m2 via a file: repository.
        let mut opts = LoadOptions::isolated(d.join("cache"));
        opts.extra_repositories.push(Repository::new("fake", &format!("file://{}", d.join("home/m2").display())));
        let project = load(&d.join("proj"), &opts);
        let names: Vec<_> = project.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["com.x:root", "com.x:core", "com.x:app"]);
        let core = project.module("com.x:core").unwrap();
        assert_eq!(core.kotlin.version.as_deref(), Some("1.9.0"));
        assert_eq!(core.kotlin.jvm_target.as_deref(), Some("17"));
        assert_eq!(core.kotlin.compiler_plugins, vec!["spring"]);
        let app = project.module("com.x:app").unwrap();
        assert_eq!(app.module_dependencies, vec![ModuleDependency { module: "com.x:core".into(), scope: Scope::Compile }]);
        // core's dependency `a` reaches app transitively, but core itself is not a binary.
        assert_eq!(app.dependencies.iter().map(|a| a.coord.artifact.as_str()).collect::<Vec<_>>(), vec!["a"]);
        assert!(app.complete, "{:?}", app.problems);
        assert!(app.source_roots.iter().any(|r| r.path.ends_with("src/main/kotlin")));
    }
}
