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
        Fixture::with_module(|_| {})
    }

    /// Like `new`, with the module adjusted (compiler options, explicit API mode).
    fn with_module(adjust: impl FnOnce(&mut Module)) -> Fixture {
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
        let mut module = module;
        adjust(&mut module);
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

/// The file after applying the fix of `rule` titled `title` (the first fix or an alternative).
fn fixed_with(text: &str, rule: &str, title: &str) -> String {
    let (lints, text) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    let l = lints.iter().find(|l| l.rule.id == rule).unwrap_or_else(|| panic!("no {rule} in {lints:#?}"));
    let fix = l.fix.iter().chain(&l.alternatives).find(|f| f.title == title).unwrap_or_else(|| panic!("no fix '{title}' in {l:#?}"));
    apply(&text, &fix.edits)
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
    // Code actions (`until` → `..<`) are offered, not reported.
    let found: Vec<_> = found.into_iter().filter(|f| !f.starts_with("ReplaceUntilWithRangeUntil")).collect();
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
    assert!(!found.iter().any(|f| f.contains("min") || f.contains("read") || f.contains("alive") || f.contains("withParams")), "{found:#?}");

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

// ---------------------------------------------------------------- compiler checks

/// Only compiler-category findings.
fn compiler(text: &str) -> Vec<String> {
    let (lints, text) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    lints
        .iter()
        .filter(|l| l.rule.category == Category::Compiler)
        .map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())]))
        .collect()
}

#[test]
fn compiler_declaration_errors() {
    let found = compiler(
        r#"package a

private private fun twice() {}
abstract final class Both
class Lates {
    lateinit val a: String
    lateinit var b: Int
    lateinit var c: String?
    lateinit var d: String
        private set
    const val e = 1
    abstract fun f()
    abstract val g: Int = 1
    fun noBody()
}
interface I constructor() {
    init {}
    val p: Int = 1
}
interface J : Any()
data class Empty()
data class NotProp(val a: Int, b: Int)
fun topNoBody()
val uninitialized: Int
class Members {
    val missing: Int
    val assigned: Int
    init { assigned = 1 }
}
class Dup {
    val x = 1
    val x = 2
    fun f(a: Int) {}
    fun f(a: Int) {}
    fun <T : CharSequence> g(t: T) {}
    fun <T : Number> g(t: T) {}
}
object Obj { open fun o() {} }
abstract class WithBody { abstract fun body() {} }
"#,
    );
    has(&found, "REPEATED_MODIFIER: private");
    has(&found, "INCOMPATIBLE_MODIFIERS: final");
    has(&found, "INAPPLICABLE_LATEINIT_MODIFIER: lateinit");
    assert_eq!(found.iter().filter(|f| f.starts_with("INAPPLICABLE_LATEINIT")).count(), 3, "{found:#?}");
    has(&found, "CONST_VAL_NOT_TOP_LEVEL_OR_OBJECT: e");
    has(&found, "ABSTRACT_FUNCTION_IN_NON_ABSTRACT_CLASS: f");
    has(&found, "ABSTRACT_PROPERTY_WITH_INITIALIZER: g");
    has(&found, "ABSTRACT_PROPERTY_IN_NON_ABSTRACT_CLASS: g");
    has(&found, "NON_ABSTRACT_FUNCTION_WITH_NO_BODY: noBody");
    has(&found, "CONSTRUCTOR_IN_INTERFACE: constructor()");
    has(&found, "ANONYMOUS_INITIALIZER_IN_INTERFACE: init");
    has(&found, "PROPERTY_INITIALIZER_IN_INTERFACE: p");
    has(&found, "SUPERTYPE_INITIALIZED_IN_INTERFACE: Any()");
    has(&found, "DATA_CLASS_WITHOUT_PARAMETERS: Empty");
    has(&found, "DATA_CLASS_NOT_PROPERTY_PARAMETER: b");
    has(&found, "NON_MEMBER_FUNCTION_NO_BODY: topNoBody");
    has(&found, "MUST_BE_INITIALIZED: uninitialized");
    has(&found, "MUST_BE_INITIALIZED_OR_BE_ABSTRACT: missing");
    has(&found, "REDECLARATION: x");
    has(&found, "CONFLICTING_OVERLOADS: f");
    has(&found, "NON_FINAL_MEMBER_IN_OBJECT: open");
    has(&found, "ABSTRACT_FUNCTION_WITH_BODY: body");
    for ok in ["assigned", "d", "g"] {
        assert!(!found.iter().any(|f| f.starts_with("MUST_BE") && f.ends_with(&format!(": {ok}"))), "{ok}: {found:#?}");
    }
    assert!(!found.iter().any(|f| f == "CONFLICTING_OVERLOADS: g"), "{found:#?}");
}

#[test]
fn compiler_class_hierarchy_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

class Final
open class Base(val x: Int)
interface Shape { fun area(): Double; val name: String }
abstract class Animal { abstract fun sound(): String; open fun legs() = 4 }

class A : Final()
class B : Base
class C : Base(1), Animal()
interface D : Base
class Square : Shape {
    override fun area() = 1.0
}
class Dog : Animal()
object Cat : Animal() {
    override fun sound() = "meow"
    override fun purr() {}
}
class Delegated(s: Shape) : Shape by s
"#,
    );
    has(&found, "FINAL_SUPERTYPE: Final");
    has(&found, "SUPERTYPE_NOT_INITIALIZED: Base");
    has(&found, "MANY_CLASSES_IN_SUPERTYPE_LIST: Animal");
    has(&found, "INTERFACE_WITH_SUPERCLASS: Base");
    has(&found, "ABSTRACT_MEMBER_NOT_IMPLEMENTED: Square");
    has(&found, "ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED: Dog");
    has(&found, "NOTHING_TO_OVERRIDE: purr");
    assert!(!found.iter().any(|f| f.contains("Delegated") || f.contains("Cat") && f.starts_with("ABSTRACT")), "{found:#?}");

    // The fix implements the missing members.
    let text = "package a\n\ninterface Shape { fun area(): Double }\nclass Square : Shape {\n}\n";
    assert_eq!(
        fixed(text, "ABSTRACT_MEMBER_NOT_IMPLEMENTED"),
        "package a\n\ninterface Shape { fun area(): Double }\nclass Square : Shape {\n    override fun area(): Double {\n        TODO(\"Not yet implemented\")\n    }\n}\n"
    );
}

#[test]
fn compiler_expression_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

class Box(val size: Int) {
    val doubled = size * 2
    var label = ""
}

@Deprecated("use other")
fun old() = 1

fun f(p: Int, box: Box, s: String) {
    prnitln("typo")
    val n = s.lenght
    val v = 1
    v = 2
    p = 3
    box.size = 4
    box.label = "ok"
    old()
    s?.length
    s!!.length
    val e = s ?: "x"
    if (s == null) println()
    two(1)
    two(1, 2, 3)
    two(1, c = 2)
    val i: Int = "text"
    val t: String = null
    val l: Long = 1
    val any: Any = 1
    val num: Number = 2.5
}

fun two(a: Int, b: Int) = a + b
fun str(): String = 1
"#,
    );
    has(&found, "UNRESOLVED_REFERENCE: prnitln");
    has(&found, "UNRESOLVED_REFERENCE: lenght");
    has(&found, "VAL_REASSIGNMENT: v");
    has(&found, "VAL_REASSIGNMENT: p");
    has(&found, "VAL_REASSIGNMENT: box.size");
    has(&found, "DEPRECATION: old");
    has(&found, "UNNECESSARY_SAFE_CALL: ?.");
    has(&found, "UNNECESSARY_NOT_NULL_ASSERTION: !!");
    has(&found, "USELESS_ELVIS: ?: \"x\"");
    has(&found, "SENSELESS_COMPARISON: s == null");
    has(&found, "NO_VALUE_FOR_PARAMETER: )");
    has(&found, "TOO_MANY_ARGUMENTS: 3");
    has(&found, "NAMED_PARAMETER_NOT_FOUND: c");
    has(&found, "INITIALIZER_TYPE_MISMATCH: \"text\"");
    has(&found, "NULL_FOR_NONNULL_TYPE: null");
    has(&found, "RETURN_TYPE_MISMATCH: 1");
    assert!(!found.iter().any(|f| f.contains("label") || f.contains("size *") || f.contains(": 1\n") || f.ends_with(": 2.5")), "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("INITIALIZER_TYPE_MISMATCH")).count(), 1, "{found:#?}");
    assert_eq!(fixed("fun f() {\n    val v = 1\n    v = 2\n}\n", "VAL_REASSIGNMENT"), "fun f() {\n    var v = 1\n    v = 2\n}\n");
    assert_eq!(fixed("fun f(s: String) = s?.length\n", "UNNECESSARY_SAFE_CALL"), "fun f(s: String) = s.length\n");
    assert_eq!(fixed("val t: String = null\n", "NULL_FOR_NONNULL_TYPE"), "val t: String? = null\n");
    assert_eq!(fixed("val t: () -> Unit = null\n", "NULL_FOR_NONNULL_TYPE"), "val t: (() -> Unit)? = null\n");
    assert_eq!(fixed("val i: String? = 1\n", "INITIALIZER_TYPE_MISMATCH"), "val i: Int? = 1\n");
    assert_eq!(fixed_with("fun str(): String = 1\n", "RETURN_TYPE_MISMATCH", "Change return type to 'Int'"), "fun str(): Int = 1\n");
}

#[test]
fn compiler_checks_stay_quiet_when_unsure() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

open class Shape
class Circle(val radius: Double) : Shape()

class Holder(param: Int) {
    val fromParam = param + 1
    init { println(param) }
}

fun area(s: Shape): Double {
    if (s is Circle) return s.radius * s.radius
    return 0.0
}

fun dsl() = buildString {
    appendLine("x")
    append(length)
}

fun calls() = unknownDsl { whatever() }

fun deferred(c: Boolean): Int {
    val x: Int
    if (c) x = 1 else x = 2
    return x
}

data class P(val a: Int, val b: Int)
fun destructure(p: P): Int {
    val (a, b) = p
    return p.copy(a = 2).component1() + a + b
}

enum class Color { RED, GREEN }
fun names() = Color.values().map { it.name } + Color.entries.map { it.ordinal.toString() }
"#,
    );
    // Inside a lambda passed to an unknown function only the function is reported.
    assert_eq!(found, vec!["UNRESOLVED_REFERENCE: unknownDsl"]);
}

#[test]
fn compiler_flow_checks() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

fun noReturn(x: Int): Int {
    println(x)
}

fun unreachable(): Int {
    return 1
    println("never")
}

fun shadow(x: Int) {
    val y = 1
    run {
        val y = 2
        println(y)
    }
    listOf(1).forEach { x -> println(x) }
    val y = 3
}

fun throws(): Int {
    throw IllegalStateException()
}

fun todo(): Int {
    TODO()
}
"#,
    );
    has(&found, "NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY: }");
    assert_eq!(found.iter().filter(|f| f.starts_with("NO_RETURN")).count(), 1, "{found:#?}");
    has(&found, "UNREACHABLE_CODE: println(\"never\")");
    has(&found, "NAME_SHADOWING: y");
    has(&found, "NAME_SHADOWING: x");
    has(&found, "REDECLARATION: y");
}

#[test]
fn compiler_when_exhaustiveness() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

enum class Color { RED, GREEN, BLUE }
sealed interface Result
data class Ok(val v: Int) : Result
object Failed : Result
sealed class Nested : Result { class A : Nested(); class B : Nested() }

fun name(c: Color) = when (c) {
    Color.RED -> "r"
    Color.GREEN -> "g"
}

fun all(c: Color) = when (c) {
    Color.RED, Color.GREEN, Color.BLUE -> 1
    else -> 2
}

fun result(r: Result) = when (r) {
    is Ok -> 1
    Failed -> 2
    is Nested.A -> 3
}

fun covered(r: Result) = when (r) {
    is Ok -> 1
    Failed -> 2
    is Nested -> 3
}

fun flag(b: Boolean?) = when (b) {
    true -> 1
    false -> 0
}

fun noSubject(x: Int) = when {
    x > 0 -> "pos"
    x < 0 -> "neg"
}

fun ranges(x: Int) = when (x) {
    in 0..10 -> 1
    else -> 0
}
"#,
    );
    has(&found, "NO_ELSE_IN_WHEN: when");
    assert_eq!(found.iter().filter(|f| f.starts_with("NO_ELSE_IN_WHEN")).count(), 4, "{found:#?}");
    has(&found, "REDUNDANT_ELSE_IN_WHEN: else");
    assert_eq!(found.iter().filter(|f| f.starts_with("REDUNDANT_ELSE")).count(), 1, "{found:#?}");

    let text = "package a\n\nenum class Color { RED, GREEN, BLUE }\nfun name(c: Color) = when (c) {\n    Color.RED -> \"r\"\n}\n";
    assert_eq!(
        fixed(text, "NO_ELSE_IN_WHEN"),
        "package a\n\nenum class Color { RED, GREEN, BLUE }\nfun name(c: Color) = when (c) {\n    Color.RED -> \"r\"\n    Color.GREEN -> TODO()\n    Color.BLUE -> TODO()\n}\n"
    );
    assert!(fixed_with(text, "NO_ELSE_IN_WHEN", "Add 'else' branch").contains("    Color.RED -> \"r\"\n    else -> TODO()\n}"));
}

#[test]
fn unused_parameters() {
    let f = Fixture::new();
    let found = lint(
        r#"package a

fun publicFn(used: Int, unused: String) = used
class Service(private val repo: String, plain: Int, used: Int) {
    val x = used
    constructor(a: Int, b: Int) : this("", a, a)
    fun member(p: Int): Int = 1
}
fun lambdas() = listOf(1 to 2).map { (a, b) -> a }.forEach { x -> }
fun defaults(a: Int, b: Int = a) = b
fun shadowed(x: Int) {
    val y = listOf(1).map { x -> x }
    println(y)
}
fun receiverCall(body: String.() -> Int) = "x".body()
"#,
    );
    has(&found, "UNUSED_PARAMETER: unused");
    has(&found, "UNUSED_PARAMETER: plain");
    has(&found, "UNUSED_PARAMETER: b");
    has(&found, "UNUSED_PARAMETER: p");
    has(&found, "UNUSED_ANONYMOUS_PARAMETER: b");
    has(&found, "UNUSED_ANONYMOUS_PARAMETER: x");
    let params = |found: &[String]| found.iter().filter(|f| f.starts_with("UNUSED_PARAMETER")).cloned().collect::<Vec<_>>();
    if f.typed() {
        // The outer `x` is shadowed by the lambda's, so it is unused.
        has(&found, "UNUSED_PARAMETER: x");
        assert_eq!(params(&found).len(), 5, "{found:#?}");
    }
    for ok in ["used", "a", "repo", "body"] {
        assert!(!params(&found).iter().any(|f| f.ends_with(&format!(": {ok}"))), "{ok}: {found:#?}");
    }

    // Signatures fixed by something else are not reported.
    let found = lint(
        r#"package a

interface Handler { fun handle(event: String) }
open class Base { open fun hook(x: Int) {} }
class Impl : Base(), Handler {
    override fun handle(event: String) { println() }
    override fun hook(x: Int) { println() }
}
open class Open { open fun extensible(x: Int) { println() } }
@Suppress("X") @Deprecated("old") fun deprecated(x: Int) { println() }
annotation class Test
@Test fun annotated(x: Int) { println() }
fun stub(x: Int): Int = TODO()
fun empty(x: Int) {}
fun throwing(x: Int): Int = throw UnsupportedOperationException()
fun main(args: Array<String>) { println() }
fun referenced(x: Int) = 1
val ref = listOf(1).map(::referenced)
operator fun String.getValue(thisRef: Any?, property: Any?): String = this
"#,
    );
    assert!(params(&found).is_empty(), "{found:#?}");

    if !f.typed() {
        return;
    }
    // Removing a private function's parameter also removes the arguments.
    let text = "package a\n\nprivate fun f(a: Int, b: Int, c: Int) = a + c\nfun g() = f(1, 2, 3) + f(a = 1, b = 2, c = 3) + f(c = 3, a = 1, b = 2)\n";
    assert_eq!(fixed(text, "UNUSED_PARAMETER"), "package a\n\nprivate fun f(a: Int, c: Int) = a + c\nfun g() = f(1, 3) + f(a = 1, c = 3) + f(c = 3, a = 1)\n");
    let text = "package a\n\nprivate fun last(a: Int, b: Int) = a\nfun g() = last(1, 2)\n";
    assert_eq!(fixed(text, "UNUSED_PARAMETER"), "package a\n\nprivate fun last(a: Int) = a\nfun g() = last(1)\n");
    assert_eq!(fixed("fun f() = listOf(1).map { x -> 2 }\n", "UNUSED_ANONYMOUS_PARAMETER"), "fun f() = listOf(1).map { _ -> 2 }\n");
}

