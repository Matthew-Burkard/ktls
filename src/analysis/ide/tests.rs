use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::analysis::{AnalysisHost, Workspace};
use crate::project::coords::Coordinate;
use crate::project::pom::Scope;
use crate::project::resolver::ResolvedArtifact;
use crate::project::{BuildSystem, ModelSource, Module, Project, SourceKind, SourceRoot};

fn stdlib() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".sdkman/candidates/kotlin/current/lib/kotlin-stdlib.jar")).filter(|p| p.is_file())
}

const MODEL: &str = r#"package app.model

/**
 * A user of the system.
 *
 * @param name the display name
 */
data class User(val name: String, val age: Int = 0) {
    fun greet(prefix: String): String = "$prefix $name"
    companion object {
        fun create(): User = User("default")
    }
}

interface Repository<T> {
    fun all(): List<T>
}

object Registry {
    fun register(name: String) {}
}

fun String.shout(): String = uppercase()
"#;

struct Fx {
    host: AnalysisHost,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

fn fixture() -> Option<Fx> {
    let stdlib = stdlib()?;
    let jdk = crate::project::jdk::find(None, None)?;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let module = Module {
        name: ":app".into(),
        dir: root.clone(),
        source_roots: vec![
            SourceRoot { path: root.join("src"), kind: SourceKind::Main, generated: false, source_set: "main".into() },
            SourceRoot { path: root.join("test"), kind: SourceKind::Test, generated: false, source_set: "test".into() },
        ],
        dependencies: vec![ResolvedArtifact {
            coord: Coordinate::new("org.jetbrains.kotlin", "kotlin-stdlib", "2.0.0"),
            scope: Scope::Compile,
            file: Some(stdlib),
            direct: true,
            depth: 0,
            via: None,
        }],
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
    host.set_workspace(Workspace::new(vec![project], Some(jdk)).with_cache(root.join("cache"), true));
    host.set_file(root.join("src/app/model/Model.kt"), Arc::from(MODEL), Some(1));
    Some(Fx { host, root, _tmp: tmp })
}

impl Fx {
    /// Open `text` (with a `$0` cursor marker) and return the definition target texts.
    fn goto(&mut self, text: &str) -> Vec<String> {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        definition(&snap, &path, offset, false)
            .into_iter()
            .map(|t| {
                let content = std::fs::read_to_string(&t.path).unwrap_or_else(|_| snap.file(&t.path).unwrap().text.to_string());
                let name = &content[usize::from(t.range.start())..usize::from(t.range.end())];
                format!("{}:{name}", t.path.file_name().unwrap().to_string_lossy())
            })
            .collect()
    }

    fn hover(&mut self, text: &str) -> String {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        super::hover(&snap, &path, offset).map(|h| h.markdown).unwrap_or_default()
    }

    fn open(&mut self, text: &str) -> (PathBuf, TextSize) {
        let offset = text.find("$0").expect("cursor marker");
        let clean = text.replacen("$0", "", 1);
        let path = self.root.join("src/app/Main.kt");
        self.host.set_file(path.clone(), Arc::from(clean), Some(1));
        (path, TextSize::from(offset as u32))
    }
}

const HEADER: &str = "package app\n\nimport app.model.User\nimport app.model.Repository\nimport app.model.Registry\nimport app.model.shout\n\n";

#[test]
fn locals_members_and_chains() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(repo: Repository<User>) {{\n{body}\n}}\n");
    assert_eq!(fx.goto(&src("    val count = 1\n    println(cou$0nt)")), vec!["Main.kt:count"]);
    assert_eq!(fx.goto(&src("    re$0po.all()")), vec!["Main.kt:repo"]);
    assert_eq!(fx.goto(&src("    repo.a$0ll()")), vec!["Model.kt:all"]);
    // Generic substitution through a library call chain: List<User>.first().name
    assert_eq!(fx.goto(&src("    repo.all().first().na$0me")), vec!["Model.kt:name"]);
    assert_eq!(fx.goto(&src("    repo.all().map { it.gr$0eet(\"hi\") }")), vec!["Model.kt:greet"]);
    assert_eq!(fx.goto(&src("    repo.all().forEach { u -> u.ag$0e }")), vec!["Model.kt:age"]);
    assert_eq!(fx.goto(&src("    for (u in repo.all()) { u.na$0me }")), vec!["Model.kt:name"]);
    assert_eq!(fx.goto(&src("    val (n, a) = User(\"x\")\n    n.leng$0th")).len(), 1, "String.length via component1");
    assert_eq!(fx.goto(&src("    val u = Us$0er(\"x\")")), vec!["Model.kt:User"]);
    assert_eq!(fx.goto(&src("    User.cr$0eate()")), vec!["Model.kt:create"]);
    assert_eq!(fx.goto(&src("    User.create().gre$0et(\"a\")")), vec!["Model.kt:greet"]);
    assert_eq!(fx.goto(&src("    Registry.regis$0ter(\"a\")")), vec!["Model.kt:register"]);
    assert_eq!(fx.goto(&src("    \"a\".sho$0ut()")), vec!["Model.kt:shout"]);
    assert_eq!(fx.goto(&src("    User(\"x\").apply { gre$0et(\"a\") }")), vec!["Model.kt:greet"]);
    // A call prefers an inherited function over a same-named property being declared;
    // a plain reference still means the property.
    let h = fx.hover(&format!("{HEADER}class Sub : Base() {{\n    val make = ma$0ke(1)\n}}\nopen class Base {{ fun make(x: Int) = x }}\n"));
    assert!(h.contains("fun make(x: Int)"), "{h}");
    let h = fx.hover(&format!("{HEADER}class Sub : Base() {{\n    val make = make(1)\n    fun f() = ma$0ke\n}}\nopen class Base {{ fun make(x: Int) = x }}\n"));
    assert!(h.contains("val make"), "{h}");
    assert_eq!(fx.hover(&format!("{HEADER}fun v(vararg xs: User) {{ x$0s.size }}\n")).lines().nth(1), Some("xs: Array<out User>"));
    assert_eq!(fx.goto(&format!("{HEADER}fun v(vararg xs: User) {{ xs.si$0ze }}\n")).len(), 1);
    assert_eq!(fx.goto(&src("    with(User(\"x\")) { na$0me }")), vec!["Model.kt:name"]);
    assert_eq!(fx.goto(&src("    val u: User? = null\n    u?.na$0me ?: \"\"")), vec!["Model.kt:name"]);
    assert_eq!(fx.goto(&src("    val u = User(\"x\")\n    println(\"$u$0\")")), vec!["Main.kt:u"]);
    assert_eq!(fx.goto(&src("    val m = mapOf(1 to User(\"x\"))\n    m[1]!!.na$0me")), vec!["Model.kt:name"]);
}

#[test]
fn types_imports_and_declarations() {
    let Some(mut fx) = fixture() else { return };
    assert_eq!(fx.goto(&format!("{HEADER}fun f(u: Us$0er) {{}}\n")), vec!["Model.kt:User"]);
    assert_eq!(fx.goto("package app\n\nimport app.model.Us$0er\n"), vec!["Model.kt:User"]);
    assert_eq!(fx.goto("package app\n\nimport app.model.sh$0out\n"), vec!["Model.kt:shout"]);
    assert_eq!(fx.goto(&format!("{HEADER}class Box<T>(val t: T$0)\n")), vec!["Main.kt:T"]);
    assert_eq!(fx.goto(&format!("{HEADER}class Me(val x: Int) {{\n    fun f() = this.$0x\n}}\n")), vec!["Main.kt:x"]);
    assert_eq!(fx.goto(&format!("{HEADER}class Me(val x: Int) {{\n    fun f() = $0x + 1\n}}\n")), vec!["Main.kt:x"]);
    assert_eq!(fx.goto(&format!("{HEADER}fun ru$0n() {{}}\n")), vec!["Main.kt:run"], "a declaration resolves to itself");
    assert!(fx.goto(&format!("{HEADER}fun f() {{ undefin$0ed() }}\n")).is_empty());
}

#[test]
fn library_targets() {
    let Some(mut fx) = fixture() else { return };
    // Real sources when a -sources.jar sits next to the jar (as with kotlinc's lib dir),
    // otherwise a generated stub; either way the name is selected.
    let t = fx.goto(&format!("{HEADER}fun f() {{ list$0Of(1, 2) }}\n"));
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(t[0].ends_with(":listOf"), "{t:?}");
    let t = fx.goto(&format!("{HEADER}fun f() {{ val s = \"x\"; s.len$0gth }}\n"));
    assert!(t.len() == 1 && t[0].ends_with(":length"), "{t:?}");
    let t = fx.goto(&format!("{HEADER}fun f() {{ java.util.ArrayList<String>().ad$0d(\"x\") }}\n"));
    assert!(t.len() == 1 && t[0].ends_with(":add"), "{t:?}");
}

#[test]
fn stubs_for_classes_without_sources() {
    let Some(fx) = fixture() else { return };
    let mut host = fx.host;
    let snap = host.snapshot();
    let path = fx.root.join("src/app/model/Model.kt");
    let e = infer::Engine::new(&snap, &path).unwrap();
    let c = e.res.classpath.find_class("java.util.concurrent.ConcurrentHashMap").unwrap();
    let put = c.members.iter().find(|m| m.name == "putIfAbsent").unwrap();
    let t = library::stub(&snap, &e.res.classpath, put).unwrap();
    let text = std::fs::read_to_string(&t.path).unwrap();
    assert!(text.contains("package java.util.concurrent"), "{text}");
    assert!(text.contains("class ConcurrentHashMap<K, V>"), "{text}");
    assert_eq!(&text[usize::from(t.range.start())..usize::from(t.range.end())], "putIfAbsent");
}

#[test]
fn hover_contents() {
    let Some(mut fx) = fixture() else { return };
    let h = fx.hover(&format!("{HEADER}fun f() {{ val u = Us$0er(\"x\") }}\n"));
    assert!(h.contains("data class User"), "{h}");
    assert!(h.contains("A user of the system."), "{h}");
    assert!(h.contains("**Parameters**\n- `name` — the display name"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f(r: Repository<User>) {{ val n$0 = r.all().size }}\n"));
    assert!(h.contains("val n: Int"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f(r: Repository<User>) {{ r.all().forEach {{ u$0 -> }} }}\n"));
    assert!(h.contains("u: User"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f(r: Repository<User>) {{ val n = r.all().size }}\n").replace("val n", "val n$0"));
    assert!(h.contains("val n: Int"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f(r: Repository<User>) {{ r.all().fir$0st() }}\n"));
    assert!(h.contains("fun <T> List<T>.first(): T"), "{h}");
    assert!(h.contains("`returns: User` here"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f(r: Repository<User>) {{ r.all().map {{ i$0t }} }}\n"));
    assert!(h.contains("it: User"), "{h}");
    let h = fx.hover(&format!("{HEADER}fun f() {{ printl$0n(\"x\") }}\n"));
    assert!(h.contains("fun println(message: Any?): Unit"), "{h}");
}

#[test]
fn doc_rendering() {
    let md = render_doc("/**\n * Returns the [first] element, see {@link java.util.List#get}.\n * <p>Second <code>para</code>.\n *\n * @param index the index\n *   continued\n * @return the element\n * @throws IndexOutOfBoundsException if bad\n */");
    assert!(md.contains("Returns the `first` element, see `java.util.List.get`."), "{md}");
    assert!(md.contains("Second `para`."), "{md}");
    assert!(md.contains("- `index` — the index continued"), "{md}");
    assert!(md.contains("**Returns** the element"), "{md}");
    assert!(md.contains("**Throws** `IndexOutOfBoundsException` if bad"), "{md}");
}



impl Fx {
    fn refs(&mut self, text: &str, include_declaration: bool) -> Vec<String> {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        let mut out: Vec<String> = references(&snap, &path, offset, include_declaration, &|| false)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let text = snap.file(&t.path).unwrap().text.clone();
                let line = text[..usize::from(t.range.start())].matches('\n').count() + 1;
                format!("{}:{line}:{}", t.path.file_name().unwrap().to_string_lossy(), &text[usize::from(t.range.start())..usize::from(t.range.end())])
            })
            .collect();
        out.sort();
        out
    }
}

#[test]
fn find_references() {
    let Some(mut fx) = fixture() else { return };
    let main = format!(
        "{HEADER}class Other {{ fun greet(x: String) = x }}\n\nfun run(repo: Repository<User>) {{\n    val u: User = User(\"a\")\n    u.greet(\"hi\")\n    repo.all().first().greet(\"x\")\n    Other().greet(\"no\")\n    val count = 1\n    println(count + count)\n}}\n"
    );
    // A class: import, type usages, constructor call, its own declaration.
    let refs = fx.refs(&main.replacen("val u: User", "val u: Us$0er", 1), true);
    assert_eq!(
        refs,
        vec!["Main.kt:10:User", "Main.kt:11:User", "Main.kt:11:User", "Main.kt:3:User", "Model.kt:11:User", "Model.kt:11:User", "Model.kt:8:User"],
        "{refs:?}"
    );
    // A member: calls through a typed local and a generic chain, but not Other.greet.
    let refs = fx.refs(&main.replacen("u.greet", "u.gre$0et", 1), true);
    assert_eq!(refs, vec!["Main.kt:12:greet", "Main.kt:13:greet", "Model.kt:9:greet"], "{refs:?}");
    let refs = fx.refs(&main.replacen("u.greet", "u.gre$0et", 1), false);
    assert_eq!(refs, vec!["Main.kt:12:greet", "Main.kt:13:greet"]);
    // A local.
    let refs = fx.refs(&main.replacen("val count", "val cou$0nt", 1), true);
    assert_eq!(refs, vec!["Main.kt:15:count", "Main.kt:16:count", "Main.kt:16:count"]);
    // From the declaration in another file.
    let refs = fx.refs(&main.replacen("Other().greet", "Other().gr$0eet", 1), true);
    assert_eq!(refs, vec!["Main.kt:14:greet", "Main.kt:8:greet"]);
}

#[test]
fn references_include_overrides() {
    let Some(mut fx) = fixture() else { return };
    // Lines 8.. after the 7-line HEADER.
    let main = format!(
        "{HEADER}{}",
        [
            "open class Animal { open fun speak() = \"\" }",   // 8
            "open class Dog : Animal() { override fun speak() = \"woof\" }", // 9
            "class Puppy : Dog() { override fun speak() = \"yip\" }", // 10
            "class Cat : Animal()", // 11
            "interface Named { fun title(): String }", // 12
            "class Thing : Named { override fun title() = \"t\"; override fun toString() = \"x\" }", // 13
            "class Unrelated { fun speak() = \"\" }", // 14
            "fun use(a: Animal, d: Dog, p: Puppy, c: Cat, u: Unrelated, n: Named, t: Thing) {", // 15
            "    a.speak()", // 16
            "    d.speak()", // 17
            "    p.speak()", // 18
            "    c.speak()", // 19
            "    u.speak()", // 20
            "    n.title()", // 21
            "    t.title()", // 22
            "    t.toString()", // 23
            "    a.toString()", // 24
            "}", // 25
            "",
        ]
        .join("\n")
    );
    let all_speak = vec![
        "Main.kt:10:speak", "Main.kt:16:speak", "Main.kt:17:speak", "Main.kt:18:speak", "Main.kt:19:speak", "Main.kt:8:speak", "Main.kt:9:speak",
    ];
    // From the base declaration, from a call on the base, and from an override: same family.
    assert_eq!(fx.refs(&main.replacen("open fun speak", "open fun sp$0eak", 1), true), all_speak);
    assert_eq!(fx.refs(&main.replacen("a.speak", "a.sp$0eak", 1), true), all_speak);
    assert_eq!(fx.refs(&main.replacen("Puppy : Dog() { override fun speak", "Puppy : Dog() { override fun sp$0eak", 1), true), all_speak);
    // The unrelated class's method has its own references.
    assert_eq!(fx.refs(&main.replacen("u.speak", "u.sp$0eak", 1), true), vec!["Main.kt:14:speak", "Main.kt:20:speak"]);
    // Interface members and their implementations.
    assert_eq!(
        fx.refs(&main.replacen("t.title", "t.ti$0tle", 1), true),
        vec!["Main.kt:12:title", "Main.kt:13:title", "Main.kt:21:title", "Main.kt:22:title"]
    );
    // Overriding a library member does not pull in every call of the library member.
    assert_eq!(fx.refs(&main.replacen("override fun toString", "override fun toStr$0ing", 1), true), vec!["Main.kt:13:toString", "Main.kt:23:toString"]);
    // Without declarations: only the calls.
    assert_eq!(fx.refs(&main.replacen("a.speak", "a.sp$0eak", 1), false), vec!["Main.kt:16:speak", "Main.kt:17:speak", "Main.kt:18:speak", "Main.kt:19:speak"]);
}

#[test]
fn references_through_labeled_this_and_type_parameters() {
    let Some(mut fx) = fixture() else { return };
    let refs = |fx: &mut Fx, body: &str| fx.refs(&format!("{HEADER}{body}"), false);
    // `this@Outer` from an inner class, past a lambda receiver, and next to a same-named
    // member of the inner class (which plain `foo()` means).
    let src = "class A {\n    private fun f$0oo() = 1\n    inner class I {\n        fun foo() = 2\n        fun a() = this@A.foo() + foo()\n        fun b() = with(\"s\") { this@A.foo() }\n    }\n}\n";
    assert_eq!(refs(&mut fx, src), vec!["Main.kt:12:foo", "Main.kt:13:foo"]);
    assert_eq!(refs(&mut fx, &src.replacen("f$0oo", "foo", 1).replacen("this@A.foo() +", "this@A.fo$0o() +", 1)), vec!["Main.kt:12:foo", "Main.kt:13:foo"]);
    // Lambda labels: explicit, and the name of the function the lambda is passed to.
    let src = "class A {\n    private fun f$0oo() = 1\n    fun a(o: A) = o.run lbl@{ \"s\".run { this@lbl.foo() } }\n    fun b(o: A) = with(o) { \"s\".run { this@with.foo() } }\n    fun A.c() = \"s\".run { this@c.foo() }\n}\n";
    assert_eq!(refs(&mut fx, src), vec!["Main.kt:10:foo", "Main.kt:11:foo", "Main.kt:12:foo"]);
    // A value whose type is a type parameter has the members of its bounds.
    let src = "class A<T : A<T>> {\n    private fun f$0oo() = this\n    fun a(t: T) = t.foo().foo()\n    fun <U : A<U>> b(u: U?) = u?.foo()\n    fun <V> c(v: V) where V : A<V> = v.foo()\n    fun <W : A<W>> W.d() = foo()\n}\n";
    assert_eq!(refs(&mut fx, src), vec!["Main.kt:10:foo", "Main.kt:10:foo", "Main.kt:11:foo", "Main.kt:12:foo", "Main.kt:13:foo"]);
    assert_eq!(fx.goto(&format!("{HEADER}fun <T : CharSequence> f(t: T) = t.len$0gth\n")).len(), 1);
}

#[test]
fn references_through_inferred_receivers_and_extensions() {
    let Some(mut fx) = fixture() else { return };
    let cls = "class A {\n    fun f$0oo(): A = this\n}\n";
    for (label, body) in [
        ("lazy property", "val l by lazy { A() }\nfun g() = l.foo()"),
        ("custom delegate", "class D { operator fun getValue(r: Any?, p: kotlin.reflect.KProperty<*>) = A() }\nval d by D()\nfun g() = d.foo()"),
        ("lambda in a val", "val f = { a: A -> a }\nfun g() = f(A()).foo()"),
        ("lambda in a local", "fun g() { val f = { a: A -> a }; f(A()).foo() }"),
        ("builder inference", "fun g() = buildList { add(A()) }.first().foo()"),
        ("builder map", "fun g() = buildMap { put(1, A()) }[1]?.foo()"),
        ("SAM constructor", "fun interface F { fun h(a: A) }\nval x = F { it.foo() }"),
        ("this smart cast", "fun Any.g() { if (this is A) foo() }"),
        ("this smart cast qualified", "fun Any.g() { if (this is A) this.foo() }"),
    ] {
        let refs = fx.refs(&format!("{HEADER}{cls}{body}\n"), false);
        assert!(refs.len() == 1 && refs[0].ends_with(":foo"), "{label}: {refs:?}");
    }
    // Extensions on a companion, called through the class, and callable references to extensions.
    let src = "class B { companion object }\nfun B.Companion.e$0x() = 1\nfun k() = B.ex() + B.Companion.ex()\nclass C\nfun C.cx() = 1\nval r = B.Companion::ex\nval s = C::cx\n";
    assert_eq!(fx.refs(&format!("{HEADER}{src}"), false), vec!["Main.kt:10:ex", "Main.kt:10:ex", "Main.kt:13:ex"]);
    assert_eq!(fx.refs(&format!("{HEADER}{}", src.replacen("e$0x", "ex", 1).replacen("fun C.cx", "fun C.c$0x", 1)), false), vec!["Main.kt:14:cx"]);
}

#[test]
fn references_to_constructors() {
    let Some(mut fx) = fixture() else { return };
    let src = format!(
        "{HEADER}{}",
        [
            "open class A private constructor(x: Int) {", // 8
            "    constructor() : this(1)", // 9
            "    constructor(s: String) : this()", // 10
            "    companion object { fun m() = A(2) }", // 11
            "}", // 12
            "class B : A {", // 13
            "    constructor() : super()", // 14
            "}", // 15
            "class C : app.A(\"s\")", // 16
            "fun f() = A(\"s\")", // 17
            "",
        ]
        .join("\n")
    );
    let primary = vec!["Main.kt:11:A", "Main.kt:9:this"];
    // From a delegation and from the `constructor` keyword. The calls pick their
    // constructor by argument types; supertype mentions without a call are not calls.
    assert_eq!(fx.refs(&src.replacen(": this(1)", ": th$0is(1)", 1), false), primary);
    assert_eq!(fx.refs(&src.replacen("private constructor", "private construct$0or", 1), false), primary);
    assert_eq!(fx.refs(&src.replacen("private constructor", "private construct$0or", 1), true), vec!["Main.kt:11:A", "Main.kt:8:", "Main.kt:9:this"]);
    // The no-argument secondary constructor: a `this()` and a subclass's `super()`.
    assert_eq!(fx.refs(&src.replacen("    constructor() : this(1)", "    construct$0or() : this(1)", 1), false), vec!["Main.kt:10:this", "Main.kt:14:super"]);
    // The `String` one: a qualified supertype call and a plain call.
    assert_eq!(fx.refs(&src.replacen("    constructor(s: String)", "    construct$0or(s: String)", 1), false), vec!["Main.kt:16:A", "Main.kt:17:A"]);
    // A call still means the class.
    assert!(fx.refs(&src.replacen("A(2)", "A$0(2)", 1), false).contains(&"Main.kt:13:A".to_string()));
}

#[test]
fn overrides_in_other_source_sets_and_anonymous_objects() {
    let Some(mut fx) = fixture() else { return };
    // An override in test sources: not visible from the base's (main) module.
    fx.host.set_file(
        fx.root.join("test/app/SpecTest.kt"),
        Arc::from("package app\n\nclass TestAnimal : Animal() {\n    override fun speak() = \"test\"\n}\nfun check(t: TestAnimal) = t.speak()\n"),
        Some(1),
    );
    let main = format!(
        "{HEADER}{}",
        [
            "open class Animal { open fun speak() = \"\" }", // 8
            "fun use(a: Animal) {",                             // 9
            "    val anon = object : Animal() {",               // 10
            "        val extra = 1",                            // 11
            "        override fun speak() = \"anon\"",        // 12
            "    }",                                            // 13
            "    anon.speak()",                                 // 14
            "    anon.extra",                                   // 15
            "    a.speak()",                                    // 16
            "}",                                                // 17
            "",
        ]
        .join("\n")
    );
    assert_eq!(
        fx.refs(&main.replacen("open fun speak", "open fun sp$0eak", 1), true),
        vec!["Main.kt:12:speak", "Main.kt:14:speak", "Main.kt:16:speak", "Main.kt:8:speak", "SpecTest.kt:4:speak", "SpecTest.kt:6:speak"]
    );
    assert_eq!(
        fx.refs(&main.replacen("open fun speak", "open fun sp$0eak", 1), false),
        vec!["Main.kt:14:speak", "Main.kt:16:speak", "SpecTest.kt:6:speak"]
    );
    // Members of anonymous objects resolve (definition, hover, references).
    assert_eq!(fx.goto(&main.replacen("anon.extra", "anon.ex$0tra", 1)), vec!["Main.kt:extra"]);
    assert!(fx.hover(&main.replacen("anon.extra", "anon.ex$0tra", 1)).contains("val extra"));
    assert_eq!(fx.refs(&main.replacen("val extra", "val ex$0tra", 1), true), vec!["Main.kt:11:extra", "Main.kt:15:extra"]);
}

#[test]
fn super_calls_and_member_extensions() {
    let Some(mut fx) = fixture() else { return };
    let main = format!(
        "{HEADER}{}",
        [
            "open class Base { open fun go() = 1 }",                                       // 8
            "class Sub : Base() { override fun go() = super.go() + 1 }",                    // 9
            "class Col<T>(val v: T) { fun read(): T = v }",                                // 10
            "open class Tbl { fun <N> Col<N>.auto(): Col<N> = this; fun int(n: String) = Col(1) }", // 11
            "val t = object : Tbl() { val id = int(\"id\").auto() }",                    // 12
            "fun use() = t.id.read()",                                                      // 13
            "",
        ]
        .join("\n")
    );
    // `super.go()` is a call of Base.go, part of the family.
    assert_eq!(fx.refs(&main.replacen("open fun go", "open fun g$0o", 1), true), vec!["Main.kt:8:go", "Main.kt:9:go", "Main.kt:9:go"]);
    // A member extension of the implicit `Tbl` receiver, then a chain through it.
    assert_eq!(fx.goto(&main.replacen(".auto()", ".au$0to()", 1)), vec!["Main.kt:auto"]);
    assert_eq!(fx.goto(&main.replacen("t.id.read", "t.id.re$0ad", 1)), vec!["Main.kt:read"]);
}

#[test]
fn operator_conventions() {
    let Some(mut fx) = fixture() else { return };
    let main = format!(
        "{HEADER}{}",
        [
            "class Money(val cents: Long) {",                                      // 8
            "    operator fun plus(o: Money) = Money(cents + o.cents)",            // 9
            "    operator fun plus(n: Int) = Money(cents + n)",                    // 10
            "    operator fun minus(o: Money) = this",                             // 11
            "    operator fun unaryMinus() = this",                                // 12
            "    operator fun get(i: Int): Long = cents",                          // 13
            "    operator fun set(i: Int, v: Long) {}",                            // 14
            "    operator fun contains(x: Long) = true",                           // 15
            "    operator fun compareTo(o: Money) = 0",                            // 16
            "    operator fun invoke() = cents",                                   // 17
            "    operator fun component1() = cents",                               // 18
            "    operator fun iterator(): Iterator<Long> = listOf(cents).iterator()", // 19
            "    operator fun inc() = this",                                       // 20
            "}",                                                                   // 21
            "class Lazyish { operator fun getValue(r: Any?, p: Any?): Int = 1 }",  // 22
            "fun use(a: Money, b: Money) {",                                       // 23
            "    var c = a + b",                                                   // 24
            "    c = a + 1",                                                       // 25
            "    c += b",                                                          // 26
            "    val d = a - b",                                                   // 27
            "    val e = -a",                                                      // 28
            "    val f = a[0]",                                                    // 29
            "    a[1] = 2L",                                                       // 30
            "    if (5L in a && 6L !in b) {}",                                     // 31
            "    if (a < b) {}",                                                   // 32
            "    a()",                                                             // 33
            "    val (x) = a",                                                     // 34
            "    for (v in a) {}",                                                 // 35
            "    c++",                                                             // 36
            "    val g by Lazyish()",                                              // 37
            "    val n = 1 + 2",                                                   // 38
            "    val u = User(\"x\")",                                             // 39
            "    val (nm, ag) = u",                                                // 40
            "}",
            "",
        ]
        .join("\n")
    );
    let at = |needle: &str, cursor: &str| main.replacen(needle, cursor, 1);
    // Overloads are picked by argument type; plain refs from `fun plus(o: Money)`.
    assert_eq!(fx.refs(&at("operator fun plus(o", "operator fun pl$0us(o"), true), vec!["Main.kt:24:+", "Main.kt:26:+=", "Main.kt:9:plus"]);
    assert_eq!(fx.refs(&at("operator fun plus(n", "operator fun pl$0us(n"), true), vec!["Main.kt:10:plus", "Main.kt:25:+"]);
    assert_eq!(fx.refs(&at("operator fun minus", "operator fun mi$0nus"), false), vec!["Main.kt:27:-"]);
    assert_eq!(fx.refs(&at("operator fun unaryMinus", "operator fun unaryMi$0nus"), false), vec!["Main.kt:28:-"]);
    assert_eq!(fx.refs(&at("operator fun get", "operator fun g$0et"), false), vec!["Main.kt:29:["]);
    assert_eq!(fx.refs(&at("operator fun set", "operator fun s$0et"), false), vec!["Main.kt:30:["]);
    assert_eq!(fx.refs(&at("operator fun contains", "operator fun con$0tains"), false), vec!["Main.kt:31:!in", "Main.kt:31:in"]);
    assert_eq!(fx.refs(&at("operator fun compareTo", "operator fun compa$0reTo"), false), vec!["Main.kt:32:<"]);
    assert_eq!(fx.refs(&at("operator fun invoke", "operator fun inv$0oke"), false), vec!["Main.kt:33:("]);
    assert_eq!(fx.refs(&at("operator fun component1", "operator fun compo$0nent1"), false), vec!["Main.kt:34:x"]);
    assert_eq!(fx.refs(&at("operator fun iterator", "operator fun iter$0ator"), false), vec!["Main.kt:35:in"]);
    assert_eq!(fx.refs(&at("operator fun inc", "operator fun in$0c"), false), vec!["Main.kt:36:++"]);
    assert_eq!(fx.refs(&at("operator fun getValue", "operator fun getVal$0ue"), false), vec!["Main.kt:37:by"]);
    // Data class properties are used by destructuring (synthesized componentN).
    assert_eq!(fx.refs(&at("val (nm, ag) = u", "val (nm, ag) = u\n    u.na$0me"), false), vec!["Main.kt:40:nm", "Main.kt:41:name", "Model.kt:9:name"]);
    // Go to definition and hover on operator tokens.
    assert_eq!(fx.goto(&at("var c = a + b", "var c = a $0+ b")), vec!["Main.kt:plus"]);
    assert_eq!(fx.goto(&at("val f = a[0]", "val f = a$0[0]")), vec!["Main.kt:get"]);
    assert!(fx.hover(&at("if (a < b)", "if (a $0< b)")).contains("operator fun compareTo(o: Money): Int"));
    // Built-in operators resolve to the library declaration.
    let h = fx.hover(&at("val n = 1 + 2", "val n = 1 $0+ 2"));
    assert!(h.contains("operator fun plus(other: Int): Int"), "{h}");
}


impl Fx {
    /// Rename at the `$0` marker of `text` (opened as Main.kt); the changed files' new texts.
    fn rename(&mut self, text: &str, new_name: &str) -> Result<(Vec<(String, String)>, Option<(String, String)>), String> {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        let r = rename::rename(&snap, &path, offset, new_name, &|| false)?.unwrap();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut files: Vec<&PathBuf> = r.edits.iter().map(|(p, ..)| p).collect();
        files.dedup();
        for f in files {
            let mut t = snap.file(f).unwrap().text.to_string();
            for (_, range, new) in r.edits.iter().filter(|(p, ..)| p == f).rev() {
                t.replace_range(usize::from(range.start())..usize::from(range.end()), new);
            }
            out.push((f.file_name().unwrap().to_string_lossy().into_owned(), t));
        }
        let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
        Ok((out, r.file_rename.map(|(a, b)| (name(&a), name(&b)))))
    }
}

fn file<'a>(files: &'a [(String, String)], name: &str) -> &'a str {
    &files.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("{name} not changed: {files:#?}")).1
}

#[test]
fn rename_class_and_members() {
    let Some(mut fx) = fixture() else { return };
    let main = format!("{HEADER}/** See [User], [app.model.User] and [User.greet]; not [UserX] or [Users]. */\nfun run(u: Use$0r): User {{\n    val (n, a) = u\n    return User(name = n + a)\n}}\n");
    let (files, moved) = fx.rename(&main, "Person").unwrap();
    assert_eq!(moved, None, "Model.kt is not named after the class");
    let m = file(&files, "Main.kt");
    assert!(m.contains("import app.model.Person\n") && m.contains("fun run(u: Person): Person {") && m.contains("return Person(name = n + a)"), "{m}");
    assert!(m.contains("/** See [Person], [app.model.Person] and [Person.greet]; not [UserX] or [Users]. */"), "{m}");
    let model = file(&files, "Model.kt");
    assert!(model.contains("data class Person(") && model.contains("fun create(): Person = Person(\"default\")"), "{model}");

    // A constructor property: declaration, member uses, string templates, KDoc and
    // named arguments; destructuring entries are other variables.
    let main = format!("{HEADER}fun run(u: User): String {{\n    val (n, _) = u\n    println(n)\n    return User(na$0me = \"x\").name + u.name\n}}\n");
    let (files, _) = fx.rename(&main, "displayName").unwrap();
    let m = file(&files, "Main.kt");
    assert!(m.contains("User(displayName = \"x\").displayName + u.displayName") && m.contains("val (n, _) = u"), "{m}");
    let model = file(&files, "Model.kt");
    assert!(model.contains("@param displayName the display name") && model.contains("(val displayName: String,") && model.contains("\"$prefix $displayName\""), "{model}");

    // A function parameter with named arguments at call sites.
    let main = format!("{HEADER}fun run() {{\n    Registry.register(name = \"a\")\n    Registry.reg$0ister(\"b\")\n}}\n");
    let (files, _) = fx.rename(&main, "add").unwrap();
    assert!(file(&files, "Main.kt").contains("Registry.add(name = \"a\")\n    Registry.add(\"b\")"));
    let main = format!("{HEADER}fun greet(who$0: String, times: Int = who.length) = who.repeat(times)\nfun run() = greet(who = \"a\")\n");
    let (files, _) = fx.rename(&main, "person").unwrap();
    assert_eq!(file(&files, "Main.kt"), format!("{HEADER}fun greet(person: String, times: Int = person.length) = person.repeat(times)\nfun run() = greet(person = \"a\")\n"));

    // Locals.
    let main = format!("{HEADER}fun run() {{\n    val cou$0nt = 1\n    println(\"$count ${{count + 1}}\")\n}}\n");
    let (files, _) = fx.rename(&main, "total").unwrap();
    assert!(file(&files, "Main.kt").contains("val total = 1\n    println(\"$total ${total + 1}\")"));
}

#[test]
fn rename_override_family_and_files() {
    let Some(mut fx) = fixture() else { return };
    let main = format!("{HEADER}class Repo : Repository<User> {{\n    override fun a$0ll(): List<User> = emptyList()\n}}\nfun run(r: Repository<User>) = r.all()\n");
    let (files, _) = fx.rename(&main, "everything").unwrap();
    assert!(file(&files, "Model.kt").contains("fun everything(): List<T>"));
    let m = file(&files, "Main.kt");
    assert!(m.contains("override fun everything()") && m.contains("r.everything()"), "{m}");

    // A top-level class in a file named after it moves along.
    fx.host.set_file(fx.root.join("src/app/Account.kt"), Arc::from("package app\n\nclass Account\n"), Some(1));
    let (files, moved) = fx.rename(&format!("{HEADER}fun run(a: Acc$0ount) = a\n"), "Customer").unwrap();
    assert_eq!(moved, Some(("Account.kt".to_string(), "Customer.kt".to_string())));
    assert_eq!(file(&files, "Account.kt"), "package app\n\nclass Customer\n");

    // Type parameters.
    let (files, _) = fx.rename(&format!("{HEADER}fun <T$0 : Any> id(x: T): List<T> = listOf(x)\n"), "E").unwrap();
    assert!(file(&files, "Main.kt").contains("fun <E : Any> id(x: E): List<E> = listOf(x)"));

    // Names that need backticks get them.
    let (files, _) = fx.rename(&format!("{HEADER}fun run() {{\n    val x$0 = 1\n    println(x)\n}}\n"), "object").unwrap();
    assert!(file(&files, "Main.kt").contains("val `object` = 1\n    println(`object`)"));
}

#[test]
fn rename_refusals() {
    let Some(mut fx) = fixture() else { return };
    let err = |fx: &mut Fx, text: &str, name: &str| fx.rename(text, name).unwrap_err();
    assert!(err(&mut fx, &format!("{HEADER}fun run() = print$0ln(1)\n"), "out").contains("library"));
    assert!(err(&mut fx, &format!("{HEADER}class A {{ override fun toStr$0ing() = \"\" }}\n"), "x").contains("overrides a library"));
    assert!(err(&mut fx, &format!("{HEADER}fun run() = listOf(1).map {{ i$0t + 1 }}\n"), "x").contains("implicit"));
    assert!(err(&mut fx, &format!("{HEADER}fun run() {{\n    val x$0 = 1\n}}\n"), "a.b").contains("not a valid"));
    let (path, offset) = fx.open(&format!("{HEADER}fun run(u: User) = u.na$0me\n"));
    let snap = fx.host.snapshot();
    let (range, name) = rename::prepare_rename(&snap, &path, offset).unwrap();
    assert_eq!(name, "name");
    assert_eq!(&snap.file(&path).unwrap().text[usize::from(range.start())..usize::from(range.end())], "name");
}

impl Fx {
    fn complete(&mut self, text: &str) -> completion::Completion {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        completion::complete(&snap, &path, offset, completion::Options { snippets: true }).expect("no completion")
    }

    fn labels(&mut self, text: &str) -> Vec<String> {
        self.complete(text).items.into_iter().map(|i| i.label).collect()
    }
}

#[track_caller]
fn has_all(labels: &[String], expected: &[&str]) {
    for e in expected {
        assert!(labels.iter().any(|l| l == e), "missing {e:?} in {labels:?}");
    }
}

#[test]
fn completes_members_and_extensions() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(u: User, xs: List<User>) {{\n{body}\n}}\n");
    let labels = fx.labels(&src("    u.$0"));
    has_all(&labels, &["name", "age", "greet", "toString", "hashCode"]);
    assert!(!labels.iter().any(|l| l == "shout"), "String extension offered on User");
    // Prefix filtering, stdlib extensions with substituted types.
    let c = fx.complete(&src("    xs.fi$0"));
    let first = c.items.iter().find(|i| i.label == "first" && i.label_detail.as_deref() == Some("() for List<User>")).expect("first()");
    assert_eq!(first.description.as_deref(), Some("User"));
    assert!(c.items.iter().any(|i| i.label == "filter"));
    assert!(c.items.iter().all(|i| completion::matches("fi", &i.label)), "{:?}", c.items.iter().map(|i| &i.label).collect::<Vec<_>>());
    // Members sort before extensions; calls get parentheses.
    let c = fx.complete(&src("    u.gr$0"));
    assert_eq!(c.items[0].label, "greet");
    assert_eq!(c.items[0].insert.as_deref(), Some("greet($0)"));
    assert!(c.items[0].snippet);
    let labels = fx.labels(&src("    \"x\".sho$0"));
    has_all(&labels, &["shout"]);
    // Companion and object members through the class name.
    has_all(&fx.labels(&src("    User.$0")), &["create"]);
    has_all(&fx.labels(&src("    Registry.$0")), &["register"]);
    // Java getters as properties.
    has_all(&fx.labels(&src("    java.io.File(\"a\").$0")), &["name", "getName", "absolutePath"]);
}

#[test]
fn completes_scope_types_and_imports() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(param: Int) {{\n    val count = 1\n{body}\n}}\n");
    let c = fx.complete(&src("    co$0"));
    assert_eq!(c.items[0].label, "count");
    let labels = fx.labels(&src("    $0"));
    has_all(&labels, &["count", "param", "run", "listOf", "println", "User", "val", "return"]);
    has_all(&fx.labels(&src("    listOf(1).map { i$0 }")), &["it"]);
    // Inside a class: members of `this`.
    has_all(&fx.labels(&format!("{HEADER}class Box(val size: Int) {{\n    fun f() = si$0\n}}\n")), &["size"]);

    // Types.
    let labels = fx.labels(&format!("{HEADER}fun <T> f(x: Us$0) {{}}\n"));
    has_all(&labels, &["User"]);
    has_all(&fx.labels(&format!("{HEADER}fun <Elem> f(x: El$0) {{}}\n")), &["Elem"]);
    has_all(&fx.labels(&format!("{HEADER}fun f(x: Map.$0) {{}}\n")), &["Entry"]);

    // A class that needs an import.
    let c = fx.complete(&format!("{HEADER}fun f(x: UUI$0) {{}}\n"));
    let uuid = c.items.iter().find(|i| i.label == "UUID").expect("UUID");
    assert_eq!(uuid.description.as_deref(), Some("java.util"));
    assert_eq!(uuid.additional.len(), 1);
    assert_eq!(uuid.additional[0].1, "\nimport java.util.UUID");

    // Named arguments.
    let c = fx.complete(&format!("{HEADER}fun f() = User(na$0"));
    assert_eq!(c.items[0].label, "name =");
    assert_eq!(c.items[0].insert.as_deref(), Some("name = "));

    // Import paths.
    has_all(&fx.labels("package app\n\nimport app.mo$0\n"), &["model"]);
    has_all(&fx.labels("package app\n\nimport app.model.$0\n"), &["User", "Repository", "shout"]);
    has_all(&fx.labels("package app\n\nimport kotlin.coll$0\n"), &["collections"]);
}

#[test]
fn completes_overrides_and_keywords() {
    let Some(mut fx) = fixture() else { return };
    let c = fx.complete(&format!("{HEADER}class Repo : Repository<User> {{\n    $0\n}}\n"));
    let all = c.items.iter().find(|i| i.label == "override fun all(): List<User>").expect("override item");
    assert_eq!(all.insert.as_deref(), Some("override fun all(): List<User> {\n        TODO(\"Not yet implemented\")\n    }"));
    has_all(&c.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>(), &["fun", "val", "override fun toString(): String"]);
    // `override` already typed: the item replaces it.
    let c = fx.complete(&format!("{HEADER}class Repo : Repository<User> {{\n    override a$0\n}}\n"));
    let all = c.items.iter().find(|i| i.label.starts_with("override fun all")).expect("override item");
    let range = all.range.unwrap();
    let text = format!("{HEADER}class Repo : Repository<User> {{\n    override a\n}}\n");
    assert_eq!(&text[usize::from(range.start())..usize::from(range.end())], "override a");
    // Already implemented members are not offered again.
    let labels = fx.labels(&format!("{HEADER}class Repo : Repository<User> {{\n    override fun all() = emptyList<User>()\n    $0\n}}\n"));
    assert!(!labels.iter().any(|l| l.starts_with("override fun all")), "{labels:?}");
    // Nothing inside strings and declaration names.
    let (path, offset) = fx.open(&format!("{HEADER}val s = \"ab$0c\"\n"));
    assert!(completion::complete(&fx.host.snapshot(), &path, offset, completion::Options::default()).is_none());
}

#[test]
fn completion_matching() {
    use completion::matches;
    for (p, n) in [("", "x"), ("fi", "first"), ("fBN", "findByName"), ("fbn", "findByName"), ("Jo", "Join"), ("aL", "arrayListOf"), ("str", "String"), ("ut8", "utf8")] {
        assert!(matches(p, n), "{p} should match {n}");
    }
    for (p, n) in [("Jo", "JvmOverloads"), ("Jo", "JsonColumnMarker"), ("x", "first"), ("firstx", "first"), ("ft", "first")] {
        assert!(!matches(p, n), "{p} should not match {n}");
    }
}

impl Fx {
    /// Signature labels (active one marked `*`) and the active parameter's text.
    fn signatures(&mut self, text: &str) -> (Vec<String>, Option<String>) {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        let Some(h) = signature::signature_help(&snap, &path, offset) else { return (Vec::new(), None) };
        let active = &h.signatures[h.active as usize];
        let param = active.active_param.map(|i| {
            let r = active.params[i as usize].0;
            active.label[usize::from(r.start())..usize::from(r.end())].to_string()
        });
        let labels = h.signatures.iter().enumerate().map(|(i, s)| format!("{}{}", if i == h.active as usize { "*" } else { "" }, s.label)).collect();
        (labels, param)
    }
}

#[test]
fn signature_help() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(u: User, xs: List<String>) {{\n{body}\n}}\n/** Adds.\n * @param a first\n * @param b second */\nfun add(a: Int, b: Int = 0): Int = a + b\nfun add(a: String): String = a\n");
    let (labels, param) = fx.signatures(&src("    u.greet($0)"));
    assert_eq!(labels, vec!["*greet(prefix: String): String"]);
    assert_eq!(param.as_deref(), Some("prefix: String"));
    // Overloads: the one fitting two arguments is active; second parameter.
    let (labels, param) = fx.signatures(&src("    add(1, $0)"));
    assert_eq!(labels[0], "*add(a: Int, b: Int = …): Int", "{labels:?}");
    assert_eq!(labels.len(), 2);
    assert_eq!(param.as_deref(), Some("b: Int = …"));
    // Unclosed argument list, nested call: the innermost list around the cursor.
    let (labels, param) = fx.signatures(&src("    add(add(1), $0\n"));
    assert!(labels[0].starts_with("*add(a: Int"), "{labels:?}");
    assert_eq!(param.as_deref(), Some("b: Int = …"));
    let (labels, _) = fx.signatures(&src("    add(add(\"x$0\"), 1)"));
    assert!(labels.iter().any(|l| l == "*add(a: String): String"), "{labels:?}");
    // Named arguments map by name.
    let (_, param) = fx.signatures(&src("    add(b = 2, a = $0)"));
    assert_eq!(param.as_deref(), Some("a: Int"));
    // Constructors and generic substitution of library members.
    let (labels, param) = fx.signatures(&src("    User(\"a\", $0)"));
    assert_eq!(labels, vec!["*User(name: String, age: Int = …)"]);
    assert_eq!(param.as_deref(), Some("age: Int = …"));
    let (labels, _) = fx.signatures(&src("    xs.indexOf($0)"));
    assert!(labels.iter().any(|l| l.starts_with("indexOf(element: String)") || l.starts_with("*indexOf(element: String)")), "{labels:?}");
    // Values of function type.
    let (labels, _) = fx.signatures(&format!("{HEADER}fun run(cb: (Int, String) -> Unit) {{\n    cb(1, $0)\n}}\n"));
    assert_eq!(labels, vec!["*cb(p1: Int, p2: String): Unit"]);
    // Supertype constructor call.
    let (labels, _) = fx.signatures(&format!("{HEADER}open class Base(val x: Int)\nclass Sub : Base($0)\n"));
    assert_eq!(labels, vec!["*Base(x: Int)"]);
    let (labels, _) = fx.signatures(&format!("{HEADER}annotation class Ann(val name: String)\n@Ann($0)\nclass C\n"));
    assert_eq!(labels, vec!["*Ann(name: String)"]);
    let (labels, _) = fx.signatures(&format!("{HEADER}class C(val x: Int) {{\n    constructor(s: String) : this($0)\n}}\n"));
    assert_eq!(labels, vec!["*C(x: Int)"]);
    // Parameter docs and summary come from KDoc.
    let (path, offset) = fx.open(&src("    add(1$0)"));
    let h = signature::signature_help(&fx.host.snapshot(), &path, offset).unwrap();
    let s = &h.signatures[h.active as usize];
    assert_eq!(s.doc.as_deref(), Some("Adds."));
    assert_eq!(s.params[0].1.as_deref(), Some("first"));
    // Outside of any argument list.
    assert!(fx.signatures(&src("    add(1, 2)$0")).0.is_empty());
}

impl Fx {
    /// FQ names offered to import for the unresolved name at the cursor.
    fn import_candidates(&mut self, text: &str) -> Vec<String> {
        let (path, offset) = self.open(text);
        imports::import_fixes(&self.host.snapshot(), &path, TextRange::empty(offset)).into_iter().map(|f| f.fq).collect()
    }
}

#[test]
fn import_quick_fixes() {
    let Some(mut fx) = fixture() else { return };
    let src = |imports: &str, body: &str| format!("package app\n\n{imports}fun run() {{\n{body}\n}}\n");
    // Project class, used as a type and as a constructor call.
    assert_eq!(fx.import_candidates(&src("", "    val u: Us$0er? = null")), vec!["app.model.User"]);
    assert_eq!(fx.import_candidates(&src("", "    Us$0er(\"a\")")), vec!["app.model.User"]);
    // Library classes; project top-level functions.
    let c = fx.import_candidates(&src("", "    val f = Fi$0le(\"x\")"));
    assert!(c.contains(&"java.io.File".to_string()), "{c:?}");
    // Extensions: only those applicable to the receiver.
    assert_eq!(fx.import_candidates(&src("", "    \"a\".sho$0ut()")), vec!["app.model.shout"]);
    assert!(fx.import_candidates(&src("", "    1.sho$0ut()")).is_empty());
    // Library top-level functions outside the default imports.
    let c = fx.import_candidates(&src("", "    thr$0ead { }"));
    assert!(c.contains(&"kotlin.concurrent.thread".to_string()), "{c:?}");
    // Resolved names get nothing.
    assert!(fx.import_candidates(&src("import app.model.User\n\n", "    Us$0er(\"a\")")).is_empty());
    assert!(fx.import_candidates(&src("", "    print$0ln()")).is_empty());
    // The edit inserts a sorted import.
    let (path, offset) = fx.open(&src("import app.model.shout\nimport java.io.File\n\n", "    Us$0er(\"a\")"));
    let f = imports::import_fixes(&fx.host.snapshot(), &path, TextRange::empty(offset));
    assert_eq!(f[0].edit.1, "import app.model.User\n");
}

#[test]
fn organize_imports() {
    let Some(mut fx) = fixture() else { return };
    let text = "package app\n\nimport kotlin.math.max\nimport java.io.File\nimport app.model.User\nimport app.model.Registry\nimport javax.swing.JPanel as P\nimport app.model.User\n\nfun run(f: File, p: P) = User(\"a\").also { max(1, 2) }\n";
    let path = fx.root.join("src/app/Main.kt");
    fx.host.set_file(path.clone(), Arc::from(text), Some(1));
    let (range, new) = imports::organize_imports(&fx.host.snapshot(), &path).unwrap();
    assert_eq!(&text[usize::from(range.start())..usize::from(range.end())], "import kotlin.math.max\nimport java.io.File\nimport app.model.User\nimport app.model.Registry\nimport javax.swing.JPanel as P\nimport app.model.User");
    assert_eq!(new, "import app.model.User\nimport java.io.File\nimport kotlin.math.max\nimport javax.swing.JPanel as P");
    // Already organized: nothing to do.
    let clean = "package app\n\nimport app.model.User\nimport java.io.File\n\nfun run(f: File) = User(\"a\")\n";
    fx.host.set_file(path.clone(), Arc::from(clean), Some(2));
    assert!(imports::organize_imports(&fx.host.snapshot(), &path).is_none());
    // `.editorconfig` layouts: plain lexicographic, and groups separated by blank lines.
    let text = "package app\n\nimport kotlin.math.max\nimport java.io.File;\nimport app.model.User\n\nfun run(f: File) = User(\"a\").also { max(1, 2) }\n";
    fx.host.set_file(path.clone(), Arc::from(text), Some(3));
    std::fs::write(fx.root.join(".editorconfig"), "root = true\n[*.{kt,kts}]\nij_kotlin_imports_layout = *\n").unwrap();
    let (_, new) = imports::organize_imports(&fx.host.snapshot(), &path).unwrap();
    assert_eq!(new, "import app.model.User\nimport java.io.File\nimport kotlin.math.max");
    std::fs::write(fx.root.join(".editorconfig"), "root = true\n[*.{kt,kts}]\nij_kotlin_imports_layout = kotlin.**,|,*\n").unwrap();
    let (_, new) = imports::organize_imports(&fx.host.snapshot(), &path).unwrap();
    assert_eq!(new, "import kotlin.math.max\n\nimport app.model.User\nimport java.io.File");
}

#[test]
fn document_highlights() {
    let Some(mut fx) = fixture() else { return };
    let text = format!("{HEADER}class C {{\n    var count = 0\n    fun inc(other: C) {{\n        count++\n        count += other.count\n        this.count = count\n        val count = 1\n        println(count)\n    }}\n}}\n");
    let (path, offset) = fx.open(&text.replacen("var count", "var cou$0nt", 1));
    let snap = fx.host.snapshot();
    let found = document_highlight(&snap, &path, offset).unwrap();
    let file = snap.file(&path).unwrap();
    let shown: Vec<String> = found
        .iter()
        .map(|(r, w)| {
            let line = file.text[..usize::from(r.start())].matches('\n').count() - HEADER.matches('\n').count();
            format!("{line}{}", if *w { "w" } else { "r" })
        })
        .collect();
    // Declaration, `count++`, `count +=`, `other.count`, `this.count =`, `= count`; not the local.
    assert_eq!(shown, vec!["1w", "3w", "4w", "4r", "5w", "5r"]);
}

#[test]
fn keyword_highlights() {
    let Some(mut fx) = fixture() else { return };
    let shown = |fx: &mut Fx, text: &str| -> Vec<String> {
        let (path, offset) = fx.open(text);
        let snap = fx.host.snapshot();
        let file = snap.file(&path).unwrap();
        document_highlight(&snap, &path, offset).unwrap().iter().map(|(r, _)| file.text[usize::from(r.start())..usize::from(r.end())].to_string()).collect()
    };
    let f = "fun f(xs: List<Int>): Int {\n    if (xs.isEmpty()) re$0turn 0\n    xs.forEach { if (it < 0) return it }\n    val g = fun(): Int { return 1 }\n    if (xs.size > 9) throw Error()\n    return xs.first()\n}\n";
    assert_eq!(shown(&mut fx, f), vec!["fun", "return", "return", "throw", "return"]);
    assert_eq!(shown(&mut fx, &f.replace("re$0turn 0", "return 0").replacen("fun f", "f$0un f", 1)), vec!["fun", "return", "return", "throw", "return"]);
    let l = "fun f() {\n    outer@ for (i in 0..9) {\n        while (true) {\n            if (i == 1) bre$0ak@outer\n            if (i == 2) break\n            continue@outer\n        }\n        if (i == 3) break\n    }\n}\n";
    assert_eq!(shown(&mut fx, l), vec!["for", "break", "continue", "break"]);
    assert_eq!(shown(&mut fx, &l.replace("bre$0ak", "break").replace("while (true)", "wh$0ile (true)")), vec!["while", "break"]);
}

impl Fx {
    fn names(&mut self, targets: Vec<NavTarget>) -> Vec<String> {
        let snap = self.host.snapshot();
        let mut out: Vec<String> = targets
            .into_iter()
            .map(|t| {
                let content = std::fs::read_to_string(&t.path).unwrap_or_else(|_| snap.file(&t.path).unwrap().text.to_string());
                let line = content[..usize::from(t.range.start())].matches('\n').count() + 1;
                format!("{}:{line}:{}", t.path.file_name().unwrap().to_string_lossy(), &content[usize::from(t.range.start())..usize::from(t.range.end())])
            })
            .collect();
        out.sort();
        out
    }
}

#[test]
fn implementations_and_type_definitions() {
    let Some(mut fx) = fixture() else { return };
    let shapes = "package app.shapes\n\ninterface Shape {\n    fun area(): Double\n}\nabstract class Polygon : Shape\nclass Square(val s: Double) : Polygon() {\n    override fun area() = s * s\n}\nclass Circle(val r: Double) : Shape {\n    override fun area() = 3.14 * r * r\n}\nclass Rect : Square(1.0)\n";
    fx.host.set_file(fx.root.join("src/app/shapes/Shapes.kt"), Arc::from(shapes), Some(1));
    // Another file, inheriting through `Polygon` only.
    fx.host.set_file(fx.root.join("src/app/Tri.kt"), Arc::from("package app\n\nimport app.shapes.Polygon\n\nclass Tri : Polygon() {\n    override fun area() = 0.5\n}\n"), Some(1));
    let never = || false;
    let imps = |fx: &mut Fx, text: &str| {
        let (path, offset) = fx.open(text);
        let found = hierarchy::implementations(&fx.host.snapshot(), &path, offset, &never);
        fx.names(found)
    };
    let main = |body: &str| format!("package app\n\nimport app.shapes.*\n\n{body}\n");
    assert_eq!(imps(&mut fx, &main("fun f(s: Sha$0pe) {}")), vec!["Shapes.kt:10:Circle", "Shapes.kt:13:Rect", "Shapes.kt:6:Polygon", "Shapes.kt:7:Square", "Tri.kt:5:Tri"]);
    // Transitive inheritors (`Rect` through `Square`).
    assert_eq!(imps(&mut fx, &main("fun f(s: Pol$0ygon) {}")), vec!["Shapes.kt:13:Rect", "Shapes.kt:7:Square", "Tri.kt:5:Tri"]);
    assert_eq!(imps(&mut fx, &main("fun f(s: Shape) = s.ar$0ea()")), vec!["Shapes.kt:11:area", "Shapes.kt:8:area", "Tri.kt:6:area"]);
    // From an override: only the overrides below it.
    fx.host.set_file(fx.root.join("src/app/Sub.kt"), Arc::from("package app\n\nimport app.shapes.*\n\nopen class Base : Shape {\n    override fun area() = 1.0\n}\nclass Sub : Base() {\n    override fun area() = 2.0\n}\n"), Some(1));
    assert_eq!(imps(&mut fx, &main("fun f(s: Base) = s.ar$0ea()")), vec!["Sub.kt:9:area"]);
    // Type definition: of a parameter, a call and a property.
    let tdef = |fx: &mut Fx, text: &str| {
        let (path, offset) = fx.open(text);
        let found = hierarchy::type_definition(&fx.host.snapshot(), &path, offset);
        fx.names(found)
    };
    assert_eq!(tdef(&mut fx, &main("fun f(sq: Square) = s$0q.area()")), vec!["Shapes.kt:7:Square"]);
    assert_eq!(tdef(&mut fx, &main("fun mk(): Circle = Circle(1.0)\nfun f() = m$0k()")), vec!["Shapes.kt:10:Circle"]);
    assert_eq!(tdef(&mut fx, &main("fun f(c: Circle) = c.r$0")).len(), 1);
    // Declaration: the overridden member, the `expect`, or the definition.
    let decl = |fx: &mut Fx, text: &str| {
        let (path, offset) = fx.open(text);
        let found = hierarchy::declaration(&fx.host.snapshot(), &path, offset);
        fx.names(found)
    };
    assert_eq!(decl(&mut fx, &main("class Mine : Shape {\n    override fun ar$0ea() = 1.0\n}")), vec!["Shapes.kt:4:area"]);
    assert_eq!(decl(&mut fx, &main("fun f(s: Sub) = s.ar$0ea()")), vec!["Sub.kt:6:area"]);
    assert_eq!(decl(&mut fx, &main("fun f(s: Circle) = s.r$0")), vec!["Shapes.kt:10:r"]);
    fx.host.set_file(fx.root.join("src/app/Platform.kt"), Arc::from("package app\n\nexpect fun platform(): String\n"), Some(1));
    assert_eq!(decl(&mut fx, "package app\n\nactual fun plat$0form(): String = \"jvm\"\n"), vec!["Platform.kt:3:platform"]);
}

impl Fx {
    /// The file with hints spliced in as `⟨label⟩`.
    fn hints(&mut self, text: &str, config: &inlay::InlayConfig) -> String {
        let path = self.root.join("src/app/Main.kt");
        self.host.set_file(path.clone(), Arc::from(text), Some(1));
        let snap = self.host.snapshot();
        let mut out = text.to_string();
        let hints = inlay::inlay_hints(&snap, &path, TextRange::up_to(TextSize::of(text)), config);
        for h in hints.iter().rev() {
            out.insert_str(usize::from(h.offset), &format!("⟨{}⟩", h.label));
        }
        out[HEADER.len()..].to_string()
    }
}

#[test]
fn inlay_hints() {
    let Some(mut fx) = fixture() else { return };
    let config = inlay::InlayConfig::default();
    let src = |body: &str| format!("{HEADER}{body}\n");
    // Inferred types; none for literals, constructor calls and explicit types.
    assert_eq!(
        fx.hints(&src("fun run(repo: Repository<User>) {\n    val all = repo.all()\n    val n = 1\n    val u = User(\"a\")\n    val s: String = \"\"\n    val first = all.first().name\n}"), &config),
        "fun run(repo: Repository<User>) {\n    val all⟨: List<User>⟩ = repo.all()\n    val n = 1\n    val u = User(⟨name:⟩\"a\")\n    val s: String = \"\"\n    val first⟨: String⟩ = all.first().name\n}\n"
    );
    // Loop variables, destructuring, lambda parameters and `it`.
    assert_eq!(
        fx.hints(&src("fun run(us: List<User>) {\n    for (u in us) {}\n    val (n, a) = us[0]\n    us.forEach { println(it) }\n    us.mapIndexed { i, u -> u }\n}"), &config),
        "fun run(us: List<User>) {\n    for (u⟨: User⟩ in us) {}\n    val (n⟨: String⟩, a⟨: Int⟩) = us[0]\n    us.forEach {⟨ it: User ->⟩ println(it) }\n    us.mapIndexed { i⟨: Int⟩, u⟨: User⟩ -> u }\n}\n"
    );
    // Return types of expression bodies; parameter names for literal arguments.
    assert_eq!(
        fx.hints(&src("fun twice(u: User) = u.greet(\"hi\")\nfun f(user: User, count: Int) {}\nfun g(u: User, user: User) = f(user, 2)"), &config),
        "fun twice(u: User)⟨: String⟩ = u.greet(\"hi\")\nfun f(user: User, count: Int) {}\nfun g(u: User, user: User)⟨: Unit⟩ = f(user, ⟨count:⟩2)\n"
    );
    let all = inlay::InlayConfig { parameter_names: inlay::ParameterNames::All, types: false, lambda_parameters: false, return_types: false, chained_calls: false, lambdas: false };
    assert_eq!(
        fx.hints(&src("fun f(user: User, count: Int) {}\nfun g(u: User, user: User, n: Int) { f(user, n); f(u, count = n) }"), &all),
        "fun f(user: User, count: Int) {}\nfun g(u: User, user: User, n: Int) { f(user, ⟨count:⟩n); f(⟨user:⟩u, count = n) }\n"
    );
}

#[test]
fn semantic_token_kinds() {
    use semantic::{TokenType as T, modifier as m};
    let Some(mut fx) = fixture() else { return };
    let text = format!("{HEADER}class Box(val item: User) {{\n    var count = 0\n    @Deprecated(\"x\") fun old() = 1\n    fun use(param: Int): String {{\n        val local = param + count\n        var v = item.greet(\"a\")\n        listOf(1).forEach {{ println(it) }}\n        return \"s\".shout() + Registry + local + v + old()\n    }}\n}}\n");
    let (path, _) = fx.open(&format!("{text}$0"));
    let snap = fx.host.snapshot();
    let tokens = semantic::semantic_tokens(&snap, &path, None);
    let at = |needle: &str, nth: usize| -> (T, u32) {
        let start = text.match_indices(needle).nth(nth).unwrap_or_else(|| panic!("{needle}")).0;
        let t = tokens.iter().find(|t| usize::from(t.range.start()) == start).unwrap_or_else(|| panic!("no token at {needle}"));
        (t.ty, t.modifiers)
    };
    assert_eq!(at("Box", 0), (T::Class, m::DECLARATION));
    assert_eq!(at("item", 0), (T::Property, m::DECLARATION | m::READONLY));
    assert_eq!(at("User", 1).0, T::Class);
    assert_eq!(at("count", 0), (T::Property, m::DECLARATION | m::MUTABLE));
    assert_eq!(at("Deprecated", 0).0, T::Decorator);
    assert_eq!(at("param", 0), (T::Parameter, m::DECLARATION | m::READONLY));
    assert_eq!(at("param", 1), (T::Parameter, m::READONLY));
    assert_eq!(at("local", 0), (T::Variable, m::DECLARATION | m::READONLY));
    assert_eq!(at("v =", 0).1, m::DECLARATION | m::MUTABLE);
    assert_eq!(at("greet", 0), (T::Method, 0));
    assert_eq!(at("listOf", 0), (T::Function, m::DEFAULT_LIBRARY));
    assert_eq!(at("forEach", 0).1 & m::EXTENSION, m::EXTENSION);
    assert_eq!(at("it)", 0).0, T::Parameter);
    assert_eq!(at("shout", 1), (T::Method, m::EXTENSION));
    assert_eq!(at("Registry", 1).0, T::Class);
    assert_eq!(at("old()", 0), (T::Method, m::DECLARATION | m::DEPRECATED));
    assert_eq!(at("old()", 1), (T::Method, m::DEPRECATED));
    assert_eq!(at("app.model", 0).0, T::Namespace);
    assert_eq!(at("val", 0).0, T::Keyword);
    assert_eq!(at("\"a\"", 0).0, T::String);
}

impl Fx {
    /// Titles of the create fixes at the cursor, and each result applied (to its file).
    fn creates(&mut self, text: &str) -> Vec<(String, String)> {
        let (path, offset) = self.open(text);
        let snap = self.host.snapshot();
        create::create_fixes(&snap, &path, TextRange::empty(offset))
            .into_iter()
            .map(|f| {
                let (p, _, _) = &f.edits[0];
                let mut content = snap.file(p).unwrap().text.to_string();
                let mut edits = f.edits.clone();
                edits.sort_by_key(|(_, r, _)| std::cmp::Reverse(r.start()));
                for (_, r, t) in edits {
                    content.replace_range(usize::from(r.start())..usize::from(r.end()), &t);
                }
                (f.title, content)
            })
            .collect()
    }
}

#[test]
fn create_from_usage() {
    let Some(mut fx) = fixture() else { return };
    // Function from a call, with parameter types from the arguments and the expected type.
    let fixes = fx.creates("package app\n\nimport app.model.User\n\nfun run(u: User) {\n    val n: Int = coun$0t(u, \"x\")\n}\n");
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    assert_eq!(fixes[0].0, "Create function 'count'");
    assert!(fixes[0].1.ends_with("}\n\nfun count(u: User, p2: String): Int {\n    TODO(\"Not yet implemented\")\n}\n"), "{}", fixes[0].1);
    // Inside a class: a private member too.
    let fixes = fx.creates("package app\n\nclass C {\n    fun f() {\n        hel$0per(1)\n    }\n}\n");
    assert_eq!(fixes[0].0, "Create member function 'helper'");
    assert!(fixes[0].1.contains("    }\n\n    private fun helper(p1: Int) {\n        TODO(\"Not yet implemented\")\n    }\n}\n"), "{}", fixes[0].1);
    // Member of a project class in another file, or an extension.
    let fixes = fx.creates("package app\n\nimport app.model.User\n\nfun run(u: User) {\n    u.ren$0ame(\"b\")\n}\n");
    assert_eq!(fixes.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), vec!["Create member function 'User.rename'", "Create extension function 'User.rename'"]);
    assert!(fixes[0].1.contains("        fun create(): User = User(\"default\")\n    }\n\n    fun rename(p1: String) {\n        TODO(\"Not yet implemented\")\n    }\n}\n"), "{}", fixes[0].1);
    assert!(fixes[1].1.ends_with("}\n\nfun User.rename(p1: String) {\n    TODO(\"Not yet implemented\")\n}\n"), "{}", fixes[1].1);
    // Class from a capitalized call.
    let fixes = fx.creates("package app\n\nfun run() {\n    val p = Poi$0nt(1, 2)\n}\n");
    assert!(fixes[0].1.ends_with("\nclass Point(val p1: Int, val p2: Int)\n"), "{}", fixes[0].1);
    // A plain name: local variable, parameter.
    let fixes = fx.creates("package app\n\nfun run(a: Int) {\n    println(lim$0it)\n}\n");
    let titles: Vec<&str> = fixes.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(titles, vec!["Create local variable 'limit'", "Create parameter 'limit'"]);
    assert!(fixes[0].1.contains("    val limit: Any = TODO(\"Not yet implemented\")\n    println(limit)"), "{}", fixes[0].1);
    assert!(fixes[1].1.contains("fun run(a: Int, limit: Any)"), "{}", fixes[1].1);
    // Resolved names get nothing.
    assert!(fx.creates("package app\n\nfun run() {\n    print$0ln()\n}\n").is_empty());
}

#[test]
fn call_and_type_hierarchies() {
    let Some(mut fx) = fixture() else { return };
    let text = "package app\n\nimport app.model.User\n\nfun leaf(u: User) = u.greet(\"x\")\nfun mid() {\n    leaf(User(\"a\"))\n    leaf(User(\"b\"))\n}\nclass Top {\n    val v = mid()\n    fun run() { mid(); println(1) }\n}\n";
    let (path, offset) = fx.open(&text.replacen("fun mid", "fun m$0id", 1));
    let snap = fx.host.snapshot();
    let root = hierarchy::prepare_call_hierarchy(&snap, &path, offset).unwrap();
    assert_eq!((root.name.as_str(), root.kind), ("mid", crate::symbols::SymbolKind::Function));
    let mut incoming: Vec<(String, usize)> = hierarchy::incoming_calls(&snap, &root.anchor, &|| false).into_iter().map(|(i, r)| (i.name, r.len())).collect();
    incoming.sort();
    assert_eq!(incoming, vec![("run".to_string(), 1), ("v".to_string(), 1)]);
    let outgoing: Vec<(String, usize)> = hierarchy::outgoing_calls(&snap, &root.anchor).into_iter().map(|(i, r)| (i.name, r.len())).collect();
    assert_eq!(outgoing, vec![("leaf".to_string(), 2), ("User".to_string(), 2)]);
    // From an outgoing item onwards: `leaf` calls `greet` (in another file).
    let leaf = hierarchy::outgoing_calls(&snap, &root.anchor).remove(0).0;
    let next: Vec<String> = hierarchy::outgoing_calls(&snap, &leaf.anchor).into_iter().map(|(i, _)| format!("{}:{}", i.path.file_name().unwrap().to_string_lossy(), i.name)).collect();
    assert_eq!(next, vec!["Model.kt:greet"]);

    // Types: supertypes (into the library) and direct subtypes.
    let text = "package app\n\ninterface Shape\nopen class Base : Shape, Comparable<Base> {\n    override fun compareTo(other: Base) = 0\n}\nclass Sub : Base()\nclass Leaf : Sub()\n";
    let (path, offset) = fx.open(&text.replacen("open class Base", "open class Ba$0se", 1));
    let snap = fx.host.snapshot();
    let root = hierarchy::prepare_type_hierarchy(&snap, &path, offset).unwrap();
    assert_eq!(root.name, "Base");
    let supers: Vec<String> = hierarchy::supertypes(&snap, &root.anchor).into_iter().map(|i| i.name).collect();
    assert_eq!(supers, vec!["Shape", "Comparable"]);
    let subs = hierarchy::subtypes(&snap, &root.anchor);
    assert_eq!(subs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["Sub"]);
    let subs2: Vec<String> = hierarchy::subtypes(&snap, &subs[0].anchor).into_iter().map(|i| i.name).collect();
    assert_eq!(subs2, vec!["Leaf"]);
}

#[test]
fn folding_and_selection() {
    let text = "package a\n\nimport a.b\nimport a.c\n\n/**\n * Doc.\n */\nclass C {\n    // one\n    // two\n    fun f(x: Int) {\n        val y = listOf(1).map {\n            it + x\n        }\n    }\n    // region helpers\n    fun g() = 1\n    // endregion\n}\n";
    let root = crate::syntax::parse(text, false).syntax();
    let line = |o: TextSize| text[..usize::from(o)].matches('\n').count();
    let folds: Vec<String> = structure::folding_ranges(&root, text).iter().map(|f| format!("{:?} {}-{}", f.kind, line(f.start), line(f.end))).collect();
    assert_eq!(folds, vec!["Imports 2-3", "Comment 5-7", "Code 8-19", "Comment 9-10", "Code 11-15", "Code 12-14", "Region 16-18"]);
    // Selection: identifier, expression, …, the braces' content, the block, the function.
    let at = TextSize::from(text.find("it + x").unwrap() as u32 + 5);
    let ranges: Vec<&str> = structure::selection_ranges(&root, text, at).iter().map(|r| &text[usize::from(r.start())..usize::from(r.end())]).collect();
    assert_eq!(ranges[0], "x");
    assert_eq!(ranges[1], "it + x");
    assert!(ranges.contains(&"{\n            it + x\n        }"), "{ranges:#?}");
    assert!(ranges.iter().any(|r| r.starts_with("fun f(x: Int)")), "{ranges:#?}");
    assert_eq!(*ranges.last().unwrap(), text);
}

#[test]
fn postfix_templates() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(u: User, xs: List<User>, n: String?, ok: Boolean) {{\n{body}\n}}\n");
    // The snippet replaces the template name; the receiver and dot are removed.
    let expand = |fx: &mut Fx, body: &str, label: &str| {
        let text = src(body);
        let c = fx.complete(&text);
        let item = c.items.iter().find(|i| i.label == label && i.kind == completion::ItemKind::Snippet).unwrap_or_else(|| panic!("no {label}"));
        let clean = text.replace("$0", "");
        let mut out = clean.clone();
        out.replace_range(usize::from(c.range.start())..usize::from(c.range.end()), item.insert.as_deref().unwrap());
        let (r, t) = &item.additional[0];
        out.replace_range(usize::from(r.start())..usize::from(r.end()), t);
        out[HEADER.len()..].to_string()
    };
    assert!(expand(&mut fx, "    u.name.va$0", "val").contains("    val ${1:s} = u.name$0\n"));
    assert!(expand(&mut fx, "    xs.va$0", "val").contains("val ${1:users} = xs$0"));
    assert!(expand(&mut fx, "    xs.fo$0", "for").contains("    for (${1:user} in xs) {\n    $0\n}"));
    assert!(expand(&mut fx, "    ok.i$0", "if").contains("    if (ok) {\n    $0\n}"));
    assert!(expand(&mut fx, "    (u.age > 1).no$0", "not").contains("    !(u.age > 1)$0"));
    assert!(expand(&mut fx, "    n.notn$0", "notnull").contains("    if (n != null) {"));
    // Only the templates that fit the type and position.
    let labels = |fx: &mut Fx, body: &str| fx.complete(&src(body)).items.into_iter().filter(|i| i.kind == completion::ItemKind::Snippet).map(|i| i.label).collect::<Vec<_>>();
    let on_user = labels(&mut fx, "    u.$0");
    assert!(on_user.contains(&"val".to_string()) && !on_user.contains(&"if".to_string()) && !on_user.contains(&"for".to_string()) && !on_user.contains(&"notnull".to_string()), "{on_user:?}");
    let in_expr = labels(&mut fx, "    val x = ok.$0");
    assert!(in_expr.contains(&"not".to_string()) && !in_expr.contains(&"val".to_string()) && !in_expr.contains(&"if".to_string()), "{in_expr:?}");
    // Snippet syntax in the receiver is escaped.
    assert!(expand(&mut fx, "    \"${u.name}\".va$0", "val").contains("= \"\\${u.name\\}\"$0"));
    // Arrays passed as arguments can be spread.
    assert!(expand(&mut fx, "    listOf(arrayOf(1).sp$0)", "spread").contains("listOf(*arrayOf(1)$0)"));
    assert!(!labels(&mut fx, "    val a = arrayOf(1).$0").contains(&"spread".to_string()));
}

#[test]
fn lambda_parameter_completion() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(xs: List<User>, m: Map<String, Int>) {{\n{body}\n}}\n");
    let snippets = |fx: &mut Fx, body: &str| fx.complete(&src(body)).items.into_iter().filter(|i| i.detail.as_deref() == Some("lambda parameters")).map(|i| (i.label, i.insert.unwrap_or_default())).collect::<Vec<_>>();
    assert_eq!(snippets(&mut fx, "    xs.forEach { $0 }"), vec![("user ->".to_string(), "${1:user} -> ".to_string())]);
    assert_eq!(snippets(&mut fx, "    xs.forEachIndexed { $0 }"), vec![("index, user ->".to_string(), "${1:index}, ${2:user} -> ".to_string())]);
    assert_eq!(snippets(&mut fx, "    xs.fold(0) { $0 }")[0].0, "acc, user ->");
    let m = snippets(&mut fx, "    m.forEach { $0 }");
    assert!(m.iter().any(|(l, _)| l == "(key, value) ->"), "{m:?}");
    // Not once the lambda has parameters, nor later in the body.
    assert!(snippets(&mut fx, "    xs.forEach { u -> $0 }").is_empty());
    assert!(snippets(&mut fx, "    xs.forEach {\n        println(it)\n        $0\n    }").is_empty());
}

