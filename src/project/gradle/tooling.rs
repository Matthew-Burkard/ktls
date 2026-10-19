//! Project import through Gradle itself: an init script registers a task that
//! dumps source sets and resolved compile classpaths as JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::project::coords::Coordinate;
use crate::project::pom::Scope;
use crate::project::resolver::ResolvedArtifact;
use crate::project::{BuildSystem, KotlinSettings, LoadOptions, ModelSource, Module, ModuleDependency, Project, SourceKind, SourceRoot};

pub const INIT_SCRIPT: &str = r#"
import groovy.json.JsonOutput
import org.gradle.api.artifacts.component.ModuleComponentIdentifier
import org.gradle.api.artifacts.component.ProjectComponentIdentifier
import org.gradle.api.artifacts.result.UnresolvedDependencyResult

def kotlinLspOutput = System.getProperty("kotlinLsp.output")

def describeError = { Throwable t ->
    def parts = []
    def seen = new HashSet()
    while (t != null && seen.add(t)) {
        if (t.message) parts << t.message.readLines().first()
        t = t.cause
    }
    parts.unique().join(": ")
}

rootProject {
    tasks.register("kotlinLspModel") { task ->
        try { task.notCompatibleWithConfigurationCache("exports the IDE model") } catch (Throwable ignored) {}
        doLast {
            def resolveConfig = { p, String name, Map attrs ->
                def cfg = p.configurations.findByName(name)
                if (cfg == null) return null
                if (!cfg.canBeResolved) return null
                def out = [artifacts: [], errors: []]
                try {
                    def view = cfg.incoming.artifactView { v ->
                        v.lenient = true
                        if (attrs != null) {
                            v.attributes { a -> attrs.each { k, val -> a.attribute(Attribute.of(k, String), val) } }
                        }
                    }
                    view.artifacts.each { a ->
                        def id = a.id.componentIdentifier
                        def e = [file: a.file.absolutePath]
                        if (id instanceof ModuleComponentIdentifier) {
                            e.group = id.group; e.module = id.module; e.version = id.version
                        } else if (id instanceof ProjectComponentIdentifier) {
                            e.project = id.projectPath
                            try { e.build = id.build.buildPath } catch (Throwable ignored) {}
                        }
                        out.artifacts << e
                    }
                } catch (Throwable t) { out.errors << describeError(t) }
                try {
                    cfg.incoming.resolutionResult.allDependencies.each { d ->
                        if (d instanceof UnresolvedDependencyResult) out.errors << ("Could not resolve " + d.requested.displayName)
                    }
                } catch (Throwable ignored) {}
                return out
            }
            def projects = []
            rootProject.allprojects.each { p ->
                def info = [path: p.path, dir: p.projectDir.absolutePath, plugins: [], sourceSets: [], compilations: [], kotlin: [:], errors: []]
                ['org.jetbrains.kotlin.jvm', 'org.jetbrains.kotlin.multiplatform', 'org.jetbrains.kotlin.android',
                 'com.android.application', 'com.android.library', 'java', 'java-library',
                 'org.jetbrains.kotlin.plugin.serialization', 'org.jetbrains.kotlin.plugin.spring',
                 'org.jetbrains.kotlin.plugin.allopen', 'org.jetbrains.kotlin.plugin.noarg', 'org.jetbrains.kotlin.plugin.jpa',
                 'org.jetbrains.kotlin.plugin.compose', 'org.jetbrains.kotlin.kapt', 'com.google.devtools.ksp'].each { id ->
                    try { if (p.pluginManager.hasPlugin(id)) info.plugins << id } catch (Throwable ignored) {}
                }
                def kext = p.extensions.findByName('kotlin')
                if (kext != null) {
                    try { info.kotlin.version = kext.coreLibrariesVersion } catch (Throwable ignored) {}
                    try {
                        kext.sourceSets.each { ks ->
                            info.kotlin.sourceSets = (info.kotlin.sourceSets ?: []) + [[name: ks.name, dirs: ks.kotlin.srcDirs*.absolutePath]]
                        }
                    } catch (Throwable t) { info.errors << describeError(t) }
                    try {
                        if (kext.hasProperty('targets')) {
                            kext.targets.each { t ->
                                def platform = t.platformType.name()
                                t.compilations.each { c ->
                                    def entry = [target: t.name, platform: platform, name: c.name,
                                                 sourceSets: c.allKotlinSourceSets*.name]
                                    try { entry.classpath = resolveConfig(p, c.compileDependencyConfigurationName, null) } catch (Throwable e) { info.errors << describeError(e) }
                                    info.compilations << entry
                                }
                            }
                        }
                    } catch (Throwable t) { info.errors << describeError(t) }
                }
                def ss = p.extensions.findByName('sourceSets')
                if (ss != null && info.compilations.isEmpty()) {
                    ss.each { s ->
                        def dirs = []
                        try { dirs.addAll(s.java.srcDirs*.absolutePath) } catch (Throwable ignored) {}
                        try { def k = s.extensions.findByName('kotlin'); if (k != null) dirs.addAll(k.srcDirs*.absolutePath) } catch (Throwable ignored) {}
                        info.sourceSets << [name: s.name, dirs: dirs.unique(), classpath: resolveConfig(p, s.compileClasspathConfigurationName, null)]
                    }
                }
                def android = p.extensions.findByName('android')
                if (android != null) {
                    def attrs = ['artifactType': 'android-classes-jar']
                    def dirs = []
                    def testDirs = []
                    try { android.sourceSets.each { s ->
                        def d = []
                        try { d.addAll(s.java.srcDirs*.absolutePath) } catch (Throwable ignored) {}
                        try { d.addAll(s.kotlin.srcDirs*.absolutePath) } catch (Throwable ignored) {}
                        if (s.name.toLowerCase().contains('test')) testDirs.addAll(d) else dirs.addAll(d)
                    } } catch (Throwable ignored) {}
                    def boot = []
                    try { boot = android.bootClasspath*.absolutePath } catch (Throwable ignored) {}
                    info.sourceSets << [name: 'main', dirs: dirs.unique(), classpath: resolveConfig(p, 'debugCompileClasspath', attrs), boot: boot]
                    info.sourceSets << [name: 'test', dirs: testDirs.unique(), classpath: resolveConfig(p, 'debugUnitTestCompileClasspath', attrs), boot: boot]
                }
                projects << info
            }
            new File(kotlinLspOutput).text = JsonOutput.toJson([gradleVersion: gradle.gradleVersion, rootName: rootProject.name, projects: projects])
        }
    }
}
"#;