#[test]
fn unsafe_calls() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

fun f(s: String?, t: String?, u: String?, v: String?, w: List<String>?) {
    s.length
    if (t != null) t.length
    u?.length == 1 && u.isEmpty()
    if (!v.isNullOrEmpty()) v.length
    w.orEmpty()
    val x: String? = null
    x.length
}
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.starts_with("UNSAFE_CALL")).collect();
    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(fixed(text, "UNSAFE_CALL").contains("    s?.length\n"));
    assert!(fixed_with(text, "UNSAFE_CALL", "Add non-null asserted (!!.) call").contains("    s!!.length\n"));
}

#[test]
fn missing_and_final_overrides() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

open class Base<T> {
    open fun take(x: T) {}
    open fun name(): String = ""
    fun fixed() {}
    open val size: Int = 0
    open fun other(x: Int) {}
}

interface Named { fun id(): Int }

class Sub(val size: Int) : Base<String>(), Named {
    fun take(x: String) {}
    @Suppress("x") suspend fun name(): String = "s"
    override fun fixed() {}
    fun other(x: String) {}
    fun <R> generic(r: R) {}
    fun id(): Int = 1
    override fun toString(): String = "sub"
    fun hashCode(): Int = 1
}
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.starts_with("VIRTUAL") || f.starts_with("OVERRIDING")).collect();
    for expected in ["VIRTUAL_MEMBER_HIDDEN: size", "VIRTUAL_MEMBER_HIDDEN: take", "VIRTUAL_MEMBER_HIDDEN: name", "OVERRIDING_FINAL_MEMBER: fixed", "VIRTUAL_MEMBER_HIDDEN: id", "VIRTUAL_MEMBER_HIDDEN: hashCode"] {
        has(&found, expected);
    }
    assert_eq!(found.len(), 6, "{found:#?}");
    let out = fixed(text, "VIRTUAL_MEMBER_HIDDEN");
    assert!(out.contains("class Sub(override val size: Int)"), "{out}");
    let (lints, _) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    let name = lints.iter().find(|l| l.rule.id == "VIRTUAL_MEMBER_HIDDEN" && l.message.starts_with("'name'")).unwrap();
    assert!(apply(text, &name.fix.as_ref().unwrap().edits).contains("@Suppress(\"x\") override suspend fun name()"));
    // A final member of a project class: make it open (same file here).
    assert!(fixed(text, "OVERRIDING_FINAL_MEMBER").contains("    open fun fixed() {}\n"));
    let hidden = "package a\n\nopen class Base {\n    internal fun f() {}\n}\nclass Sub : Base() {\n    internal fun f() {}\n}\n";
    let out = fixed(hidden, "VIRTUAL_MEMBER_HIDDEN");
    assert!(out.contains("    internal open fun f() {}\n}\nclass") && out.contains("    internal override fun f() {}"), "{out}");
}

#[test]
fn non_literal_type_mismatches() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

open class A
class B : A()
class C

fun f(a: A, b: B, s: String, vararg rest: C) {
    val x: C = a
    val y: Int = s
    val ok1: A = b
    val ok2: CharSequence = s
    val ok3: Any = a
    val w: C = B()
    val ok4: A = B()
    val ok5: Array<out C> = rest
}

fun g(a: A): C = a

fun h(x: Any): String {
    if (x is String) {
        val s: String = x
    }
    return ""
}
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.contains("TYPE_MISMATCH")).collect();
    for expected in ["INITIALIZER_TYPE_MISMATCH: a", "INITIALIZER_TYPE_MISMATCH: s", "INITIALIZER_TYPE_MISMATCH: B()", "RETURN_TYPE_MISMATCH: a"] {
        has(&found, expected);
    }
    assert_eq!(found.len(), 4, "{found:#?}");
    assert!(fixed(text, "INITIALIZER_TYPE_MISMATCH").contains("    val x: A = a\n"));
}

#[test]
fn expect_and_actual() {
    let mut fx = Fixture::new();
    let common = "package a\n\nexpect fun platform(): String\nexpect class Box {\n    fun size(): Int\n}\nexpect fun lonely()\n";
    fx.run("src/main/kotlin/a/Common.kt", common);
    let jvm = "package a\n\nactual fun platform(): String = \"jvm\"\nactual class Box {\n    actual fun size(): Int = 1\n    actual fun extra() {}\n}\nactual fun orphan() {}\n";
    let (lints, _) = fx.run("src/main/kotlin/a/Jvm.kt", jvm);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &jvm[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    assert_eq!(found, vec!["ACTUAL_WITHOUT_EXPECT: extra", "ACTUAL_WITHOUT_EXPECT: orphan"]);
    let (lints, _) = fx.run("src/main/kotlin/a/Common.kt", common);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &common[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    assert_eq!(found, vec!["NO_ACTUAL_FOR_EXPECT: lonely"]);
}

#[test]
fn argument_type_mismatches() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

class A
class B
object Bounds {
    fun of(x: Int) = x
    fun of(x: A) = 0
}
class Box<T> {
    fun <T> put(key: String, value: T) {}
    fun take(t: T) {}
}

fun one(a: A, n: Int) {}

fun f(a: A, b: B, box: Box<A>) {
    one(b, 1)
    one(a, "x")
    one(n = 2.5, a = a)
    one(a, null)
    one(A(), 1)
    Bounds.of(1)
    Bounds.of(a)
    box.put("k", 1)
    box.take(b)
    StringBuilder().append(1)
}
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.contains("TYPE_MISMATCH") || f.starts_with("NULL_FOR")).collect();
    for expected in ["ARGUMENT_TYPE_MISMATCH: b", "ARGUMENT_TYPE_MISMATCH: \"x\"", "ARGUMENT_TYPE_MISMATCH: 2.5", "NULL_FOR_NONNULL_TYPE: null"] {
        has(&found, expected);
    }
    assert_eq!(found.len(), 5, "{found:#?}");
    // Fixes: convert the value, or change the parameter's type.
    assert!(fixed(text, "ARGUMENT_TYPE_MISMATCH").contains("fun one(a: B, n: Int) {}"));
    let conv = "package a\n\nfun one(n: Int, s: String, d: Double) {}\nfun f(t: String, i: Int) {\n    one(t, i, 1)\n}\n";
    let (lints, _) = Fixture::new().run("src/main/kotlin/a/A.kt", conv);
    let titles: Vec<&str> = lints.iter().filter(|l| l.rule.id == "ARGUMENT_TYPE_MISMATCH").flat_map(|l| l.fix.iter().chain(&l.alternatives)).map(|f| f.title.as_str()).collect();
    assert!(titles.contains(&"Convert to 'Int'") && titles.contains(&"Convert to 'String'") && titles.contains(&"Convert to 'Double'"), "{titles:?}");
    let edits: Vec<Edit> = lints.iter().filter(|l| l.rule.id == "ARGUMENT_TYPE_MISMATCH").flat_map(|l| l.fix.as_ref().unwrap().edits.clone()).collect();
    let out = apply(conv, &edits);
    assert!(out.contains("one(t.toInt(), i.toString(), 1.0)"), "{out}");
    assert!(fixed_with("val s: String = 42\n", "INITIALIZER_TYPE_MISMATCH", "Convert to 'String'").contains("val s: String = \"42\""));
}

#[test]
fn suspend_calls() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

suspend fun load(): Int = 1
fun runAsync(block: suspend () -> Unit) {}
fun later(block: () -> Unit) {}
inline fun inl(block: () -> Unit) = block()

fun bad() {
    load()
}
suspend fun good() {
    load()
    inl { load() }
}
fun lam() {
    runAsync { load() }
    runAsync { inl { load() } }
}
fun lam2() {
    later { load() }
}
val p: Int get() = load()
fun seq() = sequence { yield(1) }
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.starts_with("ILLEGAL_SUSPEND")).collect();
    assert_eq!(found, vec!["ILLEGAL_SUSPEND_FUNCTION_CALL: load"; 3], "{found:#?}");
    assert!(fixed(text, "ILLEGAL_SUSPEND_FUNCTION_CALL").contains("suspend fun bad() {"));
}

#[test]
fn add_parameter_from_call() {
    if !Fixture::new().typed() {
        return;
    }
    let text = "package a\n\nfun two(a: Int, b: Int) = a + b\nfun f(label: String) {\n    two(1, 2, label, 3.5)\n    two(1, 2, c = true)\n}\n";
    assert!(fixed(text, "TOO_MANY_ARGUMENTS").contains("fun two(a: Int, b: Int, label: String, value: Double) = a + b"));
    assert!(fixed(text, "NAMED_PARAMETER_NOT_FOUND").contains("fun two(a: Int, b: Int, c: Boolean) = a + b"));
}

#[test]
fn more_compiler_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    fx.run("src/main/kotlin/a/Other.kt", "package a\n\nclass Vault {\n    private val secret = 1\n    private fun hide() {}\n    protected fun guarded() {}\n    fun open() = secret\n}\nprivate fun fileOnly() = 1\n");
    let text = r#"package a

abstract class Shape
interface Named
enum class Color { RED }
sealed class Result

fun Int.twice() = this * 2

fun make(v: Vault) {
    Shape()
    Named()
    Color()
    Result()
    v.secret
    v.hide()
    v.guarded()
    v.open()
    fileOnly()
    val n = 1 twice 2
    make(v) = 3
    break
    val x: Int
    println(x)
    val y: Int
    if (n > 1) y = 1 else y = 2
    println(y)
    val list: List<Int, Int> = listOf()
    val map: Map<String> = mapOf()
    val raw: List = listOf()
    val ok: Map<String, *> = mapOf()
    val check = (raw as Any) is List<*>
}

fun count(): Int {
    return
}

fun nothing() {
    return 1
}

class C {
    init {
        return
    }
    constructor(i: Int) {
        return
    }
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    for expected in [
        "CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS: Shape",
        "CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS: Named",
        "CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS: Color",
        "CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS: Result",
        "INVISIBLE_REFERENCE: secret",
        "INVISIBLE_REFERENCE: hide",
        "INVISIBLE_REFERENCE: guarded",
        "INFIX_MODIFIER_REQUIRED: twice",
        "VARIABLE_EXPECTED: make(v)",
        "BREAK_OR_CONTINUE_OUTSIDE_A_LOOP: break",
        "UNINITIALIZED_VARIABLE: x",
        "WRONG_NUMBER_OF_TYPE_ARGUMENTS: List<Int, Int>",
        "WRONG_NUMBER_OF_TYPE_ARGUMENTS: Map<String>",
        "WRONG_NUMBER_OF_TYPE_ARGUMENTS: List",
        "RETURN_TYPE_MISMATCH: return",
        "RETURN_TYPE_MISMATCH: 1",
        "RETURN_NOT_ALLOWED: return",
    ] {
        has(&found, expected);
    }
    // Not: visible members, the second initialized val, star projections, `is` checks,
    // a bare `return` in a constructor.
    for unexpected in ["INVISIBLE_REFERENCE: open", "UNINITIALIZED_VARIABLE: y", "WRONG_NUMBER_OF_TYPE_ARGUMENTS: Map<String, *>", "WRONG_NUMBER_OF_TYPE_ARGUMENTS: List<*>"] {
        assert!(!found.iter().any(|f| f == unexpected), "{unexpected} in {found:#?}");
    }
    assert_eq!(found.iter().filter(|f| f.starts_with("RETURN_NOT_ALLOWED")).count(), 1, "{found:#?}");
}

#[test]
fn unsafe_calls_with_smart_casts() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

class Node(val next: Node?, val name: String)

fun f(s: String?, t: String?, start: Node?) {
    if (s != null) println(s.length)
    s.length
    if (t == null) return
    t.length
    var n: Node? = start
    while (n != null) {
        println(n.name)
        n = n.next
    }
    var m: Node? = start
    if (m != null) {
        m = m.next
        m.name
    }
}
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.starts_with("UNSAFE_CALL")).collect();
    // The use after the `if` block, and the use after reassignment; not the checked ones.
    assert_eq!(found.len(), 2, "{found:#?}");
    let (lints, _) = Fixture::new().run("src/main/kotlin/a/A.kt", text);
    let lines: Vec<usize> = lints.iter().filter(|l| l.rule.id == "UNSAFE_CALL").map(|l| text[..usize::from(l.range.start())].matches('\n').count() + 1).collect();
    assert_eq!(lines, vec![7, 18]);
}

#[test]
fn typing_compiler_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    fx.run("src/main/kotlin/a/Other.kt", "package a\n\nprivate fun hidden() = 1\n");
    let text = r#"package a

class Money(val cents: Int) {
    fun plus(other: Money) = Money(cents + other.cents)
}
class Box<T : Number>(val v: T)
fun <T : CharSequence> first(t: T) = t
fun pick(i: Int) = i
fun pick(d: Double) = d
open class Animal
class Dog : Animal()
class Car

fun f(a: Money, b: Money, n: Int, s: String, flag: Boolean?, dog: Dog, car: Car, any: Any) {
    a + b
    val box: Box<String>? = null
    val ok: Box<Int>? = null
    first<Int>(1)
    var count: Int = 0
    count = "x"
    count = 2
    if (n) println()
    while (s) {}
    if (flag) println()
    if (flag != null && flag) println()
    val same = s as String
    val never = car as Dog
    val fine = any as Dog
    val always = dog is Animal
    val never2 = dog !is Animal
    pick("x")
    pick(1)
    hidden()
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    for expected in [
        "OPERATOR_MODIFIER_REQUIRED: +",
        "UPPER_BOUND_VIOLATED: String",
        "UPPER_BOUND_VIOLATED: Int",
        "ASSIGNMENT_TYPE_MISMATCH: \"x\"",
        "CONDITION_TYPE_MISMATCH: n",
        "CONDITION_TYPE_MISMATCH: s",
        "CONDITION_TYPE_MISMATCH: flag",
        "USELESS_CAST:  as String",
        "CAST_NEVER_SUCCEEDS: as",
        "USELESS_IS_CHECK: dog is Animal",
        "USELESS_IS_CHECK: dog !is Animal",
        "NONE_APPLICABLE: pick",
        "INVISIBLE_REFERENCE: hidden",
    ] {
        has(&found, expected);
    }
    assert_eq!(found.iter().filter(|f| f.starts_with("CONDITION_TYPE_MISMATCH")).count(), 3, "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("UPPER_BOUND_VIOLATED")).count(), 2, "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("NONE_APPLICABLE")).count(), 1, "{found:#?}");
    assert!(!found.iter().any(|f| f.contains("any as Dog") || f.starts_with("ASSIGNMENT_TYPE_MISMATCH: 2")), "{found:#?}");
}

#[test]
fn signature_and_override_compiler_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

private class Secret
internal class Inner
enum class Color { RED, GREEN }

open class Base {
    open var size: Int = 0
    open val name: String = ""
    open fun count(): Int = 0
    open fun run(times: Int) {}
    open fun label(): CharSequence = ""
}

class Derived : Base() {
    override val size: Int = 1
    override val name: Int = 2
    override fun count(): String = ""
    private override fun run(times: Int = 2) {}
    override fun label(): String = ""
}

fun many(vararg a: Int, vararg b: Int) {}
fun <reified T> notInline(t: T) {}
inline fun <reified T> ok() = T::class
fun <T> klass() = T::class
inline fun plain(x: Int) = x + 1
inline fun lambda(f: () -> Unit) = f()
fun nullable(s: String??) {}

fun exposeReturn(): Secret = Secret()
fun exposeParam(s: Inner) {}
private fun hiddenOk(): Secret = Secret()
internal fun internalOk(i: Inner) {}
class Public(val s: Secret)
class Holder private constructor(private val s: Secret)
open class Sub : Inner()
internal interface Hidden
class Impl : Hidden
interface Leaky : Hidden

fun g(s: String, n: Int, c: Color): Int {
    if (s == n) println()
    if (s == "x") println()
    when (n) {
        "a" -> println()
        1, 1 -> println()
        else -> println()
    }
    when (c) {
        else -> println()
        Color.RED -> println()
    }
    val v = if (n > 0) 1
    if (n > 0) return "x"
    return 1
}

