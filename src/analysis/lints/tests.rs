use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::analysis::{AnalysisHost, Workspace};
use crate::project::coords::Coordinate;
use crate::project::pom::Scope;
use crate::project::resolver::ResolvedArtifact;
use crate::project::{BuildSystem, ModelSource, Project, SourceKind, SourceRoot};

fn stdlib() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".sdkman/candidates/kotlin/current/lib/kotlin-stdlib.jar")).filter(|p| p.is_file())
}

struct Fixture {
    host: AnalysisHost,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    /// One module with the stdlib (types available when a JDK is found).
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let deps: Vec<ResolvedArtifact> = stdlib()
            .map(|f| ResolvedArtifact {
                coord: Coordinate::new("org.jetbrains.kotlin", "kotlin-stdlib", "2.0.0"),
                scope: Scope::Compile,
                file: Some(f),
                direct: true,
                depth: 0,
                via: None,
            })
            .into_iter()
            .collect();
        let module = Module {
            name: ":app".into(),
            dir: root.clone(),
            source_roots: vec![
                SourceRoot { path: root.join("src/main/kotlin"), kind: SourceKind::Main, generated: false, source_set: "main".into() },
                SourceRoot { path: root.join("src/test/kotlin"), kind: SourceKind::Test, generated: false, source_set: "test".into() },
            ],
            dependencies: deps,
            complete: true,
            ..Default::default()
        };
        let project = Project {
            root: root.clone(),
            build_system: BuildSystem::Gradle,
            modules: vec![module],
            problems: vec![],
            source: ModelSource::Static,
            build_files: vec![],
            repositories: vec![],
        };
        let mut host = AnalysisHost::default();
        host.set_workspace(Workspace::new(vec![project], crate::project::jdk::find(None, None)));
        Fixture { host, root, _tmp: tmp }
    }

    fn typed(&self) -> bool {
        stdlib().is_some() && crate::project::jdk::find(None, None).is_some()
    }

    fn run(&mut self, rel: &str, text: &str) -> (Vec<Lint>, String) {
        let p = self.root.join(rel);
        self.host.set_file(p.clone(), Arc::from(text), Some(1));
        let snap = self.host.snapshot();
        (crate::analysis::diagnostics::lints(&snap, &p), text.to_string())
    }
}

/// `rule: flagged text` for every finding in `src/main/kotlin/a/A.kt`.
fn lint(text: &str) -> Vec<String> {
    lint_at("src/main/kotlin/a/A.kt", text)
}

fn lint_at(rel: &str, text: &str) -> Vec<String> {
    let (lints, text) = Fixture::new().run(rel, text);
    lints.iter().map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect()
}

/// The file after applying the first fix of `rule`.
fn fixed(text: &str, rule: &str) -> String {
    let (lints, text) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    let l = lints.iter().find(|l| l.rule.id == rule).unwrap_or_else(|| panic!("no {rule} in {lints:#?}"));
    apply(&text, &l.fix.as_ref().expect("no fix").edits)
}

fn apply(text: &str, edits: &[Edit]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
    let mut out = text.to_string();
    for e in edits {
        out.replace_range(usize::from(e.range.start())..usize::from(e.range.end()), &e.text);
    }
    out
}

#[track_caller]
fn has(found: &[String], expected: &str) {
    assert!(found.iter().any(|f| f == expected), "missing {expected:?} in {found:#?}");
}