#[test]
fn annotation_completion() {
    let Some(mut fx) = fixture() else { return };
    let src = |ann: &str| format!("{HEADER}annotation class Marker\nclass Main\nclass C(\n    {ann} val x: Int,\n)\n");
    let c = fx.complete(&src("@$0"));
    let first: Vec<&str> = c.items.iter().take_while(|i| i.expected).map(|i| i.label.as_str()).collect();
    assert!(first.contains(&"Marker") && first.contains(&"Deprecated") && !first.contains(&"Main"), "{first:?}");
    has_all(&c.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>(), &["field:", "get:", "param:"]);
    let labels = fx.labels(&src("@fi$0"));
    has_all(&labels, &["field:", "file:"]);
    let labels = fx.labels(&src("@field:$0"));
    assert!(!labels.iter().any(|l| l.ends_with(':')), "{labels:?}");
    has_all(&labels, &["Marker"]);
}

#[test]
fn kdoc_links() {
    let Some(mut fx) = fixture() else { return };
    let doc = |link: &str| format!("{HEADER}/**\n * Greets {link}.\n * @param count how many\n */\nfun greet(user: User, count: Int) {{}}\n");
    let snap_goto = |fx: &mut Fx, text: &str| {
        let (path, offset) = fx.open(text);
        let found = crate::analysis::ide::definition(&fx.host.snapshot(), &path, offset, false);
        fx.names(found)
    };
    assert_eq!(snap_goto(&mut fx, &doc("[Us$0er]")), vec!["Model.kt:8:User"]);
    assert_eq!(snap_goto(&mut fx, &doc("[User.na$0me]")), vec!["Model.kt:8:name"]);
    assert_eq!(snap_goto(&mut fx, &doc("[us$0er]")), vec!["Main.kt:12:user"]);
    assert_eq!(snap_goto(&mut fx, &format!("{HEADER}/**\n * @param co$0unt how many\n */\nfun greet(count: Int) {{}}\n")), vec!["Main.kt:11:count"]);
    assert!(snap_goto(&mut fx, &doc("Us$0er")).is_empty());
    // Completion in a link: parameters, names in scope, members of a qualifier.
    let labels = fx.labels(&doc("[$0"));
    has_all(&labels, &["user", "count", "User", "greet"]);
    has_all(&fx.labels(&doc("[User.$0")), &["name", "age", "greet"]);
    let c = fx.complete(&doc("[Us$0"));
    assert!(c.items.iter().all(|i| i.insert.is_none() && i.additional.is_empty()));
    // Tag names after an `@` that starts a line.
    let tags = fx.labels(&format!("{HEADER}/**\n * Greets.\n * @$0\n */\nfun greet(count: Int) {{}}\n"));
    has_all(&tags, &["param", "return", "throws", "see"]);
    assert_eq!(fx.labels(&format!("{HEADER}/**\n * @re$0\n */\nfun greet(count: Int) {{}}\n")), vec!["return", "receiver"]);
    // References include KDoc links (from the link or from the declaration).
    let refs = |fx: &mut Fx, text: &str| {
        let (path, offset) = fx.open(text);
        let found = crate::analysis::ide::references(&fx.host.snapshot(), &path, offset, false, &|| false).unwrap();
        fx.names(found)
    };
    let main = "package app\n\nimport app.model.User\n\n/** Uses [User] and [user], not [Users]. */\nfun greet(user: User) = user.name\n";
    let r = refs(&mut fx, &main.replace("[user]", "[us$0er]"));
    assert_eq!(r, vec!["Main.kt:5:user", "Main.kt:6:user"]);
    let r = refs(&mut fx, &main.replace("fun greet(user: User)", "fun greet(user: Us$0er)"));
    assert!(r.contains(&"Main.kt:5:User".to_string()) && r.contains(&"Main.kt:3:User".to_string()), "{r:?}");
}

