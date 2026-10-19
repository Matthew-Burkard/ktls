//! Binary dependencies: jars, class directories and the JDK image, indexed lazily.
//!
//! Building an index only lists class names (zip central directories, jimage
//! tables). Class files are parsed on first use and cached.

pub mod classfile;
pub mod jimage;
pub mod kmeta;
pub mod signature;

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;

use self::classfile::{ACC_ABSTRACT, ACC_ANNOTATION, ACC_BRIDGE, ACC_ENUM, ACC_FINAL, ACC_INTERFACE, ACC_PRIVATE, ACC_PROTECTED, ACC_PUBLIC, ACC_STATIC, ACC_SYNTHETIC, ACC_VARARGS, ClassFile};
use crate::project::jdk::{Jdk, JdkClasses};
use crate::symbols::{Modifiers, Origin, Param, Symbol, SymbolKind, TypeArg, TypeParam, TypeRef, Variance, Visibility};

enum Data {
    Jar(Mutex<zip::ZipArchive<BufReader<std::fs::File>>>),
    Dir(PathBuf),
    JImage(jimage::JImage),
}

/// One classpath entry.
pub struct Archive {
    pub path: PathBuf,
    data: Data,
    /// internal class name → entry name (jimage: `module/path`)
    classes: Vec<(Box<str>, Box<str>)>,
    builtins: Vec<String>,
}

impl Archive {
    pub fn open(path: &Path) -> anyhow::Result<Archive> {
        if path.is_dir() {
            let mut classes = Vec::new();
            for e in walkdir::WalkDir::new(path).into_iter().flatten() {
                if let Some(rel) = e.path().strip_prefix(path).ok().and_then(|r| r.to_str()) {
                    let rel = rel.replace('\\', "/");
                    if let Some(name) = rel.strip_suffix(".class") {
                        classes.push((name.into(), rel.clone().into()));
                    }
                }
            }
            return Ok(Archive { path: path.to_path_buf(), data: Data::Dir(path.to_path_buf()), classes, builtins: Vec::new() });
        }
        if path.file_name().is_some_and(|n| n == "modules") {
            let img = jimage::JImage::open(path)?;
            let classes = img
                .class_entries()
                .into_iter()
                .filter(|(_, p)| p != "module-info.class")
                .map(|(m, p)| (p.trim_end_matches(".class").into(), format!("{m}/{p}").into()))
                .collect();
            return Ok(Archive { path: path.to_path_buf(), data: Data::JImage(img), classes, builtins: Vec::new() });
        }
        let file = std::fs::File::open(path)?;
        let zip = zip::ZipArchive::new(BufReader::new(file))?;
        let mut classes = Vec::new();
        let mut builtins = Vec::new();
        // `.jmod` files keep classes under `classes/`.
        let jmod = path.extension().is_some_and(|e| e == "jmod");
        let names: Vec<String> = zip.file_names().filter_map(Result::ok).map(|n| n.into_owned()).collect();
        for name in &names {
            let name = name.as_str();
            let rel = if jmod {
                match name.strip_prefix("classes/") {
                    Some(r) => r,
                    None => continue,
                }
            } else {
                name
            };
            if rel.starts_with("META-INF/") || rel == "module-info.class" {
                continue;
            }
            if let Some(n) = rel.strip_suffix(".class") {
                classes.push((n.into(), name.into()));
            } else if name.ends_with(".kotlin_builtins") {
                builtins.push(name.to_string());
            }
        }
        Ok(Archive { path: path.to_path_buf(), data: Data::Jar(Mutex::new(zip)), classes, builtins })
    }

    pub fn read(&self, entry: &str) -> Option<Vec<u8>> {
        match &self.data {
            Data::Jar(zip) => {
                let mut zip = zip.lock();
                let mut f = zip.by_name(entry).ok()?;
                let mut buf = Vec::with_capacity(f.size() as usize);
                f.read_to_end(&mut buf).ok()?;
                Some(buf)
            }
            Data::Dir(d) => std::fs::read(d.join(entry)).ok(),
            Data::JImage(img) => {
                let (module, path) = entry.split_once('/')?;
                img.read(module, path)
            }
        }
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }
}