fun body(): Int = "x"
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    for expected in [
        "VAR_OVERRIDDEN_BY_VAL: size",
        "PROPERTY_TYPE_MISMATCH_ON_OVERRIDE: Int",
        "RETURN_TYPE_MISMATCH_ON_OVERRIDE: String",
        "CANNOT_WEAKEN_ACCESS_PRIVILEGE: private",
        "DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE: 2",
        "MULTIPLE_VARARG_PARAMETERS: vararg",
        "REIFIED_TYPE_PARAMETER_NO_INLINE: reified",
        "TYPE_PARAMETER_AS_REIFIED: T",
        "NOTHING_TO_INLINE: inline",
        "REDUNDANT_NULLABLE: ?",
        "EXPOSED_FUNCTION_RETURN_TYPE: Secret",
        "EXPOSED_PARAMETER_TYPE: Inner",
        "EXPOSED_PARAMETER_TYPE: Secret",
        "EXPOSED_SUPER_CLASS: Inner",
        "EXPOSED_SUPER_INTERFACE: Hidden",
        "EQUALITY_NOT_APPLICABLE: ==",
        "INCOMPATIBLE_TYPES: \"a\"",
        "DUPLICATE_BRANCH_CONDITION_IN_WHEN: 1",
        "ELSE_MISPLACED_IN_WHEN: else",
        "INVALID_IF_AS_EXPRESSION: if",
        "RETURN_TYPE_MISMATCH: \"x\"",
    ] {
        has(&found, expected);
    }
    let count = |rule: &str| found.iter().filter(|f| f.starts_with(rule)).count();
    // `label(): String` narrows `CharSequence`; `private` / `internal` / private-constructor
    // declarations and a class implementing an internal interface expose nothing; `lambda`
    // and `ok` have something to inline.
    assert_eq!(count("RETURN_TYPE_MISMATCH_ON_OVERRIDE"), 1, "{found:#?}");
    assert_eq!(count("EXPOSED_"), 5, "{found:#?}");
    assert_eq!(count("NOTHING_TO_INLINE"), 1, "{found:#?}");
    assert_eq!(count("EQUALITY_NOT_APPLICABLE"), 1, "{found:#?}");
    assert_eq!(count("RETURN_TYPE_MISMATCH:"), 2, "{found:#?}");
    assert_eq!(count("TYPE_PARAMETER_AS_REIFIED"), 1, "{found:#?}");
}

#[test]
fn quick_win_compiler_errors() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    fx.run(
        "src/main/kotlin/a/Api.kt",
        r#"package a

@RequiresOptIn
annotation class Internal

@RequiresOptIn(level = RequiresOptIn.Level.WARNING)
annotation class Experimental

@Internal
fun internalCall() = 1

@Experimental
fun experimentalCall() = 2

@Deprecated("gone", level = DeprecationLevel.ERROR)
fun removed() = 3

@Deprecated("old")
fun old() = 4
"#,
    );
    let text = r#"package a

open class Loop1 : Loop2()
open class Loop2 : Loop1()

interface Left { fun hello() = "left" }
interface Right { fun hello() = "right" }
class Both : Left, Right
class Resolved : Left, Right { override fun hello() = "both" }

class NoIterator
class Holder
class Delegate { operator fun getValue(thisRef: Any?, p: kotlin.reflect.KProperty<*>) = 1 }

fun String.length(): Int = 0
fun String.get(index: Int): Char = 'a'
fun String.trimmed(): String = this

fun <T : Int> bounded(t: T) = t
fun <T : Int?> nullableBound(t: T) = t

fun use(any: Any, n: NoIterator) {
    internalCall()
    experimentalCall()
    removed()
    old()
    for (x in n) {}
    for (c in "abc") {}
    val d by Holder()
    val ok by Delegate()
    var settable by Delegate()
    val list = any as List<String>
    val star = any as List<*>
}

@OptIn(Internal::class)
fun optedIn() = internalCall()

@Internal
fun propagated() = internalCall()

fun <T> unchecked(x: Any): T = x as T
inline fun <reified T> checked(x: Any): T = x as T
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    for expected in [
        "CYCLIC_INHERITANCE_HIERARCHY: Loop2",
        "CYCLIC_INHERITANCE_HIERARCHY: Loop1",
        "MANY_IMPL_MEMBER_NOT_IMPLEMENTED: Both",
        "OPT_IN_USAGE_ERROR: internalCall",
        "OPT_IN_USAGE: experimentalCall",
        "DEPRECATION_ERROR: removed",
        "DEPRECATION: old",
        "ITERATOR_MISSING: n",
        "DELEGATE_SPECIAL_FUNCTION_MISSING: Holder()",
        "DELEGATE_SPECIAL_FUNCTION_MISSING: Delegate()",
        "UNCHECKED_CAST: as",
        "EXTENSION_SHADOWED_BY_MEMBER: get",
        "FINAL_UPPER_BOUND: Int",
    ] {
        has(&found, expected);
    }
    let count = |rule: &str| found.iter().filter(|f| f.starts_with(rule)).count();
    // Opted in / propagated uses, `Resolved`, `for` over a String, `val` by a delegate with
    // `getValue`, `List<*>`, reified casts, `trimmed`, `length` (a property
    // member) and `Int?` bounds are fine.
    assert_eq!(count("OPT_IN_USAGE_ERROR"), 1, "{found:#?}");
    assert_eq!(count("MANY_IMPL_MEMBER_NOT_IMPLEMENTED"), 1, "{found:#?}");
    assert_eq!(count("ITERATOR_MISSING"), 1, "{found:#?}");
    assert_eq!(count("DELEGATE_SPECIAL_FUNCTION_MISSING"), 2, "{found:#?}");
    assert_eq!(count("UNCHECKED_CAST"), 2, "{found:#?}");
    assert_eq!(count("EXTENSION_SHADOWED_BY_MEMBER"), 1, "{found:#?}");
    assert_eq!(count("FINAL_UPPER_BOUND"), 1, "{found:#?}");
    assert_eq!(count("DEPRECATION:"), 1, "{found:#?}");
}

#[test]
fn library_deprecation_and_opt_in() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

import kotlin.uuid.Uuid

fun f(s: String, c: Char) {
    s.toUpperCase()
    c.toInt()
    Uuid.random()
}

@OptIn(kotlin.uuid.ExperimentalUuidApi::class)
fun g() = Uuid.random()
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    // `toUpperCase` is deprecated in the stdlib (`@DeprecatedSinceKotlin`; the module's
    // version is unknown here, so the declared level applies). `Uuid` was experimental
    // (`@WasExperimental`): no opt-in needed. `Char.toInt` is a built-in without class
    // file annotations.
    has(&found, "DEPRECATION: toUpperCase");
    assert!(!found.iter().any(|f| f.starts_with("OPT_IN")), "{found:#?}");
}

#[test]
fn checks_on_call_results() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

class User(val name: String) {
    fun nick(): String? = null
    fun greet(): String = "hi"
}

fun find(id: Int): User? = null
fun all(): List<User> = emptyList()
fun <T> pick(t: T): T = t

fun f(m: Map<String, User>, u: User) {
    find(1).greet()
    find(1)?.greet()
    m["a"].greet()
    u.nick().length
    val x = find(2)
    x.greet()
    all().first().gret()
    find(3)!!.nick()?.length
    val n: Int = find(4)!!.greet()
    if (all().isEmpty()) println()
    find(5).let { println(it) }
    pick(u).greet()
    pick(find(6)).greet()
    all().first().name.length
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{} line {}: {}", l.rule.id, text[..usize::from(l.range.start())].lines().count(), &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    // Nullable call results (`find(1)`, `m["a"]`, `u.nick()`, `pick(find(6))` with `T` =
    // `User?`) and a local initialized with one; a member missing on a call chain; a call
    // chain of the wrong type. Safe calls, `!!` and extensions (`let`) are not reported.
    let expected = [
        "UNSAFE_CALL line 13: .",
        "UNSAFE_CALL line 15: .",
        "UNSAFE_CALL line 16: .",
        "UNSAFE_CALL line 18: .",
        "UNRESOLVED_REFERENCE line 19: gret",
        "INITIALIZER_TYPE_MISMATCH line 21: find(4)!!.greet()",
        "UNSAFE_CALL line 25: .",
    ];
    assert_eq!(found, expected, "{found:#?}");
}

#[test]
fn override_signature_mismatch() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

open class Base<T> {
    open fun run(times: Int) {}
    open fun <T> wrap(value: T): List<T> = listOf(value)
    open fun take(item: T) {}
}
interface Shape { fun area(scale: Double): Double }

class Derived : Base<String>(), Shape {
    override fun run(times: String) {}
    override fun run(times: Int, extra: Int) {}
    override fun area(scale: Int): Double = 0.0
    override fun <T> wrap(value: T): List<T> = listOf(value)
    override fun take(item: String) {}
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{} line {}", l.rule.id, text[..usize::from(l.range.start())].lines().count())).collect();
    // `run(String)`, `run(Int, Int)` and `area(Int)` override nothing, so `area(Double)` is
    // not implemented; `wrap` (its own `T`) and `take` (the class's `T` = String) match.
    let expected = ["ABSTRACT_MEMBER_NOT_IMPLEMENTED line 10", "NOTHING_TO_OVERRIDE line 11", "NOTHING_TO_OVERRIDE line 12", "NOTHING_TO_OVERRIDE line 13"];
    assert_eq!(found, expected, "{found:#?}");
    let msg = lints.iter().find(|l| l.rule.id == "NOTHING_TO_OVERRIDE").map(|l| l.message.clone()).unwrap_or_default();
    assert!(msg.contains("Potential signature for overriding: fun run(times: Int)"), "{msg}");
}

#[test]
fn inferred_type_arguments() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

class User(val name: String, val age: Int) {
    fun nick(): String? = null
}

fun users(): List<User> = emptyList()

fun f(u: User) {
    listOf(u).first().nick().length
    u.let { it.nick() }.length
    users().map { it.name }.first().nmae
    users().map { it.nick() }.first().length
    users().firstOrNull().name
    users().filter { it.age > 3 }.first().nick().length
    val n: Int = users().map { it.name }.first()
    listOf(1).first().inc()
    users().maxByOrNull { it.age }.name
    u.run { name }.length
    users().associateBy { it.name }["x"].age
    users().map { user -> user.nick() }.first().length
    users().map { it.name }.first().length
    users().filter { it.nick() != null }.first().nick()?.length
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{} line {}: {}", l.rule.id, text[..usize::from(l.range.start())].lines().count(), &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    // Type parameters bound by arguments (`listOf(u)`), receivers and lambda results
    // (`let`, `map`, `associateBy`); lambda parameters typed by their call (`it`, `user`).
    // Integer literals bind nothing (`listOf(1)`); non-null results are fine.
    let expected = [
        "UNSAFE_CALL line 10: .",
        "UNSAFE_CALL line 11: .",
        "UNRESOLVED_REFERENCE line 12: nmae",
        "UNSAFE_CALL line 13: .",
        "UNSAFE_CALL line 14: .",
        "UNSAFE_CALL line 15: .",
        "INITIALIZER_TYPE_MISMATCH line 16: users().map { it.name }.first()",
        "UNSAFE_CALL line 18: .",
        "UNSAFE_CALL line 20: .",
        "UNSAFE_CALL line 21: .",
    ];
    assert_eq!(found, expected, "{found:#?}");
}

#[test]
fn flow_analysis() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

fun cond(): Boolean = true
fun stop(): Nothing = throw IllegalStateException()
fun maybe(): Nothing? = null

fun f(n: Int) {
    val a: Int
    if (n > 0) a = 1
    println(a)
    val b: Int
    if (n > 0) b = 1 else b = 2
    println(b)
    val c: Int
    while (cond()) { c = 1 }
    var d = 0
    d = 5
    println(d)
    var e = 1
    e = 2
    var g = 0
    for (i in 0 until n) g += i
    println(g)
    var h: Int
    try { h = 1 } catch (x: Exception) { h = 2 }
    println(h)
    val k: Int
    when (n) {
        1 -> k = 1
        else -> stop()
    }
    println(k)
    var m = 0
    run { m = 1 }
    println(m)
}

fun dead(n: Int): Int {
    if (n > 0) return 1 else throw IllegalStateException()
    println(n)
}

fun alive(): Int? {
    maybe()
    return 1
}

fun loop() {
    while (true) {
        if (cond()) break
    }
    println(1)
    while (true) { }
    println(2)
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{} line {}: {}", l.rule.id, text[..usize::from(l.range.start())].lines().count(), &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    // `a` is unassigned when `n <= 0`; `c` may be assigned twice by the loop; `d`'s first
    // value and `e`'s are never read; code after `if`/`else` that both leave, and after a
    // loop without `break`, is unreachable. `b`, `g` (read on the next iteration), `h`
    // (assigned in both), `k` (`stop()` never returns), `m` (assigned in a lambda),
    // `maybe()` (returns null) and the loop with `break` are fine.
    let expected = [
        "UNINITIALIZED_VARIABLE line 10: a",
        "VAL_REASSIGNMENT line 15: c",
        "VARIABLE_WITH_REDUNDANT_INITIALIZER line 16: 0",
        "ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE line 19: e",
        "UNREACHABLE_CODE line 40: println(n)",
        "UNREACHABLE_CODE line 54: println(2)",
    ];
    for x in expected {
        has(&found, x);
    }
    assert_eq!(found.len(), expected.len(), "{found:#?}");
}

#[test]
fn nullable_values_used_as_non_null() {
    if !Fixture::new().typed() {
        return;
    }
    let mut fx = Fixture::new();
    let text = r#"package a

class User(val name: String, val nick: String?)

fun takes(s: String) {}
fun find(): User? = null
fun fail(): Nothing = throw IllegalStateException()

fun f(s: String?, u: User, list: List<Int>?, n: Int?) {
    s.length
    takes(s)
    val t: String = s
    takes(u.nick)
    u.nick.length
    for (i in list) println(i)
    list[0]
    n + 1
    val m: Int = list?.size
    s + "suffix"
}

fun g(s: String?): String = s

fun narrowed(s: String?, u: User, list: MutableList<Int>?, n: Int?) {
    if (s == null) fail()
    s.length
    var l = list
    if (l == null) l = mutableListOf()
    l.add(1)
    if (u.nick != null) u.nick.length
    requireNotNull(n)
    n + 1
}
"#;
    let (lints, text) = fx.run("src/main/kotlin/a/A.kt", text);
    let found: Vec<String> = lints.iter().filter(|l| l.rule.category == Category::Compiler).map(|l| format!("{} line {}", l.rule.id, text[..usize::from(l.range.start())].lines().count())).collect();
    // Members, arguments, initializers, operators, indexing and loops on nullable values;
    // `String?.plus` exists. Narrowed by a `Nothing` call, an assignment, a check of the
    // property chain and `requireNotNull`: fine.
    let expected = [
        "UNSAFE_CALL line 10",
        "ARGUMENT_TYPE_MISMATCH line 11",
        "INITIALIZER_TYPE_MISMATCH line 12",
        "ARGUMENT_TYPE_MISMATCH line 13",
        "UNSAFE_CALL line 14",
        "ITERATOR_ON_NULLABLE line 15",
        "UNSAFE_CALL line 16",
        "UNSAFE_OPERATOR_CALL line 17",
        "INITIALIZER_TYPE_MISMATCH line 18",
        "RETURN_TYPE_MISMATCH line 22",
    ];
    assert_eq!(found, expected, "{found:#?}");
}

#[test]
fn quick_fixes_for_compiler_errors() {
    if !Fixture::new().typed() {
        return;
    }
    // Initialization.
    assert_eq!(fixed_with("package a\n\nvar count: Int\n", "MUST_BE_INITIALIZED", "Add initializer"), "package a\n\nvar count: Int = 0\n");
    assert_eq!(fixed_with("package a\n\nvar name: String\n", "MUST_BE_INITIALIZED", "Make 'lateinit'"), "package a\n\nlateinit var name: String\n");
    assert_eq!(fixed_with("package a\n\nval items: List<String>\n", "MUST_BE_INITIALIZED", "Add initializer"), "package a\n\nval items: List<String> = emptyList()\n");
    assert_eq!(
        fixed_with("package a\n\nabstract class A {\n    val id: Long\n}\n", "MUST_BE_INITIALIZED_OR_BE_ABSTRACT", "Make 'id' abstract"),
        "package a\n\nabstract class A {\n    abstract val id: Long\n}\n"
    );
    // Missing return.
    assert_eq!(
        fixed_with("package a\n\nfun f(x: Int): String {\n    println(x)\n}\n", "NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY", "Add 'return' statement"),
        "package a\n\nfun f(x: Int): String {\n    println(x)\n    return \"\"\n}\n"
    );
    // Abstract class instead of implementing.
    assert_eq!(
        fixed_with("package a\n\ninterface Shape { fun area(): Double }\nopen class Square : Shape\n", "ABSTRACT_MEMBER_NOT_IMPLEMENTED", "Make 'Square' abstract"),
        "package a\n\ninterface Shape { fun area(): Double }\nabstract class Square : Shape\n"
    );
    // Downcast.
    assert_eq!(
        fixed_with("package a\n\nfun takes(s: String) {}\nfun f(any: Any) {\n    takes(any)\n}\n", "ARGUMENT_TYPE_MISMATCH", "Cast to 'String'"),
        "package a\n\nfun takes(s: String) {}\nfun f(any: Any) {\n    takes(any as String)\n}\n"
    );
    // Null check around the statement.
    assert_eq!(
        fixed_with("package a\n\nfun f(s: String?) {\n    println(s.length)\n}\n", "UNSAFE_CALL", "Surround with null check"),
        "package a\n\nfun f(s: String?) {\n    if (s != null) {\n        println(s.length)\n    }\n}\n"
    );
}

