use super::*;

/// The Kotlin standard library shipped with a local kotlinc, if any.
fn stdlib() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("KOTLIN_STDLIB").map(PathBuf::from),
        dirs::home_dir().map(|h| h.join(".sdkman/candidates/kotlin/current/lib/kotlin-stdlib.jar")),
        Some(PathBuf::from("/usr/share/kotlin/lib/kotlin-stdlib.jar")),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file())
}

fn index() -> Option<ClasspathIndex> {
    let stdlib = stdlib()?;
    let jdk = crate::project::jdk::find(None, None);
    Some(ClasspathIndex::build(&[stdlib], jdk.as_ref()))
}

fn member<'a>(c: &'a LibClass, name: &str) -> Option<&'a Symbol> {
    c.members.iter().find(|m| m.name == name)
}

#[test]
fn builtins_from_stdlib() {
    let Some(idx) = index() else { return };
    assert!(idx.problems.is_empty(), "{:?}", idx.problems);
    assert!(idx.has_builtins());
    let string = idx.find_class("kotlin.String").expect("kotlin.String");
    assert_eq!(string.symbol.kind, SymbolKind::Class);
    let length = member(&string, "length").expect("String.length");
    assert_eq!(length.ty.as_ref().unwrap().display(), "Int");
    let plus = member(&string, "plus").unwrap();
    assert!(plus.modifiers.has(Modifiers::OPERATOR));
    assert_eq!(plus.params[0].ty.display(), "Any?");

    let list = idx.find_class("kotlin.collections.List").unwrap();
    assert_eq!(list.symbol.kind, SymbolKind::Interface);
    assert_eq!(list.symbol.type_params[0].name, "E");
    assert_eq!(list.symbol.type_params[0].variance, Variance::Out);
    assert_eq!(list.symbol.supertypes[0].display(), "Collection<E>");
    let get = member(&list, "get").unwrap();
    assert_eq!(get.signature(), "abstract operator fun get(index: Int): E");

    let entry = idx.find_class("kotlin.collections.Map.Entry").unwrap();
    assert_eq!(entry.symbol.name, "Entry");
    assert!(idx.find_class("kotlin.Int").unwrap().members.iter().any(|m| m.name == "rangeTo"));
}