#[test]
fn kdoc_semantic_tokens() {
    let Some(mut fx) = fixture() else { return };
    let text = format!("{HEADER}/**\n * Greets [User.name] of [user].\n * @param user who\n */\nfun greet(user: User) {{}}\n");
    let (path, _) = fx.open(&format!("{text}$0"));
    let tokens = semantic::semantic_tokens(&fx.host.snapshot(), &path, None);
    let shown: Vec<String> = tokens
        .iter()
        .filter(|t| t.modifiers & semantic::modifier::DOCUMENTATION != 0 && t.ty != semantic::TokenType::Comment)
        .map(|t| format!("{:?}:{}", t.ty, &text[usize::from(t.range.start())..usize::from(t.range.end())]))
        .collect();
    assert_eq!(shown, vec!["Class:User", "Property:name", "Parameter:user", "Decorator:@param", "Parameter:user"]);
    // The comment around them is still covered, without overlaps.
    let kdoc: Vec<_> = tokens.iter().filter(|t| t.modifiers & semantic::modifier::DOCUMENTATION != 0).collect();
    assert!(kdoc.windows(2).all(|w| w[0].range.end() == w[1].range.start()));
}

#[test]
fn generate_actions() {
    let Some(mut fx) = fixture() else { return };
    let run = |fx: &mut Fx, text: &str, title: &str| -> Option<String> {
        let (path, offset) = fx.open(text);
        let gs = crate::analysis::ide::generate::actions(&fx.host.snapshot(), &path, offset);
        let g = gs.into_iter().find(|g| g.title == title)?;
        let mut out = text.replace("$0", "");
        out.replace_range(usize::from(g.range.start())..usize::from(g.range.end()), &g.text);
        Some(out[HEADER.len()..].to_string())
    };
    let class = format!("{HEADER}class Po$0int(val x: Int, val label: String?, ids: Int) {{\n    val tags = arrayOf(\"a\")\n    val sum get() = x + 1\n}}\n");
    let eq = run(&mut fx, &class, "Generate equals() and hashCode()").unwrap();
    assert!(eq.contains("    override fun equals(other: Any?): Boolean {\n        if (this === other) return true\n        if (other !is Point) return false\n        if (x != other.x) return false\n        if (label != other.label) return false\n        if (!tags.contentEquals(other.tags)) return false\n        return true\n    }\n\n    override fun hashCode(): Int {\n        var result = x.hashCode()\n        result = 31 * result + (label?.hashCode() ?: 0)\n        result = 31 * result + tags.contentHashCode()\n        return result\n    }\n}\n"), "{eq}");
    let ts = run(&mut fx, &class, "Generate toString()").unwrap();
    assert!(ts.contains("    override fun toString(): String = \"Point(x=$x, label=$label, tags=${tags.contentToString()})\"\n}"), "{ts}");
    // Without a body, and not for data classes or classes that already have them.
    let bare = run(&mut fx, &format!("{HEADER}class B$0ox(val v: Int)\n"), "Generate toString()").unwrap();
    assert_eq!(bare, "class Box(val v: Int) {\n    override fun toString(): String = \"Box(v=$v)\"\n}\n");
    assert!(run(&mut fx, &format!("{HEADER}data class B$0ox(val v: Int)\n"), "Generate toString()").is_none());
    assert!(run(&mut fx, &format!("{HEADER}class B$0ox(val v: Int) {{\n    override fun toString() = \"\"\n}}\n"), "Generate toString()").is_none());
    // KDoc stub.
    let doc = run(&mut fx, &format!("{HEADER}class A {{\n    fun ad$0d(a: Int, b: Int): Int = a + b\n}}\n"), "Add KDoc").unwrap();
    assert_eq!(doc, "class A {\n    /**\n     * \n     *\n     * @param a \n     * @param b \n     * @return \n     */\n    fun add(a: Int, b: Int): Int = a + b\n}\n");
    assert!(run(&mut fx, &format!("{HEADER}/** Doc. */\nfun ad$0d() {{}}\n"), "Add KDoc").is_none());
    // Nothing from inside a function body.
    let (path, offset) = fx.open(&format!("{HEADER}class A(val v: Int) {{
    fun f() {{
        prin$0tln()
    }}
}}
"));
    assert!(crate::analysis::ide::generate::actions(&fx.host.snapshot(), &path, offset).is_empty());
}