#[test]
fn idiom_inspections() {
    let fx = Fixture::new();
    if !fx.typed() {
        return;
    }
    let found = lint(
        r#"package a

class C {
    var count = 0
    val name: String = ""
        get() = name
    var label: String = ""
        set(v) { label = v }
    var ok: String = ""
        get() = field.trim()
        set(v) { field = v }

    fun f(xs: List<Int>, m: MutableMap<String, Int>, s: String, words: List<String>) {
        var total = 0
        total = total + xs.size
        this.count = this.count * 2
        m.put("a", 1)
        println(xs.filter { it > 1 }.first())
        println(xs.map { it * 2 }.filterNotNull())
        println(xs.filter { it > 0 }.size)
        println(xs.sortedBy { -it }.firstOrNull())
        xs.forEach { -> println() }
        xs.forEach { it -> println(it) }
        println(listOf<Int>(1, 2))
        println(s.orEmpty())
        println(s.isNullOrEmpty())
        xs.forEach { println(it); words.forEach { println(it) } }
    }
}
"#,
    );
    has(&found, "ReplaceWithOperatorAssignment: total = total + xs.size");
    has(&found, "ReplaceWithOperatorAssignment: this.count = this.count * 2");
    has(&found, "ReplacePutWithAssignment: put");
    has(&found, "SimplifiableCallChain: filter { it > 1 }.first()");
    has(&found, "SimplifiableCallChain: map { it * 2 }.filterNotNull()");
    has(&found, "SimplifiableCallChain: filter { it > 0 }.size");
    has(&found, "SimplifiableCallChain: sortedBy { -it }.firstOrNull()");
    has(&found, "RedundantLambdaArrow: ->");
    has(&found, "RedundantLambdaArrow: it ->");
    has(&found, "RemoveExplicitTypeArguments: <Int>");
    has(&found, "UselessCallOnNotNull: .orEmpty()");
    has(&found, "UselessCallOnNotNull: isNullOrEmpty");
    has(&found, "RecursivePropertyAccessor: name");
    has(&found, "RecursivePropertyAccessor: label");
    has(&found, "NestedLambdaShadowedImplicitParameter: it");
    assert_eq!(found.iter().filter(|f| f.starts_with("RecursivePropertyAccessor")).count(), 2, "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("NestedLambda")).count(), 1, "{found:#?}");

    let body = |s: &str| format!("package a\n\nfun f(xs: List<Int>, m: MutableMap<String, Int>, s: String) {{\n    var t = 0\n    {s}\n    println(t)\n}}\n");
    let check = |src: &str, rule: &str, want: &str| assert_eq!(fixed(&body(src), rule), body(want));
    check("t = t - xs[0]", "ReplaceWithOperatorAssignment", "t -= xs[0]");
    check("m.put(\"k\", 2)", "ReplacePutWithAssignment", "m[\"k\"] = 2");
    check("println(xs\n        .filter { it > 1 }\n        .isEmpty())", "SimplifiableCallChain", "println(xs\n        .none { it > 1 })");
    check("println(xs.map { \"$it\" }.joinToString())", "SimplifiableCallChain", "println(xs.joinToString { \"$it\" })");
    check("xs.forEach { it -> println(it) }", "RedundantLambdaArrow", "xs.forEach { println(it) }");
    check("println(listOf<String>(\"a\", s))", "RemoveExplicitTypeArguments", "println(listOf(\"a\", s))");
    check("println(s.orEmpty())", "UselessCallOnNotNull", "println(s)");
    assert_eq!(fixed("class C {\n    val v: Int = 1\n        get() = v + 1\n}\n", "RecursivePropertyAccessor"), "class C {\n    val v: Int = 1\n        get() = field + 1\n}\n");

    // Not flagged: types with `plusAssign`, a used `put` result, other receivers' properties,
    // type arguments the arguments do not fix, nullable receivers, unused outer `it`.
    let quiet = lint(
        r#"package a

class D(val other: D?) {
    val name: String get() = other?.name ?: ""
    var list = mutableListOf<Int>()

    fun g(m: MutableMap<String, Int>, s: String?, xs: List<Int>) {
        list = list + 1
        val old = m.put("a", 1)
        println(old)
        println(listOf<Number>(1, 2))
        println(emptyList<Int>())
        println(listOf<Long>(1))
        println(s.orEmpty())
        xs.forEach { _ -> xs.forEach { println(it) } }
        xs.forEach { xs.forEach { x -> println(x) }; println(it) }
        xs.map { it.takeIf { it > 0 } }
        println(xs.map { if (it > 9) return; "$it" }.joinToString())
        val f: (Int) -> Int = { it -> it }
        println(f)
    }
}
"#,
    );
    for rule in ["ReplaceWithOperatorAssignment", "ReplacePutWithAssignment", "RecursivePropertyAccessor", "RemoveExplicitTypeArguments", "UselessCallOnNotNull", "NestedLambda", "SimplifiableCallChain"] {
        assert!(!quiet.iter().any(|f| f.starts_with(rule)), "{rule}: {quiet:#?}");
    }
}

#[test]
fn declaration_shape_errors() {
    let fx = Fixture::new();
    if !fx.typed() {
        return;
    }
    let found = lint(
        r#"package a

val ro: Int = 1
    set(v) {}
val lazyOne by lazy { 1 }
    get() = 2
val String.ext: Int = 1
inline val inl: Int = 1
val untyped
abstract class Abs {
    abstract var a: Int
        private set
    open var o: Int = 0
        private set
    private var hidden: Int = 0
        public set
    val g: Int = 0
        private get
}
interface I {
    val d by lazy { 1 }
    private fun nobody()
}
fun interface Two {
    fun a()
    fun b()
}
fun interface Gen {
    fun <T> a(t: T = TODO())
}
value class Empty()
value class Pair2(val a: Int, val b: Int)
value class Mutable(var a: Int)
open value class Opened(val a: Int)
@JvmInline
value class Ok(val a: Int) {
    val twice = a * 2
}
sealed class S public constructor()
enum class Op {
    PLUS { override fun apply(x: Int) = x },
    MINUS;
    abstract fun apply(x: Int): Int
}
interface J
class Twice : J, J
fun f(cb: (() -> Unit)?) {
    cb()
    val local
    local = 1
    { println() }
    cb
    1
    println(local)
}
"#,
    );
    for expected in [
        "VAL_WITH_SETTER: set(v) {}",
        "ACCESSOR_FOR_DELEGATED_PROPERTY: get() = 2",
        "EXTENSION_PROPERTY_WITH_BACKING_FIELD: ext",
        "INLINE_PROPERTY_WITH_BACKING_FIELD: inl",
        "PROPERTY_WITH_NO_TYPE_NO_INITIALIZER: untyped",
        "PRIVATE_SETTER_FOR_ABSTRACT_PROPERTY: private set",
        "PRIVATE_SETTER_FOR_OPEN_PROPERTY: private set",
        "SETTER_VISIBILITY_INCONSISTENT_WITH_PROPERTY_VISIBILITY: public set",
        "GETTER_VISIBILITY_DIFFERS_FROM_PROPERTY_VISIBILITY: private get",
        "DELEGATED_PROPERTY_IN_INTERFACE: d",
        "PRIVATE_FUNCTION_WITH_NO_BODY: nobody",
        "FUN_INTERFACE_WRONG_COUNT_OF_ABSTRACT_MEMBERS: Two",
        "FUN_INTERFACE_ABSTRACT_METHOD_WITH_TYPE_PARAMETERS: a",
        "FUN_INTERFACE_ABSTRACT_METHOD_WITH_DEFAULT_VALUE: a",
        "VALUE_CLASS_EMPTY_CONSTRUCTOR: Empty",
        "INLINE_CLASS_CONSTRUCTOR_WRONG_PARAMETERS_SIZE: Pair2",
        "VALUE_CLASS_CONSTRUCTOR_NOT_FINAL_READ_ONLY_PARAMETER: a",
        "VALUE_CLASS_NOT_FINAL: open",
        "VALUE_CLASS_WITHOUT_JVM_INLINE_ANNOTATION: value",
        "PROPERTY_WITH_BACKING_FIELD_INSIDE_VALUE_CLASS: twice",
        "NON_PRIVATE_OR_PROTECTED_CONSTRUCTOR_IN_SEALED: public",
        "ABSTRACT_MEMBER_NOT_IMPLEMENTED_BY_ENUM_ENTRY: MINUS",
        "SUPERTYPE_APPEARS_TWICE: J",
        "UNSAFE_IMPLICIT_INVOKE_CALL: cb",
        "VARIABLE_WITH_NO_TYPE_NO_INITIALIZER: local",
        "UNUSED_LAMBDA_EXPRESSION: {",
        "UNUSED_EXPRESSION: cb",
        "UNUSED_EXPRESSION: 1",
    ] {
        has(&found, expected);
    }
    // `Ok` is fine apart from its backing field; `PLUS` implements `apply`.
    assert!(!found.iter().any(|f| f.ends_with(": Ok") || f.ends_with(": PLUS")), "{found:#?}");
    assert_eq!(fixed("fun f(cb: (() -> Unit)?) {\n    cb()\n}\n", "UNSAFE_IMPLICIT_INVOKE_CALL"), "fun f(cb: (() -> Unit)?) {\n    cb?.invoke()\n}\n");
    assert_eq!(fixed("fun f() {\n    { println() }\n    println()\n}\n", "UNUSED_LAMBDA_EXPRESSION"), "fun f() {\n    run { println() }\n    println()\n}\n");

    // Sealed classes: inheritors in another package of the same module.
    let mut fx = Fixture::new();
    fx.run("src/main/kotlin/a/S.kt", "package a\n\nsealed interface Shape\n");
    let (lints, text) = fx.run("src/main/kotlin/b/C.kt", "package b\n\nimport a.Shape\n\nclass Circle : Shape\nfun f() = object : Shape {}\n");
    let found: Vec<String> = lints.iter().map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    has(&found, "SEALED_INHERITOR_IN_DIFFERENT_PACKAGE: Shape");
    has(&found, "SEALED_SUPERTYPE_IN_LOCAL_CLASS: Shape");

    // Values that are used: the last statement of a lambda or `if` branch, `when` bodies.
    let quiet = lint("package a\n\nfun g(xs: List<Int>, ok: Boolean): Int {\n    val r = xs.map { x -> x }\n    val v = if (ok) {\n        println()\n        1\n    } else 2\n    println(r)\n    return when (v) {\n        1 -> v\n        else -> 0\n    }\n}\n");
    assert!(!quiet.iter().any(|f| f.starts_with("UNUSED_EXPRESSION") || f.starts_with("UNUSED_LAMBDA")), "{quiet:#?}");
}

#[test]
fn inline_jvm_label_and_variance_errors() {
    let fx = Fixture::new();
    if !fx.typed() {
        return;
    }
    let found = lint(
        r#"package a

class Clash {
    val size: Int = 0
    fun getSize(): Int = 1
    fun sum(xs: List<String>): Int = 0
    fun sum(xs: List<Int>): Int = 0
}

private fun secret() = 1
internal fun inner() = 2
@PublishedApi internal fun published() = 3

inline fun twice(block: () -> Unit, crossinline later: () -> Unit) {
    val saved = block
    Thread { block() }.start()
    Thread { later() }.start()
    println(saved)
    println(secret() + inner() + published())
}

inline fun runLater(crossinline f: () -> Unit) = Thread { f() }

fun use(xs: List<Int>): Int {
    Thread { return }.start()
    runLater { return }
    xs.forEach { if (it > 1) return it }
    outer@ for (x in xs) {
        xs.forEach lit@{ if (it == x) return@lit }
        break@outer
    }
    run lbl@{ for (x in xs) { break@lbl } }
    for (x in xs) { continue@nowhere }
    return@nope 1
}

class Producer<out T>(val item: T) {
    fun put(t: T) {}
    fun all(): List<T> = listOf(item)
    fun sink(xs: MutableList<T>) {}
    fun cmp(c: Comparable<T>) {}
    fun ok(t: @UnsafeVariance T) {}
    private fun hidden(t: T) {}
}

class Consumer<in T> {
    fun get(): T = TODO()
    fun take(t: T) {}
    fun callback(f: (T) -> Unit) {}
}
"#,
    );
    for expected in [
        "CONFLICTING_JVM_DECLARATIONS: size",
        "CONFLICTING_JVM_DECLARATIONS: getSize",
        "USAGE_IS_NOT_INLINABLE: block",
        "NON_LOCAL_RETURN_NOT_ALLOWED: block",
        "NON_PUBLIC_CALL_FROM_PUBLIC_INLINE: secret",
        "NON_PUBLIC_CALL_FROM_PUBLIC_INLINE: inner",
        "RETURN_NOT_ALLOWED: return",
        "NON_LOCAL_RETURN_NOT_ALLOWED: return",
        "NOT_A_LOOP_LABEL: @lbl",
        "UNRESOLVED_LABEL: @nowhere",
        "UNRESOLVED_LABEL: @nope",
        "TYPE_VARIANCE_CONFLICT: T",
    ] {
        has(&found, expected);
    }
    let count = |rule: &str| found.iter().filter(|f| f.starts_with(rule)).count();
    // `sum` twice; `put`, `sink` (out class); `get`, `callback` (in class).
    assert_eq!(count("CONFLICTING_JVM_DECLARATIONS"), 4, "{found:#?}");
    assert_eq!(count("TYPE_VARIANCE_CONFLICT"), 4, "{found:#?}");
    assert_eq!(count("NON_PUBLIC_CALL_FROM_PUBLIC_INLINE"), 2, "{found:#?}");
    // `later` is crossinline; the forEach `return` is inlined; `return@lit` and `break@outer` resolve.
    assert_eq!(count("NON_LOCAL_RETURN_NOT_ALLOWED"), 2, "{found:#?}");
    assert_eq!(count("RETURN_NOT_ALLOWED"), 1, "{found:#?}");
    assert_eq!(count("UNRESOLVED_LABEL"), 2, "{found:#?}");

    // Fixes add the modifiers.
    let src = "inline fun f(block: () -> Unit) {\n    Thread { block() }.start()\n}\n";
    assert_eq!(fixed(src, "NON_LOCAL_RETURN_NOT_ALLOWED"), "inline fun f(crossinline block: () -> Unit) {\n    Thread { block() }.start()\n}\n");
    let src = "inline fun f(block: () -> Unit) {\n    val b = block\n    println(b)\n}\n";
    assert_eq!(fixed(src, "USAGE_IS_NOT_INLINABLE"), "inline fun f(noinline block: () -> Unit) {\n    val b = block\n    println(b)\n}\n");
}

#[test]
fn missing_return_on_some_paths() {
    let fx = Fixture::new();
    if !fx.typed() {
        return;
    }
    let missing = |body: &str| {
        let src = format!("package a\n\nenum class Color {{ RED, GREEN }}\nval lock = Any()\n\n{body}\n");
        lint(&src).iter().filter(|f| f.starts_with("NO_RETURN")).count()
    };
    // Some paths fall off the end.
    assert_eq!(missing("fun f(x: Int): Int {\n    if (x > 0) return 1\n}"), 1);
    assert_eq!(missing("fun f(x: Int): Int {\n    if (x > 0) {\n        return 1\n    } else {\n        println(x)\n    }\n}"), 1);
    assert_eq!(missing("fun f(x: Int): Int {\n    when (x) {\n        1 -> return 1\n        2 -> return 2\n    }\n}"), 1);
    assert_eq!(missing("fun f(xs: List<Int>): Int {\n    for (x in xs) {\n        if (x > 1) return x\n    }\n}"), 1);
    assert_eq!(missing("fun f(x: Int): Int {\n    try {\n        return x\n    } catch (e: Exception) {\n        println(e)\n    }\n}"), 1);
    assert_eq!(missing("fun f(c: Color): Int {\n    when (c) {\n        Color.RED -> return 1\n    }\n}"), 1);
    assert_eq!(missing("val g: Int\n    get() {\n        if (lock.hashCode() > 0) return 1\n    }"), 1);
    assert_eq!(missing("fun f(xs: List<Int>): Int {\n    xs.forEach { println(it) }\n}"), 1);
    // Every path returns, throws, or never ends.
    assert_eq!(missing("fun f(x: Int): Int {\n    if (x > 0) return 1 else return 2\n}"), 0);
    assert_eq!(missing("fun f(x: Int): Int {\n    when {\n        x > 0 -> return 1\n        else -> throw IllegalStateException()\n    }\n}"), 0);
    assert_eq!(missing("fun f(c: Color): Int {\n    when (c) {\n        Color.RED -> return 1\n        Color.GREEN -> return 2\n    }\n}"), 0);
    assert_eq!(missing("fun f(x: Int): Int {\n    while (true) {\n        if (x > 0) return x\n    }\n}"), 0);
    assert_eq!(missing("fun f(x: Int): Int {\n    if (x > 0) return 1\n    error(\"no\")\n}"), 0);
    assert_eq!(missing("fun f(x: Int?): Int {\n    val y = x ?: return 0\n    return y\n}"), 0);
    assert_eq!(missing("fun f(x: Int): Int {\n    try {\n        return x\n    } finally {\n        println()\n    }\n}"), 0);
    // Inline lambdas may return for the function (contracts are not read).
    assert_eq!(missing("fun f(x: Int): Int {\n    synchronized(lock) {\n        return x\n    }\n}"), 0);
    assert_eq!(missing("fun f(x: Int): Int {\n    run { return x }\n}"), 0);
    // Unit, Nothing, and expression bodies need no return.
    assert_eq!(missing("fun f(x: Int): Unit {\n    println(x)\n}\nfun g(): Nothing {\n    throw Exception()\n}\nfun h(x: Int): Int = x"), 0);
}

#[test]
fn redundant_types_and_qualifiers() {
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

import java.util.UUID

class User(val name: String) {
    class Id
    fun id(): User.Id = User.Id()
}

fun f(u: java.util.UUID, l: kotlin.collections.List<Int>, m: java.util.Locale): String {
    val n: Int = 1
    val big: Long = 1
    val s: String = "x"
    val user: User = User("a")
    val any: Any = User("a")
    val maybe: User? = User("a")
    val id = java.util.UUID.randomUUID()
    val other = java.util.Locale.getDefault()
    val boxed = java.lang.Integer::class.java
    return n.toString() + big + s + user + any + maybe + id + other + boxed
}

class Entries : AbstractMap<String, Int>() {
    override val entries: Set<Map.Entry<String, Int>> get() = emptySet()
}
"#,
    );
    has(&found, "RedundantExplicitType: Int");
    has(&found, "RedundantExplicitType: String");
    has(&found, "RedundantExplicitType: User");
    has(&found, "RemoveRedundantQualifierName: java.util.");
    has(&found, "RemoveRedundantQualifierName: kotlin.collections.");
    has(&found, "RemoveRedundantQualifierName: User.");
    let explicit: Vec<_> = found.iter().filter(|f| f.starts_with("RedundantExplicitType")).collect();
    assert_eq!(explicit.len(), 3, "{explicit:#?}");
    let qualifiers: Vec<_> = found.iter().filter(|f| f.starts_with("RemoveRedundantQualifierName")).collect();
    // `java.util.UUID` twice, `kotlin.collections.List`, `User.Id` twice; not `java.util.Locale` (not imported).
    assert_eq!(qualifiers.len(), 5, "{qualifiers:#?}");
    assert!(fixed("package a\n\nfun f() {\n    val n: Int = 1\n    println(n)\n}\n", "RedundantExplicitType").contains("val n = 1"));
}

#[test]
fn projection_compiler_checks() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

class Box<in T>
abstract class Base<T>
abstract class Impl : Base<out String>(), Comparable<*>

fun f(a: List<out String>, b: List<in String>, c: Box<in Int>, d: MutableList<out String>, e: Box<out Int>, g: Comparable<in Int>) {}
"#,
    );
    has(&found, "REDUNDANT_PROJECTION: out");
    has(&found, "REDUNDANT_PROJECTION: in");
    has(&found, "CONFLICTING_PROJECTION: in");
    has(&found, "CONFLICTING_PROJECTION: out");
    has(&found, "PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE: out String");
    has(&found, "PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE: *");
    let count = |id: &str| found.iter().filter(|f| f.starts_with(id)).count();
    // `List<out>` and `Box<in>` redundant (Comparable's `in` too); `List<in>`, `Box<out>` conflicting; `MutableList<out>` fine.
    assert_eq!(count("REDUNDANT_PROJECTION"), 3, "{found:#?}");
    assert_eq!(count("CONFLICTING_PROJECTION"), 2, "{found:#?}");
    assert!(fixed("package a\n\nfun f(a: List<out String>) = a\n", "REDUNDANT_PROJECTION").contains("List<String>"));
}

