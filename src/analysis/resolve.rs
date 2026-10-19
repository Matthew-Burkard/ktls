//! Name resolution for imports and type references.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::source::{FileSummary, Import, Language};
use super::{FileContext, Snapshot};
use crate::classpath::{ClasspathIndex, LibClass};
use crate::symbols::{Symbol, SymbolKind, TypeRef};

/// Default imports of every Kotlin/JVM file.
pub const KOTLIN_DEFAULT_IMPORTS: &[&str] = &[
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "java.lang",
    "kotlin.jvm",
];

/// A resolved classifier.
#[derive(Clone, Debug)]
pub enum Classifier {
    /// Declared in a source file.
    Source(Symbol),
    Library(Arc<LibClass>),
    /// A library type alias.
    LibraryAlias(Symbol),
}

impl Classifier {
    pub fn symbol(&self) -> &Symbol {
        match self {
            Classifier::Source(s) | Classifier::LibraryAlias(s) => s,
            Classifier::Library(c) => &c.symbol,
        }
    }

    pub fn fq_name(&self) -> &str {
        &self.symbol().fq_name
    }
}

/// Result of resolving a dotted name.
#[derive(Clone, Debug)]
pub enum Resolved {
    Classifier(Classifier),
    TypeParameter(String),
    /// A classifier declared locally (in a function body), which is not indexed.
    Local(String),
    Package(String),
    /// Top-level/member callables (imports of functions, properties, enum entries...).
    Callables(Vec<Symbol>),
}

/// Type-level scope at a position in a file.
#[derive(Clone, Debug, Default)]
pub struct TypeScope {
    pub type_params: Vec<String>,
    /// FQ names of enclosing classes, outermost first.
    pub enclosing: Vec<String>,
    /// Classifiers declared in enclosing function bodies.
    pub locals: Vec<String>,
    /// Supertype names (as written) of enclosing classes and object literals, innermost
    /// first: their nested classifiers are in scope too.
    pub supertypes: Vec<Vec<String>>,
}

pub struct Resolver<'s> {
    snap: &'s Snapshot,
    pub ctx: Option<FileContext>,
    visible: HashSet<FileContext>,
    pub classpath: Arc<ClasspathIndex>,
    summary: Option<Arc<FileSummary>>,
    path: PathBuf,
}

thread_local! {
    /// Bounds mutual recursion through supertypes declared in other files.
    static HIERARCHY_DEPTH: Cell<u32> = const { Cell::new(0) };
}

impl<'s> Resolver<'s> {
    pub fn new(snap: &'s Snapshot, path: &Path) -> Self {
        let ctx = snap.context_of(path);
        let visible = ctx.map(|c| snap.workspace.visible_contexts(c)).unwrap_or_default();
        let classpath = snap.workspace.classpath(ctx);
        let summary = snap.file(path).map(|f| f.summary());
        Resolver { snap, ctx, visible, classpath, summary, path: path.to_path_buf() }
    }

    fn is_visible(&self, other: Option<FileContext>) -> bool {
        match (self.ctx, other) {
            (Some(_), Some(o)) => self.visible.contains(&o),
            // Files outside any module only see themselves.
            _ => false,
        }
    }