impl Fx {
    /// Apply the assist titled `title` (starting with it) at `$0`, or on the selection
    /// between `$0` and `$1`.
    fn assist(&mut self, text: &str, title: &str) -> Option<String> {
        let start = text.find("$0").unwrap();
        let clean0 = text.replacen("$0", "", 1);
        let end = clean0.find("$1").unwrap_or(start);
        let clean = clean0.replacen("$1", "", 1);
        let path = self.root.join("src/app/Main.kt");
        self.host.set_file(path.clone(), Arc::from(clean.as_str()), Some(1));
        let range = TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32));
        let found = crate::analysis::ide::assists::assists(&self.host.snapshot(), &path, range);
        let (_, fix) = found.into_iter().find(|(_, f)| f.title.starts_with(title))?;
        let mut edits = fix.edits.clone();
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
        let mut out = clean;
        for e in edits {
            out.replace_range(usize::from(e.range.start())..usize::from(e.range.end()), &e.text);
        }
        Some(out.strip_prefix(HEADER).map_or(out.clone(), str::to_string))
    }
}

#[test]
fn refactoring_assists() {
    let Some(mut fx) = fixture() else { return };
    let f = |body: &str| format!("{HEADER}fun run(u: User, xs: List<User>, ok: Boolean) {{\n{body}\n}}\n");
    let body = |out: String| out.trim_start_matches("fun run(u: User, xs: List<User>, ok: Boolean) {\n").trim_end_matches("\n}\n").to_string();
    // Extract variable.
    assert_eq!(body(fx.assist(&f("    println($0u.name.length + 1$1)"), "Extract variable").unwrap()), "    val value = u.name.length + 1\n    println(value)");
    assert_eq!(body(fx.assist(&f("    println($0u.greet()$1)"), "Extract variable").unwrap()), "    val greet = u.greet()\n    println(greet)");
    assert_eq!(body(fx.assist(&f("    val name = 1\n    println($0u.name$1)"), "Extract variable").unwrap()), "    val name = 1\n    val name2 = u.name\n    println(name2)");
    // Not out of conditional or lambda code.
    assert!(fx.assist(&f("    if (ok) println($0u.name$1)"), "Extract variable").is_none());
    assert!(fx.assist(&f("    ok && $0u.name.isEmpty()$1"), "Extract variable").is_none());
    // Inside a lambda: into the lambda's body.
    assert_eq!(body(fx.assist(&f("    xs.forEach {\n        println($0it.name$1)\n    }"), "Extract variable").unwrap()), "    xs.forEach {\n        val name = it.name\n        println(name)\n    }");
    assert!(fx.assist(&f("    println($0u.na$1me)"), "Extract variable").is_none());
    // Explicit types.
    assert_eq!(body(fx.assist(&f("    val n$0ames = xs.map { it.name }"), "Specify type explicitly").unwrap()), "    val names: List<String> = xs.map { it.name }");
    assert_eq!(body(fx.assist(&f("    val n$0: Int = u.age"), "Remove explicit type").unwrap()), "    val n = u.age");
    assert!(fx.assist(&f("    val n$0: Any = u.age"), "Remove explicit type").is_none());
    let imported = fx.assist(&format!("{HEADER}fun f() {{\n    val f$0ile = java.io.File(\"a\")\n}}\n"), "Specify type explicitly").unwrap();
    assert!(imported.contains("import java.io.File") && imported.contains("val file: File = "), "{imported}");
    // Block body.
    assert_eq!(fx.assist(&format!("{HEADER}fun t$0wice(x: Int) = x * 2\n"), "Convert to block body").unwrap(), "fun twice(x: Int): Int {\n    return x * 2\n}\n");
    assert_eq!(fx.assist(&format!("{HEADER}fun s$0ay() = println(1)\n"), "Convert to block body").unwrap(), "fun say() {\n    println(1)\n}\n");
    // Argument names.
    assert_eq!(fx.assist(&format!("{HEADER}fun f() = User($0\"a\", 3)\n"), "Add names").unwrap(), "fun f() = User(name = \"a\", age = 3)\n");
    // `it` → named.
    assert_eq!(body(fx.assist(&f("    xs.forEach { println($0it.name + it.greet()) }"), "Replace 'it'").unwrap()), "    xs.forEach { user -> println(user.name + user.greet()) }");
}