#[test]
fn platform_classes_mapped_to_kotlin() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

fun f(a: java.lang.Integer, b: java.util.List<String>, c: String, d: List<Int>, e: Integer?, g: java.util.ArrayList<Int>, h: Any): java.lang.Object? = null
"#,
    );
    let mapped: Vec<_> = found.iter().filter(|f| f.starts_with("PLATFORM_CLASS_MAPPED_TO_KOTLIN")).collect();
    assert_eq!(mapped, vec!["PLATFORM_CLASS_MAPPED_TO_KOTLIN: java.lang.Integer", "PLATFORM_CLASS_MAPPED_TO_KOTLIN: java.util.List", "PLATFORM_CLASS_MAPPED_TO_KOTLIN: Integer", "PLATFORM_CLASS_MAPPED_TO_KOTLIN: java.lang.Object"]);
}

#[test]
fn simplification_inspections() {
    let found = lint(
        r#"package a

class A constructor(val x: Int)
class B private constructor(val x: Int)
enum class E { X(), Y, Z(1); constructor(n: Int = 0) }

fun f(xs: List<Int>, s: String, t: String, ok: Boolean, o: java.util.Optional<Int>, n: Boolean?) {
    if (!xs.isEmpty()) println(1)
    if (!s.isBlank()) println(2)
    if (!o.isEmpty()) println(3)
    if (!(xs.size == 1)) println(4)
    if (!(s in xs.map { it.toString() })) println(5)
    if (!(s is CharSequence)) println(6)
    if (ok && true) println(7)
    if (false || ok) println(8)
    if (ok == false) println(9)
    if (n == true) println(10)
    println(xs.count() + s.count() + xs.count { it > 1 })
    if (s.lowercase() == t.lowercase()) println(11)
}
"#,
    );
    has(&found, "RedundantConstructorKeyword: constructor");
    has(&found, "RedundantEnumConstructorInvocation: ()");
    assert_eq!(found.iter().filter(|f| f.starts_with("RedundantConstructorKeyword")).count(), 1, "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("RedundantEnumConstructorInvocation")).count(), 1, "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    has(&found, "ReplaceNegatedIsEmptyWithIsNotEmpty: !xs.isEmpty()");
    has(&found, "ReplaceNegatedIsEmptyWithIsNotEmpty: !s.isBlank()");
    assert!(!found.iter().any(|f| f.contains("!o.isEmpty()")), "Optional.isEmpty has no isNotEmpty: {found:#?}");
    has(&found, "SimplifyNegatedBinaryExpression: !(xs.size == 1)");
    has(&found, "SimplifyNegatedBinaryExpression: !(s is CharSequence)");
    assert_eq!(found.iter().filter(|f| f.starts_with("SimplifyNegatedBinaryExpression")).count(), 3, "{found:#?}");
    has(&found, "SimplifyBooleanWithConstants: ok && true");
    has(&found, "SimplifyBooleanWithConstants: false || ok");
    has(&found, "SimplifyBooleanWithConstants: ok == false");
    assert!(!found.iter().any(|f| f.contains("n == true")), "nullable Boolean: {found:#?}");
    has(&found, "ReplaceCollectionCountWithSize: xs.count()");
    has(&found, "ReplaceCollectionCountWithSize: s.count()");
    assert_eq!(found.iter().filter(|f| f.starts_with("ReplaceCollectionCountWithSize")).count(), 2, "{found:#?}");
    has(&found, "ReplaceWithIgnoreCaseEquals: s.lowercase() == t.lowercase()");
    let check = |body: &str, rule: &str, expected: &str| {
        let text = format!("package a\n\nfun f(xs: List<Int>, s: String, t: String, ok: Boolean) {{\n    println({body})\n}}\n");
        let out = fixed(&text, rule);
        assert!(out.contains(&format!("println({expected})")), "{out}");
    };
    check("!xs.isEmpty()", "ReplaceNegatedIsEmptyWithIsNotEmpty", "xs.isNotEmpty()");
    check("!(s == t)", "SimplifyNegatedBinaryExpression", "s != t");
    check("ok == false", "SimplifyBooleanWithConstants", "!ok");
    check("xs.count()", "ReplaceCollectionCountWithSize", "xs.size");
    check("s.uppercase() != t.uppercase()", "ReplaceWithIgnoreCaseEquals", "!s.equals(t, ignoreCase = true)");
    assert!(fixed("package a\n\nclass A constructor(val x: Int)\n", "RedundantConstructorKeyword").contains("class A(val x: Int)"));
}

#[test]
fn java_static_methods_with_kotlin_analogs() {
    if !Fixture::new().typed() {
        return;
    }
    let text = r#"package a

import java.util.Arrays

fun f(a: IntArray, s: String, c: Char, x: Double) {
    println(Math.max(1, 2) + Math.abs(-1))
    System.out.println(Arrays.toString(a))
    println(Integer.parseInt(s) + Math.pow(x, 2.0))
    Arrays.fill(a, 0, 1, 5)
    println(Character.isDigit(c))
}
"#;
    let found = lint(text);
    let hits: Vec<_> = found.iter().filter(|f| f.starts_with("ReplaceJavaStaticMethodWithKotlinAnalog")).map(|f| f.split(": ").nth(1).unwrap().to_string()).collect();
    assert_eq!(hits, vec!["Math.max(1, 2)", "Math.abs(-1)", "System.out.println(Arrays.toString(a))", "Arrays.toString(a)", "Integer.parseInt(s)", "Math.pow(x, 2.0)", "Character.isDigit(c)"], "{found:#?}");
    let out = fixed(&text.replace("Math.max(1, 2) + ", ""), "ReplaceJavaStaticMethodWithKotlinAnalog");
    assert!(out.contains("println(abs(-1))") && out.contains("import kotlin.math.abs\n"), "{out}");
    let out = fixed(&text.replace("Math.max(1, 2) + Math.abs(-1)", "1").replace("System.out.println(", "println("), "ReplaceJavaStaticMethodWithKotlinAnalog");
    assert!(out.contains("println(a.contentToString())"), "{out}");
}

#[test]
fn suspend_companion_readln_and_main() {
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

class Config {
    companion object {
        fun load() = Config()
        val name = "c"
    }
}

suspend fun pause() = kotlinx.coroutines.delay(1)
suspend fun plain(x: Int): Int = x + 1
suspend fun calls(): Int {
    pause()
    return 1
}
suspend fun invokes(block: suspend () -> Unit) = block()
interface Repo { suspend fun get(): Int }
open class Base { open suspend fun f() {} }

fun g() {
    println(Config.Companion.load())
    println(Config.Companion.name + Config.name)
    val a = readLine()!!
    val b = readLine()
    println(a + b)
}
"#,
    );
    let suspends: Vec<_> = found.iter().filter(|f| f.starts_with("RedundantSuspendModifier")).collect();
    assert_eq!(suspends.len(), 1, "only 'plain': {found:#?}");
    let companions: Vec<_> = found.iter().filter(|f| f.starts_with("RedundantCompanionReference")).collect();
    assert_eq!(companions.len(), 2, "{found:#?}");
    has(&found, "ReplaceReadLineWithReadln: readLine()!!");
    has(&found, "ReplaceReadLineWithReadln: readLine()");
    assert!(fixed("package a\n\nsuspend fun plain(x: Int): Int = x + 1\n", "RedundantSuspendModifier").contains("\nfun plain"));
    assert!(fixed("package a\n\nclass C { companion object { fun f() = 1 } }\nfun g() = C.Companion.f()\n", "RedundantCompanionReference").contains("= C.f()"));
    assert!(fixed("package a\n\nfun g() = readLine()!!.length\n", "ReplaceReadLineWithReadln").contains("= readln().length"));
    // `main` returning a value is not an entry point.
    let main = lint("package a\n\nfun main() = \"x\".length\n");
    has(&main, "MainFunctionReturnUnit: main");
    let ok = lint("package a\n\nfun main() = println(1)\nfun main(args: Array<String>) {}\n");
    assert!(!ok.iter().any(|f| f.starts_with("MainFunctionReturnUnit")), "{ok:#?}");
}

#[test]
fn restriction_and_value_compiler_checks() {
    let found = compiler(
        r#"package a

object O<T>
enum class E<T> { A }
val f = fun named() {}
val g = fun String.() {}
open class C {
    open tailrec fun down(n: Int): Int = if (n == 0) 0 else down(n - 1)
    tailrec fun fact(n: Int, acc: Int = 1): Int = if (n <= 1) acc else fact(n - 1, acc * n)
    tailrec fun none(n: Int): Int = n + 1
}
"#,
    );
    has(&found, "TYPE_PARAMETERS_IN_OBJECT: <T>");
    has(&found, "TYPE_PARAMETERS_IN_ENUM: <T>");
    has(&found, "ANONYMOUS_FUNCTION_WITH_NAME: named");
    assert_eq!(found.iter().filter(|f| f.starts_with("ANONYMOUS_FUNCTION_WITH_NAME")).count(), 1, "{found:#?}");
    has(&found, "TAILREC_ON_VIRTUAL_MEMBER_ERROR: tailrec");
    has(&found, "NO_TAIL_CALLS_FOUND: none");
    assert_eq!(found.iter().filter(|f| f.starts_with("NO_TAIL_CALLS_FOUND") || f.starts_with("TAILREC")).count(), 2, "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

enum class Color { RED }

fun f(x: Int, d: Double, s: String?, a: IntArray, b: IntArray, any: Any, xs: List<Any>, c: Color) {
    println(x / 0 + x % 0 + d / 0)
    println(s ?: null)
    println(a == b)
    println(any is List<String>)
    println(any is List<*>)
    println(xs is ArrayList<Any>)
    when (c) {
        null -> println()
        else -> println()
    }
}
"#,
    );
    assert_eq!(found.iter().filter(|f| f.starts_with("DIVISION_BY_ZERO")).count(), 2, "{found:#?}");
    has(&found, "USELESS_ELVIS_RIGHT_IS_NULL: ?: null");
    has(&found, "ARRAY_EQUALITY_OPERATOR_CAN_BE_REPLACED_WITH_EQUALS: ==");
    has(&found, "CANNOT_CHECK_FOR_ERASED: List<String>");
    assert_eq!(found.iter().filter(|f| f.starts_with("CANNOT_CHECK_FOR_ERASED")).count(), 1, "{found:#?}");
    has(&found, "SENSELESS_NULL_IN_WHEN: null");
    assert!(fixed("package a\n\nfun f(a: IntArray, b: IntArray) = a != b\n", "ARRAY_EQUALITY_OPERATOR_CAN_BE_REPLACED_WITH_EQUALS").contains("= !a.contentEquals(b)"));
}

#[test]
fn impossible_smart_casts() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

open class C {
    var name: String? = null
    val fixed: String? = null
    val computed: String? get() = name
    open val opened: String? = null

    fun f() {
        if (name != null) println(name.length)
        if (this.name != null) println(this.name.length)
        if (fixed != null) println(fixed.length)
        if (computed != null && computed.length > 1) println()
        if (opened == null) return
        println(opened.length)
        println(name?.length)
        if (name != null) println(name?.length)
    }
}
"#,
    );
    let hits: Vec<_> = found.iter().filter(|f| f.starts_with("SMARTCAST_IMPOSSIBLE")).collect();
    assert_eq!(hits, vec!["SMARTCAST_IMPOSSIBLE: name", "SMARTCAST_IMPOSSIBLE: this.name", "SMARTCAST_IMPOSSIBLE: computed", "SMARTCAST_IMPOSSIBLE: opened"], "{found:#?}");
}

#[test]
fn annotation_targets() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

@Target(AnnotationTarget.FUNCTION)
annotation class OnFun

@Target(AnnotationTarget.CLASS, AnnotationTarget.PROPERTY)
annotation class OnClassOrProperty

@OnFun
class C(@OnClassOrProperty val p: Int, @OnClassOrProperty q: Int) {
    @JvmField val f = 1
    @JvmField fun g() {}
    @get:JvmName("h") val h = 1
    @OnFun fun ok() {}
    @JvmStatic fun notStatic() {}
    companion object {
        @JvmStatic fun fine() {}
    }
}

interface I { @JvmField val x: Int }

@JvmStatic fun top() {}
"#,
    );
    let wrong: Vec<_> = found.iter().filter(|f| f.starts_with("WRONG_ANNOTATION_TARGET")).collect();
    assert_eq!(wrong, vec!["WRONG_ANNOTATION_TARGET: @OnFun", "WRONG_ANNOTATION_TARGET: @OnClassOrProperty", "WRONG_ANNOTATION_TARGET: @JvmField", "WRONG_ANNOTATION_TARGET: @JvmField"], "{found:#?}");
    let statics: Vec<_> = found.iter().filter(|f| f.starts_with("JVM_STATIC_NOT_IN_OBJECT_OR_COMPANION")).collect();
    assert_eq!(statics.len(), 2, "{found:#?}");
}

