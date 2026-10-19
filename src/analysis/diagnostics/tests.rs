use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::analysis::{AnalysisHost, Workspace};
use crate::project::coords::Coordinate;
use crate::project::pom::Scope;
use crate::project::resolver::ResolvedArtifact;
use crate::project::{BuildSystem, ModelSource, Module, ModuleDependency, Project, SourceKind, SourceRoot};

fn stdlib() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".sdkman/candidates/kotlin/current/lib/kotlin-stdlib.jar")).filter(|p| p.is_file())
}

struct Fixture {
    host: AnalysisHost,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    /// Two modules: `:lib` and `:app` (depends on `:lib`), both with the stdlib.
    fn new() -> Option<Fixture> {
        let stdlib = stdlib()?;
        let jdk = crate::project::jdk::find(None, None)?;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dep = ResolvedArtifact {
            coord: Coordinate::new("org.jetbrains.kotlin", "kotlin-stdlib", "2.0.0"),
            scope: Scope::Compile,
            file: Some(stdlib),
            direct: true,
            depth: 0,
            via: None,
        };
        let module = |name: &str, deps: Vec<ModuleDependency>| Module {
            name: name.into(),
            dir: root.join(name.trim_start_matches(':')),
            source_roots: vec![
                SourceRoot { path: root.join(name.trim_start_matches(':')).join("src/main/kotlin"), kind: SourceKind::Main, generated: false, source_set: "main".into() },
                SourceRoot { path: root.join(name.trim_start_matches(':')).join("src/test/kotlin"), kind: SourceKind::Test, generated: false, source_set: "test".into() },
            ],
            dependencies: vec![dep.clone()],
            module_dependencies: deps,
            complete: true,
            ..Default::default()
        };
        let project = Project {
            root: root.clone(),
            build_system: BuildSystem::Gradle,
            modules: vec![module(":lib", vec![]), module(":app", vec![ModuleDependency { module: ":lib".into(), scope: Scope::Compile }])],
            problems: vec![],
            source: ModelSource::Static,
            build_files: vec![],
            repositories: vec![],
        };
        let mut host = AnalysisHost::default();
        host.set_workspace(Workspace::new(vec![project], Some(jdk)));
        Some(Fixture { host, root, _tmp: tmp })
    }

    fn add(&mut self, rel: &str, text: &str) -> PathBuf {
        let p = self.root.join(rel);
        self.host.set_file(p.clone(), Arc::from(text), Some(1));
        p
    }

    fn diagnostics(&mut self, path: &std::path::Path) -> Vec<String> {
        let snap = self.host.snapshot();
        let file = snap.file(path).unwrap().clone();
        let (diags, coverage) = file_diagnostics(&snap, path);
        assert_eq!(coverage, Coverage::Full);
        diags
            .iter()
            .map(|d| format!("{}: {}", &file.text[usize::from(d.range.start())..usize::from(d.range.end())], d.code))
            .collect()
    }
}

#[test]
fn resolves_library_and_source_types() {
    let Some(mut fx) = Fixture::new() else { return };
    fx.add(
        "lib/src/main/kotlin/com/acme/lib/Api.kt",
        "package com.acme.lib\n\nopen class Base {\n    class Nested\n}\ninterface Repository<T>\ntypealias Names = List<String>\nfun helper(): Int = 1\n",
    );
    let app = fx.add(
        "app/src/main/kotlin/com/acme/app/Main.kt",
        r#"package com.acme.app

import com.acme.lib.Base
import com.acme.lib.Repository
import com.acme.lib.Names
import com.acme.lib.helper
import java.util.concurrent.ConcurrentHashMap
import java.util.*
import kotlin.collections.Map.Entry as MapEntry
import com.acme.lib.DoesNotExist
import org.nonexistent.Thing

class Impl : Base(), Repository<String>, Comparable<Impl> {
    override fun compareTo(other: Impl): Int = 0
    fun nested(): Nested = TODO()
    fun <T : Any> generic(x: T, y: List<T>): Map<String, T?> = emptyMap()
    fun javaTypes(m: ConcurrentHashMap<String, Int>, l: LinkedList<String>, e: MapEntry<String, Int>, t: Thread.State): Names = listOf()
    fun builtins(a: Any, n: Nothing?, u: Unit, s: Sequence<Int>, r: IntRange, re: Regex, f: (Int) -> String, ex: Exception) {}
    fun qualified(x: java.io.File, y: kotlin.collections.ArrayList<String>) {}
    fun bad(x: Missing, y: java.util.Missing2, z: List<AlsoMissing>) {
        class Local
        val l: Local = Local()
        val q: Impl.Inner = TODO()
    }
    inner class Inner
    companion object {
        class FromCompanion
    }
    fun companionType(): FromCompanion = TODO()
}

@JvmInline
value class Id(val raw: Long)

@Suppress("UNUSED")
fun main(args: Array<String>) {
    val x = helper()
    val map = HashMap<String, MutableList<Int>>()
}
"#,
    );
    let diags = fx.diagnostics(&app);
    assert_eq!(
        diags,
        vec!["DoesNotExist: UNRESOLVED_REFERENCE", "nonexistent: UNRESOLVED_REFERENCE", "Missing: UNRESOLVED_REFERENCE", "Missing2: UNRESOLVED_REFERENCE", "AlsoMissing: UNRESOLVED_REFERENCE"]
    );
}