    fn visible_fn(&self) -> impl Fn(Option<FileContext>) -> bool + '_ {
        move |c| self.is_visible(c)
    }

    pub fn summary(&self) -> Option<&FileSummary> {
        self.summary.as_deref()
    }

    /// Declarations of this file are always visible to itself.
    fn own_classifier(&self, fq: &str) -> Option<Symbol> {
        self.summary()?.classifiers().into_iter().find(|d| d.symbol.fq_name == fq).map(|d| d.symbol.clone())
    }

    pub fn find_classifier(&self, fq: &str) -> Option<Classifier> {
        if let Some(s) = self.own_classifier(fq) {
            return Some(Classifier::Source(s));
        }
        let visible = self.visible_fn();
        if let Some(loc) = self.snap.index.classifier(fq, &visible).next()
            && let Some(f) = self.snap.file(&loc.path) {
                let s = f.summary();
                if let Some(d) = loc.resolve(&s) {
                    return Some(Classifier::Source(d.symbol.clone()));
                }
            }
        if let Some(c) = self.classpath.find_class(fq) {
            return Some(Classifier::Library(c));
        }
        // Library type aliases live among package members.
        let (pkg, name) = fq.rsplit_once('.').unwrap_or(("", fq));
        self.classpath
            .package_members(pkg)
            .iter()
            .find(|m| m.kind == SymbolKind::TypeAlias && m.name == name)
            .map(|s| Classifier::LibraryAlias(s.clone()))
    }

    pub fn package_exists(&self, pkg: &str) -> bool {
        let visible = self.visible_fn();
        self.summary().is_some_and(|s| s.package == pkg || s.package.starts_with(&format!("{pkg}.")))
            || self.snap.index.has_package(pkg, &visible)
            || self.classpath.has_package(pkg)
    }

    /// Top-level callables named `name` in `pkg` (sources + libraries).
    pub fn top_level_callables(&self, pkg: &str, name: &str) -> Vec<Symbol> {
        let mut out: Vec<Symbol> = self.classpath.package_members(pkg).iter().filter(|m| m.name == name && m.kind != SymbolKind::TypeAlias).cloned().collect();
        let visible = self.visible_fn();
        let own = self.summary().filter(|s| s.package == pkg);
        let mut seen_paths = HashSet::new();
        for loc in self.snap.index.top_level(pkg, &visible) {
            seen_paths.insert(loc.path.clone());
            if let Some(f) = self.snap.file(&loc.path) {
                let s = f.summary();
                if let Some(d) = loc.resolve(&s)
                    && d.symbol.name == name && d.symbol.kind.is_callable() {
                        out.push(d.symbol.clone());
                    }
            }
        }
        if let Some(s) = own
            && !seen_paths.iter().any(|p| **p == *self.path.as_path()) {
                out.extend(s.decls.iter().filter(|d| d.symbol.name == name && d.symbol.kind.is_callable()).map(|d| d.symbol.clone()));
            }
        out
    }

    /// Members (callables + nested classifiers) of a classifier named `name`.
    fn member_named(&self, owner: &Classifier, name: &str) -> Option<Resolved> {
        let fq = format!("{}.{name}", owner.fq_name());
        if let Some(c) = self.find_classifier(&fq) {
            return Some(Resolved::Classifier(c));
        }
        let members: Vec<Symbol> = match owner {
            Classifier::Library(c) => c.members.iter().filter(|m| m.name == name).cloned().collect(),
            Classifier::Source(s) => self.source_members(s).into_iter().filter(|m| m.name == name).collect(),
            Classifier::LibraryAlias(_) => Vec::new(),
        };
        (!members.is_empty()).then_some(Resolved::Callables(members))
    }

    /// Member declarations of a source classifier.
    pub fn source_members(&self, s: &Symbol) -> Vec<Symbol> {
        let crate::symbols::Origin::Source { file, .. } = &s.origin else { return Vec::new() };
        let Some(f) = self.snap.file(file) else { return Vec::new() };
        let summary = f.summary();
        fn find<'a>(ds: &'a [super::source::SourceDecl], fq: &str) -> Option<&'a super::source::SourceDecl> {
            for d in ds {
                if d.symbol.fq_name == fq && d.symbol.kind.is_classifier() {
                    return Some(d);
                }
                if let Some(x) = find(&d.children, fq) {
                    return Some(x);
                }
            }
            None
        }
        find(&summary.decls, &s.fq_name).map(|d| d.children.iter().map(|c| c.symbol.clone()).collect()).unwrap_or_default()
    }

    /// Resolve an import directive. `Err(i)` = segment `i` is unresolved.
    pub fn resolve_import(&self, imp: &Import) -> Result<Resolved, usize> {
        let segs: Vec<&str> = imp.segments.iter().map(|(s, _)| s.as_str()).collect();
        if segs.is_empty() {
            return Err(0);
        }
        if imp.star && self.package_exists(&segs.join(".")) {
            return Ok(Resolved::Package(segs.join(".")));
        }
        // Try package prefixes from the longest; `a.b.c` may name both a package and a
        // function `c` in package `a.b`.
        let mut first_err = None;
        for pkg_len in (0..segs.len()).rev() {
            if pkg_len > 0 && !self.package_exists(&segs[..pkg_len].join(".")) {
                continue;
            }
            match self.resolve_import_from(imp, &segs, pkg_len) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        Err(first_err.unwrap_or(0))
    }

    fn resolve_import_from(&self, imp: &Import, segs: &[&str], pkg_len: usize) -> Result<Resolved, usize> {
        let pkg = segs[..pkg_len].join(".");
        let Some(first) = segs.get(pkg_len) else { return Err(segs.len().saturating_sub(1)) };
        let first_fq = if pkg.is_empty() { first.to_string() } else { format!("{pkg}.{first}") };
        let mut current = match self.find_classifier(&first_fq) {
            Some(c) => Resolved::Classifier(c),
            None => {
                // `import pkg.topLevelFunction`
                let callables = self.top_level_callables(&pkg, first);
                if callables.is_empty() || segs.len() > pkg_len + 1 || imp.star {
                    return Err(pkg_len);
                }
                return Ok(Resolved::Callables(callables));
            }
        };
        for (i, seg) in segs.iter().enumerate().skip(pkg_len + 1) {
            let Resolved::Classifier(c) = &current else { return Err(i) };
            current = match self.member_named(c, seg) {
                Some(r) => r,
                None => return Err(i),
            };
        }
        Ok(current)
    }

    /// Resolve the first segment of a type name.
    fn resolve_simple(&self, name: &str, scope: &TypeScope) -> Option<Resolved> {
        if scope.type_params.iter().any(|t| t == name) {
            return Some(Resolved::TypeParameter(name.to_string()));
        }
        if scope.locals.iter().any(|t| t == name) {
            return Some(Resolved::Local(name.to_string()));
        }
        // Enclosing classes (innermost first), their companions and supertypes.
        for outer in scope.enclosing.iter().rev() {
            if let Some(c) = self.nested_in_hierarchy(outer, name, &mut HashSet::new(), 0) {
                return Some(Resolved::Classifier(c));
            }
            if outer.rsplit('.').next() == Some(name)
                && let Some(c) = self.find_classifier(outer) {
                    return Some(Resolved::Classifier(c));
                }
        }
        // Nested classifiers inherited from supertypes of enclosing (possibly anonymous) classes.
        if !scope.supertypes.is_empty() {
            let outer = TypeScope { supertypes: Vec::new(), ..scope.clone() };
            for st in &scope.supertypes {
                let segs: Vec<&str> = st.iter().map(String::as_str).collect();
                if let Ok(Resolved::Classifier(c)) = self.resolve_type_name(&segs, &outer)
                    && let Some(n) = self.nested_in_hierarchy(c.fq_name(), name, &mut HashSet::new(), 0) {
                        return Some(Resolved::Classifier(n));
                    }
            }
        }
        let summary = self.summary();
        // Explicit imports.
        if let Some(s) = summary {
            for imp in &s.imports {
                if imp.imported_name() == Some(name)
                    && let Ok(Resolved::Classifier(c)) = self.resolve_import(imp) {
                        return Some(Resolved::Classifier(c));
                    }
            }
        }
        let package = summary.map(|s| s.package.as_str()).unwrap_or("");
        let in_pkg = |pkg: &str| {
            let fq = if pkg.is_empty() { name.to_string() } else { format!("{pkg}.{name}") };
            self.find_classifier(&fq)
        };
        if let Some(c) = in_pkg(package) {
            return Some(Resolved::Classifier(c));
        }
        // Star imports.
        if let Some(s) = summary {
            for imp in s.imports.iter().filter(|i| i.star) {
                if let Some(c) = in_pkg(&imp.path()) {
                    return Some(Resolved::Classifier(c));
                }
            }
        }
        let defaults: &[&str] = match summary.map(|s| s.language) {
            Some(Language::Java) => &["java.lang"],
            _ => KOTLIN_DEFAULT_IMPORTS,
        };
        for pkg in defaults {
            if let Some(c) = in_pkg(pkg) {
                return Some(Resolved::Classifier(c));
            }
        }
        if self.package_exists(name) {
            return Some(Resolved::Package(name.to_string()));
        }
        None
    }

    /// A nested classifier `name` in `owner` or in any of its supertypes.
    fn nested_in_hierarchy(&self, owner: &str, name: &str, seen: &mut HashSet<String>, depth: u32) -> Option<Classifier> {
        if depth > 6 || !seen.insert(owner.to_string()) || HIERARCHY_DEPTH.get() > 12 {
            return None;
        }
        HIERARCHY_DEPTH.set(HIERARCHY_DEPTH.get() + 1);
        let r = self.nested_in_hierarchy_inner(owner, name, seen, depth);
        HIERARCHY_DEPTH.set(HIERARCHY_DEPTH.get() - 1);
        r
    }

    fn nested_in_hierarchy_inner(&self, owner: &str, name: &str, seen: &mut HashSet<String>, depth: u32) -> Option<Classifier> {
        if let Some(c) = self.find_classifier(&format!("{owner}.{name}")) {
            return Some(c);
        }
        if let Some(c) = self.find_classifier(&format!("{owner}.Companion.{name}")) {
            return Some(c);
        }
        let cls = self.find_classifier(owner)?;
        let supertypes: Vec<TypeRef> = cls.symbol().supertypes.clone();
        // Source supertypes are unresolved names: resolve them in the declaring file.
        let owner_resolver = match &cls.symbol().origin {
            crate::symbols::Origin::Source { file, .. } if *file != self.path => Some(Resolver::new(self.snap, file)),
            _ => None,
        };
        let r = owner_resolver.as_ref().unwrap_or(self);
        for st in supertypes {
            let fq = match &st {
                TypeRef::Named { fq: Some(fq), .. } => Some(fq.to_string()),
                TypeRef::Named { name: n, .. } => match r.resolve_type_name(&n.split('.').collect::<Vec<_>>(), &TypeScope::default()) {
                    Ok(Resolved::Classifier(c)) => Some(c.fq_name().to_string()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(fq) = fq
                && let Some(c) = self.nested_in_hierarchy(&fq, name, seen, depth + 1) {
                    return Some(c);
                }
        }
        None
    }

    /// Resolve a dotted type name (`Map.Entry`, `java.util.List`). `Err(i)`: segment `i` unresolved.
    pub fn resolve_type_name(&self, segments: &[&str], scope: &TypeScope) -> Result<Resolved, usize> {
        let first = segments.first().ok_or(0usize)?;
        let mut current = self.resolve_simple(first, scope).ok_or(0usize)?;
        for (i, seg) in segments.iter().enumerate().skip(1) {
            current = match &current {
                Resolved::Package(p) => {
                    let fq = format!("{p}.{seg}");
                    match self.find_classifier(&fq) {
                        Some(c) => Resolved::Classifier(c),
                        None if self.package_exists(&fq) => Resolved::Package(fq),
                        None => return Err(i),
                    }
                }
                Resolved::Classifier(c) => match self.find_classifier(&format!("{}.{seg}", c.fq_name())) {
                    Some(n) => Resolved::Classifier(n),
                    None => match self.nested_in_hierarchy(c.fq_name(), seg, &mut HashSet::new(), 0) {
                        Some(n) => Resolved::Classifier(n),
                        None => return Err(i),
                    },
                },
                // Nested classes of local classes are not tracked.
                Resolved::Local(_) => return Ok(current),
                _ => return Err(i),
            };
        }
        match current {
            Resolved::Package(_) => Err(segments.len() - 1),
            r => Ok(r),
        }
    }
}