#[test]
fn bug_style_and_redundancy_batch() {
    let syntactic = lint(
        r#"package a

interface I { fun f() }
class D(var x: I) : I by x
fun run() = { println() }
fun typed(): () -> Unit = { println() }
var nothing = null
val fine = null
val y = -1.inc()
val z = (-1).inc()
open class Base { open fun f(a: Int): Int = a }
class Sub : Base() { override fun f(a: Int) = super.f(a) }
class Sub2 : Base() { override fun f(a: Int): Int { return super.f(a + 1) } }
class Final { protected val p = 1 }
open class Open { protected val p = 1 }
fun find(m: Map<String, String>, k: String): String? {
    return m[k] ?: return null
}
"#,
    );
    has(&syntactic, "DelegationToVarProperty: x");
    has(&syntactic, "FunctionWithLambdaExpressionBody: =");
    assert_eq!(syntactic.iter().filter(|f| f.starts_with("FunctionWithLambdaExpressionBody")).count(), 1, "{syntactic:#?}");
    has(&syntactic, "ImplicitNullableNothingType: nothing");
    assert!(!syntactic.iter().any(|f| f == "ImplicitNullableNothingType: fine"), "{syntactic:#?}");
    has(&syntactic, "WrapUnaryOperator: -1.inc()");
    assert_eq!(syntactic.iter().filter(|f| f.starts_with("WrapUnaryOperator")).count(), 1, "{syntactic:#?}");
    has(&syntactic, "RedundantOverride: f");
    assert_eq!(syntactic.iter().filter(|f| f.starts_with("RedundantOverride")).count(), 1, "{syntactic:#?}");
    has(&syntactic, "ProtectedInFinal: protected");
    assert_eq!(syntactic.iter().filter(|f| f.starts_with("ProtectedInFinal")).count(), 1, "{syntactic:#?}");
    has(&syntactic, "RedundantElvisReturnNull: ?: return null");
    assert!(fixed("package a\n\nval y = -1.inc()\n", "WrapUnaryOperator").contains("= (-1).inc()"));
    assert!(fixed("package a\n\nfun run() = { println() }\n", "FunctionWithLambdaExpressionBody").contains("fun run() { println() }"));
    if !Fixture::new().typed() {
        return;
    }
    let typed = lint(
        r#"package a

data class P(val name: String, val age: Int)

open class Widget {
    open val size: Int = 1
    val area = size * 2
    init {
        draw()
        register(this)
        listOf(1).forEach { draw() }
    }
    open fun draw() {}
    fun register(w: Widget) {}
}

fun f(p: P, s: String) {
    var xs = listOf(1)
    xs += 2
    val ys = mutableListOf(1)
    ys += 2
    println(p.copy("b", 3) + p.copy(name = "c"))
    println(s.substring(0, 2) + s.substring(0, s.length - 1) + s.substring(1, 2))
}
"#,
    );
    has(&typed, "SuspiciousCollectionReassignment: +=");
    assert_eq!(typed.iter().filter(|f| f.starts_with("SuspiciousCollectionReassignment")).count(), 1, "{typed:#?}");
    has(&typed, "CopyWithoutNamedArguments: copy");
    assert_eq!(typed.iter().filter(|f| f.starts_with("CopyWithoutNamedArguments")).count(), 1, "{typed:#?}");
    has(&typed, "ReplaceSubstringWithTake: substring");
    has(&typed, "ReplaceSubstringWithDropLast: substring");
    let leaks: Vec<_> = typed.iter().filter(|f| f.starts_with("LeakingThis")).collect();
    assert_eq!(leaks, vec!["LeakingThis: size", "LeakingThis: draw", "LeakingThis: this"], "{typed:#?}");
    let out = fixed("package a\n\ndata class P(val name: String, val age: Int)\nfun f(p: P) = p.copy(\"b\", 3)\n", "CopyWithoutNamedArguments");
    assert!(out.contains("p.copy(name = \"b\", age = 3)"), "{out}");
}

#[test]
fn identity_lateinit_and_constant_arguments() {
    let found = compiler(
        r#"package a

class C {
    lateinit var a: String
    lateinit var b: String
    init {
        a = "x"
        if (a.isEmpty()) b = "y"
    }
}
"#,
    );
    has(&found, "UNNECESSARY_LATEINIT: lateinit");
    assert_eq!(found.iter().filter(|f| f.starts_with("UNNECESSARY_LATEINIT")).count(), 1, "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

const val NAME = "n"
val other = "o"

annotation class Ann(val v: String)

@Ann(NAME) class A
@Ann(other) class B

fun f(x: Int, y: Int, s: String, t: String) = x === y || s === t
"#,
    );
    has(&found, "ANNOTATION_ARGUMENT_MUST_BE_CONST: other");
    assert_eq!(found.iter().filter(|f| f.starts_with("ANNOTATION_ARGUMENT_MUST_BE_CONST")).count(), 1, "{found:#?}");
    has(&found, "FORBIDDEN_IDENTITY_EQUALS: ===");
    assert_eq!(found.iter().filter(|f| f.starts_with("FORBIDDEN_IDENTITY_EQUALS")).count(), 1, "{found:#?}");
}

#[test]
fn parameters_copies_destructuring_kdoc_and_getters() {
    let found = lint(
        r#"package a

class A(private val x: Int, private val y: Int, val z: Int) {
    val doubled = x * 2
    init { println(y) }
    fun show() = y
}
"#,
    );
    let params: Vec<_> = found.iter().filter(|f| f.starts_with("CanBeParameter")).collect();
    assert_eq!(params, vec!["CanBeParameter: val"], "{found:#?}");
    assert!(fixed("package a\n\nclass A(private val x: Int) {\n    val d = x\n}\n", "CanBeParameter").contains("class A(x: Int)"));
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

data class Person(val name: String, val age: Int)

fun f(p: Person, file: java.io.File, xs: java.util.ArrayList<Int>, n: java.util.concurrent.atomic.AtomicInteger) {
    p.copy(age = 1)
    val q = p.copy(age = 2)
    val (age, name) = p
    val (name2, age2) = p
    println(q.toString() + age + name + name2 + age2)
    println(file.getName() + file.isFile() + file.name + xs.isEmpty() + n.getAndIncrement())
}
"#,
    );
    has(&found, "UnusedDataClassCopyResult: copy");
    assert_eq!(found.iter().filter(|f| f.starts_with("UnusedDataClassCopyResult")).count(), 1, "{found:#?}");
    let wrong: Vec<_> = found.iter().filter(|f| f.starts_with("DestructuringWrongName")).collect();
    assert_eq!(wrong, vec!["DestructuringWrongName: age", "DestructuringWrongName: name"], "{found:#?}");
    has(&found, "UsePropertyAccessSyntax: getName");
    has(&found, "UsePropertyAccessSyntax: isFile");
    assert_eq!(found.iter().filter(|f| f.starts_with("UsePropertyAccessSyntax")).count(), 2, "{found:#?}");
    assert!(fixed("package a\n\nfun f(file: java.io.File) = file.getName()\n", "UsePropertyAccessSyntax").contains("= file.name"));
}

#[test]
fn idiom_folds_and_guards() {
    let check = |body: &str, rule: &str, expected: &str| {
        let text = format!("package a\n\nfun f(x: String?, y: String, xs: List<Any?>, n: Int): Any? {{\n{body}\n    return null\n}}\n");
        let out = fixed(&text, rule);
        assert!(out.contains(expected), "{rule}: {out}");
    };
    check("    val a = if (x != null) x else y", "IfThenToElvis", "val a = x ?: y");
    check("    val a = if (x == null) y else x", "IfThenToElvis", "val a = x ?: y");
    check("    val a = if (x != null) x.length else null", "IfThenToSafeAccess", "val a = x?.length");
    check("    if (x != null) x.trim()", "IfThenToSafeAccess", "    x?.trim()");
    check("    val a = x?.trim()\n    if (a == null) return null\n    println(a)", "FoldInitializerAndIfToElvis", "val a = x?.trim() ?: return null\n    println(a)");
    check("    val s = if (n == 1) \"a\" else if (n == 2) \"b\" else if (n == 3) \"c\" else \"d\"", "CascadeIf", "val s = when {\n        n == 1 -> \"a\"\n        n == 2 -> \"b\"\n        n == 3 -> \"c\"\n        else -> \"d\"\n    }");
    check("    val v: Int\n    v = n + 1\n    println(v)", "JoinDeclarationAndAssignment", "val v: Int = n + 1\n    println(v)");
    let found = lint("package a\n\nfun f(x: String?) = if (x != null) x else null\nfun g(n: Int) = if (n == 1) 1 else if (n == 2) 2 else 3\n");
    assert!(!found.iter().any(|f| f.starts_with("IfThenToElvis") || f.starts_with("CascadeIf")), "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    check("    if (x == null) throw IllegalArgumentException(\"no x\")", "ReplaceGuardClauseWithFunctionCall", "    requireNotNull(x) { \"no x\" }");
    check("    if (n < 0) throw IllegalStateException()", "ReplaceGuardClauseWithFunctionCall", "    check(n >= 0)");
    check("    if (!y.isEmpty()) throw IllegalArgumentException(y)", "ReplaceGuardClauseWithFunctionCall", "    require(y.isEmpty()) { y }");
    check("    println(xs.filter { it is String })", "SimplifiableCall", "println(xs.filterIsInstance<String>())");
    check("    println(xs.filter { v -> v != null })", "SimplifiableCall", "println(xs.filterNotNull())");
    check("    println(listOf(y).associate { it to it.length })", "ReplaceAssociateFunction", "println(listOf(y).associateWith { it.length })");
    check("    println(listOf(y).associate { it.length to it })", "ReplaceAssociateFunction", "println(listOf(y).associateBy { it.length })");
}

#[test]
fn templates_lifts_and_redundant_calls() {
    let check = |body: &str, rule: &str, expected: &str| {
        let text = format!("package a\n\nfun f(x: String, n: Int, xs: List<Int>): Any? {{\n{body}\n    return null\n}}\n");
        let out = fixed(&text, rule);
        assert!(out.contains(expected), "{rule}: {out}");
    };
    check("    println(\"a\" + x + \"b\" + n + \"_\" + (n + 1))", "ConvertToStringTemplate", "println(\"a${x}b${n}_${n + 1}\")");
    check("    println(\"a\" + x + \" b\")", "ConvertToStringTemplate", "println(\"a$x b\")");
    check("    if (n > 1) return 1 else return 2", "LiftReturnOrAssignment", "    return if (n > 1) 1 else 2");
    check("    var v = 0\n    when (n) {\n        1 -> v = 1\n        else -> v = 2\n    }\n    println(v)", "LiftReturnOrAssignment", "    v = when (n) {\n        1 -> 1\n        else -> 2\n    }");
    let found = lint("package a\n\nfun f(x: String) = \"a\" + \"b\"\nfun g(n: Int): Int {\n    if (n > 1) return 1 else println()\n    return 0\n}\n");
    assert!(!found.iter().any(|f| f.starts_with("ConvertToStringTemplate") || f.starts_with("LiftReturnOrAssignment")), "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    check("    requireNotNull(x)\n    println(x)", "RedundantRequireNotNullCall", "{\n    println(x)");
    check("    println(xs.asSequence().toList())", "RedundantAsSequence", "println(xs.toList())");
    check("    println(runCatching { n + 1 }.getOrThrow())", "RedundantRunCatching", "println(run { n + 1 })");
    assert!(fixed("package a\n\nfun g(n: Int): String? {\n    if (n > 1) return \"a\"\n    return \"b\"\n}\n", "RedundantNullableReturnType").contains("fun g(n: Int): String {"));
    let found = lint("package a\n\nfun g(n: Int): String? = if (n > 1) \"a\" else null\nopen class O { open fun h(): String? = \"a\" }\n");
    assert!(!found.iter().any(|f| f.starts_with("RedundantNullableReturnType")), "{found:#?}");
}

#[test]
fn reference_compiler_checks() {
    let found = compiler("package a\n\nclass C {\n    constructor(val x: Int)\n}\nfun f(val x: Int) {\n    for (val i in 0..1) {}\n    try {} catch (var e: Exception) {}\n}\n");
    for code in ["VAL_OR_VAR_ON_FUN_PARAMETER", "VAL_OR_VAR_ON_SECONDARY_CONSTRUCTOR_PARAMETER", "VAL_OR_VAR_ON_LOOP_PARAMETER", "VAL_OR_VAR_ON_CATCH_PARAMETER"] {
        assert!(found.iter().any(|f| f.starts_with(code)), "{code}: {found:#?}");
    }
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

class Plain
class WithCompanion { companion object }
object Single
abstract class Base { abstract fun f(): Int; open fun g() = 1 }
class Impl : Base() {
    override fun f() = super.f()
    override fun g() = super.g()
}
class NotData(val a: Int)
data class Pair2(val a: Int, val b: Int)
fun compute() = 1
const val A = 1
val b = 2
const val C = A
const val D = b
const val E = compute()

fun use(file: java.io.File) {
    val x = Plain
    val y = WithCompanion
    val z = Single
    val g = compute
    val h = ::compute
    val n = file.name
    val (p, q) = NotData(1)
    val (r, s) = Pair2(1, 2)
    println(listOf(x, y, z, g, h, n, p, q, r, s))
}
"#,
    );
    let codes = |c: &str| found.iter().filter(|f| f.starts_with(c)).cloned().collect::<Vec<_>>();
    assert_eq!(codes("NO_COMPANION_OBJECT"), vec!["NO_COMPANION_OBJECT: Plain"], "{found:#?}");
    assert_eq!(codes("FUNCTION_CALL_EXPECTED"), vec!["FUNCTION_CALL_EXPECTED: compute"], "{found:#?}");
    assert_eq!(codes("ABSTRACT_SUPER_CALL"), vec!["ABSTRACT_SUPER_CALL: f"], "{found:#?}");
    assert_eq!(codes("CONST_VAL_WITH_NON_CONST_INITIALIZER"), vec!["CONST_VAL_WITH_NON_CONST_INITIALIZER: b", "CONST_VAL_WITH_NON_CONST_INITIALIZER: compute()"], "{found:#?}");
    assert_eq!(codes("COMPONENT_FUNCTION_MISSING"), vec!["COMPONENT_FUNCTION_MISSING: p", "COMPONENT_FUNCTION_MISSING: q"], "{found:#?}");
}

#[test]
fn more_declaration_restrictions() {
    let found = compiler(
        r#"package a

class Outer {
    typealias Nested = Int
    companion object {}
    companion object Second {}
    inner class Ok
}
object O {
    inner class Bad
    companion object {}
    constructor()
}
interface I : Runnable by Runnable {}
class P(val x: Int) {
    constructor() : this(1)
    constructor(s: String) {}
}
data class D(vararg val xs: Int)
typealias Bounded<T : Any> = List<T>
fun <T : String.() -> Unit> g(t: T) {}
fun f(n: Int) {
    interface LocalI
    object LocalO
    enum class LocalE { A }
    when (var x = n) { else -> {} }
    when { n > 1, n < 0 -> {} }
}
"#,
    );
    for code in [
        "TOPLEVEL_TYPEALIASES_ONLY: Nested",
        "MANY_COMPANION_OBJECTS: companion",
        "WRONG_MODIFIER_CONTAINING_DECLARATION: inner",
        "COMPANION_OBJECT_NOT_ALLOWED: companion",
        "SECONDARY_CONSTRUCTOR_IN_OBJECT: constructor",
        "DELEGATION_IN_INTERFACE: Runnable by Runnable",
        "PRIMARY_CONSTRUCTOR_DELEGATION_CALL_EXPECTED: constructor",
        "DATA_CLASS_VARARG_PARAMETER: vararg",
        "BOUND_ON_TYPE_ALIAS_PARAMETER_NOT_ALLOWED: Any",
        "UPPER_BOUND_IS_EXTENSION_FUNCTION_TYPE: String.() -> Unit",
        "LOCAL_INTERFACE_NOT_ALLOWED: LocalI",
        "LOCAL_OBJECT_NOT_ALLOWED: LocalO",
        "LOCAL_ENUM_NOT_ALLOWED: LocalE",
        "ILLEGAL_DECLARATION_IN_WHEN_SUBJECT: var x = n",
        "COMMA_IN_WHEN_CONDITION_WITHOUT_ARGUMENT: n > 1, n < 0",
    ] {
        has(&found, code);
    }
    assert_eq!(found.iter().filter(|f| f.starts_with("PRIMARY_CONSTRUCTOR_DELEGATION_CALL_EXPECTED")).count(), 1, "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("WRONG_MODIFIER_CONTAINING_DECLARATION")).count(), 1, "{found:#?}");
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler("package a\n\nenum class Color { RED, GREEN }\nfun f(c: Any): Boolean {\n    val r: Color.RED? = null\n    return c is Color.RED || r == null\n}\n");
    has(&found, "ENUM_ENTRY_AS_TYPE: Color.RED");
    has(&found, "IS_ENUM_ENTRY: Color.RED");
}

#[test]
fn operator_modifier_and_argument_checks() {
    let found = compiler(
        r#"package a

class V(val x: Int) {
    operator fun plus(o: V) = V(x + o.x)
    operator fun minus() = this
    operator fun frobnicate() = 1
    operator fun inc() {}
    operator fun compareTo(o: V): Boolean = true
    operator fun plusAssign(o: V): V = this
    operator fun component1() = x
    operator fun get(i: Int, j: Int) = x
    infix fun and(o: V) = this
    infix fun or(a: V, b: V) = this
    open inline fun bad() {}
}
operator fun top(v: V) = v
infix fun lone(v: V) = v
val outside = this
fun f(x: Int, y: Int) = x + y
val g = f(x = 1, x = 2)
"#,
    );
    let codes = |c: &str| found.iter().filter(|f| f.starts_with(c)).count();
    assert_eq!(codes("INAPPLICABLE_OPERATOR_MODIFIER"), 3, "minus(), frobnicate, top: {found:#?}");
    assert_eq!(codes("INC_DEC_SHOULD_NOT_RETURN_UNIT"), 1, "{found:#?}");
    assert_eq!(codes("COMPARE_TO_TYPE_MISMATCH"), 1, "{found:#?}");
    assert_eq!(codes("ASSIGNMENT_OPERATOR_SHOULD_RETURN_UNIT"), 1, "{found:#?}");
    assert_eq!(codes("INAPPLICABLE_INFIX_MODIFIER"), 2, "or, lone: {found:#?}");
    assert_eq!(codes("DECLARATION_CANT_BE_INLINED"), 1, "{found:#?}");
    has(&found, "NO_THIS: this");
    has(&found, "ARGUMENT_PASSED_TWICE: x");
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

fun f(a: Int, b: Int) = a + b
fun v(vararg xs: Int) = xs.size
fun g(arr: IntArray, s: StringBuilder) {
    println(f(1, a = 2))
    println(f(*arr))
    println(v(*arr))
    println(java.io.File(pathname = "x"))
}
"#,
    );
    has(&found, "ARGUMENT_PASSED_TWICE: 1");
    has(&found, "NON_VARARG_SPREAD: *");
    assert_eq!(found.iter().filter(|f| f.starts_with("NON_VARARG_SPREAD")).count(), 1, "{found:#?}");
    has(&found, "NAMED_ARGUMENTS_NOT_ALLOWED: pathname");
}

#[test]
fn property_initialization_order() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

class C {
    val a = b + 1
    val c = lazyB()
    val d: () -> Int = { b }
    val e get() = b
    init { println(this.b) }
    val b = 2
    val f = b
    fun lazyB() = b
}
"#,
    );
    let hits: Vec<_> = found.iter().filter(|f| f.starts_with("UNINITIALIZED_VARIABLE")).collect();
    assert_eq!(hits, vec!["UNINITIALIZED_VARIABLE: b", "UNINITIALIZED_VARIABLE: b"], "{found:#?}");
}

#[test]
fn let_when_constants_and_suspicious_declarations() {
    let found = lint(
        r#"package a

class C {
    var counter: Int = 0
        get() = 42
    var ok: Int = 0
        get() = field + 1
}

fun f(n: Int) {
    if (true) println()
    while (true) { break }
    if (1 > 2) println()
    val k = n * 2
    when (k) {
        1 -> println()
        else -> println()
    }
}
"#,
    );
    has(&found, "SuspiciousVarProperty: counter");
    assert_eq!(found.iter().filter(|f| f.starts_with("SuspiciousVarProperty")).count(), 1, "{found:#?}");
    let constants: Vec<_> = found.iter().filter(|f| f.starts_with("KotlinConstantConditions")).collect();
    assert_eq!(constants, vec!["KotlinConstantConditions: true", "KotlinConstantConditions: 1 > 2"], "{found:#?}");
    has(&found, "MoveVariableDeclarationIntoWhen: k");
    let moved = fixed("package a\n\nfun f(n: Int) {\n    val k = n * 2\n    when (k) {\n        else -> println()\n    }\n}\n", "MoveVariableDeclarationIntoWhen");
    assert!(moved.contains("{\n    when (val k = n * 2) {"), "{moved}");
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

data class T3(val a: Int, val b: Int, val c: Int)
open class Base { @Deprecated("old") open fun old() {} }
class Sub : Base() { override fun old() {} }

val home = System.getProperty("user.home")
private val hidden = System.getProperty("x")

fun g(s: String?, t: T3) {
    println(s?.let { it.length })
    println(s?.let { v -> v.trim() })
    println(s?.let { it.length + it.length })
    val (a, b) = t
    println(a + b)
}
"#,
    );
    has(&found, "OverridingDeprecatedMember: old");
    has(&found, "HasPlatformType: home");
    assert!(!found.iter().any(|f| f == "HasPlatformType: hidden"), "{found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("SimpleRedundantLet")).count(), 2, "{found:#?}");
    has(&found, "IncompleteDestructuring: (a, b)");
    let out = fixed("package a\n\nfun g(s: String?) = s?.let { it.length }\n", "SimpleRedundantLet");
    assert!(out.contains("= s?.length"), "{out}");
}

#[test]
fn mismatches_in_branches() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

fun f(c: Boolean, n: Int): Int {
    val a: Int = if (c) 1 else "x"
    val b: String = when (n) {
        1 -> "one"
        2 -> 2
        else -> throw Error()
    }
    var v: Int = 0
    v = if (c) 1 else "y"
    println(a + b.length + v)
    return if (c) "z" else 2
}
fun g(c: Boolean): String = if (c) "a" else 'b'
val h: () -> Int = { "x" }
val k: (Int) -> Int = { if (it > 1) it else 2 }
val u: () -> Unit = { 1 }
"#,
    );
    let mismatches: Vec<_> = found.iter().filter(|f| f.contains("MISMATCH")).collect();
    assert_eq!(mismatches.iter().filter(|m| m.as_str() == "RETURN_TYPE_MISMATCH: \"x\"").count(), 1, "{mismatches:#?}");
    for expected in ["INITIALIZER_TYPE_MISMATCH: \"x\"", "INITIALIZER_TYPE_MISMATCH: 2", "ASSIGNMENT_TYPE_MISMATCH: \"y\"", "RETURN_TYPE_MISMATCH: \"z\"", "RETURN_TYPE_MISMATCH: 'b'"] {
        assert!(mismatches.iter().any(|m| m.as_str() == expected), "missing {expected}: {mismatches:#?}");
    }
    assert!(!mismatches.iter().any(|m| m.ends_with(": 1") || m.ends_with("\"one\"") || m.ends_with("\"a\"")), "{mismatches:#?}");
}