#[test]
fn kotlin_class_files() {
    let Some(idx) = index() else { return };
    let pair = idx.find_class("kotlin.Pair").unwrap();
    assert!(pair.is_kotlin);
    assert!(pair.symbol.modifiers.has(Modifiers::DATA));
    let ctor = pair.members.iter().find(|m| m.kind == SymbolKind::Constructor).unwrap();
    assert_eq!(ctor.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
    assert_eq!(member(&pair, "first").unwrap().ty.as_ref().unwrap().display(), "A");

    let regex = idx.find_class("kotlin.text.Regex").unwrap();
    let find = member(&regex, "find").unwrap();
    assert_eq!(find.signature(), "fun find(input: CharSequence, startIndex: Int = …): MatchResult?");
    assert!(regex.companion.is_some());

    // File facades are not classes.
    assert!(idx.find_class("kotlin.collections.CollectionsKt").is_none());
    let lazy = idx.find_class("kotlin.LazyThreadSafetyMode").unwrap();
    assert_eq!(lazy.symbol.kind, SymbolKind::Enum);
    assert!(lazy.members.iter().any(|m| m.kind == SymbolKind::EnumEntry && m.name == "SYNCHRONIZED"));
}

#[test]
fn top_level_members() {
    let Some(idx) = index() else { return };
    let members = idx.package_members("kotlin.collections");
    let list_of: Vec<&Symbol> = members.iter().filter(|m| m.name == "listOf").collect();
    assert!(list_of.len() >= 2, "listOf overloads: {}", list_of.len());
    let map = members.iter().find(|m| m.name == "map" && m.receiver.as_ref().is_some_and(|r| r.fq_name() == Some("kotlin.collections.Iterable"))).expect("Iterable.map");
    assert_eq!(map.signature(), "inline fun <T, R> Iterable<T>.map(transform: (T) -> R): List<R>");
    // typealias ArrayList = java.util.ArrayList
    let alias = members.iter().find(|m| m.kind == SymbolKind::TypeAlias && m.name == "ArrayList").unwrap();
    assert_eq!(alias.ty.as_ref().unwrap().fq_name(), Some("java.util.ArrayList"));
    let println = idx.package_members("kotlin.io");
    assert!(println.iter().any(|m| m.name == "println"));
    let lazy = idx.lookup("kotlin", "lazy").1;
    assert!(!lazy.is_empty());
    assert!(idx.has_package("kotlin.collections"));
    assert!(idx.has_package("kotlin"));
    assert!(!idx.has_package("kotlin.nonexistent"));
}

#[test]
fn jdk_classes() {
    let Some(idx) = index() else { return };
    if idx.find_class("java.lang.Object").is_none() {
        return; // no JDK found
    }
    let map = idx.find_class("java.util.HashMap").unwrap();
    assert!(!map.is_kotlin);
    assert_eq!(map.symbol.type_params.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["K", "V"]);
    assert!(map.symbol.supertypes.iter().any(|s| s.fq_name() == Some("kotlin.collections.MutableMap")));
    let put = member(&map, "put").unwrap();
    assert_eq!(put.signature(), "fun put(p0: K!, p1: V!): V!");
    let entry = idx.find_class("java.util.Map.Entry").unwrap();
    assert_eq!(entry.symbol.name, "Entry");
    let thread = idx.find_class("java.lang.Thread").unwrap();
    assert!(thread.nested.iter().any(|n| n == "State"));
    let sleep = thread.members.iter().find(|m| m.name == "sleep").unwrap();
    assert!(sleep.modifiers.has(Modifiers::STATIC));
    let string = idx.find_class("java.lang.String").unwrap();
    let format = string.members.iter().find(|m| m.name == "format").unwrap();
    assert!(format.params.last().unwrap().vararg);
    assert!(idx.package_classes("java.util").iter().any(|c| c == "ArrayList"));
}

#[test]
fn synthetic_function_types() {
    let idx = ClasspathIndex::empty();
    let f2 = idx.find_class("kotlin.Function2").unwrap();
    assert_eq!(f2.symbol.type_params.len(), 3);
    assert_eq!(f2.members[0].signature(), "operator fun invoke(p1: P1, p2: P2): R");
}

#[test]
fn whole_stdlib_decodes() {
    let Some(stdlib) = stdlib() else { return };
    let idx = ClasspathIndex::build(&[stdlib], None);
    let mut kotlin_classes = 0;
    let mut errors = 0;
    let all: Vec<String> = idx.packages.values().flatten().map(|s| s.to_string()).collect();
    for internal in &all {
        match idx.class_by_internal(internal) {
            Some(c) => {
                if c.is_kotlin && c.symbol.kind != SymbolKind::Package {
                    kotlin_classes += 1;
                    assert!(!c.symbol.fq_name.is_empty(), "{internal}");
                    for m in &c.members {
                        assert!(!m.name.is_empty(), "{internal}: empty member name");
                    }
                }
            }
            None => errors += 1,
        }
    }
    assert_eq!(errors, 0);
    assert!(kotlin_classes > 300, "{kotlin_classes}");
    // Every package's top-level members decode without empty names or error types.
    let pkgs: Vec<String> = idx.packages.keys().map(|k| k.to_string()).collect();
    let mut total = 0;
    for p in pkgs {
        for m in idx.package_members(&p).iter() {
            total += 1;
            assert!(!m.name.is_empty());
            if let Some(t) = &m.ty {
                assert!(!t.display().contains("<error>"), "{}: {}", m.fq_name, m.signature());
            }
        }
    }
    assert!(total > 3000, "{total}");
}
