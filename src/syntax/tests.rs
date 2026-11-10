use super::*;

#[track_caller]
fn ok(text: &str) -> Parse {
    let parse = parse(text, false);
    assert_eq!(parse.syntax().text().to_string(), text, "tree is not lossless");
    if !parse.errors().is_empty() {
        panic!("unexpected errors: {:#?}\n{}", parse.errors(), parse.debug_tree());
    }
    parse
}

#[track_caller]
fn errs(text: &str) -> Vec<String> {
    let parse = parse(text, false);
    assert_eq!(parse.syntax().text().to_string(), text, "tree is not lossless");
    parse.errors().iter().map(|e| e.message.clone()).collect()
}

#[test]
fn package_and_imports() {
    let p = ok("package a.b.c\n\nimport x.y.Z\nimport x.y.*\nimport foo.bar as baz\n");
    let file = p.file();
    assert_eq!(file.package().unwrap().name().unwrap().text(), "a.b.c");
    let imports: Vec<_> = file.imports().collect();
    assert_eq!(imports.len(), 3);
    assert!(imports[1].is_star());
    assert_eq!(imports[2].alias().unwrap().text(), "baz");
}

#[test]
fn file_annotations() {
    let p = ok("@file:JvmName(\"Utils\")\n@file:Suppress(\"unused\")\npackage foo\n");
    assert_eq!(p.file().file_annotations().len(), 2);
}