#[test]
fn module_visibility() {
    let Some(mut fx) = Fixture::new() else { return };
    fx.add("app/src/main/kotlin/a/AppOnly.kt", "package a\nclass AppOnly\n");
    fx.add("app/src/test/kotlin/a/TestOnly.kt", "package a\nclass TestOnly\n");
    let lib = fx.add("lib/src/main/kotlin/a/Lib.kt", "package a\nclass Lib(val x: AppOnly)\n");
    assert_eq!(fx.diagnostics(&lib), vec!["AppOnly: UNRESOLVED_REFERENCE"], "lib must not see app");
    let main = fx.add("app/src/main/kotlin/a/Main.kt", "package a\nclass Main(val l: Lib, val t: TestOnly)\n");
    assert_eq!(fx.diagnostics(&main), vec!["TestOnly: UNRESOLVED_REFERENCE"], "main must not see test sources");
    let test = fx.add("app/src/test/kotlin/a/MainTest.kt", "package a\nclass MainTest(val l: Lib, val t: TestOnly, val m: Main)\n");
    assert!(fx.diagnostics(&test).is_empty());
}

#[test]
fn redeclaration_and_syntax_errors() {
    let Some(mut fx) = Fixture::new() else { return };
    fx.add("app/src/main/kotlin/a/One.kt", "package a\nclass Dup\n");
    let two = fx.add("app/src/main/kotlin/a/Two.kt", "package a\nclass Dup\nfun broken( {\n");
    let d = fx.diagnostics(&two);
    assert!(d.iter().any(|d| d == "Dup: REDECLARATION"), "{d:?}");
    assert!(d.iter().any(|d| d.ends_with("SYNTAX")), "{d:?}");
}

#[test]
fn edits_reuse_the_index() {
    let Some(mut fx) = Fixture::new() else { return };
    let p = fx.add("app/src/main/kotlin/a/A.kt", "package a\nclass A { fun f() = 1 }\n");
    let s1 = fx.host.snapshot();
    fx.host.set_file(p.clone(), Arc::from("package a\nclass A { fun f() = 2 + 3 }\n"), Some(2));
    let s2 = fx.host.snapshot();
    assert!(Arc::ptr_eq(&s1.index, &s2.index), "body-only edits keep the index");
    fx.host.set_file(p, Arc::from("package a\nclass B\n"), Some(3));
    let s3 = fx.host.snapshot();
    assert!(!Arc::ptr_eq(&s2.index, &s3.index));
}

#[test]
fn java_sources_are_visible_to_kotlin() {
    let Some(mut fx) = Fixture::new() else { return };
    fx.host.set_file(
        fx.root.join("app/src/main/kotlin/j/Helper.java"),
        Arc::from("package j;\npublic class Helper { public static class Inner {} }\n"),
        Some(1),
    );
    let k = fx.add("app/src/main/kotlin/k/Use.kt", "package k\nimport j.Helper\nfun f(h: Helper, i: Helper.Inner, x: j.Helper) {}\n");
    assert!(fx.diagnostics(&k).is_empty());
}