/// Process-wide archive cache: modules sharing a jar share one `Archive`.
fn archive_cache() -> &'static Mutex<HashMap<PathBuf, (Option<SystemTime>, Arc<Archive>)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Option<SystemTime>, Arc<Archive>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn open_cached(path: &Path) -> anyhow::Result<Arc<Archive>> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some((t, a)) = archive_cache().lock().get(path)
        && *t == mtime && !path.is_dir() {
            return Ok(a.clone());
        }
    let a = Arc::new(Archive::open(path)?);
    archive_cache().lock().insert(path.to_path_buf(), (mtime, a.clone()));
    Ok(a)
}

/// A library class with Kotlin-level member view.
#[derive(Debug, Clone)]
pub struct LibClass {
    pub internal: String,
    pub symbol: Symbol,
    pub members: Vec<Symbol>,
    /// Simple names of nested classes.
    pub nested: Vec<String>,
    pub companion: Option<String>,
    pub is_kotlin: bool,
}

/// Index over one module's binary classpath.
pub struct ClasspathIndex {
    archives: Vec<Arc<Archive>>,
    /// internal name → (archive index, entry index)
    classes: HashMap<Box<str>, (u32, u32)>,
    /// dotted package → top-level class internal names
    packages: HashMap<Box<str>, Vec<Box<str>>>,
    builtin_classes: HashMap<String, Arc<LibClass>>,
    builtin_members: HashMap<String, Vec<Symbol>>,
    /// Packages declared by `.kotlin_builtins` fragments (and their parents).
    builtin_packages: std::collections::HashSet<String>,
    class_cache: RwLock<HashMap<Box<str>, Option<Arc<LibClass>>>>,
    package_cache: RwLock<HashMap<Box<str>, Arc<Vec<Symbol>>>>,
    /// Kotlin package → facade classes declared in a different JVM package
    /// (`@file:JvmPackageName`, e.g. kotlin-test's `kotlin.test` annotations).
    relocated_facades: HashMap<String, Vec<Box<str>>>,
    pub problems: Vec<String>,
}

/// A name segment like `Foo$1` or `Foo$lambda$2` is not a source-level class.
fn is_anonymous(internal: &str) -> bool {
    internal.split('$').skip(1).any(|seg| seg.is_empty() || seg.starts_with(|c: char| c.is_ascii_digit()) || seg.starts_with("lambda") || seg == "DefaultImpls" || seg == "WhenMappings")
}

impl ClasspathIndex {
    pub fn empty() -> ClasspathIndex {
        Self::build(&[], None)
    }

    /// Build an index over `jdk` (first) and `entries`, in classpath order.
    ///
    /// Uses rayon unless called from a rayon worker: a worker that blocks on a lazily
    /// initialized index while also participating in its parallel build can deadlock.
    pub fn build(entries: &[PathBuf], jdk: Option<&Jdk>) -> ClasspathIndex {
        Self::build_with(entries, jdk, rayon::current_thread_index().is_none())
    }

