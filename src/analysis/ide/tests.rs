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