#[derive(Deserialize, Debug)]
struct Output {
    #[serde(rename = "gradleVersion")]
    gradle_version: String,
    #[serde(rename = "rootName")]
    root_name: Option<String>,
    projects: Vec<ProjectOut>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct ProjectOut {
    path: String,
    dir: String,
    plugins: Vec<String>,
    #[serde(rename = "sourceSets")]
    source_sets: Vec<SourceSetOut>,
    compilations: Vec<CompilationOut>,
    kotlin: KotlinOut,
    errors: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct KotlinOut {
    version: Option<String>,
    #[serde(rename = "sourceSets")]
    source_sets: Vec<NamedDirs>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct NamedDirs {
    name: String,
    dirs: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct SourceSetOut {
    name: String,
    dirs: Vec<String>,
    classpath: Option<ClasspathOut>,
    boot: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct CompilationOut {
    target: String,
    platform: String,
    name: String,
    #[serde(rename = "sourceSets")]
    source_sets: Vec<String>,
    classpath: Option<ClasspathOut>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct ClasspathOut {
    artifacts: Vec<ArtifactOut>,
    errors: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct ArtifactOut {
    file: String,
    group: Option<String>,
    module: Option<String>,
    version: Option<String>,
    project: Option<String>,
}

fn gradle_command(root: &Path) -> anyhow::Result<PathBuf> {
    let wrapper = root.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" });
    if wrapper.is_file() {
        return Ok(wrapper);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let g = dir.join("gradle");
            if g.is_file() {
                return Ok(g);
            }
        }
    }
    bail!("neither a Gradle wrapper nor `gradle` on PATH was found")
}

pub fn load(root: &Path, opts: &LoadOptions) -> anyhow::Result<Project> {
    let gradle = gradle_command(root)?;
    let work = opts.cache_dir.join("gradle-import");
    std::fs::create_dir_all(&work)?;
    let script = work.join("kotlin-lsp-init.gradle");
    std::fs::write(&script, INIT_SCRIPT)?;
    let out_file = work.join(format!("model-{}.json", crate::util::short_hash(&root.display().to_string())));
    let _ = std::fs::remove_file(&out_file);

    let mut cmd = Command::new(&gradle);
    cmd.current_dir(root)
        .arg("--init-script")
        .arg(&script)
        .arg("--quiet")
        .arg("--console=plain")
        .arg("--no-configuration-cache")
        .arg(format!("-DkotlinLsp.output={}", out_file.display()))
        .arg("kotlinLspModel")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if opts.offline {
        cmd.arg("--offline");
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", gradle.display()))?;
    // Drain output on threads so the pipes never fill up.
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let progress = opts.progress.clone();
    let out_thread = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(p) = &progress {
                p(&line);
            }
        }
    });
    let err_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut s = String::new();
        let _ = std::io::BufReader::new(stderr).read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("Gradle did not finish within 20 minutes");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = out_thread.join();
    let stderr = err_thread.join().unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).take(20).collect();
        bail!("Gradle failed ({status}): {}", tail.join("\n"));
    }
    let json = std::fs::read_to_string(&out_file).context("Gradle did not write the model")?;
    let output: Output = serde_json::from_str(&json).context("parsing the Gradle model")?;
    let mut project = convert(root, output);
    // Same build files as the static model, for change detection.
    let settings = super::model::read_settings(root);
    let mut files = settings.files.clone();
    for (_, dir) in &settings.projects {
        if let Some(bf) = super::model::build_file(dir) {
            files.push(bf);
        }
    }
    files.sort();
    files.dedup();
    project.build_files = files;
    Ok(project)
}

fn artifacts(cp: &ClasspathOut, scope: Scope) -> (Vec<ResolvedArtifact>, Vec<String>, Vec<(PathBuf, Scope)>) {
    let mut deps = Vec::new();
    let mut projects = Vec::new();
    let mut files = Vec::new();
    for a in &cp.artifacts {
        if let Some(p) = &a.project {
            projects.push(p.clone());
            continue;
        }
        match (&a.group, &a.module, &a.version) {
            (Some(g), Some(m), Some(v)) => deps.push(ResolvedArtifact {
                coord: Coordinate::new(g, m, v),
                scope,
                file: Some(PathBuf::from(&a.file)),
                direct: false,
                depth: 0,
                via: None,
            }),
            _ => files.push((PathBuf::from(&a.file), scope)),
        }
    }
    (deps, projects, files)
}

fn convert(root: &Path, out: Output) -> Project {
    let mut modules = Vec::new();
    for p in out.projects {
        let dir = PathBuf::from(&p.dir);
        let mut roots: Vec<SourceRoot> = Vec::new();
        let mut deps: Vec<ResolvedArtifact> = Vec::new();
        let mut module_deps: Vec<ModuleDependency> = Vec::new();
        let mut local: Vec<(PathBuf, Scope)> = Vec::new();
        let mut problems: Vec<String> = p.errors.clone();

        let add_cp = |cp: &Option<ClasspathOut>, scope: Scope, deps: &mut Vec<ResolvedArtifact>, module_deps: &mut Vec<ModuleDependency>, local: &mut Vec<(PathBuf, Scope)>, problems: &mut Vec<String>| {
            let Some(cp) = cp else { return };
            let (d, projects, files) = artifacts(cp, scope);
            for a in d {
                if !deps.iter().any(|x| x.coord == a.coord) {
                    deps.push(a);
                }
            }
            for pr in projects {
                if pr != p.path && !module_deps.iter().any(|m| m.module == pr) {
                    module_deps.push(ModuleDependency { module: pr, scope });
                }
            }
            for f in files {
                // The module's own compiled output is not a dependency.
                if f.0.starts_with(dir.join("build")) {
                    continue;
                }
                if !local.contains(&f) {
                    local.push(f);
                }
            }
            problems.extend(cp.errors.iter().cloned());
        };

        if !p.compilations.is_empty() {
            // Kotlin Multiplatform: use the JVM (or Android) target's compilations.
            let dirs_of: HashMap<&str, &Vec<String>> = p.kotlin.source_sets.iter().map(|s| (s.name.as_str(), &s.dirs)).collect();
            let jvm: Vec<&CompilationOut> = p.compilations.iter().filter(|c| c.platform == "jvm").collect();
            let chosen: Vec<&CompilationOut> = if jvm.is_empty() { p.compilations.iter().filter(|c| c.platform == "androidJvm").collect() } else { jvm };
            for c in &chosen {
                let kind = if c.name.to_lowercase().contains("test") { SourceKind::Test } else { SourceKind::Main };
                let scope = if kind == SourceKind::Test { Scope::Test } else { Scope::Compile };
                add_cp(&c.classpath, scope, &mut deps, &mut module_deps, &mut local, &mut problems);
                for ss in &c.source_sets {
                    for d in dirs_of.get(ss.as_str()).into_iter().flat_map(|v| v.iter()) {
                        let path = PathBuf::from(d);
                        if !roots.iter().any(|r| r.path == path) {
                            roots.push(SourceRoot { path, kind, generated: false, source_set: ss.clone() });
                        }
                    }
                }
            }
            // Source sets of other targets are still indexed (with the JVM classpath).
            for ss in &p.kotlin.source_sets {
                for d in &ss.dirs {
                    let path = PathBuf::from(d);
                    if path.is_dir() && !roots.iter().any(|r| r.path == path) {
                        let kind = if ss.name.ends_with("Test") { SourceKind::Test } else { SourceKind::Main };
                        roots.push(SourceRoot { path, kind, generated: false, source_set: ss.name.clone() });
                    }
                }
            }
            if chosen.is_empty() {
                problems.push("No JVM target: library dependencies are only approximated".into());
            }
        } else {
            for s in &p.source_sets {
                let kind = if s.name.to_lowercase().contains("test") { SourceKind::Test } else { SourceKind::Main };
                let scope = if kind == SourceKind::Test { Scope::Test } else { Scope::Compile };
                add_cp(&s.classpath, scope, &mut deps, &mut module_deps, &mut local, &mut problems);
                for b in &s.boot {
                    let f = (PathBuf::from(b), Scope::Provided);
                    if !local.contains(&f) {
                        local.push(f);
                    }
                }
                for d in &s.dirs {
                    let path = PathBuf::from(d);
                    if !roots.iter().any(|r| r.path == path) {
                        let generated = d.contains("/build/generated/");
                        roots.push(SourceRoot { path, kind, generated, source_set: s.name.clone() });
                    }
                }
            }
        }
        // Generated sources that exist on disk (kapt/ksp output directories).
        for (ss, kind) in [("main", SourceKind::Main), ("test", SourceKind::Test)] {
            for r in crate::project::conventional_roots(&dir, ss, kind, &[]) {
                if r.generated && !roots.iter().any(|x| x.path == r.path) {
                    roots.push(r);
                }
            }
        }
        if roots.is_empty() && deps.is_empty() {
            continue; // aggregator project
        }
        let kotlin = KotlinSettings {
            version: p.kotlin.version.clone(),
            multiplatform: p.plugins.iter().any(|x| x == "org.jetbrains.kotlin.multiplatform"),
            compiler_plugins: p
                .plugins
                .iter()
                .filter_map(|id| match id.as_str() {
                    "org.jetbrains.kotlin.plugin.serialization" => Some("serialization"),
                    "org.jetbrains.kotlin.plugin.spring" => Some("spring"),
                    "org.jetbrains.kotlin.plugin.allopen" => Some("allopen"),
                    "org.jetbrains.kotlin.plugin.noarg" => Some("noarg"),
                    "org.jetbrains.kotlin.plugin.jpa" => Some("jpa"),
                    "org.jetbrains.kotlin.plugin.compose" => Some("compose"),
                    "org.jetbrains.kotlin.kapt" => Some("kapt"),
                    "com.google.devtools.ksp" => Some("ksp"),
                    _ => None,
                })
                .map(str::to_string)
                .collect(),
            ..Default::default()
        };
        problems.sort();
        problems.dedup();
        modules.push(Module {
            name: p.path.clone(),
            dir,
            source_roots: roots,
            dependencies: deps,
            module_dependencies: module_deps,
            local_classpath: local,
            kotlin,
            java_release: None,
            complete: problems.is_empty(),
            problems,
        });
    }
    let _ = out.root_name;
    log::info!("Imported {} with Gradle {}", root.display(), out.gradle_version);
    Project {
        root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        modules,
        problems: Vec::new(),
        source: ModelSource::Tooling,
        build_files: Vec::new(),
        repositories: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_model_json() {
        let json = r#"{"gradleVersion":"8.5","rootName":"demo","projects":[
          {"path":":","dir":"/r","plugins":[],"sourceSets":[],"compilations":[],"kotlin":{},"errors":[]},
          {"path":":app","dir":"/r/app","plugins":["org.jetbrains.kotlin.jvm"],
           "sourceSets":[{"name":"main","dirs":["/r/app/src/main/java","/r/app/src/main/kotlin"],
              "classpath":{"artifacts":[
                {"file":"/c/kotlin-stdlib-1.9.0.jar","group":"org.jetbrains.kotlin","module":"kotlin-stdlib","version":"1.9.0"},
                {"file":"/r/core/build/libs/core.jar","project":":core"},
                {"file":"/r/app/libs/local.jar"}],"errors":[]}},
             {"name":"test","dirs":["/r/app/src/test/kotlin"],"classpath":{"artifacts":[
                {"file":"/c/junit.jar","group":"junit","module":"junit","version":"4.13"}],"errors":["Could not resolve x:y:1"]}}],
           "compilations":[],"kotlin":{"version":"1.9.0","sourceSets":[]},"errors":[]}
        ]}"#;
        let out: Output = serde_json::from_str(json).unwrap();
        let project = convert(Path::new("/r"), out);
        assert_eq!(project.modules.len(), 1);
        let app = &project.modules[0];
        assert_eq!(app.name, ":app");
        assert_eq!(app.kotlin.version.as_deref(), Some("1.9.0"));
        assert_eq!(app.dependencies.len(), 2);
        assert_eq!(app.module_dependencies[0].module, ":core");
        assert_eq!(app.local_classpath.len(), 1);
        assert!(!app.complete);
        assert_eq!(app.classpath(SourceKind::Main).len(), 2);
        assert_eq!(app.classpath(SourceKind::Test).len(), 3);
    }
}