#[test]
fn object_checks_defaults_and_java_idioms() {
    let found = lint_at(
        "src/main/kotlin/a/Other.kt",
        r#"package a

annotation class Names(val value: Array<String>)
object Marker
fun greet(name: String, loud: Boolean = false, times: Int = 1) = name
@Names(value = arrayOf("a", "b"))
class Widget {
    fun f(x: Any): String? {
        val q = greet("a", loud = false)
        val r = greet("a", false, 2)
        val run = { 1 }()
        val s = (x as? String)!!
        return if (x is Marker) q + r + run + s else null
    }
}
"#,
    );
    assert!(!found.iter().any(|f| f.starts_with("FileNameMatchesClass")), "{found:#?}");
    has(&lint_at("src/main/kotlin/a/Other.kt", "package a\n\nclass Widget\n"), "FileNameMatchesClass: Widget");
    assert!(lint_at("src/main/kotlin/a/Widget.kt", "package a\n\nclass Widget\n").iter().all(|f| !f.starts_with("FileNameMatchesClass")));
    for code in ["RedundantObjectTypeCheck: is", "ReplaceArrayOfWithLiteral: arrayOf", "RedundantValueArgument: loud = false", "RedundantLambdaOrAnonymousFunction: { 1 }()", "ReplaceNotNullAssertionWithElvisReturn: !!"] {
        has(&found, code);
    }
    // `greet("a", false, 2)`: only a trailing positional argument could go, and 2 is not the default.
    assert_eq!(found.iter().filter(|f| f.starts_with("RedundantValueArgument")).count(), 1, "{found:#?}");
    let out = fixed("package a\n\nfun greet(name: String, loud: Boolean = false) = name\nval g = greet(\"a\", loud = false)\n", "RedundantValueArgument");
    assert!(out.contains("greet(\"a\")"), "{out}");
    if !Fixture::new().typed() {
        return;
    }
    let found = lint("package a\n\nfun f(xs: MutableList<Int>, m: Map<String, Int>) {\n    java.util.Collections.sort(xs)\n    m.forEach { k, v -> println(k + v) }\n}\n");
    has(&found, "JavaCollectionsStaticMethod: java.util.Collections.sort(xs)");
    has(&found, "JavaMapForEach: forEach");
    let out = fixed("package a\n\nfun f(m: Map<String, Int>) = m.forEach { k, v -> println(k + v) }\n", "JavaMapForEach");
    assert!(out.contains("forEach { (k, v) -> "), "{out}");
}

#[test]
fn constructor_ranges_and_equality_inspections() {
    let found = lint(
        r#"package a

class P(x: Int, private val y: Int) {
    val x = x
    constructor() : this(1, 2) {}
}
class Q {
    override fun equals(other: Any?): Boolean = this == other
}
fun f() {
    for (i in 10..1) println(i)
    for (i in 1..10) println(i)
}
"#,
    );
    has(&found, "CanBePrimaryConstructorProperty: x");
    has(&found, "RemoveEmptySecondaryConstructorBody: {}");
    has(&found, "RecursiveEqualsCall: ==");
    has(&found, "EmptyRange: 10..1");
    assert_eq!(found.iter().filter(|f| f.starts_with("EmptyRange")).count(), 1, "{found:#?}");
    let out = fixed("package a\n\nclass P(x: Int) {\n    val x = x\n}\n", "CanBePrimaryConstructorProperty");
    assert!(out.contains("class P(val x: Int) {\n}"), "{out}");
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

object Single : java.io.Serializable
object WithResolve : java.io.Serializable { private fun readResolve(): Any = WithResolve }

suspend fun s() {
    Thread.sleep(1)
}

fun g(n: Int, s: String, xs: List<String>) {
    for (i in 0 until n) println(i)
    println(if (s.isEmpty()) "none" else s)
    println(n >= 1 && n <= 10)
    println("${n.toString()} ${xs.filterNotNull()}")
}
"#,
    );
    has(&found, "JavaIoSerializableObjectMustHaveReadResolve: Single");
    assert_eq!(found.iter().filter(|f| f.starts_with("JavaIoSerializableObjectMustHaveReadResolve")).count(), 1, "{found:#?}");
    has(&found, "BlockingMethodInNonBlockingContext: sleep");
    has(&found, "ReplaceUntilWithRangeUntil: until");
    has(&found, "ReplaceIsEmptyWithIfEmpty: if");
    has(&found, "ConvertTwoComparisonsToRangeCheck: n >= 1 && n <= 10");
    has(&found, "RemoveToStringInStringTemplate: toString");
    has(&found, "UselessCallOnCollection: filterNotNull");
    let check = |body: &str, rule: &str, expected: &str| {
        let out = fixed(&format!("package a\n\nfun g(n: Int, s: String) {{\n    println({body})\n}}\n"), rule);
        assert!(out.contains(expected), "{rule}: {out}");
    };
    check("if (s.isEmpty()) \"none\" else s", "ReplaceIsEmptyWithIfEmpty", "println(s.ifEmpty { \"none\" })");
    check("n >= 1 && n < 10", "ConvertTwoComparisonsToRangeCheck", "println(n in 1..<10)");
    check("\"${n.toString()}\"", "RemoveToStringInStringTemplate", "println(\"${n}\")");
}

#[test]
fn inner_let_format_and_reference_inspections() {
    let found = lint("package a\n\nfun f(xs: List<Int>) = xs.map { ::println }\n");
    has(&found, "SuspiciousCallableReferenceInLambda: ::println");
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

enum class Color { RED }

class Outer(val size: Int) {
    inner class Plain { fun f() = 1 }
    inner class UsesOuter { fun f() = size }
    inner class UsesOther { fun g() = Plain() }
}

fun twice(x: Int) = x * 2

fun f(name: String, n: Int, xs: List<Int>) {
    println(String.format("%s has %s", name, n))
    println("%s!".format(name))
    println(String.format("%d", n))
    val m = hashMapOf<Color, Int>()
    println(xs.mapIndexed { i, _ -> i * 2 })
    println(name.let { println(it) })
    println(xs.map { twice(it) })
    println(m)
}
"#,
    );
    let inner: Vec<_> = found.iter().filter(|f| f.starts_with("RedundantInnerClassModifier")).collect();
    assert_eq!(inner.len(), 1, "only Plain: {found:#?}");
    assert_eq!(found.iter().filter(|f| f.starts_with("ReplaceStringFormatWithLiteral")).count(), 2, "{found:#?}");
    has(&found, "ReplaceWithEnumMap: hashMapOf");
    has(&found, "ReplaceMapIndexedWithListGenerator: mapIndexed");
    has(&found, "ComplexRedundantLet: let");
    has(&found, "ConvertLambdaToReference: { twice(it) }");
    let check = |body: &str, rule: &str, expected: &str| {
        let out = fixed(&format!("package a\n\nenum class Color {{ RED }}\nfun twice(x: Int) = x * 2\nfun f(name: String, n: Int, xs: List<Int>) {{\n    println({body})\n}}\n"), rule);
        assert!(out.contains(expected), "{rule}: {out}");
    };
    check("String.format(\"%s has %s\", name, n)", "ReplaceStringFormatWithLiteral", "println(\"$name has $n\")");
    check("hashMapOf<Color, Int>()", "ReplaceWithEnumMap", "println(EnumMap<Color, Int>(Color::class.java))");
    check("xs.map { twice(it) }", "ConvertLambdaToReference", "println(xs.map(::twice))");
    check("name.let { println(it) }", "ComplexRedundantLet", "println(println(name))");
}

#[test]
fn receivers_sam_and_use_inspections() {
    if !Fixture::new().typed() {
        return;
    }
    let found = lint(
        r#"package a

fun String.shout() = uppercase()
fun String.ignore() = 1
fun String.self() = this.length
fun Int.twice(other: Int) = other * 2

fun run(executor: java.util.concurrent.Executor, s: String) {
    executor.execute(Runnable { println() })
    val r = object : Runnable {
        override fun run() {
            println(s)
        }
    }
    with(s) { println(1) }
    with(s) { println(length) }
    val reader = java.io.StringReader(s)
    try {
        println(reader.read())
    } finally {
        reader.close()
    }
    println(r)
}
"#,
    );
    let receivers: Vec<_> = found.iter().filter(|f| f.starts_with("UnusedReceiverParameter")).collect();
    assert_eq!(receivers.len(), 2, "ignore, twice: {found:#?}");
    has(&found, "RedundantSamConstructor: Runnable");
    has(&found, "ObjectLiteralToLambda: object");
    assert_eq!(found.iter().filter(|f| f.starts_with("RedundantWith")).count(), 1, "{found:#?}");
    has(&found, "ConvertTryFinallyToUseCall: try");
    let out = fixed("package a\n\nfun f(e: java.util.concurrent.Executor) = e.execute(Runnable { println() })\n", "RedundantSamConstructor");
    assert!(out.contains("e.execute({ println() })"), "{out}");
    let out = fixed("package a\n\nval r = object : Runnable {\n    override fun run() = println()\n}\n", "ObjectLiteralToLambda");
    assert!(out.contains("val r = Runnable { println() }"), "{out}");
}

#[test]
fn explicit_api_mode_and_missing_docs() {
    let mut f = Fixture::with_module(|m| m.kotlin.explicit_api = true);
    let (lints, text) = f.run("src/main/kotlin/a/A.kt", "package a\n\nfun open() = 1\npublic fun typed(): Int = 1\ninternal fun hidden() = 2\nclass Box {\n    public val size = 1\n    private val secret = 2\n}\n");
    let mut found: Vec<String> = lints.iter().filter(|l| l.rule.id.contains("API_MODE")).map(|l| format!("{}: {}", l.rule.id, &text[usize::from(l.range.start())..usize::from(l.range.end())])).collect();
    found.sort();
    assert_eq!(found, vec![
        "NO_EXPLICIT_RETURN_TYPE_IN_API_MODE: open",
        "NO_EXPLICIT_RETURN_TYPE_IN_API_MODE: size",
        "NO_EXPLICIT_VISIBILITY_IN_API_MODE: Box",
        "NO_EXPLICIT_VISIBILITY_IN_API_MODE: open",
    ]);
    // Off by default; on when configured.
    assert!(!lint("package a\n\nclass Undocumented\n").iter().any(|l| l.starts_with("KDocMissingDocumentation")));
    let mut f = Fixture::new();
    let mut rules = HashMap::new();
    rules.insert("KDocMissingDocumentation".to_string(), Level::Info);
    f.host.set_lint_config(LintConfig { enabled: true, rules });
    let (lints, _) = f.run("src/main/kotlin/a/A.kt", "package a\n\n/** Documented. */\nclass Doc\nclass Undocumented\nprivate class Hidden\n");
    let ids: Vec<&str> = lints.iter().filter(|l| l.rule.id == "KDocMissingDocumentation").map(|l| l.message.as_str()).collect();
    assert_eq!(ids, vec!["Missing documentation for 'Undocumented'"]);
}

