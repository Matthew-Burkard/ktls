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