#[test]
fn classes() {
    ok(r#"
/** Doc */
@Serializable
data class User(val name: String, var age: Int = 0, private val tags: List<String> = emptyList()) : Base(name), Comparable<User> {
    override fun compareTo(other: User): Int = age - other.age
    companion object {
        const val MAX = 10
        fun create(): User = User("a")
    }
    init {
        require(age >= 0)
    }
    constructor(name: String) : this(name, 0)
    inner class Nested<T : Any>(t: T)
}

sealed interface Shape
object Circle : Shape
enum class Color(val rgb: Int) { RED(0xFF0000), GREEN(0x00FF00) { override fun x() = 1 }, BLUE(0x0000FF); fun x() = 0 }
enum class Simple { A, B, C }
annotation class Ann(val value: String)
value class Id(val raw: Long)
fun interface Runnable2 { fun run() }
class Impl(private val d: Dep) : Dep by d
abstract class Generic<in T, out R, reified X> where T : Comparable<T>, R : Any
class Private private constructor(val x: Int)
class Injected @Inject constructor(val x: Int)
expect class Platform
actual typealias Platform2 = String
"#);
}

#[test]
fn functions() {
    let p = ok(r#"
fun main() {}
fun <T> List<T>.second(): T = this[1]
fun String?.orEmpty2(): String = this ?: ""
suspend inline fun <reified T : Any> foo(noinline a: () -> Unit, crossinline b: suspend (Int) -> String, vararg xs: Int): Map<String, List<T>> where T : Comparable<T> {
    return emptyMap()
}
private fun Outer.Inner.ext() = Unit
fun ((Int) -> Unit).invoke2() {}
fun f(block: String.(Int) -> Unit, x: Array<out Number>, y: Comparable<in Int>, z: List<*>): Unit = TODO()
fun nullableFn(f: (() -> Unit)?) {}
fun <T> definitelyNotNull(t: T & Any) {}
context(Logger) fun logged() {}
"#);
    let decls: Vec<_> = p.file().declarations().collect();
    assert_eq!(decls.len(), 10);
    let ast::Decl::Fun(second) = &decls[1] else { panic!() };
    assert_eq!(decls[1].name().unwrap(), "second");
    assert!(second.receiver().is_some());
    assert_eq!(decls[2].name().unwrap(), "orEmpty2");
    assert_eq!(decls[4].name().unwrap(), "ext");
}

#[test]
fn properties() {
    ok(r#"
val x = 1
var y: String? = null
    private set
val z: Int
    get() = 42
var w: Int = 0
    get() { return field }
    set(value) { field = value }
val lazyValue by lazy { compute() }
val <T> List<T>.lastIndex2: Int get() = size - 1
lateinit var late: String
const val C = "c"
"#);
}

#[test]
fn expressions() {
    ok(r#"
fun test() {
    val a = 1 + 2 * 3 - 4 / 5 % 6
    val b = a > 1 && a < 10 || !flag
    val c = x?.y?.z ?: return
    val d = list.map { it * 2 }.filter { it > 2 }.sum()
    val e = if (a > 1) "big" else "small"
    val f = when (a) {
        1, 2 -> "low"
        in 3..10 -> "mid"
        !in 11..<20 -> "x"
        is Int -> "int"
        else -> "high"
    }
    val g = when {
        a > 1 -> 1
        else -> { 2 }
    }
    val h = try { risky() } catch (e: IOException) { null } finally { cleanup() }
    val i = arr[0][1]
    val j = foo<Int>(1)
    val k = String::class
    val l = ::println
    val m = obj::method
    val m2 = Array<*>?::contentToString
    val m3 = Any?::toString
    val m4 = map.forEach { (k, v) -> println(k) }
    val n = a as String
    val o = a as? String
    val p = a is String && b !is Int
    val q = a!!.length
    val r = "hello $name, ${user.name.uppercase()}!"
    val s = """raw $x ${y}"""
    val t = object : Runnable { override fun run() {} }
    val u = fun(x: Int): Int { return x }
    val v = { a: Int, b: Int -> a + b }
    val w = 1 to 2
    val xx = a shl 2
    val (first, second) = pair
    val yy = listOf(1, 2, 3).fold(0) { acc, i -> acc + i }
    val zz = Foo.Bar.baz()
    x = 5
    x += 1
    i++
    --i
    for (item in items) println(item)
    for ((k2, v2) in map) { println(k2 + v2) }
    while (true) { break }
    do { x-- } while (x > 0)
    loop@ for (i in 0 until 10) { if (i == 5) continue@loop else break@loop }
    run outer@{ return@outer }
    this@Outer.foo()
    super<Base>.foo()
    throw IllegalStateException("x")
    val isLess = a < b
    val cmp = if (a < b) 1 else 2
    val gen = mutableMapOf<String, List<Int>>()
    callback { x -> x }
    with(sb) { append("x") }
    val e2 = when (val r = compute()) { is Ok -> r.value; else -> null }
    val nested = "a ${"b ${c}"} d"
    @Suppress("UNCHECKED_CAST") val cast = x as T
    val lambdaInParens = foo({ it }, 2)
    val chain = a
        .b()
        ?.c()
    val elvisNewline = a
        ?: b
    val andNewline = a
        && b
}
"#);
}

#[test]
fn when_guards() {
    ok("fun f(x: Any) = when (x) { is String if x.isNotEmpty() -> 1; else -> 0 }\n");
}

#[test]
fn generic_call_vs_comparison() {
    let p = ok("fun f() { val a = x < y; val b = foo<Bar>(); val c = if (a < b && c > d) 1 else 2 }\n");
    let tree = p.debug_tree();
    assert!(tree.contains("TYPE_ARGUMENT_LIST"), "{tree}");
    assert_eq!(tree.matches("TYPE_ARGUMENT_LIST").count(), 1, "{tree}");
}

#[test]
fn newline_terminates_statements() {
    let p = ok("fun f() {\n    val a = b\n    -c\n}\n");
    // `-c` must be a separate prefix statement, not `b - c`.
    assert!(p.debug_tree().contains("PREFIX_EXPR"));
}

#[test]
fn newlines_inside_parentheses() {
    // Inside (...) newlines are insignificant; inside lambdas they matter again.
    ok("fun f() {\n    g(\n        \"a\" + 1\n            + 2\n    )\n    val x = (a\n        + b)\n    h(1, { x\n        -y })\n}\n");
    let p = ok("fun f() {\n    h({ x\n        -y })\n}\n");
    assert!(p.debug_tree().contains("PREFIX_EXPR"), "the lambda body has two statements");
}

#[test]
fn package_semicolon() {
    let p = ok("package a.b;\n\nimport c.D\n");
    assert_eq!(p.file().imports().count(), 1);
}

#[test]
fn trailing_commas() {
    ok("fun f(\n    a: Int,\n    b: Int,\n) = g(\n    a,\n    b,\n)\nclass C<T,>(val x: Int,)\n");
}

#[test]
fn kdoc_attached_to_declaration() {
    let p = ok("/** The doc */\nfun documented() {}\n");
    let decl = p.file().declarations().next().unwrap();
    assert_eq!(decl.kdoc().unwrap(), "/** The doc */");
}

#[test]
fn script_mode() {
    let text = "plugins {\n    kotlin(\"jvm\") version \"1.9.0\"\n}\ndependencies { implementation(\"a:b:1\") }\nval x = 1\nprintln(x)\n";
    let p = parse(text, true);
    assert!(p.errors().is_empty(), "{:?}", p.errors());
    assert_eq!(p.syntax().text().to_string(), text);
}

#[test]
fn error_recovery() {
    let e = errs("fun f( { }\nfun g() {}\n");
    assert!(!e.is_empty());
    let p = parse("fun f( { }\nfun g() {}\n", false);
    let names: Vec<_> = p.file().declarations().filter_map(|d| d.name()).collect();
    assert!(names.contains(&"g".to_string()), "{names:?}\n{}", p.debug_tree());

    let e = errs("class A {\n    fun\n    val x = 1\n}\n");
    assert!(e.iter().any(|m| m.contains("function name")), "{e:?}");

    let e = errs("val x = 1 2\n");
    assert!(!e.is_empty());
    let e = errs("fun f() { val x = 1 2 }\n");
    assert!(e.iter().any(|m| m.contains("Unexpected tokens")), "{e:?}");

    let e = errs("val s = \"unterminated\nval y = 2\n");
    assert!(e.iter().any(|m| m.contains("'\"'")), "{e:?}");

    let e = errs("}\nfun ok() {}\n");
    assert!(!e.is_empty());
}

#[test]
fn never_panics_on_garbage() {
    let inputs = [
        "", "fun", "class", "val", "(", ")", "{{{{", "}}}}", "fun f(", "class A : ", "val x = ", "when {", "\"${",
        "@", "@file:", "import", "package", "fun <", "a<b<c", "if (", "try", "for (", "object : {", "typealias",
        "x as", "enum class E { A, B, ; fun", "fun f() = when (x) { 1 -> }", "lambda@", "fun f() { a.b.", "val (a, b",
        "fun f(): (Int) ->", "class C<in", "@Ann(", "fun f() { return@ }", "\"\"\"", "'", "val x = 'ab",
    ];
    for input in inputs {
        eprintln!("input: {input:?}");
        let p = parse(input, false);
        assert_eq!(p.syntax().text().to_string(), input);
        let p = parse(input, true);
        assert_eq!(p.syntax().text().to_string(), input);
    }
    // Every prefix of a realistic file.
    let text = include_str!("tests/sample.kt");
    for end in (0..text.len()).filter(|i| text.is_char_boundary(*i)) {
        eprintln!("prefix: {:?}", &text[end.saturating_sub(30)..end]);
        let p = parse(&text[..end], false);
        assert_eq!(p.syntax().text().len(), TextSize::from(end as u32));
    }
}

#[test]
fn sample_file_parses_cleanly() {
    ok(include_str!("tests/sample.kt"));
}


#[test]
fn unclosed_parens_report_one_error() {
    // Each mistake should produce exactly one error, and the `}` must not be swallowed.
    for (text, expected) in [
        ("fun main() {\n    val x = foo(\n}\n", vec!["Expecting ')'"]),
        ("fun main() {\n    foo(1, 2\n}\n", vec!["Expecting ')'"]),
        ("fun f( {\n}\n", vec!["Expecting ')'"]),
    ] {
        let p = parse(text, false);
        let msgs: Vec<&str> = p.errors().iter().map(|e| e.message.as_str()).collect();
        assert_eq!(msgs, expected, "{text:?}\n{}", p.debug_tree());
    }
}

/// Inputs nested `n` levels deep in each way the grammar recurses or chains.
fn deeply_nested(n: usize) -> Vec<(&'static str, String)> {
    vec![
        ("parens", format!("val x = {}1{}\n", "(".repeat(n), ")".repeat(n))),
        ("prefix", format!("val x = {}1\n", "-".repeat(n))),
        ("binary", format!("val x = 1{}\n", " + 1".repeat(n))),
        ("calls", format!("val x = a{}\n", ".b()".repeat(n))),
        ("lambdas", format!("val x = {}1{}\n", "run { ".repeat(n), " }".repeat(n))),
        ("ifs", format!("fun f() {{\n{}g()\n{}}}\n", "if (a) {\n".repeat(n), "}\n".repeat(n))),
        ("functions", format!("{}{}", "fun f() {\n".repeat(n), "}\n".repeat(n))),
        ("classes", format!("{}{}", "class A {\n".repeat(n), "}\n".repeat(n))),
        ("types", format!("val x: {}Int{} = y\n", "List<".repeat(n), ">".repeat(n))),
        ("function types", format!("val x: {}Unit = y\n", "() -> ".repeat(n))),
        ("templates", format!("val x = {}1{}\n", "\"${".repeat(n), "}\"".repeat(n))),
        ("unclosed", format!("fun f() {{\n{}", "(".repeat(n))),
    ]
}

fn tree_depth(node: &SyntaxNode) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    for event in node.preorder() {
        match event {
            rowan::WalkEvent::Enter(_) => {
                depth += 1;
                max = max.max(depth);
            }
            rowan::WalkEvent::Leave(_) => depth -= 1,
        }
    }
    max
}

#[test]
fn nesting_depth_is_bounded() {
    let limit = parser::MAX_DEPTH as usize;
    for (what, text) in deeply_nested(20_000) {
        let parse = parse(&text, false);
        assert_eq!(parse.syntax().text().to_string(), text, "{what}: tree is not lossless");
        let depth = tree_depth(&parse.syntax());
        // A few wrapper nodes (the node being started, its ERROR child) may sit on top.
        assert!(depth <= limit + 8, "{what}: tree is {depth} deep");
        assert!(parse.errors().iter().any(|e| e.message.contains("too deeply")), "{what}: {:?}", parse.errors().first());
        // Not one per skipped token (unclosed brackets do get one each, up to the limit).
        assert!(parse.errors().len() < limit + 100, "{what}: {} errors", parse.errors().len());
    }
}

#[test]
fn realistic_nesting_is_parsed_normally() {
    for (what, text) in deeply_nested(150) {
        if matches!(what, "unclosed" | "prefix") {
            continue;
        }
        let parse = parse(&text, false);
        assert!(parse.errors().is_empty(), "{what}: {:?}", parse.errors().first());
    }
}