    pub fn build_with(entries: &[PathBuf], jdk: Option<&Jdk>, parallel: bool) -> ClasspathIndex {
        let mut paths: Vec<PathBuf> = Vec::new();
        if let Some(jdk) = jdk {
            match &jdk.classes {
                JdkClasses::JImage(p) | JdkClasses::RtJar(p) => paths.push(p.clone()),
                JdkClasses::Jmods(dir) => {
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        // jmods are zips with a 4-byte header; zip readers cope with the prefix.
                        paths.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jmod")));
                    }
                }
            }
        }
        paths.extend(entries.iter().cloned());
        let opened: Vec<anyhow::Result<Arc<Archive>>> =
            if parallel { paths.par_iter().map(|p| open_cached(p)).collect() } else { paths.iter().map(|p| open_cached(p)).collect() };
        let mut problems = Vec::new();
        let mut archives = Vec::new();
        for (p, a) in paths.iter().zip(opened) {
            match a {
                Ok(a) => archives.push(a),
                Err(e) => problems.push(format!("cannot open {}: {e}", p.display())),
            }
        }
        let mut classes: HashMap<Box<str>, (u32, u32)> = HashMap::new();
        let mut packages: HashMap<Box<str>, Vec<Box<str>>> = HashMap::new();
        for (ai, a) in archives.iter().enumerate() {
            for (ei, (internal, entry)) in a.classes.iter().enumerate() {
                let _ = entry;
                if classes.contains_key(internal) {
                    continue;
                }
                classes.insert(internal.clone(), (ai as u32, ei as u32));
                if !internal.contains('$') && !internal.ends_with("package-info") {
                    let pkg = internal.rsplit_once('/').map(|(p, _)| p.replace('/', ".")).unwrap_or_default();
                    packages.entry(pkg.into()).or_default().push(internal.clone());
                }
            }
        }
        let mut idx = ClasspathIndex {
            archives,
            classes,
            packages,
            builtin_classes: HashMap::new(),
            builtin_members: HashMap::new(),
            builtin_packages: Default::default(),
            class_cache: RwLock::new(HashMap::new()),
            package_cache: RwLock::new(HashMap::new()),
            relocated_facades: HashMap::new(),
            problems,
        };
        idx.load_builtins();
        idx.relocated_facades = idx.find_relocated_facades(parallel);
        idx
    }

    fn load_builtins(&mut self) {
        for a in &self.archives {
            for entry in &a.builtins {
                let Some(bytes) = a.read(entry) else { continue };
                let Some((package, classes, members)) = kmeta::decode_builtins(&bytes) else {
                    self.problems.push(format!("cannot decode {entry} in {}", a.path.display()));
                    continue;
                };
                for c in classes {
                    let fq = c.symbol.fq_name.clone();
                    self.builtin_classes.entry(fq.clone()).or_insert_with(|| {
                        Arc::new(LibClass {
                            internal: fq.replace('.', "/"),
                            symbol: c.symbol,
                            members: c.members,
                            nested: c.nested,
                            companion: c.companion,
                            is_kotlin: true,
                        })
                    });
                }
                let mut p = package.as_str();
                loop {
                    self.builtin_packages.insert(p.to_string());
                    match p.rfind('.') {
                        Some(i) => p = &p[..i],
                        None => break,
                    }
                }
                self.builtin_members.entry(package).or_default().extend(members);
            }
        }
    }

    pub fn archives(&self) -> &[Arc<Archive>] {
        &self.archives
    }

    pub fn class_count(&self) -> usize {
        self.classes.len() + self.builtin_classes.len()
    }

    pub fn has_builtins(&self) -> bool {
        !self.builtin_classes.is_empty()
    }

    pub fn has_package(&self, pkg: &str) -> bool {
        if self.packages.contains_key(pkg) || self.builtin_packages.contains(pkg) {
            return true;
        }
        if self.relocated_facades.keys().any(|p| p == pkg || p.starts_with(&format!("{pkg}."))) {
            return true;
        }
        let prefix = format!("{pkg}.");
        self.packages.keys().any(|p| p.starts_with(&prefix))
    }

    /// Simple names of top-level classes in a package (excluding Kotlin file facades).
    pub fn package_classes(&self, pkg: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .packages
            .get(pkg)
            .map(|v| v.iter().filter(|i| !is_anonymous(i)).map(|i| i.rsplit('/').next().unwrap().to_string()).collect())
            .unwrap_or_default();
        let prefix = if pkg.is_empty() { String::new() } else { format!("{pkg}.") };
        for fq in self.builtin_classes.keys() {
            if let Some(rest) = fq.strip_prefix(&prefix)
                && !rest.contains('.') {
                    out.push(rest.to_string());
                }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Internal-name candidates for a dotted FQ name (`a.b.Outer.Inner`).
    fn internal_candidates(fq: &str) -> Vec<String> {
        let parts: Vec<&str> = fq.split('.').collect();
        (1..=parts.len())
            .rev()
            .map(|k| {
                let (pkg, cls) = parts.split_at(k - 1);
                let cls = cls.join("$");
                if pkg.is_empty() { cls } else { format!("{}/{}", pkg.join("/"), cls) }
            })
            .collect()
    }

    /// Look up a class by Kotlin FQ name. Kotlin file facades are not classes.
    pub fn find_class(&self, fq: &str) -> Option<Arc<LibClass>> {
        if let Some(c) = self.builtin_classes.get(fq) {
            return Some(c.clone());
        }
        if let Some(c) = synthetic_function_class(fq) {
            return Some(c);
        }
        for internal in Self::internal_candidates(fq) {
            if self.classes.contains_key(internal.as_str()) {
                return self.class_by_internal(&internal).filter(|c| !is_facade(c));
            }
        }
        None
    }

    pub fn class_by_internal(&self, internal: &str) -> Option<Arc<LibClass>> {
        if let Some(hit) = self.class_cache.read().get(internal) {
            return hit.clone();
        }
        let parsed = self.parse_class(internal);
        self.class_cache.write().insert(internal.into(), parsed.clone());
        parsed
    }

    fn parse_class(&self, internal: &str) -> Option<Arc<LibClass>> {
        let &(ai, ei) = self.classes.get(internal)?;
        let archive = &self.archives[ai as usize];
        let entry = &archive.classes[ei as usize].1;
        let bytes = archive.read(entry)?;
        let cf = classfile::parse(&bytes).ok()?;
        Some(Arc::new(convert_class(&cf, &archive.path)))
    }

    /// The raw class file of an indexed class.
    pub fn class_file(&self, internal: &str) -> Option<ClassFile> {
        self.read_class(internal)
    }

    /// The archive (jar / jimage / directory) containing `internal`.
    pub fn archive_of(&self, internal: &str) -> Option<&Arc<Archive>> {
        self.classes.get(internal).map(|&(ai, _)| &self.archives[ai as usize])
    }

    /// The archive holding `.kotlin_builtins` (the Kotlin standard library).
    pub fn builtins_archive(&self) -> Option<&Arc<Archive>> {
        self.archives.iter().find(|a| !a.builtins.is_empty())
    }

    /// Internal names of top-level classes in a JVM package (`kotlin.collections`).
    pub fn package_internal_names(&self, pkg: &str) -> Vec<String> {
        self.packages.get(pkg).map(|v| v.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
    }

    fn read_class(&self, internal: &str) -> Option<ClassFile> {
        let &(ai, ei) = self.classes.get(internal)?;
        let archive = &self.archives[ai as usize];
        classfile::parse(&archive.read(&archive.classes[ei as usize].1)?).ok()
    }

    /// Computed eagerly: initializing lazily from inside parallel lookups could deadlock.
    fn find_relocated_facades(&self, parallel: bool) -> HashMap<String, Vec<Box<str>>> {
        let candidates: Vec<&str> = self.packages.values().flatten().map(|b| &**b).filter(|i| i.ends_with("Kt") || i.contains("Kt__")).collect();
        let check = |internal: &&str| {
            let cf = self.read_class(internal)?;
            let pn = cf.kotlin.as_ref()?.package_name.clone()?;
            let jvm_pkg = internal.rsplit_once('/').map(|(p, _)| p.replace('/', ".")).unwrap_or_default();
            (pn != jvm_pkg).then(|| (pn, Box::<str>::from(*internal)))
        };
        let found: Vec<(String, Box<str>)> =
            if parallel { candidates.par_iter().filter_map(check).collect() } else { candidates.iter().filter_map(check).collect() };
        let mut map: HashMap<String, Vec<Box<str>>> = HashMap::new();
        for (pn, internal) in found {
            map.entry(pn).or_default().push(internal);
        }
        map
    }

    /// Kotlin top-level functions, properties and type aliases of a package.
    pub fn package_members(&self, pkg: &str) -> Arc<Vec<Symbol>> {
        if let Some(hit) = self.package_cache.read().get(pkg) {
            return hit.clone();
        }
        let mut members: Vec<Symbol> = self.builtin_members.get(pkg).cloned().unwrap_or_default();
        let mut classes: Vec<&Box<str>> = self.packages.get(pkg).map(|v| v.iter().collect()).unwrap_or_default();
        classes.extend(self.relocated_facades.get(pkg).into_iter().flatten());
        {
            let found: Vec<Vec<Symbol>> = classes
                .par_iter()
                .filter_map(|internal| {
                    let cf = self.read_class(internal)?;
                    let meta = cf.kotlin.as_ref().filter(|m| m.kind == 2 || m.kind == 5)?;
                    let archive = &self.archives[self.classes.get(&***internal)?.0 as usize];
                    match kmeta::decode(meta, pkg, &archive.path, internal)? {
                        kmeta::Decoded::Package(syms) => Some(syms),
                        _ => None,
                    }
                })
                .collect();
            members.extend(found.into_iter().flatten());
        }
        members.retain(|m| m.visibility != Visibility::Private);
        let members = Arc::new(members);
        self.package_cache.write().insert(pkg.into(), members.clone());
        members
    }

    /// Classes and top-level declarations named `name` in `pkg`.
    pub fn lookup(&self, pkg: &str, name: &str) -> (Option<Arc<LibClass>>, Vec<Symbol>) {
        let fq = if pkg.is_empty() { name.to_string() } else { format!("{pkg}.{name}") };
        let class = self.find_class(&fq);
        let members = self.package_members(pkg).iter().filter(|m| m.name == name).cloned().collect();
        (class, members)
    }

    /// Nested class `name` of `outer`.
    pub fn nested_class(&self, outer: &LibClass, name: &str) -> Option<Arc<LibClass>> {
        self.find_class(&format!("{}.{name}", outer.symbol.fq_name))
    }

    /// All indexed top-level class FQ names (for workspace symbols / auto-import).
    pub fn all_classes(&self) -> impl Iterator<Item = String> + '_ {
        self.packages
            .values()
            .flatten()
            .filter(|i| !is_anonymous(i))
            .map(|i| signature::internal_to_fq(i))
            .chain(self.builtin_classes.keys().cloned())
    }
}

fn is_facade(c: &LibClass) -> bool {
    c.symbol.kind == SymbolKind::Package
}

/// `kotlin.Function3`, `kotlin.coroutines.SuspendFunction1`... are synthesized by the compiler.
fn synthetic_function_class(fq: &str) -> Option<Arc<LibClass>> {
    let (n, suspend, reflect) = if let Some(n) = fq.strip_prefix("kotlin.Function") {
        (n, false, false)
    } else if let Some(n) = fq.strip_prefix("kotlin.coroutines.SuspendFunction") {
        (n, true, false)
    } else if let Some(n) = fq.strip_prefix("kotlin.reflect.KFunction") {
        (n, false, true)
    } else {
        let n = fq.strip_prefix("kotlin.reflect.KSuspendFunction")?;
        (n, true, true)
    };
    let n: usize = n.parse().ok().filter(|n| *n <= 22)?;
    let mut sym = Symbol::new(SymbolKind::Interface, fq.rsplit('.').next().unwrap(), fq.to_string(), Origin::Builtin);
    for i in 1..=n {
        sym.type_params.push(TypeParam { name: format!("P{i}"), bounds: vec![], variance: Variance::In, reified: false });
    }
    sym.type_params.push(TypeParam { name: "R".into(), bounds: vec![], variance: Variance::Out, reified: false });
    let r = || TypeArg::Type { variance: Variance::Out, ty: TypeRef::TypeParam { name: "R".into(), nullable: false } };
    sym.supertypes.push(TypeRef::with_args("kotlin.Function", vec![r()]));
    if reflect {
        sym.supertypes.push(TypeRef::with_args("kotlin.reflect.KFunction", vec![r()]));
    }
    let mut invoke = Symbol::new(SymbolKind::Function, "invoke", format!("{fq}.invoke"), Origin::Builtin);
    invoke.modifiers.set(Modifiers::OPERATOR);
    if suspend {
        invoke.modifiers.set(Modifiers::SUSPEND);
    }
    invoke.params = (1..=n)
        .map(|i| Param { name: format!("p{i}"), ty: TypeRef::TypeParam { name: format!("P{i}"), nullable: false }, has_default: false, vararg: false })
        .collect();
    invoke.ty = Some(TypeRef::TypeParam { name: "R".into(), nullable: false });
    Some(Arc::new(LibClass { internal: fq.replace('.', "/"), symbol: sym, members: vec![invoke], nested: vec![], companion: None, is_kotlin: true }))
}

// ------------------------------------------------------- class conversion

const NULLABLE: &[&str] = &[
    "Lorg/jetbrains/annotations/Nullable;",
    "Ljavax/annotation/Nullable;",
    "Ljavax/annotation/CheckForNull;",
    "Landroidx/annotation/Nullable;",
    "Landroid/support/annotation/Nullable;",
    "Lorg/jspecify/annotations/Nullable;",
    "Lorg/checkerframework/checker/nullness/qual/Nullable;",
    "Ledu/umd/cs/findbugs/annotations/Nullable;",
    "Lio/reactivex/annotations/Nullable;",
    "Lorg/eclipse/jdt/annotation/Nullable;",
    "Llombok/Nullable;",
];

const NOT_NULL: &[&str] = &[
    "Lorg/jetbrains/annotations/NotNull;",
    "Ljavax/annotation/Nonnull;",
    "Landroidx/annotation/NonNull;",
    "Landroid/support/annotation/NonNull;",
    "Lorg/jspecify/annotations/NonNull;",
    "Lorg/checkerframework/checker/nullness/qual/NonNull;",
    "Ledu/umd/cs/findbugs/annotations/NonNull;",
    "Lio/reactivex/annotations/NonNull;",
    "Lorg/eclipse/jdt/annotation/NonNull;",
    "Llombok/NonNull;",
];

/// Apply nullability annotations to a Java (platform) type.
fn with_nullability(t: TypeRef, annotations: &[String]) -> TypeRef {
    if annotations.iter().any(|a| NULLABLE.contains(&a.as_str())) {
        match t {
            TypeRef::Platform(inner) => inner.nullable(true),
            other => other,
        }
    } else if annotations.iter().any(|a| NOT_NULL.contains(&a.as_str())) {
        match t {
            TypeRef::Platform(inner) => *inner,
            other => other,
        }
    } else {
        t
    }
}

fn java_visibility(access: u16) -> Visibility {
    if access & ACC_PUBLIC != 0 {
        Visibility::Public
    } else if access & ACC_PROTECTED != 0 {
        Visibility::Protected
    } else if access & ACC_PRIVATE != 0 {
        Visibility::Private
    } else {
        Visibility::PackagePrivate
    }
}

pub fn convert_class(cf: &ClassFile, archive: &Path) -> LibClass {
    let origin = Origin::Library { archive: archive.to_path_buf(), class: cf.name.clone() };
    let fq = signature::internal_to_fq(&cf.name);
    let package = cf.name.rsplit_once('/').map(|(p, _)| p.replace('/', ".")).unwrap_or_default();
    if let Some(meta) = &cf.kotlin {
        match kmeta::decode(meta, &package, archive, &cf.name) {
            Some(kmeta::Decoded::Class(k)) => {
                let mut symbol = k.symbol;
                symbol.origin = origin.clone();
                if cf.deprecated {
                    symbol.modifiers.set(Modifiers::DEPRECATED);
                }
                return LibClass { internal: cf.name.clone(), symbol, members: k.members, nested: k.nested, companion: k.companion, is_kotlin: true };
            }
            Some(_) => {
                // File facade / synthetic: not a class from Kotlin's point of view.
                let symbol = Symbol::new(SymbolKind::Package, &package, package.clone(), origin);
                return LibClass { internal: cf.name.clone(), symbol, members: Vec::new(), nested: Vec::new(), companion: None, is_kotlin: true };
            }
            None => {}
        }
    }
    let inner = cf.inner_info();
    let access = inner.map(|i| i.access).unwrap_or(cf.access);
    let simple = inner.and_then(|i| i.simple_name.clone()).unwrap_or_else(|| fq.rsplit('.').next().unwrap_or(&fq).to_string());
    let kind = if access & ACC_ANNOTATION != 0 {
        SymbolKind::Annotation
    } else if access & ACC_INTERFACE != 0 {
        SymbolKind::Interface
    } else if access & ACC_ENUM != 0 {
        SymbolKind::Enum
    } else {
        SymbolKind::Class
    };
    let mut symbol = Symbol::new(kind, &simple, fq.clone(), origin.clone());
    symbol.visibility = java_visibility(access);
    if access & ACC_ABSTRACT != 0 && kind == SymbolKind::Class {
        symbol.modifiers.set(Modifiers::ABSTRACT);
    } else if access & ACC_FINAL == 0 && kind == SymbolKind::Class {
        symbol.modifiers.set(Modifiers::OPEN);
    }
    if inner.is_some() && access & ACC_STATIC == 0 && kind == SymbolKind::Class {
        symbol.modifiers.set(Modifiers::INNER);
    }
    if cf.deprecated {
        symbol.modifiers.set(Modifiers::DEPRECATED);
    }
    match &cf.signature {
        Some(sig) => {
            let (tps, supers) = signature::class(sig);
            symbol.type_params = tps;
            symbol.supertypes = supers.into_iter().map(strip_platform).collect();
        }
        None => {
            symbol.supertypes = cf
                .super_name
                .iter()
                .chain(cf.interfaces.iter())
                .map(|i| strip_platform(signature::field(&format!("L{i};"))))
                .collect();
        }
    }
    symbol.supertypes.retain(|t| !matches!(t.fq_name(), Some("kotlin.Any") | Some("java.lang.Record") | Some("kotlin.Annotation")));

    let mut members = Vec::new();
    for m in &cf.methods {
        if m.access & (ACC_SYNTHETIC | ACC_BRIDGE) != 0 || m.name == "<clinit>" || m.access & ACC_PRIVATE != 0 {
            continue;
        }
        if kind == SymbolKind::Enum && matches!(m.name.as_str(), "values" | "valueOf") && m.access & ACC_STATIC != 0 {
            continue;
        }
        let is_ctor = m.name == "<init>";
        let sig = signature::method(m.signature.as_deref().unwrap_or(&m.descriptor));
        // Inner class constructors take the outer instance first in the descriptor.
        let mut ptypes = sig.params;
        if is_ctor && m.signature.is_none() && symbol.modifiers.has(Modifiers::INNER) && !ptypes.is_empty() {
            ptypes.remove(0);
        }
        let n = ptypes.len();
        let mut s = Symbol::new(
            if is_ctor { SymbolKind::Constructor } else { SymbolKind::Function },
            &m.name,
            format!("{fq}.{}", m.name),
            origin.clone(),
        );
        s.visibility = java_visibility(m.access);
        s.type_params = sig.type_params;
        s.params = ptypes
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let anns = m.param_annotations.get(i).cloned().unwrap_or_default();
                let vararg = m.access & ACC_VARARGS != 0 && i + 1 == n;
                let ty = with_nullability(t, &anns);
                // vararg arrays are spelled as their element type in Kotlin.
                let ty = if vararg { vararg_element(ty) } else { ty };
                Param { name: m.param_names.get(i).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("p{i}")), ty, has_default: false, vararg }
            })
            .collect();
        let anns: Vec<String> = m.annotations.iter().map(|a| a.type_desc.clone()).collect();
        s.ty = Some(if is_ctor { TypeRef::named(&fq) } else { with_nullability(sig.ret, &anns) });
        if m.access & ACC_STATIC != 0 {
            s.modifiers.set(Modifiers::STATIC);
        }
        if m.access & ACC_ABSTRACT != 0 {
            s.modifiers.set(Modifiers::ABSTRACT);
        }
        if m.deprecated {
            s.modifiers.set(Modifiers::DEPRECATED);
        }
        members.push(s);
    }
    for f in &cf.fields {
        if f.access & (ACC_SYNTHETIC | ACC_PRIVATE) != 0 {
            continue;
        }
        let ty = signature::field(f.signature.as_deref().unwrap_or(&f.descriptor));
        let anns: Vec<String> = f.annotations.iter().map(|a| a.type_desc.clone()).collect();
        let is_entry = f.access & ACC_ENUM != 0;
        let mut s = Symbol::new(if is_entry { SymbolKind::EnumEntry } else { SymbolKind::Field }, &f.name, format!("{fq}.{}", f.name), origin.clone());
        s.visibility = java_visibility(f.access);
        s.ty = Some(if is_entry { TypeRef::named(&fq) } else { with_nullability(ty, &anns) });
        if f.access & ACC_STATIC != 0 {
            s.modifiers.set(Modifiers::STATIC);
        }
        if f.access & ACC_FINAL == 0 {
            s.modifiers.set(Modifiers::VAR);
        }
        if f.deprecated {
            s.modifiers.set(Modifiers::DEPRECATED);
        }
        members.push(s);
    }
    let nested = cf
        .inner_classes
        .iter()
        .filter(|ic| ic.outer.as_deref() == Some(cf.name.as_str()) && ic.access & ACC_PRIVATE == 0 && ic.access & ACC_SYNTHETIC == 0)
        .filter_map(|ic| ic.simple_name.clone())
        .collect();
    LibClass { internal: cf.name.clone(), symbol, members, nested, companion: None, is_kotlin: false }
}

fn strip_platform(t: TypeRef) -> TypeRef {
    match t {
        TypeRef::Platform(inner) => *inner,
        other => other,
    }
}

fn vararg_element(t: TypeRef) -> TypeRef {
    let inner = strip_platform(t);
    match &inner {
        TypeRef::Named { fq: Some(fq), args, .. } if &**fq == "kotlin.Array" => match args.first() {
            Some(TypeArg::Type { ty, .. }) => ty.clone(),
            _ => inner,
        },
        TypeRef::Named { fq: Some(fq), .. } => {
            let elem = match &**fq {
                "kotlin.IntArray" => "kotlin.Int",
                "kotlin.LongArray" => "kotlin.Long",
                "kotlin.ByteArray" => "kotlin.Byte",
                "kotlin.ShortArray" => "kotlin.Short",
                "kotlin.CharArray" => "kotlin.Char",
                "kotlin.FloatArray" => "kotlin.Float",
                "kotlin.DoubleArray" => "kotlin.Double",
                "kotlin.BooleanArray" => "kotlin.Boolean",
                _ => return inner,
            };
            TypeRef::named(elem)
        }
        _ => inner,
    }
}

#[cfg(test)]
mod tests;