#[test]
fn rewrite_assists() {
    let Some(mut fx) = fixture() else { return };
    let f = |body: &str| format!("{HEADER}fun run(u: User, xs: List<User>, ok: Boolean, n: Int) {{\n{body}\n}}\n");
    let body = |out: String| out.trim_start_matches("fun run(u: User, xs: List<User>, ok: Boolean, n: Int) {\n").trim_end_matches("\n}\n").to_string();
    // Inline variable (parenthesized where precedence needs it; templates get braces).
    assert_eq!(
        body(fx.assist(&f("    val s$0um = n + 1\n    println(sum * 2)\n    println(\"$sum\")\n    println(sum)"), "Inline variable").unwrap()),
        "    println((n + 1) * 2)\n    println(\"${n + 1}\")\n    println(n + 1)"
    );
    assert_eq!(body(fx.assist(&f("    val name = u.name\n    println(na$0me.length)"), "Inline variable").unwrap()), "    println(u.name.length)");
    assert!(fx.assist(&f("    var c$0 = 1\n    println(c)"), "Inline variable").is_none());
    // Braces.
    assert_eq!(body(fx.assist(&f("    i$0f (ok) println(1) else println(2)"), "Add braces").unwrap()), "    if (ok) {\n        println(1)\n    } else {\n        println(2)\n    }");
    assert_eq!(body(fx.assist(&f("    f$0or (x in xs) {\n        println(x)\n    }"), "Remove braces").unwrap()), "    for (x in xs) println(x)");
    assert!(fx.assist(&f("    i$0f (ok) {\n        if (n > 1) println(1)\n    } else println(2)"), "Remove braces").is_none());
    // if ↔ when.
    assert_eq!(
        body(fx.assist(&f("    i$0f (n > 1) println(1) else if (ok) {\n        println(2)\n    } else println(3)"), "Replace 'if' with 'when'").unwrap()),
        "    when {\n        n > 1 -> println(1)\n        ok -> {\n            println(2)\n        }\n        else -> println(3)\n    }"
    );
    assert_eq!(
        body(fx.assist(&f("    wh$0en (n) {\n        1, 2 -> println(1)\n        in 3..4 -> println(2)\n        else -> println(3)\n    }"), "Replace 'when' with 'if'").unwrap()),
        "    if (n == 1 || n == 2) {\n        println(1)\n    } else if (n in 3..4) {\n        println(2)\n    } else {\n        println(3)\n    }"
    );
    assert!(fx.assist(&f("    wh$0en (u.greet()) {\n        \"a\" -> println(1)\n        else -> {}\n    }"), "Replace 'when' with 'if'").is_none());
    // Lambda out of parentheses.
    assert_eq!(body(fx.assist(&f("    xs.forEach({ println(i$0t) })"), "Move lambda").unwrap()), "    xs.forEach { println(it) }");
    assert_eq!(body(fx.assist(&f("    xs.fold(0, { a, x -> a$0 + x.age })"), "Move lambda").unwrap()), "    xs.fold(0) { a, x -> a + x.age }");
    assert_eq!(body(fx.assist(&f("    xs.fold(0) $0{ a, x -> a + x.age }"), "Move lambda argument into").unwrap()), "    xs.fold(0, { a, x -> a + x.age })");
    assert_eq!(body(fx.assist(&f("    xs.forEach $0{ println(it) }"), "Move lambda argument into").unwrap()), "    xs.forEach({ println(it) })");
    // Concatenation → template.
    assert_eq!(body(fx.assist(&f("    println(\"Hi \" + u.na$0me + \"!\" + n + \"x\")"), "Convert concatenation").unwrap()), "    println(\"Hi ${u.name}!${n}x\")");
    assert_eq!(body(fx.assist(&f("    println(\"n=\" + n$0)"), "Convert concatenation").unwrap()), "    println(\"n=$n\")");
    assert!(fx.assist(&f("    println(n + n$0 + \"x\")"), "Convert concatenation").is_none());
}