#[test]
fn generic_argument_mismatches() {
    if !Fixture::new().typed() {
        return;
    }
    let found = compiler(
        r#"package a

open class Animal
class Dog : Animal()

fun f(dogs: List<Dog>, animals: MutableList<Animal>, names: List<String?>, cmp: Comparable<Animal>) {
    val a: List<Animal> = dogs
    val b: List<String> = dogs
    val c: MutableList<Dog> = animals
    val d: MutableList<Animal> = animals
    val e: List<String> = names
    val g: Comparable<Dog> = cmp
    val h: List<Any?> = names
    println(listOf(a, b, c, d, e, g, h))
}
"#,
    );
    let hits: Vec<_> = found.iter().filter(|f| f.starts_with("INITIALIZER_TYPE_MISMATCH")).cloned().collect();
    assert_eq!(hits, vec!["INITIALIZER_TYPE_MISMATCH: dogs", "INITIALIZER_TYPE_MISMATCH: animals", "INITIALIZER_TYPE_MISMATCH: names"], "{found:#?}");
}

#[test]
fn compiler_quick_fixes() {
    let check = |text: &str, rule: &str, expected: &str| {
        let out = fixed(text, rule);
        assert!(out.contains(expected), "{rule}: {out}");
    };
    check("package a\n\nfun f() {\n    val x: Int\n    x = 1\n    x = 2\n    println(x)\n}\n", "VAL_REASSIGNMENT", "var x: Int");
    check("package a\n\nfun f(): Int {\n    return 1\n    println()\n}\n", "UNREACHABLE_CODE", "return 1\n}");
    check("package a\n\nfun f() {\n    var x: Int = 1\n    x = 2\n    println(x)\n}\n", "VARIABLE_WITH_REDUNDANT_INITIALIZER", "var x: Int\n");
    check("package a\n\nfun g()\n", "NON_MEMBER_FUNCTION_NO_BODY", "fun g() {\n    TODO(\"Not yet implemented\")\n}");
    check("package a\n\nabstract class A { abstract fun f() = 1 }\n", "ABSTRACT_FUNCTION_WITH_BODY", "{ fun f() = 1 }");
    check("package a\n\nopen class A { open tailrec fun f(n: Int): Int = if (n == 0) 0 else f(n - 1) }\n", "TAILREC_ON_VIRTUAL_MEMBER_ERROR", "open fun f(");
    check("package a\n\nabstract class A { abstract val x: Int = 1 }\n", "ABSTRACT_PROPERTY_WITH_INITIALIZER", "abstract val x: Int }");
    check("package a\n\ninterface I { val x: Int = 1 }\n", "PROPERTY_INITIALIZER_IN_INTERFACE", "val x: Int get() = 1");
    if !Fixture::new().typed() {
        return;
    }
    check("package a\n\nfun later(f: () -> Unit) = f()\nfun h() {\n    later { return }\n}\n", "RETURN_NOT_ALLOWED", "later { return@later }");
    check("package a\n\nfun g(n: Int?) = -n\n", "UNSAFE_OPERATOR_CALL", "-n!!");
    check("package a\n\nfun t(a: Int, b: Int) = a + b\nval v = t(1)\n", "NO_VALUE_FOR_PARAMETER", "t(1, b = TODO())");
    check("package a\n\nfun f(c: Boolean): Int {\n    val x = if (c) 1\n    return x\n}\n", "INVALID_IF_AS_EXPRESSION", "if (c) 1 else TODO()");
    check("package a\n\nclass Base\nclass Sub : Base()\n", "FINAL_SUPERTYPE", "open class Base");
    check("package a\n\nclass V { fun plus(o: V) = this }\nfun f(a: V, b: V) = a + b\n", "OPERATOR_MODIFIER_REQUIRED", "operator fun plus");
    check("package a\n\nclass C { private fun secret() = 1 }\nfun f(c: C) = c.secret()\n", "INVISIBLE_REFERENCE", "{ fun secret() = 1 }");
    check("package a\n\nval b = 2\nconst val C = b\n", "CONST_VAL_WITH_NON_CONST_INITIALIZER", "\nval C = b");
    check("package a\n\nenum class E { A }\nfun f(x: Any) = x is E.A\n", "IS_ENUM_ENTRY", "x == E.A");
    check("package a\n\ninternal fun helper() = 1\ninline fun api() = helper()\n", "NON_PUBLIC_CALL_FROM_PUBLIC_INLINE", "@PublishedApi\ninternal fun helper()");
}

#[test]
fn nullable_values_returned_as_non_null() {
    let text = r#"
import java.io.File
class User(val nick: String?) {
    fun bare(): String { return nick }
    fun checked(): String { if (nick != null) return nick; return "" }
}
fun maybeFile(): File? = null
fun javaClass(): File { return maybeFile() }
fun javaOk(): File { return File("x") }
fun javaNull(): File? { return maybeFile() }
fun letResult(u: User?): String { return u?.let { it.nick } }
fun letElvis(u: User?): String { return u?.let { it.nick } ?: "" }
fun <T> generic(x: T?): T { return x }
fun <T> genericChecked(x: T?): T { if (x != null) return x; throw Exception() }
class Box<T>(val v: T?, val w: T)
fun <R> genericProperty(b: Box<R>): R { return b.v }
fun <R> genericPropertyBody(b: Box<R>): R = b.v
fun <R> genericPropertyOk(b: Box<R>): R { return b.w }
fun <R> genericPropertyChecked(b: Box<R>): R { if (b.v != null) return b.v; throw Exception() }
fun boxed(b: Box<String>): String { return b.v }
fun elvisNull(u: User?): String { return u?.nick ?: null }
fun elvisOk(u: User?): String { return u?.nick ?: "" }
"#;
    let found: Vec<String> = compiler(text).into_iter().filter(|f| f.contains("TYPE_MISMATCH") || f.contains("NONNULL")).collect();
    for expected in ["RETURN_TYPE_MISMATCH: nick", "RETURN_TYPE_MISMATCH: maybeFile()", "RETURN_TYPE_MISMATCH: u?.let { it.nick }", "RETURN_TYPE_MISMATCH: x", "NULL_FOR_NONNULL_TYPE: null"] {
        has(&found, expected);
    }
    assert_eq!(found.iter().filter(|f| *f == "RETURN_TYPE_MISMATCH: b.v").count(), 3, "{found:#?}");
    assert_eq!(found.len(), 8, "{found:#?}");
}

#[test]
fn wrong_uses_of_declarations() {
    let text = r#"package a

interface I
class C : I?
object O { const val y by lazy { 1 } }
class Acc {
    val bad: Int get() = "s"
    val typed: Int get(): String = "s"
    var set: Int = 1
        set(v: String) {}
    var hidden = 1
        private set
    var t = ""
    fun h() = this::t.isInitialized
}
fun setters(a: Acc) { a.hidden = 2 }
fun spread(a: IntArray?) = listOf(*a)
fun big() { val i: Int = 3000000000 }
fun invoked(s: String, l: List<Int>) { s.length(); l.size() }
fun values(f: (Int) -> Int) { f("s"); f(1, 2); f() }
fun checks(x: String) { x is Int; when (x) { 1 -> {}; else -> {} }; x == 1 }
fun readOnly(l: List<Int>, m: Map<String, Int>) { l[0] = 1; m["a"] = 1 }
fun operators(a: Int, b: String, c: Boolean) { a + b; c + 1; -b; a > "x" }
enum class E { A }
fun entries(e: E) = when (e) { E.A -> 1; E.B -> 2 }
class Cycle { constructor(x: Int) : this(x, 1); constructor(x: Int, y: Int) : this(x) }
open class Base(x: Int)
class NoSuper : Base { constructor() }
"#;
    let found = compiler(text);
    for expected in [
        "NULLABLE_SUPERTYPE: I?",
        "CONST_VAL_WITH_DELEGATE: y",
        "RETURN_TYPE_MISMATCH: \"s\"",
        "WRONG_GETTER_RETURN_TYPE: String",
        "WRONG_SETTER_PARAMETER_TYPE: String",
        "INVISIBLE_SETTER: hidden",
        "LATEINIT_INTRINSIC_CALL_ON_NON_LATEINIT: isInitialized",
        "SPREAD_OF_NULLABLE: *a",
        "INITIALIZER_TYPE_MISMATCH: 3000000000",
        "FUNCTION_EXPECTED: length",
        "FUNCTION_EXPECTED: size",
        "ARGUMENT_TYPE_MISMATCH: \"s\"",
        "TOO_MANY_ARGUMENTS: 2",
        "NO_VALUE_FOR_PARAMETER: )",
        "INCOMPATIBLE_TYPES: Int",
        "INCOMPATIBLE_TYPES: 1",
        "EQUALITY_NOT_APPLICABLE: ==",
        "NO_SET_METHOD: l[0]",
        "NO_SET_METHOD: m[\"a\"]",
        "NONE_APPLICABLE: +",
        "UNRESOLVED_REFERENCE: +",
        "UNRESOLVED_REFERENCE: -",
        "NONE_APPLICABLE: >",
        "UNRESOLVED_REFERENCE: B",
        "CYCLIC_CONSTRUCTOR_DELEGATION_CALL: this(x, 1)",
        "CYCLIC_CONSTRUCTOR_DELEGATION_CALL: this(x)",
        "EXPLICIT_DELEGATION_CALL_REQUIRED: constructor",
    ] {
        has(&found, expected);
    }
    assert_eq!(fixed("fun f(s: String) = s.length()\n", "FUNCTION_EXPECTED"), "fun f(s: String) = s.length\n");

    // Valid uses of the same shapes.
    let ok = r#"package a

class Acc {
    val good: Int get() = 1
    val block: Int
        get() { return 1 }
    var hidden = 1
        private set
    lateinit var t: String
    fun bump() { hidden = 2 }
    fun h() = this::t.isInitialized
    val f: () -> Int = { 1 }
}
fun calls(a: Acc, s: String, g: (Int) -> String, h: (Int, String) -> Unit) { a.f(); s.length; g(1); h(1, "x"); h(1) { } }
fun spread(a: IntArray?) = if (a != null) listOf(*a) else null
fun big() { val l: Long = 3000000000; val i: Int = -2147483648 }
fun checks(x: String, y: Long, z: Any) { x is CharSequence; when (x) { "a" -> {}; else -> {} }; y == 1L; z == 1; when (y) { 1L -> {}; else -> {} } }
fun writable(l: MutableList<Int>, m: MutableMap<String, Int>, arr: IntArray) { l[0] = 1; m["a"] = 1; arr[0] = 1 }
fun operators(a: Int, b: String, c: Char, d: Double, l: Long) { "s" + a; b + null; a + 2L; d + 1; c + 1; c - 'a'; a < 2L; -a; -d; l * 2 }
enum class E { A }
fun entries(e: E) = when (e) { E.A -> E.entries.size + E.valueOf("A").ordinal + E.values().size }
object Obj { fun f() = 1 }
class Comp { companion object { const val K = 1 } }
fun Comp.Companion.ext() = 2
fun statics() = Obj.f() + Comp.K + Comp.ext()
class Chain(val a: Int) { constructor() : this(1); constructor(s: String) : this() }
open class Base(x: Int = 0)
class Implicit : Base { constructor() }
open class Needs(x: Int)
class Explicit : Needs { constructor() : super(1) }
"#;
    let found: Vec<String> = compiler(ok).into_iter().filter(|f| !f.starts_with("UNUSED_EXPRESSION") && !f.starts_with("USELESS_IS_CHECK")).collect();
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn values_operators_and_shapes() {
    let text = r#"package a

fun assigns(a: Box) { var x = 1; x = "s"; a.n = "s"; var i = 0; i += "s"; var s = ""; s -= 1; for (k in 1..2) { k = 3 } }
class Box { var n = 1 }
fun ranges() = 1.."a"
fun throws() { throw "error" }
fun catches() { try {} catch (e: String) {} }
fun branches(x: Int) = when (x) { is String -> 1; else -> 2 }
object O { fun f() = 1 }
fun invokes() { val x = 1; x(1); O().f() }
annotation class Ann(var a: Int, val b: List<Int>)
class Holder { val s: String? = null; fun f() = s.length }
const val C = listOf(1)
fun varargs(vararg x: Int) {}
fun callsVarargs() = varargs(1, "a")
fun expected() { val xs: List<Int> = listOf("a"); val ys: Set<String> = setOf(1) }
fun lambdas() { val f: (Int) -> Int = { a, b -> a }; val g: (Int) -> Int = { s: String -> 1 }; listOf(1).map { a, b -> a } }
class Task : Runnable
enum class E(val x: Int) { A, B("s") }
"#;
    let found = compiler(text);
    for expected in [
        "ASSIGNMENT_TYPE_MISMATCH: \"s\"",
        "NONE_APPLICABLE: +=",
        "UNRESOLVED_REFERENCE: -=",
        "VAL_REASSIGNMENT: k",
        "NONE_APPLICABLE: ..",
        "TYPE_MISMATCH: \"error\"",
        "TYPE_MISMATCH: String",
        "INCOMPATIBLE_TYPES: String",
        "FUNCTION_EXPECTED: x",
        "FUNCTION_EXPECTED: O",
        "VAR_ANNOTATION_PARAMETER: var",
        "INVALID_TYPE_OF_ANNOTATION_MEMBER: List<Int>",
        "UNSAFE_CALL: .",
        "TYPE_CANT_BE_USED_FOR_CONST_VAL: C",
        "ARGUMENT_TYPE_MISMATCH: \"a\"",
        "ARGUMENT_TYPE_MISMATCH: 1",
        "EXPECTED_PARAMETERS_NUMBER_MISMATCH: a, b",
        "EXPECTED_PARAMETER_TYPE_MISMATCH: String",
        "ABSTRACT_MEMBER_NOT_IMPLEMENTED: Task",
        "NO_VALUE_FOR_PARAMETER: A",
        "ARGUMENT_TYPE_MISMATCH: \"s\"",
    ] {
        has(&found, expected);
    }
    assert_eq!(found.iter().filter(|f| *f == "ASSIGNMENT_TYPE_MISMATCH: \"s\"").count(), 2, "{found:#?}");

    let ok = r#"package a

import java.io.Closeable

open class Base
class Derived : Base()
fun assigns() { var x = 1; x = 2; var b = Base(); b = Derived(); var s = ""; s += 1; var l = 1L; l += 1; var d = 1.0; d -= 1 }
fun ranges(c: Char) = (1..2L).count() + ('a'..c).count()
fun throws() { throw IllegalStateException("x") }
fun catches() { try {} catch (e: IllegalStateException) {} catch (@Suppress("TooGenericExceptionCaught") e: Exception) {} }
fun branches(x: Any) = when (x) { is String -> 1; is Int -> 2; else -> 3 }
class Calls(val value: Int) { fun value(s: String) = s; fun use() = value("a") }
interface Fn : (Int) -> Int
fun invokes(f: Fn, g: () -> Unit) { f(1); g() }
annotation class Ann(val a: Int, val b: Array<String>, val c: IntArray, vararg val d: String)
class Holder { val s: String? = null; fun f() = if (s != null) s.length else 0 }
const val C = 1 + 2
fun varargs(vararg x: Int) {}
fun callsVarargs(a: IntArray) { varargs(1, 2); varargs(*a) }
fun expected() { val xs: List<Number> = listOf(1, 2.0); val ys: List<Long> = listOf(1); val zs: List<String?> = listOf(null) }
fun lambdas() { val f: (Int) -> Int = { a -> a }; val g: (Int) -> Any = { a: Any -> a }; listOf(1).map { it }; mapOf(1 to 2).forEach { (k, v) -> k + v }; mapOf(1 to 2).forEach { k, v -> k + v } }
class Task : Runnable { override fun run() {} }
class Res : Closeable { override fun close() {} }
enum class E(val x: Int, val y: String = "") { A(1), B(2, "b") }
"#;
    let found: Vec<String> = compiler(ok).into_iter().filter(|f| !f.starts_with("UNUSED_") && !f.starts_with("ASSIGNED_BUT_NEVER")).collect();
    assert!(found.is_empty(), "{found:#?}");
}