#[test]
fn idiomatic_code_is_clean() {
    let found = lint(
        r#"package a

import kotlin.math.max
import kotlin.properties.Delegates

/** Uses [max]. */
class Point(val x: Int, private val y: Int) {
    var label: String = ""
        private set
    val sum get() = x + y
    var observed: Int by Delegates.notNull()

    fun scaled(factor: Int): Point {
        var acc = 0
        for (i in 0 until factor) acc += x
        val (first, _) = Pair(acc, y)
        return Point(first, y * factor)
    }

    override fun equals(other: Any?) = other is Point && other.x == x
    override fun hashCode() = x

    companion object {
        const val MAX_SIZE = 10
        val DEFAULT = Point(0, 0)
        fun of(vararg xs: Int): List<Point> = xs.map { Point(it, it) }
    }
}

enum class Color { RED, Green }

private fun helper(s: String) = s.length

fun use(p: Point, xs: List<Int>, m: Map<String, Int>): Int {
    val names = listOf("a", "b")
    if (xs.isEmpty()) return helper(names.first())
    val total = xs.fold(0) { a, b -> a + b }
    println("total=${total}s ${p.x} $total")
    return m["a"] ?: total
}
"#,
    );
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn unused_code() {
    let found = lint(
        r#"package a

import kotlin.math.max
import kotlin.math.min
import kotlin.math.abs

fun f(): Int {
    val unused = 1
    var never = 2
    val read = 3
    return read + never + min(1, 2)
}

private fun privateUnused() {}
private fun recursive(n: Int): Int = if (n == 0) 0 else recursive(n - 1)
private val unusedProp = 1
private class Unused
class Holder(private val dead: Int, private val alive: Int) {
    fun get() = alive
}

private fun withParams(used: Int, notUsed: String): Int = used
fun callsIt() = withParams(1, "")
fun publicParams(notReported: Int) = 1
"#,
    );
    has(&found, "UnusedImport: import kotlin.math.max");
    has(&found, "UnusedImport: import kotlin.math.abs");
    has(&found, "UNUSED_VARIABLE: unused");
    has(&found, "CanBeVal: var");
    has(&found, "UnusedSymbol: privateUnused");
    has(&found, "UnusedSymbol: recursive");
    has(&found, "UnusedSymbol: unusedProp");
    has(&found, "UnusedSymbol: Unused");
    has(&found, "UnusedSymbol: dead");
    has(&found, "UNUSED_PARAMETER: notUsed");
    assert!(!found.iter().any(|f| f.contains("min") || f.contains("read") || f.contains("alive") || f.contains("notReported") || f.contains("withParams")), "{found:#?}");

    assert_eq!(fixed("package a\n\nimport kotlin.math.max\nfun f() = 1\n", "UnusedImport"), "package a\n\nfun f() = 1\n");
    assert_eq!(fixed("fun f() {\n    val x = 1\n    println()\n}\n", "UNUSED_VARIABLE"), "fun f() {\n    println()\n}\n");
    assert_eq!(fixed("fun f() {\n    val x = compute()\n}\nfun compute() = 1\n", "UNUSED_VARIABLE"), "fun f() {\n    compute()\n}\nfun compute() = 1\n");
    assert_eq!(fixed("fun f(): Int {\n    var x = 1\n    return x\n}\n", "CanBeVal"), "fun f(): Int {\n    val x = 1\n    return x\n}\n");
}

#[test]
fn writes_keep_var() {
    let found = lint("fun f(): Int {\n    var a = 1\n    var b = 1\n    var c = 1\n    var d = 1\n    a = 2\n    b += 1\n    c++\n    run { d-- }\n    return a + b + c + d\n}\n");
    assert!(!found.iter().any(|f| f.starts_with("CanBeVal")), "{found:#?}");
}

#[test]
fn redundant_code() {
    let found = lint(
        r#"package a;

public class A() {
    val x: Int = 1
        get() = field
    var y = 1
        set(value) { field = value }
    init {}
    fun `ok`(): Unit {
        val s = "${x}"
        foo() { }
        Unit
        println()
        return Unit
    }
    fun foo(f: () -> Unit) = f()
    final fun g() = 1
}
class Empty {}
interface I {
    abstract fun f()
    open fun g() {}
}
fun t(c: Boolean): Boolean {
    if (c) return true else return false
}
"#,
    );
    has(&found, "RedundantSemicolon: ;");
    has(&found, "RedundantVisibilityModifier: public");
    has(&found, "RemoveEmptyPrimaryConstructor: ()");
    has(&found, "RedundantGetter: get() = field");
    has(&found, "RedundantSetter: set(value) { field = value }");
    has(&found, "RedundantEmptyInitializerBlock: init {}");
    has(&found, "RemoveRedundantBackticks: `ok`");
    has(&found, "RedundantUnitReturnType: Unit");
    has(&found, "RemoveCurlyBracesFromTemplate: ${x}");
    has(&found, "RemoveEmptyParenthesesFromLambdaCall: ()");
    has(&found, "RedundantUnitExpression: Unit");
    has(&found, "RedundantModalityModifier: final");
    has(&found, "RedundantModalityModifier: abstract");
    has(&found, "RedundantModalityModifier: open");
    has(&found, "RemoveEmptyClassBody: {}");
    has(&found, "RedundantIf: if");

    assert_eq!(fixed("val s = \"${x}y ${y}\"\nval x = 1\nval y = 2\n", "RemoveCurlyBracesFromTemplate"), "val s = \"${x}y $y\"\nval x = 1\nval y = 2\n");
    assert_eq!(fixed("fun f(c: Boolean) = if (c) false else true\n", "RedundantIf"), "fun f(c: Boolean) = !c\n");
    assert_eq!(fixed("fun f(a: Int) = if (a > 1) true else false\n", "RedundantIf"), "fun f(a: Int) = a > 1\n");
    assert_eq!(fixed("class A {}\n", "RemoveEmptyClassBody"), "class A\n");
    assert_eq!(fixed("fun f(): Unit {\n}\n", "RedundantUnitReturnType"), "fun f() {\n}\n");
    assert_eq!(fixed("fun f(): Int {\n    val r = 1 + 2\n    return r\n}\n", "UnnecessaryVariable"), "fun f(): Int {\n    return 1 + 2\n}\n");
}

#[test]
fn semicolons_that_matter() {
    let found = lint("enum class E { A, B; fun f() = 1 }\nfun g() { val a = 1; val b = 2\n    println(a + b) }\nfun h(x: Int) {\n    while (x > 0);\n}\n");
    assert!(!found.iter().any(|f| f.starts_with("RedundantSemicolon")), "{found:#?}");
}

#[test]
fn style_suggestions() {
    let f = Fixture::new();
    if !f.typed() {
        return;
    }
    let found = lint(
        r#"package a

val GREETING = "hi"

fun f(xs: List<Int>, s: String, m: MutableMap<String, Int>): Int {
    for (i in 0 until xs.size) println(i)
    for (i in 0..s.length - 1) println(i)
    for (i in 1..10 - 1) println(i)
    if (xs.size == 0) return 0
    if (s.length > 0) println("$s")
    println(xs.get(0))
    m.set("a", 1)
    println(xs.size.plus(1))
    if (s.isEmpty()) {}
    return 1
}

fun g(): Int {
    return 42
}
"#,
    );
    has(&found, "MayBeConstant: GREETING");
    has(&found, "ReplaceManualRangeWithIndicesCalls: 0 until xs.size");
    has(&found, "ReplaceManualRangeWithIndicesCalls: 0..s.length - 1");
    has(&found, "ReplaceRangeToWithRangeUntil: 1..10 - 1");
    has(&found, "ReplaceSizeZeroCheckWithIsEmpty: xs.size == 0");
    has(&found, "ReplaceSizeCheckWithIsNotEmpty: s.length > 0");
    has(&found, "RemoveSingleExpressionStringTemplate: \"$s\"");
    has(&found, "ReplaceGetOrSet: get");
    has(&found, "ReplaceGetOrSet: set");
    has(&found, "ReplaceCallWithBinaryOperator: plus");
    has(&found, "ControlFlowWithEmptyBody: if");
    has(&found, "UseExpressionBody: return");

    assert_eq!(fixed("fun f(xs: List<Int>) = xs.get(0)\n", "ReplaceGetOrSet"), "fun f(xs: List<Int>) = xs[0]\n");
    assert_eq!(fixed("fun f(m: MutableMap<String, Int>) {\n    m.set(\"a\", 1)\n}\n", "ReplaceGetOrSet"), "fun f(m: MutableMap<String, Int>) {\n    m[\"a\"] = 1\n}\n");
    assert_eq!(fixed("fun f(a: Int, b: Int) = a.times(b + 1) * 2\n", "ReplaceCallWithBinaryOperator"), "fun f(a: Int, b: Int) = (a * (b + 1)) * 2\n");
    assert_eq!(fixed("fun f(xs: Array<Int>) = xs.size != 0\n", "ReplaceSizeCheckWithIsNotEmpty"), "fun f(xs: Array<Int>) = xs.isNotEmpty()\n");
    assert_eq!(fixed("fun g(): Int {\n    return 42\n}\n", "UseExpressionBody"), "fun g(): Int = 42\n");
    assert_eq!(fixed("val X = 1\n", "MayBeConstant"), "const val X = 1\n");

    // Custom types with `size` are not collections.
    let found = lint("class Box { val size = 0 }\nfun f(b: Box) = b.size == 0\nfun g(b: Box) = 0 until b.size\n");
    assert!(!found.iter().any(|f| f.contains("IsEmpty") || f.contains("Indices")), "{found:#?}");
}

#[test]
fn probable_bugs() {
    let f = Fixture::new();
    let found = lint(
        r#"package a

class E(val x: Int) {
    override fun equals(other: Any?) = other is E
}
data class D(val xs: IntArray, val ys: Array<String>?)

fun f(d: Double): Boolean {
    var v = 1
    v = v
    IllegalStateException("oops")
    return d == Double.NaN
}
"#,
    );
    has(&found, "EqualsOrHashCode: E");
    has(&found, "ArrayInDataClass: IntArray");
    has(&found, "ArrayInDataClass: Array<String>?");
    has(&found, "ConvertNaNEquality: d == Double.NaN");
    if f.typed() {
        has(&found, "SelfAssignment: v = v");
        has(&found, "ThrowableNotThrown: IllegalStateException");
        assert_eq!(fixed("fun f() {\n    IllegalStateException(\"x\")\n}\n", "ThrowableNotThrown"), "fun f() {\n    throw IllegalStateException(\"x\")\n}\n");
        // A value of a lambda is used.
        let found = lint("fun f() = run { IllegalStateException(\"x\") }\n");
        assert!(found.is_empty(), "{found:#?}");
    }
    assert_eq!(fixed("fun f(d: Double) = d != Double.NaN\n", "ConvertNaNEquality"), "fun f(d: Double) = !d.isNaN()\n");
}

#[test]
fn naming() {
    let found = lint(
        r#"package a

class lower_case
object Upper
fun Bad_Name() {}
fun Factory(): Factory = TODO()
class Factory
class P { val Bad_prop = 1 }
val top_level_ok = 1
const val notScreaming = 1
private val _backing = 1
val CONSTANT_LIKE = 1
fun f() {
    val Local = 1
    println(Local)
}
enum class E { OK, AlsoOk, not_ok }
fun `backticked name`() {}
"#,
    );
    has(&found, "ClassName: lower_case");
    has(&found, "FunctionName: Bad_Name");
    has(&found, "PropertyName: Bad_prop");
    has(&found, "ConstPropertyName: notScreaming");
    has(&found, "LocalVariableName: Local");
    has(&found, "EnumEntryName: not_ok");
    let names = ["ClassName", "FunctionName", "PropertyName", "ConstPropertyName", "LocalVariableName", "EnumEntryName"];
    for ok in ["Upper", "Factory", "_backing", "CONSTANT_LIKE", "top_level_ok", "OK", "AlsoOk", "backticked"] {
        assert!(!found.iter().any(|f| f.contains(ok) && names.iter().any(|n| f.starts_with(&format!("{n}:")))), "{ok}: {found:#?}");
    }

    let found = lint_at("src/test/kotlin/a/T.kt", "package a\nfun parses_input() {}\n");
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn packages_and_directories() {
    has(&lint_at("src/main/kotlin/a/b/A.kt", "package x.y\n"), "PackageDirectoryMismatch: x.y");
    has(&lint_at("src/main/kotlin/a/A.kt", "package Bad.a\n"), "PackageName: Bad.a");
    // The common root package may be omitted.
    assert!(lint_at("src/main/kotlin/b/A.kt", "package org.example.b\n").is_empty());
    assert!(lint_at("src/main/kotlin/A.kt", "package org.example\n").is_empty());
}

#[test]
fn suppression() {
    let found = lint(
        r#"@file:Suppress("ClassName")
package a

class lower_case

@Suppress("unused")
fun f() {
    val x = 1
}

fun g() {
    //noinspection UNUSED_VARIABLE
    val y = 1
    @Suppress("UNUSED_VARIABLE") val z = 1
    val reported = 1
}
"#,
    );
    assert_eq!(found, vec!["UNUSED_VARIABLE: reported"]);

    let text = "package a\n\n@Suppress(\"X\")\nprivate fun f(p: Int) {\n    val x = 1\n}\nfun g() = f(1)\n";
    let (lints, text) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    let root = crate::syntax::parse(&text, false).syntax();
    let local = lints.iter().find(|l| l.rule.id == "UNUSED_VARIABLE").unwrap();
    let fixes = suppress_fixes(&root, &text, local);
    assert_eq!(fixes[0].title, "Suppress 'UNUSED_VARIABLE' for property 'x'");
    assert_eq!(apply(&text, &fixes[0].edits), "package a\n\n@Suppress(\"X\")\nprivate fun f(p: Int) {\n    @Suppress(\"UNUSED_VARIABLE\")\n    val x = 1\n}\nfun g() = f(1)\n");
    assert_eq!(apply(&text, &fixes[1].edits), format!("@file:Suppress(\"UNUSED_VARIABLE\")\n\n{text}"));
    let param = lints.iter().find(|l| l.rule.id == "UNUSED_PARAMETER").unwrap();
    let fixes = suppress_fixes(&root, &text, param);
    assert_eq!(apply(&text, &fixes[0].edits), "package a\n\n@Suppress(\"X\", \"UNUSED_PARAMETER\")\nprivate fun f(p: Int) {\n    val x = 1\n}\nfun g() = f(1)\n");
}

#[test]
fn configuration() {
    let mut f = Fixture::new();
    let mut rules = HashMap::new();
    rules.insert("unused".to_string(), Level::Off);
    rules.insert("UNUSED_VARIABLE".to_string(), Level::Error);
    f.host.set_lint_config(LintConfig { enabled: true, rules });
    let (lints, _) = f.run("src/main/kotlin/a/A.kt", "package a\nimport kotlin.math.max\nfun f() {\n    val x = 1\n}\n");
    let ids: Vec<(&str, Level)> = lints.iter().map(|l| (l.rule.id, l.level)).collect();
    assert_eq!(ids, vec![("UNUSED_VARIABLE", Level::Error)]);
}

#[test]
fn rule_ids_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for r in RULES {
        assert!(seen.insert(r.id), "duplicate {}", r.id);
    }
}