#[test]
fn extract_function_and_occurrences() {
    let Some(mut fx) = fixture() else { return };
    let f = |body: &str| format!("{HEADER}class Svc {{\n    fun run(u: User, n: Int) {{\n{body}\n    }}\n}}\n");
    // Statements reading two locals, declaring one used afterwards.
    let out = fx.assist(&f("        val k = 2\n        $0val greeting = u.name + n\n        println(greeting)$1\n        println(greeting + k)"), "Extract function").unwrap();
    assert_eq!(
        out,
        "class Svc {\n    fun run(u: User, n: Int) {\n        val k = 2\n        val greeting = extracted(u, n)\n        println(greeting + k)\n    }\n\n    private fun extracted(u: User, n: Int): String {\n        val greeting = u.name + n\n        println(greeting)\n        return greeting\n    }\n}\n"
    );
    // An expression.
    let out = fx.assist(&f("        println($0u.age * n$1)"), "Extract function").unwrap();
    assert!(out.contains("        println(extracted(u, n))\n") && out.contains("    private fun extracted(u: User, n: Int): Int = u.age * n\n"), "{out}");
    // Not with jumps out, or assignments to caller locals.
    assert!(fx.assist(&f("        $0if (n > 1) return$1"), "Extract function").is_none());
    assert!(fx.assist(&f("        var c = 0\n        $0c = n$1\n        println(c)"), "Extract function").is_none());
    // Replace all occurrences.
    let out = fx.assist(&f("        println(u.name.length)\n        if (n > 0) println($0u.name.length$1 + 1)"), "Extract variable (replace all").unwrap();
    assert!(out.contains("        val length = u.name.length\n        println(length)\n        if (n > 0) println(length + 1)"), "{out}");
}

