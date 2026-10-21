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
    let (labels, param) = fx.signatures(&src("    add(add(\"x\"), $0\n"));
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
    let all = inlay::InlayConfig { parameter_names: inlay::ParameterNames::All, types: false, lambda_parameters: false, return_types: false };
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
}