#[test]
fn signature_changes() {
    let Some(mut fx) = fixture() else { return };
    // A function with calls in another file.
    let lib = "package app\n\nfun greet(name: String, times: Int, loud: Boolean = false) {\n    repeat(times) { println(name) }\n}\n";
    let lib_path = fx.root.join("src/app/Lib.kt");
    fx.host.set_file(lib_path.clone(), Arc::from(lib), Some(1));
    fx.host.set_file(fx.root.join("src/app/Use.kt"), Arc::from("package app\n\nfun use() {\n    greet(\"a\", 2)\n    greet(\"b\", 3, true)\n    greet(times = 1, name = \"c\")\n}\n"), Some(1));
    let run = |fx: &mut Fx, at: &str, title: &str| -> Option<(String, String)> {
        let offset = TextSize::from(lib.find(at).unwrap() as u32);
        let snap = fx.host.snapshot();
        let tok = snap.file(&lib_path).unwrap().parse().syntax().token_at_offset(offset).right_biased().unwrap();
        let fix = crate::analysis::ide::signature_change::parameter_assists(&snap, &lib_path, &tok).into_iter().find(|f| f.title.starts_with(title))?;
        let apply = |text: &str, edits: Vec<&crate::analysis::lints::Edit>| {
            let mut edits = edits;
            edits.sort_by_key(|e| std::cmp::Reverse(e.range.start()));
            let mut out = text.to_string();
            for e in edits {
                out.replace_range(usize::from(e.range.start())..usize::from(e.range.end()), &e.text);
            }
            out
        };
        let use_text = snap.file(&fx.root.join("src/app/Use.kt")).unwrap().text.to_string();
        Some((apply(lib, fix.edits.iter().collect()), apply(&use_text, fix.elsewhere.iter().map(|(_, e)| e).collect())))
    };
    let (decl, uses) = run(&mut fx, "times: Int", "Move parameter 'times' left").unwrap();
    assert!(decl.contains("fun greet(times: Int, name: String, loud: Boolean = false)"), "{decl}");
    assert!(uses.contains("greet(2, \"a\")\n    greet(3, \"b\", true)\n    greet(times = 1, name = \"c\")"), "{uses}");
    // Only the first of the pair passed: it gets its name.
    let (_, uses) = run(&mut fx, "times: Int", "Move parameter 'times' right").unwrap();
    assert!(uses.contains("greet(\"a\", times = 2)\n    greet(\"b\", true, 3)"), "{uses}");
    let (decl, uses) = run(&mut fx, "loud: Boolean", "Remove parameter").unwrap();
    assert!(decl.contains("fun greet(name: String, times: Int)"), "{decl}");
    assert!(uses.contains("greet(\"b\", 3)\n"), "{uses}");
    // Used parameters are not removed.
    assert!(run(&mut fx, "name: String", "Remove parameter").is_none());
}

#[test]
fn inline_function() {
    let Some(mut fx) = fixture() else { return };
    let text = "package app\n\nfun area(w: Int, h: Int = 2) = w * h\n\nfun use(n: Int) {\n    println(ar$0ea(n + 1, 3))\n    val a = area(n)\n    println(area(h = n, w = 4) + 1)\n}\n";
    let out = fx.assist(text, "Inline function").unwrap();
    assert_eq!(out, "package app\n\nfun use(n: Int) {\n    println((n + 1) * 3)\n    val a = n * 2\n    println((4 * n) + 1)\n}\n");
    // A non-trivial argument used twice is not duplicated.
    let twice = "package app\n\nfun sq(x: Int) = x * x\n\nfun use(n: Int) = s$0q(n + 1)\n";
    assert!(fx.assist(twice, "Inline function").is_none());
    let rec = "package app\n\nfun f$0(n: Int): Int = if (n > 0) f(n - 1) else 0\n";
    assert!(fx.assist(rec, "Inline function").is_none());
}

#[test]
fn extract_constant() {
    let Some(mut fx) = fixture() else { return };
    let text = "package app\n\n/** Doc. */\nfun hello(n: Int) {\n    println($0\"Hello world!\"$1)\n    println(\"Hello world!\" + n)\n}\n";
    assert_eq!(fx.assist(text, "Extract constant 'HELLO_WORLD'").unwrap(), "package app\n\nprivate const val HELLO_WORLD = \"Hello world!\"\n\n/** Doc. */\nfun hello(n: Int) {\n    println(HELLO_WORLD)\n    println(\"Hello world!\" + n)\n}\n");
    let all = fx.assist(text, "Extract constant 'HELLO_WORLD' (replace all").unwrap();
    assert!(all.contains("    println(HELLO_WORLD)\n    println(HELLO_WORLD + n)"), "{all}");
    assert!(fx.assist("package app\n\nfun f(n: Int) = println($0\"n=$n\"$1)\n", "Extract constant").is_none());
}

#[test]
fn moving_files_updates_packages_and_imports() {
    let Some(mut fx) = fixture() else { return };
    let text = "package app.util\n\nfun shout(s: String) = s.uppercase() + other()\n";
    let files = [
        ("src/app/util/Text.kt", text),
        ("src/app/util/Other.kt", "package app.util\n\nfun other() = \"!\"\nfun twice() = shout(\"a\") + shout(\"b\")\n"),
        ("src/app/Caller.kt", "package app\n\nimport app.util.shout\n\nfun m() = shout(\"a\")\n"),
    ];
    for (p, t) in files {
        fx.host.set_file(fx.root.join(p), Arc::from(t), Some(1));
    }
    let snap = fx.host.snapshot();
    let edits = crate::analysis::ide::file_rename::rename_file_edits(&snap, &fx.root.join("src/app/util/Text.kt"), &fx.root.join("src/app/text/Text.kt"));
    let applied = |rel: &str| {
        let path = fx.root.join(rel);
        let mut out = snap.file(&path).unwrap().text.to_string();
        let mut mine: Vec<_> = edits.iter().filter(|(p, _, _)| *p == path).collect();
        mine.sort_by_key(|(_, r, _)| std::cmp::Reverse(r.start()));
        for (_, r, t) in mine {
            out.replace_range(usize::from(r.start())..usize::from(r.end()), t);
        }
        out
    };
    assert_eq!(applied("src/app/util/Text.kt"), "package app.text\n\nimport app.util.other\n\nfun shout(s: String) = s.uppercase() + other()\n");
    assert_eq!(applied("src/app/util/Other.kt"), "package app.util\n\nimport app.text.shout\n\nfun other() = \"!\"\nfun twice() = shout(\"a\") + shout(\"b\")\n");
    assert_eq!(applied("src/app/Caller.kt"), "package app\n\nimport app.text.shout\n\nfun m() = shout(\"a\")\n");
    // A rename in the same directory changes nothing.
    assert!(crate::analysis::ide::file_rename::rename_file_edits(&snap, &fx.root.join("src/app/util/Text.kt"), &fx.root.join("src/app/util/Words.kt")).is_empty());
}

#[test]
fn smart_casts() {
    let Some(mut fx) = fixture() else { return };
    // The type of the name before `$0`.
    let ty = |fx: &mut Fx, body: &str| -> String {
        let text = format!("{HEADER}open class Shape\nclass Circle(val r: Int) : Shape()\n\nfun run(s: Shape, n: String?, any: Any) {{\n{body}\n}}\n");
        let (path, offset) = fx.open(&text);
        let snap = fx.host.snapshot();
        let e = crate::analysis::ide::infer::Engine::new(&snap, &path).unwrap();
        let tok = e.root.token_at_offset(offset).left_biased().unwrap();
        let nr = tok.parent().unwrap();
        e.type_of(&nr).map(|t| t.display()).unwrap_or_default()
    };
    assert_eq!(ty(&mut fx, "    if (s is Circle) println(s$0)"), "Circle");
    assert_eq!(ty(&mut fx, "    if (s !is Circle) return\n    println(s$0)"), "Circle");
    assert_eq!(ty(&mut fx, "    if (s is Circle && s$0.r > 1) {}"), "Circle");
    assert_eq!(ty(&mut fx, "    if (s !is Circle || s$0.r > 1) {}"), "Circle");
    assert_eq!(ty(&mut fx, "    if (n != null) println(n$0)"), "String");
    assert_eq!(ty(&mut fx, "    if (n == null) println(1) else println(n$0)"), "String");
    assert_eq!(ty(&mut fx, "    requireNotNull(n)\n    println(n$0)"), "String");
    assert_eq!(ty(&mut fx, "    require(any is String)\n    println(any$0)"), "String");
    assert_eq!(ty(&mut fx, "    when (s) {\n        is Circle -> println(s$0)\n        else -> {}\n    }"), "Circle");
    assert_eq!(ty(&mut fx, "    when {\n        n == null -> {}\n        else -> println(n$0)\n    }"), "String");
    // No narrowing outside the branch or for reassigned vars.
    assert_eq!(ty(&mut fx, "    if (s is Circle) println(1)\n    println(s$0)"), "Shape");
    assert_eq!(ty(&mut fx, "    var v: String? = n\n    if (v != null) {\n        v = null\n        println(v$0)\n    }"), "String?");
    // Members of the narrowed type resolve (completion, hover).
    let labels = fx.labels(&format!("{HEADER}open class Shape\nclass Circle(val r: Int) : Shape()\nfun run(s: Shape) {{\n    if (s is Circle) s.$0\n}}\n"));
    assert!(labels.iter().any(|l| l == "r"), "{labels:?}");
}

#[test]
fn formatting() {
    let fmt = |text: &str| {
        let root = crate::syntax::parse(text, false).syntax();
        let mut edits = crate::analysis::ide::format::format(&root, text, "    ", None);
        edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start()));
        let mut out = text.to_string();
        for (r, t) in edits {
            out.replace_range(usize::from(r.start())..usize::from(r.end()), &t);
        }
        out
    };
    let messy = "package a\n\nclass A(\nval x: Int,\n      val y: Int,\n) : Base() {\n  /**\n   * Doc.\n   */\n  fun f(n: Int?): Int {   \nif (n == null &&\nx > 0) {\n return 0\n      }\n  val s = n\n?.toString()\n   ?: \"\"\n        return when (n) {\n 1 ->\n2\nelse -> s.length\n}\n}\n\nval p: Int\nget() = 1\n\n  fun g() =\n  if (x > 1) 1\n  else 2\n\n      val t = \"\"\"\n   keep\n      \"\"\"\n}";
    let want = "package a\n\nclass A(\n    val x: Int,\n    val y: Int,\n) : Base() {\n    /**\n     * Doc.\n     */\n    fun f(n: Int?): Int {\n        if (n == null &&\n            x > 0) {\n            return 0\n        }\n        val s = n\n            ?.toString()\n            ?: \"\"\n        return when (n) {\n            1 ->\n                2\n            else -> s.length\n        }\n    }\n\n    val p: Int\n        get() = 1\n\n    fun g() =\n        if (x > 1) 1\n        else 2\n\n    val t = \"\"\"\n   keep\n      \"\"\"\n}\n";
    assert_eq!(fmt(messy), want);
    assert_eq!(fmt(want), want);
}

#[test]
fn completes_unimported_library_functions_and_extensions() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}fun run(d: Double) {{\n{body}\n}}\n");
    // A top-level function of a package that is not imported by default.
    let c = fx.complete(&src("    val r = sqr$0"));
    let sqrt = c.items.iter().find(|i| i.label == "sqrt" && i.description.as_deref() == Some("kotlin.math")).expect("sqrt");
    assert_eq!(sqrt.additional.len(), 1);
    assert!(sqrt.additional[0].1.contains("import kotlin.math.sqrt"), "{:?}", sqrt.additional);
    // An extension property of the receiver's type from such a package.
    let c = fx.complete(&src("    d.absol$0"));
    let abs = c.items.iter().find(|i| i.label == "absoluteValue").expect("absoluteValue");
    assert!(abs.additional[0].1.contains("import kotlin.math.absoluteValue"), "{:?}", abs.additional);
    // Not for receivers it does not apply to.
    assert!(!fx.labels(&format!("{HEADER}fun run(u: User) {{\n    u.absol$0\n}}\n")).iter().any(|l| l == "absoluteValue"));
}

#[test]
fn completes_labels() {
    let Some(mut fx) = fixture() else { return };
    let src = |body: &str| format!("{HEADER}class Outer {{\n    fun run(xs: List<Int>) {{\n        outer@ for (x in xs) {{\n            xs.forEach {{\n{body}\n            }}\n        }}\n    }}\n}}\n");
    let labels = fx.labels(&src("                ret$0"));
    has_all(&labels, &["return@forEach"]);
    let labels = fx.labels(&src("                br$0"));
    has_all(&labels, &["break@outer"]);
    let labels = fx.labels(&src("                th$0"));
    has_all(&labels, &["this@Outer"]);
    // After `@`: the label alone.
    let c = fx.complete(&src("                return@for$0"));
    assert_eq!(c.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["forEach"]);
    let c = fx.complete(&src("                continue@$0"));
    assert_eq!(c.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["outer"]);
}

#[test]
fn completes_when_branches() {
    let Some(mut fx) = fixture() else { return };
    let decls = "enum class Color { RED, GREEN, BLUE }\nsealed interface Shape\nclass Circle(val r: Double) : Shape\nobject Empty : Shape\n";
    let c = fx.complete(&format!("{HEADER}{decls}fun f(c: Color) = when (c) {{\n    Color.RED -> 1\n    $0\n}}\n"));
    assert_eq!(c.items[0].label, "Add remaining branches");
    assert_eq!(c.items[0].insert.as_deref(), Some("Color.GREEN -> TODO()\n    Color.BLUE -> TODO()"));
    let blue = c.items.iter().find(|i| i.label == "Color.BLUE").expect("Color.BLUE");
    assert_eq!(blue.insert.as_deref(), Some("Color.BLUE -> "));
    assert!(!c.items.iter().any(|i| i.label == "Color.RED"), "covered entry offered");
    // Typing part of a name.
    let labels = fx.labels(&format!("{HEADER}{decls}fun f(c: Color) = when (c) {{\n    Color.RED -> 1\n    Color.GR$0\n}}\n"));
    has_all(&labels, &["GREEN"]);
    // Sealed hierarchies.
    let labels = fx.labels(&format!("{HEADER}{decls}fun f(s: Shape) = when (s) {{\n    $0\n}}\n"));
    has_all(&labels, &["Add remaining branches", "is Circle", "Empty"]);
}

#[test]
fn completion_ranks_expected_type_first() {
    let Some(mut fx) = fixture() else { return };
    let first = |fx: &mut Fx, body: &str| -> Vec<String> {
        let c = fx.complete(&format!("{HEADER}enum class Color {{ RED, GREEN }}\nfun paint(c: Color, n: Int) {{}}\nfun run(u: User, color: Color, count: Int, flag: Boolean, name: String?) {{\n{body}\n}}\n"));
        c.items.iter().take_while(|i| i.expected).map(|i| i.label.clone()).collect()
    };
    // Argument positions: the parameter's type.
    let labels = first(&mut fx, "    paint($0)");
    has_all(&labels, &["color"]);
    assert!(!labels.iter().any(|l| l == "count" || l == "u"), "{labels:?}");
    let labels = first(&mut fx, "    paint(color, $0)");
    has_all(&labels, &["count"]);
    assert!(!labels.iter().any(|l| l == "color"), "{labels:?}");
    // Declared variable types, conditions, assignments and comparisons.
    has_all(&first(&mut fx, "    val c: Color = $0"), &["color"]);
    let labels = first(&mut fx, "    if ($0) {}");
    has_all(&labels, &["flag", "true", "false"]);
    assert!(!labels.iter().any(|l| l == "count" || l == "Boolean"), "{labels:?}");
    has_all(&first(&mut fx, "    var s: String? = null\n    s = $0"), &["name", "null"]);
    has_all(&first(&mut fx, "    if (color == $0) {}"), &["color"]);
    // Members after a dot.
    has_all(&first(&mut fx, "    val s: String = u.$0"), &["name"]);
    // No expected type: no reordering.
    assert!(first(&mut fx, "    $0").is_empty());
    // Entries of an expected enum, qualified.
    has_all(&first(&mut fx, "    paint($0)"), &["Color.RED", "Color.GREEN"]);
    let labels = first(&mut fx, "    paint(GR$0)");
    assert_eq!(labels, vec!["Color.GREEN"]);
}

#[test]
fn inlay_hints_for_chains_and_lambdas() {
    let Some(mut fx) = fixture() else { return };
    let config = inlay::InlayConfig { types: false, lambda_parameters: false, return_types: false, parameter_names: inlay::ParameterNames::None, ..Default::default() };
    let src = |body: &str| format!("{HEADER}{body}\n");
    // Types along a chain over several lines.
    assert_eq!(
        fx.hints(&src("fun run(us: List<User>) =\n    us\n        .map { it.name }\n        .first()\n        .length"), &config),
        "fun run(us: List<User>) =\n    us⟨List<User>⟩\n        .map { it.name }⟨List<String>⟩\n        .first()⟨String⟩\n        .length\n"
    );
    // Not for chains on one line, or of a single type.
    assert_eq!(fx.hints(&src("fun run(us: List<User>) = us.map { it.name }.first()"), &config), "fun run(us: List<User>) = us.map { it.name }.first()\n");
    // Receivers of lambdas, and what multi-line lambdas return.
    assert_eq!(
        fx.hints(&src("fun run(us: List<User>) {\n    val s = buildString { append(1) }\n    val ns = us.map {\n        val n = it.name\n        n.length\n    }\n    us.forEach {\n        println(it)\n        println(it)\n    }\n}"), &config),
        "fun run(us: List<User>) {\n    val s = buildString {⟨ this: StringBuilder⟩ append(1) }\n    val ns = us.map {\n        val n = it.name\n        ⟨^map⟩n.length\n    }\n    us.forEach {\n        println(it)\n        println(it)\n    }\n}\n"
    );
}

#[test]
fn hover_constant_values_and_smart_casts() {
    let Some(mut fx) = fixture() else { return };
    let decls = "const val SECOND = 1000\nconst val MINUTE = 60 * SECOND\nconst val NAME = \"app\"\nval GREETING = \"Hello, $NAME!\"\nval BIG = Int.MAX_VALUE.toLong() + 1\nconst val HEX = 0xFF\nval plain = 42\n";
    let h = |fx: &mut Fx, name: &str| fx.hover(&format!("{HEADER}{decls}fun f() = {name}$0\n"));
    assert!(h(&mut fx, "MINUTE").contains("const val MINUTE: Int = 60000"), "{}", h(&mut fx, "MINUTE"));
    assert!(h(&mut fx, "GREETING").contains("= \"Hello, app!\""), "{}", h(&mut fx, "GREETING"));
    assert!(h(&mut fx, "HEX").contains("= 255"), "{}", h(&mut fx, "HEX"));
    // A literal initializer is shown as written; calls are not folded.
    assert!(!h(&mut fx, "plain").contains("= 42"), "{}", h(&mut fx, "plain"));
    assert!(!h(&mut fx, "BIG").contains(" = "), "{}", h(&mut fx, "BIG"));
    // Locals, and the smart-cast type where a check narrowed it.
    let local = fx.hover(&format!("{HEADER}fun f() {{\n    val total = 2 * 21L\n    println(tot$0al)\n}}\n"));
    assert!(local.contains("val total: Long = 42L"), "{local}");
    let cast = fx.hover(&format!("{HEADER}fun f(x: Any) {{\n    if (x is String) println(x$0.length)\n}}\n"));
    assert!(cast.contains("Smart cast to `String`"), "{cast}");
}

#[test]
fn completes_live_templates() {
    let Some(mut fx) = fixture() else { return };
    let c = fx.complete(&format!("{HEADER}fun run(xs: List<Int>) {{\n    ite$0\n}}\n"));
    let iter = c.items.iter().find(|i| i.label == "iter").expect("iter");
    assert!(iter.snippet);
    assert_eq!(iter.insert.as_deref(), Some("for (${1:item} in ${2:items}) {\n    $0\n}"));
    has_all(&fx.labels(&format!("{HEADER}fun run() {{\n    sou$0\n}}\n")), &["sout", "soutv"]);
    has_all(&fx.labels(&format!("{HEADER}mai$0\n")), &["main"]);
    has_all(&fx.labels(&format!("{HEADER}class A {{\n    fun$0\n}}\n")), &["fun0", "fun1", "fun2"]);
    // Statement templates only in statements; `main` only at the top level.
    assert!(!fx.labels(&format!("{HEADER}fun run() = ite$0\n")).iter().any(|l| l == "iter"));
    assert!(!fx.labels(&format!("{HEADER}class A {{\n    mai$0\n}}\n")).iter().any(|l| l == "main"));
}


#[test]
fn signature_help_binds_type_parameters() {
    let Some(mut fx) = fixture() else { return };
    let first = |fx: &mut Fx, body: &str| fx.signatures(&format!("{HEADER}{body}\n")).0.into_iter().find(|l| l.starts_with('*')).unwrap_or_default();
    assert_eq!(first(&mut fx, "fun f(u: User) = listOf(u, $0)"), "*listOf(vararg elements: User): List<User>");
    assert_eq!(first(&mut fx, "fun f() = mutableMapOf(\"a\" to 1, $0)"), "*mutableMapOf(vararg pairs: Pair<String, Int>): MutableMap<String, Int>");
    // The non-generic overload is the one Kotlin picks.
    assert_eq!(first(&mut fx, "fun f(u: User) = maxOf(u.age, $0)"), "*maxOf(a: Int, b: Int): Int");
    // Receivers still bind theirs.
    assert_eq!(first(&mut fx, "fun f(us: List<User>) = us.map($0)"), "*map(transform: (User) -> R): List<R>");
}

#[test]
fn document_links() {
    let text = "// See https://kotlinlang.org/docs/home.html.\n/** Docs: (https://example.com/a_(b)) and <https://x.dev/y> */\nval u = \"https://api.example.com/v1?q=1\"\nval notUrl = \"http://\"\n";
    let links: Vec<String> = super::structure::links(&crate::syntax::parse(text, false).syntax()).into_iter().map(|(r, url)| {
        assert_eq!(&text[usize::from(r.start())..usize::from(r.end())], url);
        url
    }).collect();
    assert_eq!(links, ["https://kotlinlang.org/docs/home.html", "https://example.com/a_(b)", "https://x.dev/y", "https://api.example.com/v1?q=1"]);
}

#[test]
fn overloads_resolved_by_argument_types() {
    let Some(mut fx) = fixture() else { return };
    let decls = "fun pick(a: Int): Int = a\nfun pick(a: String): String = a\nfun pick(a: Any): Boolean = true\nfun <T> wrap(v: T): List<T> = listOf(v)\nfun wrap(v: Int): Long = 1L\n";
    let h = |fx: &mut Fx, call: &str| fx.hover(&format!("{HEADER}{decls}fun f(u: User) {{\n    val r$0 = {call}\n}}\n"));
    // The parameter type the argument fits; the most specific one among several.
    assert!(h(&mut fx, "pick(\"x\")").contains("val r: String"), "{}", h(&mut fx, "pick(\"x\")"));
    assert!(h(&mut fx, "pick(1)").contains("val r: Int"), "{}", h(&mut fx, "pick(1)"));
    assert!(h(&mut fx, "pick(u)").contains("val r: Boolean"), "{}", h(&mut fx, "pick(u)"));
    // Non-generic before generic.
    assert!(h(&mut fx, "wrap(1)").contains("val r: Long"), "{}", h(&mut fx, "wrap(1)"));
    assert!(h(&mut fx, "wrap(u)").contains("val r: List<User>"), "{}", h(&mut fx, "wrap(u)"));
}

#[test]
fn intention_assists() {
    let Some(mut fx) = fixture() else { return };
    let f = |body: &str| format!("{HEADER}fun run(u: User, xs: List<User>, ok: Boolean, n: Int) {{\n{body}\n}}\n");
    let body = |out: String| out.trim_start_matches("fun run(u: User, xs: List<User>, ok: Boolean, n: Int) {\n").trim_end_matches("\n}\n").to_string();
    // Invert `if`.
    assert_eq!(body(fx.assist(&f("    i$0f (n > 1) println(1) else println(2)"), "Invert 'if'").unwrap()), "    if (n <= 1) println(2) else println(1)");
    assert_eq!(body(fx.assist(&f("    i$0f (!ok) {\n        println(1)\n    } else println(2)"), "Invert 'if'").unwrap()), "    if (ok) println(2) else {\n        println(1)\n    }");
    assert_eq!(body(fx.assist(&f("    i$0f (u is Any) println(1) else println(2)"), "Invert 'if'").unwrap()), "    if (u !is Any) println(2) else println(1)");
    assert!(fx.assist(&f("    i$0f (ok) println(1)"), "Invert 'if'").is_none());
    // Flip a comparison.
    assert_eq!(body(fx.assist(&f("    println(n $0< u.age)"), "Flip '<'").unwrap()), "    println(u.age > n)");
    // De Morgan.
    assert_eq!(body(fx.assist(&f("    println(ok $0&& n > 1 && u.age == 2)"), "Replace '&&' with '||'").unwrap()), "    println(!(!ok || n <= 1 || u.age != 2))");
    assert_eq!(body(fx.assist(&f("    if (!(ok $0|| n > 1)) println()"), "Replace '||' with '&&'").unwrap()), "    if (!ok && n <= 1) println()");
    // Split / merge `if`.
    assert_eq!(body(fx.assist(&f("    if (ok $0&& n > 1) println(n)"), "Split into 2").unwrap()), "    if (ok) {\n        if (n > 1) println(n)\n    }");
    assert_eq!(body(fx.assist(&f("    i$0f (ok) {\n        if (n > 1 || n < 0) {\n            println(n)\n        }\n    }"), "Merge with nested").unwrap()), "    if (ok && (n > 1 || n < 0)) {\n        println(n)\n    }");
    // `when` subject.
    assert_eq!(
        body(fx.assist(&f("    wh$0en {\n        n == 1 -> println(1)\n        n in 2..3 -> println(2)\n        else -> println(3)\n    }"), "Introduce 'n' as subject").unwrap()),
        "    when (n) {\n        1 -> println(1)\n        in 2..3 -> println(2)\n        else -> println(3)\n    }"
    );
    assert!(fx.assist(&f("    wh$0en {\n        n == 1 -> println(1)\n        ok -> println(2)\n    }"), "Introduce").is_none());
    assert_eq!(
        body(fx.assist(&f("    wh$0en (n) {\n        1, 2 -> println(1)\n        !in 3..4 -> println(2)\n        else -> println(3)\n    }"), "Remove subject").unwrap()),
        "    when {\n        n == 1 || n == 2 -> println(1)\n        n !in 3..4 -> println(2)\n        else -> println(3)\n    }"
    );
    // for ↔ forEach.
    assert_eq!(body(fx.assist(&f("    f$0or (x in xs) {\n        println(x.name)\n    }"), "Replace with a 'forEach'").unwrap()), "    xs.forEach { x ->\n        println(x.name)\n    }");
    assert_eq!(body(fx.assist(&f("    f$0or (i in 0 until n) println(i)"), "Replace with a 'forEach'").unwrap()), "    (0 until n).forEach { i ->\n        println(i)\n    }");
    assert!(fx.assist(&f("    f$0or (x in xs) {\n        if (ok) break\n    }"), "Replace with a 'forEach'").is_none());
    assert_eq!(
        body(fx.assist(&f("    xs.for$0Each {\n        if (it.age > 1) return@forEach\n        println(it.name)\n    }"), "Replace with a 'for'").unwrap()),
        "    for (item in xs) {\n        if (item.age > 1) continue\n        println(item.name)\n    }"
    );
    assert_eq!(body(fx.assist(&f("    xs.for$0Each { x -> println(x) }"), "Replace with a 'for'").unwrap()), "    for (x in xs) {\n        println(x)\n    }");
}

#[test]
fn auto_import_offers_every_package_of_a_name() {
    let Some(mut fx) = fixture() else { return };
    fx.host.set_file(fx.root.join("src/lib/one/Service.kt"), Arc::from("package lib.one\n\nclass Service\nfun helper() = 1\nfun String.shout() = this\n"), Some(1));
    fx.host.set_file(fx.root.join("src/lib/two/Service.kt"), Arc::from("package lib.two\n\nannotation class Service\nfun helper() = 2\nfun String.shout() = this\n"), Some(1));
    // Same-named classes, functions and extensions of different packages are all offered.
    let mut imported = |text: &str, label: &str| -> Vec<String> {
        let mut out: Vec<String> = fx.complete(text).items.into_iter().filter(|i| i.label == label).filter_map(|i| i.description).collect();
        out.sort();
        out
    };
    assert_eq!(imported("package app\n\nval x: Servi$0\n", "Service"), ["lib.one", "lib.two"]);
    assert_eq!(imported("package app\n\n@Servi$0\nclass A\n", "Service"), ["lib.one", "lib.two"]);
    assert_eq!(imported("package app\n\nfun f() = helpe$0\n", "helper"), ["lib.one", "lib.two"]);
    assert_eq!(imported("package app\n\nfun f() = \"a\".shou$0\n", "shout"), ["app.model", "lib.one", "lib.two"]);
}
