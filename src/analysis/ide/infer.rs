//! Expression-level name resolution and approximate type inference.
//!
//! This is deliberately conservative: when a type cannot be determined with
//! reasonable confidence the result is `None` rather than a guess, so
//! navigation never jumps to an unrelated declaration.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::resolve::{Classifier, Resolved, Resolver, TypeScope, KOTLIN_DEFAULT_IMPORTS};
use crate::analysis::source::lower_type;
use crate::analysis::{FileData, Snapshot};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeArg, TypeRef, Variance};
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange};

pub type Subst = HashMap<String, TypeRef>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Val,
    Var,
    Parameter,
    LambdaParameter,
    /// The implicit `it` of a lambda.
    It,
    Function,
    Class,
}

/// A declaration that is not part of the project index (local to a body).
#[derive(Clone, Debug)]
pub struct LocalDef {
    pub name: String,
    pub kind: LocalKind,
    /// Where the name is declared (for `it`: the lambda's opening brace).
    pub name_range: TextRange,
    pub ty: Option<TypeRef>,
}

#[derive(Clone, Debug)]
pub enum Def {
    Symbol { sym: Symbol, subst: Subst },
    Local(LocalDef),
    Package(String),
}

impl Def {
    fn symbol(sym: Symbol) -> Def {
        Def::Symbol { sym, subst: Subst::new() }
    }
}

/// What the left side of a `.` denotes.
pub enum Qualifier {
    Value(TypeRef),
    Static(Classifier),
    Package(String),
    Unknown,
}

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII recursion guard shared by all engines on this thread.
struct Guard;

impl Guard {
    fn enter() -> Option<Guard> {
        let d = DEPTH.get();
        if d > 48 {
            return None;
        }
        DEPTH.set(d + 1);
        Some(Guard)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get() - 1);
    }
}

// ------------------------------------------------------------ type helpers

pub fn strip(t: &TypeRef) -> &TypeRef {
    match t {
        TypeRef::Platform(inner) => strip(inner),
        other => other,
    }
}

pub fn fq_of(t: &TypeRef) -> Option<&str> {
    match strip(t) {
        TypeRef::Named { fq: Some(fq), .. } => Some(fq),
        TypeRef::Function { .. } => None,
        _ => None,
    }
}

fn args_of(t: &TypeRef) -> &[TypeArg] {
    match strip(t) {
        TypeRef::Named { args, .. } => args,
        _ => &[],
    }
}

pub fn non_null(t: TypeRef) -> TypeRef {
    match t {
        TypeRef::Named { name, fq, args, .. } => TypeRef::Named { name, fq, args, nullable: false },
        TypeRef::Function { receiver, params, ret, suspend, .. } => TypeRef::Function { receiver, params, ret, suspend, nullable: false },
        TypeRef::TypeParam { name, .. } => TypeRef::TypeParam { name, nullable: false },
        TypeRef::Platform(inner) => non_null(*inner),
        other => other,
    }
}

fn arg_type(a: &TypeArg) -> TypeRef {
    match a {
        TypeArg::Type { ty, .. } => ty.clone(),
        TypeArg::Star => TypeRef::named("kotlin.Any").nullable(true),
    }
}

pub fn substitute(t: &TypeRef, s: &Subst) -> TypeRef {
    if s.is_empty() {
        return t.clone();
    }
    match t {
        TypeRef::TypeParam { name, nullable } => match s.get(name) {
            Some(r) => r.clone().nullable(*nullable),
            None => t.clone(),
        },
        TypeRef::Named { name, fq, args, nullable } => TypeRef::Named {
            name: name.clone(),
            fq: fq.clone(),
            args: args
                .iter()
                .map(|a| match a {
                    TypeArg::Star => TypeArg::Star,
                    TypeArg::Type { variance, ty } => TypeArg::Type { variance: *variance, ty: substitute(ty, s) },
                })
                .collect(),
            nullable: *nullable,
        },
        TypeRef::Function { receiver, params, ret, suspend, nullable } => TypeRef::Function {
            receiver: receiver.as_ref().map(|r| Box::new(substitute(r, s))),
            params: params.iter().map(|p| substitute(p, s)).collect(),
            ret: Box::new(substitute(ret, s)),
            suspend: *suspend,
            nullable: *nullable,
        },
        TypeRef::Platform(inner) => TypeRef::Platform(Box::new(substitute(inner, s))),
        other => other.clone(),
    }
}

pub fn class_type(sym: &Symbol) -> TypeRef {
    TypeRef::Named {
        name: sym.fq_name.clone(),
        fq: Some(sym.fq_name.as_str().into()),
        args: sym
            .type_params
            .iter()
            .map(|tp| TypeArg::Type { variance: Variance::Invariant, ty: TypeRef::TypeParam { name: tp.name.clone(), nullable: false } })
            .collect(),
        nullable: false,
    }
}

/// Kotlin `FunctionN` / function types as (receiver, params, return).
fn function_parts(t: &TypeRef) -> Option<(Option<TypeRef>, Vec<TypeRef>, TypeRef)> {
    match strip(t) {
        TypeRef::Function { receiver, params, ret, .. } => Some((receiver.as_deref().cloned(), params.clone(), (**ret).clone())),
        TypeRef::Named { fq: Some(fq), args, .. } if fq.starts_with("kotlin.Function") && !args.is_empty() => {
            let mut tys: Vec<TypeRef> = args.iter().map(arg_type).collect();
            let ret = tys.pop().unwrap();
            Some((None, tys, ret))
        }
        _ => None,
    }
}

/// Same member signature for override purposes (name, kind, arity).
fn same_member(a: &Symbol, b: &Symbol) -> bool {
    a.name == b.name
        && a.params.len() == b.params.len()
        && matches!(
            (a.kind, b.kind),
            (SymbolKind::Function, SymbolKind::Function) | (SymbolKind::Property | SymbolKind::Field, SymbolKind::Property | SymbolKind::Field)
        )
}

/// Bind type parameters in `pattern` (those in `vars`) by matching against `actual`.
fn unify(pattern: &TypeRef, actual: &TypeRef, vars: &HashSet<String>, s: &mut Subst, engine: &Engine) {
    let pattern = strip(pattern);
    let actual_s = strip(actual);
    match pattern {
        TypeRef::TypeParam { name, nullable } if vars.contains(name) => {
            if !s.contains_key(name) {
                let v = if *nullable { non_null(actual_s.clone()) } else { actual_s.clone() };
                if !matches!(v, TypeRef::Error) {
                    s.insert(name.clone(), v);
                }
            }
        }
        TypeRef::Named { fq: Some(pfq), args: pargs, .. } => {
            if pargs.is_empty() {
                return;
            }
            let aargs: Vec<TypeArg> = if fq_of(actual_s) == Some(&**pfq) {
                args_of(actual_s).to_vec()
            } else {
                match engine.supertype_with(actual_s, pfq) {
                    Some(st) => args_of(&st).to_vec(),
                    None => return,
                }
            };
            for (p, a) in pargs.iter().zip(aargs.iter()) {
                if let (TypeArg::Type { ty: pt, .. }, TypeArg::Type { ty: at, .. }) = (p, a) {
                    unify(pt, at, vars, s, engine);
                }
            }
        }
        TypeRef::Function { receiver, params, ret, .. } => {
            if let Some((ar, ap, aret)) = function_parts(actual_s) {
                if let (Some(r), Some(a)) = (receiver, ar.as_ref()) {
                    unify(r, a, vars, s, engine);
                }
                for (p, a) in params.iter().zip(ap.iter()) {
                    unify(p, a, vars, s, engine);
                }
                unify(ret, &aret, vars, s, engine);
            }
        }
        _ => {}
    }
}

fn token_at(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    ast::token(node, kind)
}

fn first_expr_child(n: &SyntaxNode) -> Option<SyntaxNode> {
    n.children().next()
}

/// Number of arguments of a call (value arguments + trailing lambdas).
fn call_arity(call: &SyntaxNode) -> usize {
    let args = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).count()).unwrap_or(0);
    args + call.children().filter(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT).count()
}

fn arity_fits(sym: &Symbol, n: usize) -> bool {
    let total = sym.params.len();
    let required = sym.params.iter().filter(|p| !p.has_default && !p.vararg).count();
    let vararg = sym.params.iter().any(|p| p.vararg);
    n >= required && (n <= total || vararg)
}

// ----------------------------------------------------------------- engine

pub struct Engine<'s> {
    pub snap: &'s Snapshot,
    pub path: PathBuf,
    pub file: Arc<FileData>,
    pub res: Rc<Resolver<'s>>,
    pub root: SyntaxNode,
    resolvers: RefCell<HashMap<PathBuf, Rc<Resolver<'s>>>>,
    types: RefCell<HashMap<TextRange, Option<TypeRef>>>,
    locals_cache: RefCell<LocalsCache>,
    /// Anonymous object types of this file: synthetic FQ name → `OBJECT_LITERAL` range.
    anon: RefCell<HashMap<String, TextRange>>,
    /// Memoized supertype closures (member lookup recomputes them constantly).
    closures: RefCell<HashMap<TypeRef, Rc<Vec<TypeRef>>>>,
    /// Memoized members of classifiers, by FQ name.
    source_members: RefCell<HashMap<String, Rc<Vec<Symbol>>>>,
    /// Memoized explicitly imported callables, by imported name.
    imported: RefCell<HashMap<String, Vec<Symbol>>>,
    /// Memoized lambda signatures, by lambda range.
    lambdas: RefCell<HashMap<TextRange, Option<(Option<TypeRef>, Vec<TypeRef>)>>>,
}

impl<'s> Engine<'s> {
    pub fn new(snap: &'s Snapshot, path: &Path) -> Option<Engine<'s>> {
        let file = snap.file(path)?.clone();
        let res = Rc::new(Resolver::new(snap, path));
        let root = file.parse().syntax();
        Some(Engine {
            snap,
            path: path.to_path_buf(),
            file,
            res,
            root,
            resolvers: RefCell::new(HashMap::new()),
            types: RefCell::new(HashMap::new()),
            locals_cache: RefCell::new(LocalsCache::default()),
            anon: RefCell::new(HashMap::new()),
            closures: RefCell::new(HashMap::new()),
            source_members: RefCell::new(HashMap::new()),
            imported: RefCell::new(HashMap::new()),
            lambdas: RefCell::new(HashMap::new()),
        })
    }

    pub fn package(&self) -> String {
        self.res.summary().map(|s| s.package.clone()).unwrap_or_default()
    }

    fn resolver_for(&self, path: &Path) -> Rc<Resolver<'s>> {
        if path == self.path {
            return self.res.clone();
        }
        self.resolvers.borrow_mut().entry(path.to_path_buf()).or_insert_with(|| Rc::new(Resolver::new(self.snap, path))).clone()
    }

    // ---------------------------------------------------- declared types

    /// Scope used to resolve the declared types of a source symbol.
    fn symbol_scope(&self, sym: &Symbol, r: &Resolver) -> TypeScope {
        let package = r.summary().map(|s| s.package.clone()).unwrap_or_default();
        let rest = sym.fq_name.strip_prefix(&package).map(|s| s.trim_start_matches('.')).unwrap_or(&sym.fq_name);
        let mut enclosing = Vec::new();
        let mut fq = package.clone();
        let parts: Vec<&str> = rest.split('.').collect();
        let n = if sym.kind.is_classifier() { parts.len() } else { parts.len().saturating_sub(1) };
        for p in &parts[..n] {
            fq = if fq.is_empty() { p.to_string() } else { format!("{fq}.{p}") };
            enclosing.push(fq.clone());
        }
        TypeScope { type_params: sym.type_params.iter().map(|t| t.name.clone()).collect(), enclosing, ..Default::default() }
    }

    /// Resolve the names in a type declared by `sym` (no-op for library types).
    pub fn resolve_declared(&self, sym: &Symbol, t: &TypeRef) -> TypeRef {
        let Origin::Source { file, .. } = &sym.origin else { return t.clone() };
        let r = self.resolver_for(file);
        let scope = self.symbol_scope(sym, &r);
        resolve_names(t, &r, &scope)
    }

    /// Resolve a syntactic type at its position in the current file.
    pub fn resolve_syntax_type(&self, t: &ast::TypeRef) -> TypeRef {
        let scope = scope_at(t.syntax(), &self.package(), &mut self.locals_cache.borrow_mut());
        let lowered = lower_type(t, &scope.type_params);
        resolve_names(&lowered, &self.res, &scope)
    }

    /// The type of a declaration's value (property type / function return type).
    pub fn symbol_type(&self, sym: &Symbol) -> Option<TypeRef> {
        let _g = Guard::enter()?;
        match sym.kind {
            SymbolKind::Object | SymbolKind::Class | SymbolKind::Enum | SymbolKind::Interface => return Some(class_type(sym)),
            _ => {}
        }
        if let Some(t) = &sym.ty {
            return Some(self.resolve_declared(sym, t));
        }
        // Not declared: infer from the initializer / expression body in its file.
        let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
        let engine;
        let e: &Engine = if *file == self.path {
            self
        } else {
            engine = Engine::new(self.snap, file)?;
            &engine
        };
        let tok = e.root.token_at_offset(name_range.start()).right_biased()?;
        let decl = tok.parent()?;
        e.initializer_type(&decl)
    }

    /// Type of `val x = <expr>` / `fun f() = <expr>`.
    pub fn initializer_type(&self, decl: &SyntaxNode) -> Option<TypeRef> {
        let eq = token_at(decl, SyntaxKind::EQ)?;
        let expr = decl.children().find(|c| c.text_range().start() >= eq.text_range().end())?;
        self.type_of(&expr)
    }

    // ---------------------------------------------------- class members

    // ------------------------------------------------- anonymous objects

    /// The synthetic type of an `object : T { ... }` expression in this file.
    pub fn anon_type(&self, literal: &SyntaxNode) -> TypeRef {
        let fq = format!("anon_{}_{}", crate::util::short_hash(&self.path.display().to_string()), u32::from(literal.text_range().start()));
        self.anon.borrow_mut().insert(fq.clone(), literal.text_range());
        TypeRef::named(&fq)
    }

    fn anon_node(&self, fq: &str) -> Option<SyntaxNode> {
        let range = *self.anon.borrow().get(fq)?;
        let el = crate::syntax::covering(&self.root, range)?;
        let node = match el {
            rowan::NodeOrToken::Node(n) => n,
            rowan::NodeOrToken::Token(t) => t.parent()?,
        };
        node.ancestors().find(|n| n.kind() == SyntaxKind::OBJECT_LITERAL && n.text_range() == range)
    }

    fn anon_supertypes(&self, fq: &str) -> Option<Vec<TypeRef>> {
        let node = self.anon_node(fq)?;
        let list = node.children().find(|c| c.kind() == SyntaxKind::SUPER_TYPE_LIST);
        let mut out: Vec<TypeRef> = list.into_iter().flat_map(|l| l.children().collect::<Vec<_>>()).filter_map(|e| ast::child::<ast::TypeRef>(&e)).map(|t| self.resolve_syntax_type(&t)).collect();
        if out.is_empty() {
            out.push(TypeRef::named("kotlin.Any"));
        }
        Some(out)
    }

    fn anon_members(&self, fq: &str) -> Option<Vec<Symbol>> {
        let node = self.anon_node(fq)?;
        let body = ast::child::<ast::ClassBody>(&node)?;
        Some(crate::analysis::source::lower_body_members(&self.path, &body, fq).into_iter().map(|d| d.symbol).collect())
    }

    /// The anonymous-object member declared at `name_range` (if any).
    pub fn anon_member_at(&self, decl: &SyntaxNode, name_range: TextRange) -> Option<Symbol> {
        let literal = decl.parent().filter(|p| p.kind() == SyntaxKind::CLASS_BODY)?.parent().filter(|p| p.kind() == SyntaxKind::OBJECT_LITERAL)?;
        let fq = fq_of(&self.anon_type(&literal))?.to_string();
        self.anon_members(&fq)?.into_iter().find(|m| matches!(&m.origin, Origin::Source { name_range: r, .. } if *r == name_range))
    }

    fn members_of(&self, c: &Classifier) -> Vec<Symbol> {
        self.members_ref(c).to_vec()
    }

    /// Members of a classifier (shared, computed once per classifier).
    fn members_ref(&self, c: &Classifier) -> Rc<Vec<Symbol>> {
        let fq = &c.symbol().fq_name;
        if let Some(m) = self.source_members.borrow().get(fq) {
            return m.clone();
        }
        let m = match c {
            Classifier::Library(lc) => {
                let mut m = lc.members.clone();
                if lc.is_kotlin && lc.symbol.kind == SymbolKind::Enum {
                    m.extend(enum_statics(&lc.symbol, &m));
                }
                m
            }
            Classifier::Source(s) => {
                let Origin::Source { file, .. } = &s.origin else { return Rc::new(Vec::new()) };
                let mut m = self.resolver_for(file).source_members(s);
                m.extend(synthesized_members(s, &m));
                m
            }
            Classifier::LibraryAlias(_) => Vec::new(),
        };
        let m = Rc::new(m);
        self.source_members.borrow_mut().insert(fq.clone(), m.clone());
        m
    }

    /// The topmost declaration (among project sources) that `sym` overrides, and the FQ
    /// name of its declaring class. Library supertypes are not followed, so overriding
    /// `toString()` does not make every `toString` call a reference.
    pub fn override_root(&self, sym: &Symbol) -> Option<(Symbol, String)> {
        let overridable = matches!(sym.kind, SymbolKind::Function | SymbolKind::Property)
            && matches!(sym.origin, Origin::Source { .. })
            && sym.visibility != crate::symbols::Visibility::Private;
        if !overridable {
            return None;
        }
        let container = sym.fq_name.rsplit_once('.')?.0.to_string();
        let mut top = (sym.clone(), container.clone());
        if sym.modifiers.has(Modifiers::OVERRIDE) {
            for st in self.supertype_closure(&TypeRef::named(&container)).into_iter().skip(1) {
                let Some(fq) = fq_of(&st) else { continue };
                let Some((c @ Classifier::Source(_), _)) = self.classifier_for(fq) else { continue };
                if let Some(m) = self.members_of(&c).into_iter().find(|m| same_member(m, sym)) {
                    top = (m, fq.to_string());
                }
            }
        }
        Some(top)
    }

    /// Does `sym` override `root` (declared in class `root_class`), as seen from this file?
    pub fn overrides(&self, sym: &Symbol, root: &Symbol, root_class: &str) -> bool {
        if !sym.modifiers.has(Modifiers::OVERRIDE) || !same_member(sym, root) {
            return false;
        }
        let Some((container, _)) = sym.fq_name.rsplit_once('.') else { return false };
        self.supertype_closure(&TypeRef::named(container)).iter().skip(1).any(|st| fq_of(st) == Some(root_class))
    }

    /// Follow type aliases to a class.
    fn classifier_for(&self, fq: &str) -> Option<(Classifier, Vec<TypeArg>)> {
        let c = self.res.find_classifier(fq)?;
        match &c {
            Classifier::LibraryAlias(a) | Classifier::Source(a) if a.kind == SymbolKind::TypeAlias => {
                let target = self.resolve_declared(a, a.ty.as_ref()?);
                let tfq = fq_of(&target)?.to_string();
                let cls = self.res.find_classifier(&tfq)?;
                Some((cls, args_of(&target).to_vec()))
            }
            _ => Some((c, Vec::new())),
        }
    }

    /// The class a type alias stands for (`fq` itself otherwise).
    fn expand_alias(&self, fq: &str) -> String {
        match self.res.find_classifier(fq) {
            Some(Classifier::LibraryAlias(a)) | Some(Classifier::Source(a)) if a.kind == SymbolKind::TypeAlias => {
                a.ty.as_ref().map(|t| self.resolve_declared(&a, t)).and_then(|t| fq_of(&t).map(str::to_string)).unwrap_or_else(|| fq.to_string())
            }
            _ => fq.to_string(),
        }
    }

    fn type_subst(&self, c: &Classifier, args: &[TypeArg]) -> Subst {
        c.symbol().type_params.iter().zip(args.iter()).map(|(tp, a)| (tp.name.clone(), arg_type(a))).collect()
    }

    fn supertypes_of(&self, c: &Classifier, s: &Subst) -> Vec<TypeRef> {
        let sym = c.symbol();
        let mut out: Vec<TypeRef> = sym.supertypes.iter().map(|t| substitute(&self.resolve_declared(sym, t), s)).collect();
        // Enums extend `Enum<E>` even when they implement interfaces.
        if sym.kind == SymbolKind::Enum && !out.iter().any(|t| fq_of(t) == Some("kotlin.Enum")) {
            out.insert(0, TypeRef::with_args("kotlin.Enum", vec![TypeArg::Type { variance: crate::symbols::Variance::Invariant, ty: class_type(sym) }]));
        }
        if out.is_empty() && sym.fq_name != "kotlin.Any" {
            out.push(TypeRef::named("kotlin.Any"));
        }
        out
    }

    /// `t` and all its supertypes (with type arguments substituted), nearest first.
    pub fn supertype_closure(&self, t: &TypeRef) -> Vec<TypeRef> {
        if let Some(c) = self.closures.borrow().get(t) {
            return c.to_vec();
        }
        let out = self.compute_supertype_closure(t);
        self.closures.borrow_mut().insert(t.clone(), Rc::new(out.clone()));
        out
    }

    fn compute_supertype_closure(&self, t: &TypeRef) -> Vec<TypeRef> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: VecDeque<TypeRef> = VecDeque::from([non_null(strip(t).clone())]);
        while let Some(t) = queue.pop_front() {
            let Some(fq) = fq_of(&t).map(str::to_string) else { continue };
            if !seen.insert(fq.clone()) || out.len() > 64 {
                continue;
            }
            if let Some(sts) = self.anon_supertypes(&fq) {
                queue.extend(sts);
            } else if let Some((c, alias_args)) = self.classifier_for(&fq) {
                let args = if alias_args.is_empty() { args_of(&t).to_vec() } else { alias_args };
                let s = self.type_subst(&c, &args);
                for st in self.supertypes_of(&c, &s) {
                    queue.push_back(st);
                }
            }
            out.push(t);
        }
        out
    }

    fn supertype_with(&self, t: &TypeRef, fq: &str) -> Option<TypeRef> {
        self.supertype_closure(t).into_iter().find(|s| fq_of(s) == Some(fq))
    }

    /// Instance members named `name` of type `t` (nearest declaration first).
    pub fn members_named(&self, t: &TypeRef, name: &str) -> Vec<(Symbol, Subst)> {
        self.members_where(t, name, &|_| true)
    }

    /// Members that can be called: functions first anywhere in the hierarchy, and only
    /// then properties (which would need an `invoke`).
    pub fn callable_members(&self, t: &TypeRef, name: &str) -> Vec<(Symbol, Subst)> {
        let funs = self.members_where(t, name, &|m| m.kind == SymbolKind::Function);
        if funs.is_empty() { self.members_named(t, name) } else { funs }
    }

    fn members_where(&self, t: &TypeRef, name: &str, keep: &dyn Fn(&Symbol) -> bool) -> Vec<(Symbol, Subst)> {
        for st in self.supertype_closure(t) {
            let Some(fq) = fq_of(&st) else { continue };
            if let Some(members) = self.anon_members(fq) {
                let found: Vec<(Symbol, Subst)> = members.into_iter().filter(|m| m.name == name && keep(m)).map(|m| (m, Subst::new())).collect();
                if !found.is_empty() {
                    return found;
                }
                continue;
            }
            let Some((c, alias_args)) = self.classifier_for(fq) else { continue };
            let args = if alias_args.is_empty() { args_of(&st).to_vec() } else { alias_args };
            let s = self.type_subst(&c, &args);
            let found: Vec<(Symbol, Subst)> = self
                .members_ref(&c)
                .iter()
                .filter(|m| m.name == name && m.kind != SymbolKind::Constructor && !m.modifiers.has(Modifiers::STATIC) && keep(m))
                .map(|m| (m.clone(), s.clone()))
                .collect();
            if !found.is_empty() {
                return found;
            }
            // Java getters as Kotlin properties: `name` → getName()/isName().
            if let Classifier::Library(lc) = &c
                && !lc.is_kotlin && !name.is_empty() {
                    let cap: String = name[..1].to_uppercase() + &name[1..];
                    let getters: Vec<(Symbol, Subst)> = lc
                        .members
                        .iter()
                        .filter(|m| m.kind == SymbolKind::Function && m.params.is_empty() && (m.name == format!("get{cap}") || m.name == format!("is{cap}") || m.name == format!("is{}", cap)))
                        .map(|m| (m.clone(), s.clone()))
                        .collect();
                    if !getters.is_empty() && name.chars().next().is_some_and(|c| c.is_lowercase()) {
                        return getters;
                    }
                }
        }
        Vec::new()
    }

    /// Members accessible through a classifier name: nested classes, enum entries,
    /// object members, Java statics and companion members.
    fn static_members(&self, c: &Classifier, name: &str) -> Vec<Def> {
        let sym = c.symbol();
        if let Some(n) = self.res.find_classifier(&format!("{}.{name}", sym.fq_name)) {
            return vec![Def::symbol(n.symbol().clone())];
        }
        let own: Vec<Def> = self
            .members_ref(c)
            .iter()
            .filter(|m| m.name == name)
            .filter(|m| sym.kind == SymbolKind::Object || m.kind == SymbolKind::EnumEntry || m.modifiers.has(Modifiers::STATIC))
            .cloned()
            .map(Def::symbol)
            .collect();
        if !own.is_empty() {
            return own;
        }
        if sym.kind == SymbolKind::Object {
            return self.members_named(&class_type(sym), name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
        }
        let companion = match c {
            Classifier::Library(lc) => lc.companion.clone(),
            _ => None,
        }
        .unwrap_or_else(|| "Companion".into());
        if let Some(comp) = self.res.find_classifier(&format!("{}.{companion}", sym.fq_name)) {
            return self.members_named(&class_type(comp.symbol()), name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
        }
        Vec::new()
    }

    // -------------------------------------------------------- extensions

    /// Packages searched for top-level callables, in Kotlin's priority order.
    fn callable_packages(&self) -> Vec<String> {
        let mut out = vec![self.package()];
        if let Some(s) = self.res.summary() {
            out.extend(s.imports.iter().filter(|i| i.star).map(|i| i.path()));
        }
        out.extend(KOTLIN_DEFAULT_IMPORTS.iter().map(|s| s.to_string()));
        out
    }

    /// Callables imported explicitly under `name`.
    fn imported_callables(&self, name: &str) -> Vec<Symbol> {
        if let Some(hit) = self.imported.borrow().get(name) {
            return hit.clone();
        }
        let Some(s) = self.res.summary() else { return Vec::new() };
        let mut out = Vec::new();
        for imp in &s.imports {
            if imp.imported_name() == Some(name)
                && let Ok(Resolved::Callables(c)) = self.res.resolve_import(imp) {
                    out.extend(c);
                }
        }
        self.imported.borrow_mut().insert(name.to_string(), out.clone());
        out
    }

    fn top_level_named(&self, name: &str) -> Vec<Symbol> {
        let mut out = self.imported_callables(name);
        if !out.is_empty() {
            return out;
        }
        for pkg in self.callable_packages() {
            out.extend(self.res.top_level_callables(&pkg, name));
            if !out.is_empty() {
                return out;
            }
        }
        out
    }

    /// Extension callables `name` applicable to a receiver of type `t`.
    pub fn extensions(&self, t: &TypeRef, name: &str) -> Vec<(Symbol, Subst)> {
        let candidates: Vec<(Symbol, Subst)> = {
            let mut c = self.imported_callables(name);
            for pkg in self.callable_packages() {
                c.extend(self.res.top_level_callables(&pkg, name));
            }
            c.into_iter().filter(|s| s.receiver.is_some()).map(|s| (s, Subst::new())).collect()
        };
        self.applicable_extensions(candidates, t)
    }

    /// Member extensions: extension functions declared inside a class (e.g. Exposed's
    /// `Column<N>.autoIncrement()` in `Table`), usable where that class is an implicit receiver.
    pub fn member_extensions(&self, at: &SyntaxNode, t: &TypeRef, name: &str) -> Vec<(Symbol, Subst)> {
        let mut candidates = Vec::new();
        for (r, _) in self.implicit_receivers(at) {
            candidates.extend(self.members_named(&r, name).into_iter().filter(|(m, _)| m.receiver.is_some()));
            if !candidates.is_empty() {
                break;
            }
        }
        self.applicable_extensions(candidates, t)
    }

    /// Keep extension candidates whose receiver accepts `t`, most specific first.
    fn applicable_extensions(&self, candidates: Vec<(Symbol, Subst)>, t: &TypeRef) -> Vec<(Symbol, Subst)> {
        if candidates.is_empty() {
            return Vec::new();
        }
        let closure = self.supertype_closure(t);
        let mut out = Vec::new();
        for (sym, mut s) in candidates {
            let recv = substitute(&self.resolve_declared(&sym, sym.receiver.as_ref().unwrap()), &s);
            let vars: HashSet<String> = sym.type_params.iter().map(|t| t.name.clone()).collect();
            let ok = match strip(&recv) {
                TypeRef::TypeParam { name, .. } if vars.contains(name) => {
                    s.insert(name.clone(), non_null(strip(t).clone()));
                    true
                }
                TypeRef::Named { fq: Some(fq), .. } => {
                    // Receivers declared through a type alias (`kotlin.text.StringBuilder`).
                    let target = self.expand_alias(fq);
                    let fq: &str = fq;
                    fq == "kotlin.Any" || closure.iter().any(|c| fq_of(c) == Some(fq) || fq_of(c) == Some(target.as_str()))
                }
                _ => false,
            };
            if ok {
                unify(&recv, t, &vars, &mut s, self);
                // Distance of the receiver in the supertype closure: Kotlin picks the most specific.
                let rank = match fq_of(&recv) {
                    Some(fq) => closure.iter().position(|c| fq_of(c) == Some(fq)).unwrap_or(closure.len()),
                    None => closure.len() + 1,
                };
                out.push((rank, sym, s));
            }
        }
        out.sort_by_key(|(rank, _, _)| *rank);
        out.into_iter().map(|(_, sym, s)| (sym, s)).collect()
    }

    // ------------------------------------------------------------ locals

    fn local_in_block(&self, block: &SyntaxNode, name: &str, before: crate::syntax::TextSize) -> Option<LocalDef> {
        let stmts: Vec<SyntaxNode> = block.children().filter(|s| s.text_range().end() <= before).collect();
        for st in stmts.iter().rev() {
            match st.kind() {
                SyntaxKind::PROPERTY => {
                    if let Some(tok) = token_at(st, SyntaxKind::IDENT)
                        && ast::ident_text(&tok) == name {
                            let declared = ast::child::<ast::TypeRef>(st).map(|t| self.resolve_syntax_type(&t));
                            let kind = if token_at(st, SyntaxKind::VAR_KW).is_some() { LocalKind::Var } else { LocalKind::Val };
                            let ty = declared.or_else(|| self.initializer_type(st)).or_else(|| self.delegate_type(st));
                            return Some(LocalDef { name: name.into(), kind, name_range: tok.text_range(), ty });
                        }
                    if let Some(d) = st.children().find(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                        for (i, e) in d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).enumerate() {
                            if let Some(tok) = token_at(&e, SyntaxKind::IDENT)
                                && ast::ident_text(&tok) == name {
                                    let declared = ast::child::<ast::TypeRef>(&e).map(|t| self.resolve_syntax_type(&t));
                                    let ty = declared.or_else(|| {
                                        let init = self.initializer_type(st)?;
                                        self.component_type(&init, i)
                                    });
                                    return Some(LocalDef { name: name.into(), kind: LocalKind::Val, name_range: tok.text_range(), ty });
                                }
                        }
                    }
                }
                SyntaxKind::FUN => {
                    if let Some(tok) = token_at(st, SyntaxKind::IDENT)
                        && ast::ident_text(&tok) == name {
                            let ty = ast::FunDecl::cast(st.clone()).and_then(|f| f.return_type()).map(|t| self.resolve_syntax_type(&t));
                            return Some(LocalDef { name: name.into(), kind: LocalKind::Function, name_range: tok.text_range(), ty });
                        }
                }
                SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => {
                    if let Some(tok) = token_at(st, SyntaxKind::IDENT)
                        && ast::ident_text(&tok) == name {
                            return Some(LocalDef { name: name.into(), kind: LocalKind::Class, name_range: tok.text_range(), ty: None });
                        }
                }
                _ => {}
            }
        }
        None
    }

    fn delegate_type(&self, prop: &SyntaxNode) -> Option<TypeRef> {
        // `val x by lazy { ... }` → the lambda's result type (approximated by `getValue`'s type).
        let d = prop.children().find(|c| c.kind() == SyntaxKind::PROPERTY_DELEGATE)?;
        let t = self.type_of(&first_expr_child(&d)?)?;
        if fq_of(&t) == Some("kotlin.Lazy") {
            return args_of(&t).first().map(arg_type);
        }
        None
    }

    /// `componentN()` of a type (for destructuring).
    fn component_type(&self, t: &TypeRef, i: usize) -> Option<TypeRef> {
        let name = format!("component{}", i + 1);
        if let Some((sym, s)) = self.members_named(t, &name).into_iter().next().or_else(|| self.extensions(t, &name).into_iter().next()) {
            return Some(substitute(&self.symbol_type(&sym)?, &s));
        }
        // Source data classes: componentN are synthesized from the primary constructor.
        let (c, _) = self.classifier_for(fq_of(t)?)?;
        let s = self.type_subst(&c, args_of(t));
        let prop = self.data_component_property(t, i)?;
        Some(substitute(&self.symbol_type(&prop)?, &s))
    }

    fn param_def(&self, p: &SyntaxNode, name: &str, kind: LocalKind) -> Option<LocalDef> {
        let tok = token_at(p, SyntaxKind::IDENT)?;
        if ast::ident_text(&tok) != name {
            return None;
        }
        let mut ty = ast::child::<ast::TypeRef>(p).map(|t| self.resolve_syntax_type(&t));
        // `vararg xs: T` is an `Array<out T>` (or a primitive array) inside the function.
        if ast::ValueParameter::cast(p.clone()).is_some_and(|vp| vp.is_vararg()) {
            ty = ty.map(vararg_array);
        }
        Some(LocalDef { name: name.into(), kind, name_range: tok.text_range(), ty })
    }

    /// Local declarations visible at `at` (innermost first).
    pub fn find_local(&self, at: &SyntaxNode, name: &str) -> Option<LocalDef> {
        let before = at.text_range().start();
        for anc in at.ancestors().skip(1) {
            match anc.kind() {
                SyntaxKind::BLOCK => {
                    if let Some(d) = self.local_in_block(&anc, name, before) {
                        return Some(d);
                    }
                }
                SyntaxKind::LAMBDA_EXPR => {
                    let params = anc.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST);
                    let sig = || self.lambda_signature(&anc);
                    match params {
                        Some(list) => {
                            for (i, p) in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).enumerate() {
                                if let Some(mut d) = self.param_def(&p, name, LocalKind::LambdaParameter) {
                                    if d.ty.is_none() {
                                        d.ty = sig().and_then(|(_, ps)| ps.get(i).cloned());
                                    }
                                    return Some(d);
                                }
                                if let Some(dd) = p.children().find(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                                    for (j, e) in dd.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).enumerate() {
                                        if let Some(tok) = token_at(&e, SyntaxKind::IDENT)
                                            && ast::ident_text(&tok) == name {
                                                let ty = sig().and_then(|(_, ps)| ps.get(i).cloned()).and_then(|t| self.component_type(&t, j));
                                                return Some(LocalDef { name: name.into(), kind: LocalKind::LambdaParameter, name_range: tok.text_range(), ty });
                                            }
                                    }
                                }
                            }
                        }
                        None if name == "it" => {
                            let ty = sig().and_then(|(_, ps)| ps.first().cloned());
                            let brace = anc.first_token().map(|t| t.text_range()).unwrap_or(anc.text_range());
                            return Some(LocalDef { name: "it".into(), kind: LocalKind::It, name_range: brace, ty });
                        }
                        None => {}
                    }
                }
                SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::PROPERTY_ACCESSOR => {
                    if let Some(list) = anc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) {
                        for p in list.children() {
                            if let Some(d) = self.param_def(&p, name, LocalKind::Parameter) {
                                return Some(d);
                            }
                        }
                    }
                    if anc.kind() == SyntaxKind::PROPERTY_ACCESSOR && name == "field" {
                        let prop = anc.parent()?;
                        let ty = ast::child::<ast::TypeRef>(&prop).map(|t| self.resolve_syntax_type(&t));
                        let tok = token_at(&prop, SyntaxKind::IDENT)?;
                        return Some(LocalDef { name: "field".into(), kind: LocalKind::Var, name_range: tok.text_range(), ty });
                    }
                }
                SyntaxKind::FOR_EXPR => {
                    let in_body = anc.children().find(|c| c.kind() == SyntaxKind::BODY).is_some_and(|b| b.text_range().contains_range(at.text_range()));
                    if in_body
                        && let Some(p) = anc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) {
                            let elem = || {
                                let range = anc.children().find(|c| c.kind() == SyntaxKind::LOOP_RANGE)?.children().next()?;
                                let rt = self.type_of(&range)?;
                                self.element_type(&rt)
                            };
                            if let Some(mut d) = self.param_def(&p, name, LocalKind::Val) {
                                if d.ty.is_none() {
                                    d.ty = elem();
                                }
                                return Some(d);
                            }
                            if let Some(dd) = p.children().find(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                                for (j, e) in dd.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).enumerate() {
                                    if let Some(tok) = token_at(&e, SyntaxKind::IDENT)
                                        && ast::ident_text(&tok) == name {
                                            let ty = elem().and_then(|t| self.component_type(&t, j));
                                            return Some(LocalDef { name: name.into(), kind: LocalKind::Val, name_range: tok.text_range(), ty });
                                        }
                                }
                            }
                        }
                }
                SyntaxKind::CATCH_CLAUSE => {
                    if let Some(list) = anc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) {
                        for p in list.children() {
                            if let Some(d) = self.param_def(&p, name, LocalKind::Val) {
                                return Some(d);
                            }
                        }
                    }
                }
                SyntaxKind::WHEN_EXPR => {
                    if let Some(prop) = anc.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT).and_then(|s| s.children().find(|c| c.kind() == SyntaxKind::PROPERTY))
                        && let Some(tok) = token_at(&prop, SyntaxKind::IDENT)
                            && ast::ident_text(&tok) == name && !prop.text_range().contains_range(at.text_range()) {
                                let ty = ast::child::<ast::TypeRef>(&prop).map(|t| self.resolve_syntax_type(&t)).or_else(|| self.initializer_type(&prop));
                                return Some(LocalDef { name: name.into(), kind: LocalKind::Val, name_range: tok.text_range(), ty });
                            }
                }
                SyntaxKind::CLASS => {
                    // Plain constructor parameters are visible in initializers: `init` blocks,
                    // property initializers (not accessors), supertype arguments and default
                    // values; not in member functions or nested classes.
                    if let Some(pc) = anc.children().find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR)
                        && ctor_params_visible(at, &anc)
                        && let Some(list) = pc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) {
                            for p in list.children().filter(|p| token_at(p, SyntaxKind::VAL_KW).is_none() && token_at(p, SyntaxKind::VAR_KW).is_none()) {
                                if let Some(d) = self.param_def(&p, name, LocalKind::Parameter) {
                                    return Some(d);
                                }
                            }
                        }
                }
                _ => {}
            }
        }
        None
    }

    /// Element type of an iterable/array/range (`iterator().next()`).
    fn element_type(&self, t: &TypeRef) -> Option<TypeRef> {
        let (it, s) = self.members_named(t, "iterator").into_iter().next().or_else(|| self.extensions(t, "iterator").into_iter().next())?;
        let it_ty = substitute(&self.symbol_type(&it)?, &s);
        let (next, s2) = self.members_named(&it_ty, "next").into_iter().next()?;
        Some(substitute(&self.symbol_type(&next)?, &s2))
    }

    // ----------------------------------------------------------- lambdas

    /// The expected (receiver, parameter types) of a lambda from its call site.
    pub fn lambda_signature(&self, lambda: &SyntaxNode) -> Option<(Option<TypeRef>, Vec<TypeRef>)> {
        let key = lambda.text_range();
        if let Some(hit) = self.lambdas.borrow().get(&key) {
            return hit.clone();
        }
        // Results cut short by the recursion guard are not cached.
        let complete = DEPTH.get() < 40;
        let r = self.compute_lambda_signature(lambda);
        if complete {
            self.lambdas.borrow_mut().insert(key, r.clone());
        }
        r
    }

    fn compute_lambda_signature(&self, lambda: &SyntaxNode) -> Option<(Option<TypeRef>, Vec<TypeRef>)> {
        let _g = Guard::enter()?;
        let mut node = lambda.clone();
        let mut parent = node.parent()?;
        if parent.kind() == SyntaxKind::LABELED_EXPR {
            node = parent.clone();
            parent = parent.parent()?;
        }
        let expected: TypeRef = match parent.kind() {
            SyntaxKind::LAMBDA_ARGUMENT => {
                let call = parent.parent()?;
                let (sym, s) = self.resolve_call_target(&call, false)?;
                let p = sym.params.last()?;
                substitute(&self.resolve_declared(&sym, &p.ty), &s)
            }
            SyntaxKind::VALUE_ARGUMENT => {
                let list = parent.parent()?;
                let call = list.parent()?;
                let (sym, s) = self.resolve_call_target(&call, false)?;
                let index = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).position(|a| a == parent)?;
                let named = parent.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| token_at(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
                let p = match named {
                    Some(n) => sym.params.iter().find(|p| p.name == n)?,
                    None => sym.params.get(index)?,
                };
                substitute(&self.resolve_declared(&sym, &p.ty), &s)
            }
            SyntaxKind::PROPERTY => ast::child::<ast::TypeRef>(&parent).map(|t| self.resolve_syntax_type(&t))?,
            _ => return None,
        };
        let _ = node;
        let (recv, params, _) = function_parts(&expected)?;
        Some((recv, params))
    }

    // ------------------------------------------------------ resolution

    /// Implicit receivers at `at`: lambda receivers and enclosing classes, innermost first.
    pub fn implicit_receivers(&self, at: &SyntaxNode) -> Vec<(TypeRef, Option<Classifier>)> {
        let mut out = Vec::new();
        let package = self.package();
        for anc in at.ancestors().skip(1) {
            match anc.kind() {
                SyntaxKind::LAMBDA_EXPR => {
                    if let Some((Some(r), _)) = self.lambda_signature(&anc) {
                        out.push((r, None));
                    }
                }
                SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => {
                    // `scope_at` on the class node includes the class itself as innermost
                    // enclosing class (local classes have no FQ name and are skipped).
                    let scope = scope_at(&anc, &package, &mut self.locals_cache.borrow_mut());
                    if let Some(fq) = scope.enclosing.last()
                        && let Some(c) = self.res.find_classifier(fq) {
                            out.push((class_type(c.symbol()), Some(c)));
                        }
                }
                SyntaxKind::OBJECT_LITERAL => out.push((self.anon_type(&anc), None)),
                SyntaxKind::FUN | SyntaxKind::PROPERTY => {
                    // Extension receivers: `fun String.foo() { length }`
                    if let Some(r) = anc.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE).and_then(|r| ast::child::<ast::TypeRef>(&r)) {
                        out.push((self.resolve_syntax_type(&r), None));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Resolve a simple (unqualified) name used as an expression.
    fn resolve_simple(&self, at: &SyntaxNode, name: &str, call: Option<&SyntaxNode>) -> Vec<Def> {
        if let Some(l) = self.find_local(at, name) {
            return vec![Def::Local(l)];
        }
        for (recv, cls) in self.implicit_receivers(at) {
            let members = if call.is_some() { self.callable_members(&recv, name) } else { self.members_named(&recv, name) };
            let mut found: Vec<Def> = members.into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
            if found.is_empty()
                && let Some(c) = &cls {
                    found = self.static_members(c, name);
                }
            if found.is_empty() {
                found = self.extensions(&recv, name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
            }
            if !found.is_empty() {
                return self.filter_call(found, call);
            }
        }
        let callables: Vec<Def> = self.top_level_named(name).into_iter().filter(|s| s.receiver.is_none()).map(Def::symbol).collect();
        let callables = self.filter_call(callables, call);
        if !callables.is_empty() && (call.is_some() || callables.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind != SymbolKind::Function))) {
            return callables;
        }
        let scope = scope_at(at, &self.package(), &mut self.locals_cache.borrow_mut());
        match self.res.resolve_type_name(&[name], &scope) {
            Ok(Resolved::Classifier(c)) => vec![Def::symbol(c.symbol().clone())],
            Ok(Resolved::Package(p)) => vec![Def::Package(p)],
            // In expressions a package name may start a qualified reference (`java.util.List`).
            _ if callables.is_empty() && self.res.package_exists(name) => vec![Def::Package(name.to_string())],
            _ => callables,
        }
    }

    /// Prefer overloads whose arity fits the call.
    fn filter_call(&self, defs: Vec<Def>, call: Option<&SyntaxNode>) -> Vec<Def> {
        let Some(call) = call else { return defs };
        let n = call_arity(call);
        let fitting: Vec<Def> = defs
            .iter()
            .filter(|d| match d {
                Def::Symbol { sym, .. } if sym.kind == SymbolKind::Function || sym.kind == SymbolKind::Constructor => arity_fits(sym, n),
                _ => true,
            })
            .cloned()
            .collect();
        if fitting.is_empty() { defs } else { fitting }
    }

    pub fn resolve_qualifier(&self, e: &SyntaxNode) -> Qualifier {
        match e.kind() {
            SyntaxKind::NAME_REF => {
                let Some(tok) = token_at(e, SyntaxKind::IDENT) else { return Qualifier::Unknown };
                let name = ast::ident_text(&tok);
                let defs = self.resolve_simple(e, &name, None);
                match defs.first() {
                    Some(Def::Symbol { sym, .. }) if sym.kind.is_classifier() && sym.kind != SymbolKind::Object => match self.res.find_classifier(&sym.fq_name) {
                        Some(c) => Qualifier::Static(c),
                        None => Qualifier::Unknown,
                    },
                    Some(Def::Package(p)) => Qualifier::Package(p.clone()),
                    Some(d) => match self.def_type(d, None) {
                        Some(t) => Qualifier::Value(t),
                        None => Qualifier::Unknown,
                    },
                    None => Qualifier::Unknown,
                }
            }
            SyntaxKind::DOT_QUALIFIED_EXPR => {
                let (Some(recv), Some(sel)) = (first_expr_child(e), e.children().nth(1)) else { return Qualifier::Unknown };
                if sel.kind() == SyntaxKind::NAME_REF {
                    let name = token_at(&sel, SyntaxKind::IDENT).map(|t| ast::ident_text(&t)).unwrap_or_default();
                    match self.resolve_qualifier(&recv) {
                        Qualifier::Package(p) => {
                            let fq = format!("{p}.{name}");
                            if let Some(c) = self.res.find_classifier(&fq) {
                                return if c.symbol().kind == SymbolKind::Object { Qualifier::Value(class_type(c.symbol())) } else { Qualifier::Static(c) };
                            }
                            if self.res.package_exists(&fq) {
                                return Qualifier::Package(fq);
                            }
                            return match self.res.top_level_callables(&p, &name).first().and_then(|s| self.symbol_type(s)) {
                                Some(t) => Qualifier::Value(t),
                                None => Qualifier::Unknown,
                            };
                        }
                        Qualifier::Static(c) => {
                            if let Some(n) = self.res.find_classifier(&format!("{}.{name}", c.fq_name())) {
                                return if n.symbol().kind == SymbolKind::Object { Qualifier::Value(class_type(n.symbol())) } else { Qualifier::Static(n) };
                            }
                        }
                        _ => {}
                    }
                }
                match self.type_of(e) {
                    Some(t) => Qualifier::Value(t),
                    None => Qualifier::Unknown,
                }
            }
            _ => match self.type_of(e) {
                Some(t) => Qualifier::Value(t),
                None => Qualifier::Unknown,
            },
        }
    }

    /// Resolve a NAME_REF node (in any position).
    pub fn resolve_name_ref(&self, nr: &SyntaxNode) -> Vec<Def> {
        let Some(tok) = token_at(nr, SyntaxKind::IDENT).or_else(|| token_at(nr, SyntaxKind::THIS_KW)) else { return Vec::new() };
        let name = ast::ident_text(&tok);
        let parent = nr.parent();
        let call = parent.as_ref().filter(|p| p.kind() == SyntaxKind::CALL_EXPR && first_expr_child(p).as_ref() == Some(nr)).cloned();
        let expr = call.clone().unwrap_or_else(|| nr.clone());
        let Some(outer) = expr.parent() else { return Vec::new() };
        match outer.kind() {
            SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR if first_expr_child(&outer).as_ref() != Some(&expr) => {
                let Some(recv) = first_expr_child(&outer) else { return Vec::new() };
                self.member_reference(&recv, &name, call.as_ref())
            }
            SyntaxKind::CALLABLE_REFERENCE => match first_expr_child(&outer).filter(|r| r != nr) {
                Some(recv) => self.member_reference(&recv, &name, None),
                None => self.resolve_simple(nr, &name, None),
            },
            SyntaxKind::BINARY_EXPR if outer.children().nth(1).is_some_and(|o| o.kind() == SyntaxKind::OPERATION_REFERENCE) => self.resolve_simple(nr, &name, call.as_ref()),
            _ => self.resolve_simple(nr, &name, call.as_ref()),
        }
    }

    fn member_reference(&self, recv: &SyntaxNode, name: &str, call: Option<&SyntaxNode>) -> Vec<Def> {
        match self.resolve_qualifier(recv) {
            Qualifier::Value(t) => {
                let members = if call.is_some() { self.callable_members(&t, name) } else { self.members_named(&t, name) };
                let mut defs: Vec<Def> = members.into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
                if defs.is_empty() || call.is_some() && !defs.iter().any(|d| matches!(d, Def::Symbol { sym, .. } if sym.kind == SymbolKind::Function && call.is_some_and(|c| arity_fits(sym, call_arity(c))))) {
                    let mut ext = self.member_extensions(recv, &t, name);
                    if ext.is_empty() {
                        ext = self.extensions(&t, name);
                    }
                    defs.extend(ext.into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }));
                }
                self.filter_call(defs, call)
            }
            Qualifier::Static(c) => {
                // `Type::member` refers to instance members too.
                let mut defs = self.static_members(&c, name);
                if defs.is_empty() {
                    defs = self.members_named(&class_type(c.symbol()), name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
                }
                self.filter_call(defs, call)
            }
            Qualifier::Package(p) => {
                let fq = format!("{p}.{name}");
                if let Some(c) = self.res.find_classifier(&fq) {
                    return vec![Def::symbol(c.symbol().clone())];
                }
                let callables: Vec<Def> = self.res.top_level_callables(&p, name).into_iter().map(Def::symbol).collect();
                if !callables.is_empty() {
                    return self.filter_call(callables, call);
                }
                if self.res.package_exists(&fq) { vec![Def::Package(fq)] } else { Vec::new() }
            }
            Qualifier::Unknown => Vec::new(),
        }
    }

    /// The value type of a resolved definition (`call`: it is invoked).
    pub fn def_type(&self, d: &Def, call: Option<&SyntaxNode>) -> Option<TypeRef> {
        match d {
            Def::Local(l) => {
                let t = l.ty.clone()?;
                if call.is_some() && l.kind != LocalKind::Function {
                    return function_parts(&t).map(|(_, _, r)| r);
                }
                Some(t)
            }
            Def::Package(_) => None,
            Def::Symbol { sym, subst } => {
                if sym.kind == SymbolKind::Constructor {
                    let cls = sym.fq_name.trim_end_matches(".<init>");
                    return self.res.find_classifier(cls).map(|c| class_type(c.symbol()));
                }
                if sym.kind.is_classifier() && sym.kind != SymbolKind::Object {
                    return call.map(|_| class_type(sym));
                }
                let t = substitute(&self.symbol_type(sym)?, subst);
                if call.is_some() && matches!(sym.kind, SymbolKind::Property | SymbolKind::Field) {
                    return function_parts(&t).map(|(_, _, r)| r);
                }
                Some(t)
            }
        }
    }

    /// Resolve the function invoked by a CALL_EXPR, binding type parameters from arguments.
    pub fn resolve_call_target(&self, call: &SyntaxNode, infer_lambdas: bool) -> Option<(Symbol, Subst)> {
        let callee = first_expr_child(call)?;
        let defs = match callee.kind() {
            SyntaxKind::NAME_REF => self.resolve_name_ref(&callee),
            _ => return None,
        };
        let (sym, mut s) = defs.into_iter().find_map(|d| match d {
            Def::Symbol { sym, subst } => Some((sym, subst)),
            _ => None,
        })?;
        let sym = if sym.kind.is_classifier() {
            // Constructor call: bind class type parameters through the primary constructor.
            let c = self.res.find_classifier(&sym.fq_name)?;
            let ctors: Vec<Symbol> = self.members_of(&c).into_iter().filter(|m| m.kind == SymbolKind::Constructor).collect();
            let mut ctor = ctors.iter().find(|m| arity_fits(m, call_arity(call))).or(ctors.first()).cloned().unwrap_or_else(|| sym.clone());
            ctor.type_params.extend(c.symbol().type_params.clone());
            ctor
        } else {
            sym
        };
        let vars: HashSet<String> = sym.type_params.iter().map(|t| t.name.clone()).collect();
        // Explicit type arguments.
        if let Some(targs) = call.children().find(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) {
            for (tp, a) in sym.type_params.iter().zip(targs.children().filter(|c| c.kind() == SyntaxKind::TYPE_PROJECTION)) {
                if let Some(t) = ast::child::<ast::TypeRef>(&a) {
                    s.insert(tp.name.clone(), self.resolve_syntax_type(&t));
                }
            }
        }
        // Value arguments (positional / named), then trailing lambdas.
        let args: Vec<SyntaxNode> = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect()).unwrap_or_default();
        for (i, a) in args.iter().enumerate() {
            let named = a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| token_at(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
            let Some(p) = (match named {
                Some(n) => sym.params.iter().find(|p| p.name == n),
                None => sym.params.get(i).or_else(|| sym.params.last().filter(|p| p.vararg)),
            }) else {
                continue;
            };
            let Some(expr) = a.children().last() else { continue };
            if expr.kind() == SyntaxKind::LAMBDA_EXPR {
                continue;
            }
            if let Some(at) = self.type_of(&expr) {
                let pt = self.resolve_declared(&sym, &p.ty);
                unify(&pt, &at, &vars, &mut s, self);
            }
        }
        if infer_lambdas {
            for la in call.children().filter(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) {
                let Some(p) = sym.params.last() else { break };
                let pt = substitute(&self.resolve_declared(&sym, &p.ty), &s);
                let Some((_, _, ret)) = function_parts(&pt) else { continue };
                let lambda = la.descendants().find(|n| n.kind() == SyntaxKind::LAMBDA_EXPR);
                let result = lambda.and_then(|l| l.children().find(|c| c.kind() == SyntaxKind::BLOCK)).and_then(|b| b.children().last()).and_then(|e| self.type_of(&e));
                if let Some(rt) = result {
                    unify(&ret, &rt, &vars, &mut s, self);
                }
            }
        }
        Some((sym, s))
    }

    // ------------------------------------------------------------- typing

    /// Approximate type of an expression node.
    pub fn type_of(&self, e: &SyntaxNode) -> Option<TypeRef> {
        if let Some(hit) = self.types.borrow().get(&e.text_range()) {
            return hit.clone();
        }
        let _g = Guard::enter()?;
        let t = self.type_of_uncached(e);
        if e.ancestors().last().as_ref() == Some(&self.root) {
            self.types.borrow_mut().insert(e.text_range(), t.clone());
        }
        t
    }

    fn type_of_uncached(&self, e: &SyntaxNode) -> Option<TypeRef> {
        use SyntaxKind::*;
        match e.kind() {
            LITERAL => {
                let tok = e.first_token()?;
                Some(TypeRef::named(match tok.kind() {
                    INT_LITERAL if tok.text().ends_with(['L', 'l']) => "kotlin.Long",
                    INT_LITERAL if tok.text().to_lowercase().contains('u') => "kotlin.UInt",
                    INT_LITERAL => "kotlin.Int",
                    FLOAT_LITERAL if tok.text().ends_with(['f', 'F']) => "kotlin.Float",
                    FLOAT_LITERAL => "kotlin.Double",
                    CHAR_LITERAL => "kotlin.Char",
                    TRUE_KW | FALSE_KW => "kotlin.Boolean",
                    NULL_KW => return Some(TypeRef::named("kotlin.Nothing").nullable(true)),
                    _ => return None,
                }))
            }
            STRING_TEMPLATE => Some(TypeRef::named("kotlin.String")),
            PAREN_EXPR | ANNOTATED_EXPR | LABELED_EXPR => e.children().filter(|c| !matches!(c.kind(), LABEL | ANNOTATION)).last().and_then(|c| self.type_of(&c)),
            NAME_REF => {
                let defs = self.resolve_name_ref(e);
                defs.first().and_then(|d| self.def_type(d, None))
            }
            THIS_EXPR => self.implicit_receivers(e).into_iter().next().map(|(t, _)| t),
            SUPER_EXPR => {
                // `super<T>` names the supertype; plain `super` is the superclass
                // (the first supertype that is not an interface).
                if let Some(t) = ast::child::<ast::TypeRef>(e) {
                    return Some(self.resolve_syntax_type(&t));
                }
                let (own, _) = self.implicit_receivers(e).into_iter().find(|(_, c)| c.is_some()).or_else(|| self.implicit_receivers(e).into_iter().next())?;
                let supers: Vec<TypeRef> = self.supertype_closure(&own).into_iter().skip(1).collect();
                let class = supers.iter().find(|st| {
                    fq_of(st).and_then(|fq| self.res.find_classifier(fq)).is_some_and(|c| c.symbol().kind != SymbolKind::Interface)
                        && fq_of(st) != Some("kotlin.Any")
                });
                class.or(supers.first()).cloned()
            }
            CALL_EXPR => {
                let callee = first_expr_child(e)?;
                if callee.kind() != NAME_REF {
                    // `(expr)(...)` / `f()()` → invoke
                    let ft = self.type_of(&callee)?;
                    return function_parts(&ft).map(|(_, _, r)| r);
                }
                let (sym, s) = self.resolve_call_target(e, true)?;
                if sym.kind == SymbolKind::Constructor {
                    let cls = self.res.find_classifier(sym.fq_name.trim_end_matches(".<init>"))?;
                    return Some(substitute(&class_type(cls.symbol()), &s));
                }
                if sym.kind.is_classifier() {
                    return Some(substitute(&class_type(&sym), &s));
                }
                if let Some(ret) = &sym.ty {
                    let t = substitute(&self.resolve_declared(&sym, ret), &s);
                    return if matches!(sym.kind, SymbolKind::Property | SymbolKind::Field) { function_parts(&t).map(|(_, _, r)| r) } else { Some(t) };
                }
                self.def_type(&Def::Symbol { sym, subst: s }, Some(e))
            }
            DOT_QUALIFIED_EXPR | SAFE_QUALIFIED_EXPR => {
                let sel = e.children().nth(1)?;
                let t = self.type_of(&sel)?;
                if e.kind() == SAFE_QUALIFIED_EXPR { Some(t.nullable(true)) } else { Some(t) }
            }
            CALLABLE_REFERENCE | CLASS_LITERAL | LAMBDA_EXPR | ANONYMOUS_FUNCTION => None,
            OBJECT_LITERAL => Some(self.anon_type(e)),
            IS_EXPR => Some(TypeRef::named("kotlin.Boolean")),
            BINARY_WITH_TYPE => {
                let t = ast::child::<ast::TypeRef>(e).map(|t| self.resolve_syntax_type(&t))?;
                let safe = e.children().find(|c| c.kind() == OPERATION_REFERENCE).and_then(|o| o.first_token()).is_some_and(|t| t.kind() == AS_SAFE_KW);
                Some(if safe { t.nullable(true) } else { t })
            }
            PREFIX_EXPR => {
                let op = e.children().find(|c| c.kind() == OPERATION_REFERENCE)?.first_token()?;
                let operand = e.children().nth(1)?;
                match op.kind() {
                    EXCL => Some(TypeRef::named("kotlin.Boolean")),
                    _ => self.type_of(&operand),
                }
            }
            POSTFIX_EXPR => {
                let operand = first_expr_child(e)?;
                let op = e.children().find(|c| c.kind() == OPERATION_REFERENCE)?.first_token()?;
                let t = self.type_of(&operand)?;
                Some(if op.kind() == EXCL2 { non_null(t) } else { t })
            }
            BINARY_EXPR => self.binary_type(e),
            INDEX_EXPR => {
                let recv = first_expr_child(e)?;
                let t = self.type_of(&recv)?;
                let (sym, s) = self.members_named(&t, "get").into_iter().next().or_else(|| self.extensions(&t, "get").into_iter().next())?;
                Some(substitute(&self.symbol_type(&sym)?, &s))
            }
            IF_EXPR => {
                let then = e.children().find(|c| c.kind() == THEN_BRANCH)?;
                self.branch_type(&then)
            }
            WHEN_EXPR => {
                let entry = e.children().find(|c| c.kind() == WHEN_ENTRY)?;
                self.branch_type(&entry.children().find(|c| c.kind() == BODY)?)
            }
            TRY_EXPR => {
                let block = e.children().find(|c| c.kind() == BLOCK)?;
                block.children().last().and_then(|c| self.type_of(&c))
            }
            THROW_EXPR | RETURN_EXPR | BREAK_EXPR | CONTINUE_EXPR => Some(TypeRef::named("kotlin.Nothing")),
            _ => None,
        }
    }

    fn branch_type(&self, branch: &SyntaxNode) -> Option<TypeRef> {
        let inner = branch.children().next()?;
        if inner.kind() == SyntaxKind::BLOCK {
            return inner.children().last().and_then(|c| self.type_of(&c));
        }
        self.type_of(&inner)
    }

    fn binary_type(&self, e: &SyntaxNode) -> Option<TypeRef> {
        use SyntaxKind::*;
        let lhs = first_expr_child(e)?;
        let op_node = e.children().find(|c| c.kind() == OPERATION_REFERENCE)?;
        let op = op_node.first_token()?;
        let rhs = e.children().nth(2);
        let boolean = || Some(TypeRef::named("kotlin.Boolean"));
        match op.kind() {
            EQ | PLUS_EQ | MINUS_EQ | STAR_EQ | SLASH_EQ | PERCENT_EQ => Some(TypeRef::named("kotlin.Unit")),
            AMP2 | PIPE2 | EQ2 | NOT_EQ | EQ3 | NOT_EQ2 | LT | GT | LT_EQ | GT_EQ | IN_KW | NOT_IN_KW => boolean(),
            ELVIS => {
                let l = self.type_of(&lhs);
                match l {
                    Some(t) if fq_of(&t) != Some("kotlin.Nothing") => Some(non_null(t)),
                    _ => rhs.and_then(|r| self.type_of(&r)),
                }
            }
            _ => {
                let name = match op.kind() {
                    PLUS => "plus",
                    MINUS => "minus",
                    STAR => "times",
                    SLASH => "div",
                    PERCENT => "rem",
                    RANGE => "rangeTo",
                    RANGE_UNTIL => "rangeUntil",
                    IDENT => op.text(),
                    _ => return None,
                };
                let lt = self.type_of(&lhs)?;
                let (sym, mut s) = self.members_named(&lt, name).into_iter().next().or_else(|| self.extensions(&lt, name).into_iter().next())?;
                // Bind type parameters from the right operand (`a to b`).
                if let (Some(r), Some(p)) = (rhs.as_ref(), sym.params.first())
                    && let Some(rt) = self.type_of(r) {
                        let vars: HashSet<String> = sym.type_params.iter().map(|t| t.name.clone()).collect();
                        unify(&self.resolve_declared(&sym, &p.ty), &rt, &vars, &mut s, self);
                    }
                Some(substitute(&self.symbol_type(&sym)?, &s))
            }
        }
    }

    // -------------------------------------------------------- operators

    /// Resolve an operator convention call on a receiver of type `recv`: the first of
    /// `names` that exists (members, then member extensions, then extensions). With an
    /// argument type, overloads taking exactly that type are preferred.
    fn operator_call(&self, at: &SyntaxNode, recv: &TypeRef, names: &[&str], arg: Option<&TypeRef>, arity: Option<usize>) -> Vec<Def> {
        for name in names {
            let fits = |m: &Symbol| m.kind == SymbolKind::Function && arity.is_none_or(|n| arity_fits(m, n));
            let mut cands: Vec<(Symbol, Subst)> = self.members_where(recv, name, &|m| fits(m));
            if cands.is_empty() {
                cands = self.member_extensions(at, recv, name).into_iter().filter(|(m, _)| fits(m)).collect();
            }
            if cands.is_empty() {
                cands = self.extensions(recv, name).into_iter().filter(|(m, _)| fits(m)).collect();
            }
            if cands.is_empty() {
                continue;
            }
            if let (Some(arg), true) = (arg.and_then(fq_of), cands.len() > 1) {
                let exact: Vec<(Symbol, Subst)> = cands
                    .iter()
                    .filter(|(m, s)| m.params.first().is_some_and(|p| fq_of(&substitute(&self.resolve_declared(m, &p.ty), s)) == Some(arg)))
                    .cloned()
                    .collect();
                if !exact.is_empty() {
                    cands = exact;
                }
            }
            return cands.into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
        }
        Vec::new()
    }

    /// The operator function invoked by an operator token (`+`, `[`, `in`, `by`, `(`...).
    pub fn operator_defs(&self, tok: &SyntaxToken) -> Option<Vec<Def>> {
        use SyntaxKind::*;
        let parent = tok.parent()?;
        // Binary / unary operators are wrapped in OPERATION_REFERENCE; assignments are not.
        let (expr, wrapped) = if parent.kind() == OPERATION_REFERENCE { (parent.parent()?, true) } else { (parent.clone(), false) };
        let ty = |n: &SyntaxNode| self.type_of(n);
        match (expr.kind(), tok.kind()) {
            (BINARY_EXPR, k) if wrapped || matches!(k, PLUS_EQ | MINUS_EQ | STAR_EQ | SLASH_EQ | PERCENT_EQ) => {
                let lhs = first_expr_child(&expr)?;
                let rhs = expr.children().filter(|c| c.kind() != OPERATION_REFERENCE).nth(1);
                let rt = rhs.as_ref().and_then(ty);
                let names: &[&str] = match k {
                    PLUS => &["plus"],
                    MINUS => &["minus"],
                    STAR => &["times"],
                    SLASH => &["div"],
                    PERCENT => &["rem"],
                    RANGE => &["rangeTo"],
                    RANGE_UNTIL => &["rangeUntil"],
                    PLUS_EQ => &["plusAssign", "plus"],
                    MINUS_EQ => &["minusAssign", "minus"],
                    STAR_EQ => &["timesAssign", "times"],
                    SLASH_EQ => &["divAssign", "div"],
                    PERCENT_EQ => &["remAssign", "rem"],
                    EQ2 | NOT_EQ => &["equals"],
                    LT | GT | LT_EQ | GT_EQ => &["compareTo"],
                    IN_KW | NOT_IN_KW => {
                        // `x in c` calls `c.contains(x)`.
                        let ct = rt?;
                        return Some(self.operator_call(&expr, &ct, &["contains"], ty(&lhs).as_ref(), Some(1)));
                    }
                    _ => return None,
                };
                let lt = ty(&lhs)?;
                Some(self.operator_call(&expr, &lt, names, rt.as_ref(), Some(1)))
            }
            (PREFIX_EXPR, k) => {
                let operand = expr.children().find(|c| c.kind() != OPERATION_REFERENCE)?;
                let name = match k {
                    MINUS => "unaryMinus",
                    PLUS => "unaryPlus",
                    EXCL => "not",
                    PLUS2 => "inc",
                    MINUS2 => "dec",
                    _ => return None,
                };
                Some(self.operator_call(&expr, &ty(&operand)?, &[name], None, Some(0)))
            }
            (POSTFIX_EXPR, PLUS2 | MINUS2) => {
                let operand = first_expr_child(&expr)?;
                let name = if tok.kind() == PLUS2 { "inc" } else { "dec" };
                Some(self.operator_call(&expr, &ty(&operand)?, &[name], None, Some(0)))
            }
            (WHEN_CONDITION, IN_KW | NOT_IN_KW) => {
                // `when (x) { in c -> }` calls `c.contains(x)`.
                let range = expr.children().find(|c| c.kind() != OPERATION_REFERENCE)?;
                let subject = expr.ancestors().find(|a| a.kind() == WHEN_EXPR)?.children().find(|c| c.kind() == WHEN_SUBJECT)?.children().next()?;
                let st = if subject.kind() == PROPERTY { self.initializer_type(&subject) } else { ty(&subject) };
                Some(self.operator_call(&expr, &ty(&range)?, &["contains"], st.as_ref(), Some(1)))
            }
            (FOR_EXPR, IN_KW) => {
                let range = expr.children().find(|c| c.kind() == LOOP_RANGE)?.children().next()?;
                Some(self.operator_call(&expr, &ty(&range)?, &["iterator"], None, Some(0)))
            }
            (VALUE_ARGUMENT_LIST, L_BRACK) => {
                // `a[i]` → get, `a[i] = v` → set
                let index = expr.parent().filter(|p| p.kind() == INDEX_EXPR)?;
                let recv = first_expr_child(&index)?;
                let n = expr.children().count();
                let assigned = index.parent().is_some_and(|b| {
                    b.kind() == BINARY_EXPR && first_expr_child(&b).as_ref() == Some(&index) && ast::token(&b, EQ).is_some()
                });
                let (name, arity) = if assigned { ("set", n + 1) } else { ("get", n) };
                Some(self.operator_call(&index, &ty(&recv)?, &[name], None, Some(arity)))
            }
            (VALUE_ARGUMENT_LIST, L_PAREN) => {
                // `value(args)` on a non-function value → invoke
                let call = expr.parent().filter(|p| p.kind() == CALL_EXPR)?;
                let callee = first_expr_child(&call)?;
                let callee_ty = if callee.kind() == NAME_REF {
                    let d = self.resolve_name_ref(&callee).into_iter().next()?;
                    match &d {
                        Def::Local(l) if l.kind != LocalKind::Function && l.kind != LocalKind::Class => l.ty.clone()?,
                        Def::Symbol { sym, .. } if matches!(sym.kind, SymbolKind::Property | SymbolKind::Field | SymbolKind::Object | SymbolKind::EnumEntry) => self.def_type(&d, None)?,
                        _ => return Some(Vec::new()),
                    }
                } else {
                    ty(&callee)?
                };
                if function_parts(&callee_ty).is_some() {
                    return Some(Vec::new()); // a lambda / function value: built-in invoke
                }
                Some(self.operator_call(&call, &callee_ty, &["invoke"], None, Some(call_arity(&call))))
            }
            (PROPERTY_DELEGATE, IDENT) if tok.text() == "by" => {
                let delegate = expr.children().next()?;
                let prop = expr.parent()?;
                let name = if ast::token(&prop, VAR_KW).is_some() { "setValue" } else { "getValue" };
                Some(self.operator_call(&expr, &ty(&delegate)?, &["provideDelegate", name, "getValue"], None, None))
            }
            _ => None,
        }
    }

    /// `componentN` called by the n-th entry of a destructuring declaration.
    pub fn component_defs(&self, entry: &SyntaxNode) -> Option<Vec<Def>> {
        let decl = entry.parent().filter(|p| p.kind() == SyntaxKind::DESTRUCTURING_DECLARATION)?;
        let index = decl.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).position(|c| c == *entry)?;
        let owner = decl.parent()?;
        let value_ty = match owner.kind() {
            SyntaxKind::PROPERTY => self.initializer_type(&owner)?,
            // Lambda / for-loop parameters: the parameter's (inferred) type.
            SyntaxKind::VALUE_PARAMETER => {
                let scope = owner.ancestors().find(|a| matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::FOR_EXPR))?;
                let probe = match scope.kind() {
                    SyntaxKind::LAMBDA_EXPR => scope.children().find(|c| c.kind() == SyntaxKind::BLOCK)?,
                    _ => scope.children().find(|c| c.kind() == SyntaxKind::BODY)?,
                };
                let elem = if scope.kind() == SyntaxKind::FOR_EXPR {
                    let range = scope.children().find(|c| c.kind() == SyntaxKind::LOOP_RANGE)?.children().next()?;
                    self.element_type(&self.type_of(&range)?)
                } else {
                    let i = scope.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST)?.children().position(|p| p == owner)?;
                    self.lambda_signature(&scope).and_then(|(_, ps)| ps.get(i).cloned())
                };
                let _ = probe;
                elem?
            }
            _ => return None,
        };
        // Source data classes: the synthesized componentN maps to a constructor property.
        if let Some(sym) = self.data_component_property(&value_ty, index) {
            return Some(vec![Def::Symbol { sym, subst: Subst::new() }]);
        }
        let name = format!("component{}", index + 1);
        Some(self.operator_call(&decl, &value_ty, &[name.as_str()], None, Some(0)))
    }

    /// The `i`-th primary-constructor property of a source data class.
    fn data_component_property(&self, t: &TypeRef, i: usize) -> Option<Symbol> {
        let (c, _) = self.classifier_for(fq_of(t)?)?;
        if !c.symbol().modifiers.has(Modifiers::DATA) {
            return None;
        }
        let members = self.members_of(&c);
        let ctor = members.iter().find(|m| m.kind == SymbolKind::Constructor)?;
        let Origin::Source { range: ctor_range, .. } = &ctor.origin else { return None };
        members.iter().filter(|m| m.kind == SymbolKind::Property).filter(|m| matches!(&m.origin, Origin::Source { range, .. } if ctor_range.contains_range(*range))).nth(i).cloned()
    }

    /// The infix operator / function referenced by an OPERATION_REFERENCE identifier.
    pub fn resolve_infix(&self, op: &SyntaxNode) -> Vec<Def> {
        let Some(bin) = op.parent() else { return Vec::new() };
        let Some(tok) = op.first_token().filter(|t| t.kind() == SyntaxKind::IDENT) else { return Vec::new() };
        let Some(lhs) = first_expr_child(&bin) else { return Vec::new() };
        let Some(lt) = self.type_of(&lhs) else { return Vec::new() };
        let name = tok.text();
        let mut defs: Vec<Def> = self.members_named(&lt, name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
        if defs.is_empty() {
            defs = self.extensions(&lt, name).into_iter().map(|(sym, subst)| Def::Symbol { sym, subst }).collect();
        }
        defs
    }

    /// `this` at a position: the innermost implicit receiver's classifier symbol.
    pub fn this_target(&self, at: &SyntaxNode) -> Option<Symbol> {
        let (t, c) = self.implicit_receivers(at).into_iter().next()?;
        match c {
            Some(c) => Some(c.symbol().clone()),
            None => self.res.find_classifier(fq_of(&t)?).map(|c| c.symbol().clone()),
        }
    }
}

fn vararg_array(elem: TypeRef) -> TypeRef {
    let primitive = match fq_of(&elem) {
        Some("kotlin.Int") => Some("kotlin.IntArray"),
        Some("kotlin.Long") => Some("kotlin.LongArray"),
        Some("kotlin.Short") => Some("kotlin.ShortArray"),
        Some("kotlin.Byte") => Some("kotlin.ByteArray"),
        Some("kotlin.Char") => Some("kotlin.CharArray"),
        Some("kotlin.Float") => Some("kotlin.FloatArray"),
        Some("kotlin.Double") => Some("kotlin.DoubleArray"),
        Some("kotlin.Boolean") => Some("kotlin.BooleanArray"),
        _ => None,
    };
    match primitive {
        Some(p) if !matches!(elem, TypeRef::Named { nullable: true, .. }) => TypeRef::named(p),
        _ => TypeRef::with_args("kotlin.Array", vec![TypeArg::Type { variance: Variance::Out, ty: elem }]),
    }
}

/// Resolve every unresolved name in `t` using `r`/`scope`.
fn resolve_names(t: &TypeRef, r: &Resolver, scope: &TypeScope) -> TypeRef {
    match t {
        TypeRef::Named { name, fq: None, args, nullable } => {
            let segs: Vec<&str> = name.split('.').collect();
            let args: Vec<TypeArg> = args
                .iter()
                .map(|a| match a {
                    TypeArg::Star => TypeArg::Star,
                    TypeArg::Type { variance, ty } => TypeArg::Type { variance: *variance, ty: resolve_names(ty, r, scope) },
                })
                .collect();
            match r.resolve_type_name(&segs, scope) {
                Ok(Resolved::Classifier(c)) => TypeRef::Named { name: name.clone(), fq: Some(c.fq_name().into()), args, nullable: *nullable },
                Ok(Resolved::TypeParameter(n)) => TypeRef::TypeParam { name: n, nullable: *nullable },
                _ => TypeRef::Named { name: name.clone(), fq: None, args, nullable: *nullable },
            }
        }
        TypeRef::Function { receiver, params, ret, suspend, nullable } => TypeRef::Function {
            receiver: receiver.as_ref().map(|x| Box::new(resolve_names(x, r, scope))),
            params: params.iter().map(|p| resolve_names(p, r, scope)).collect(),
            ret: Box::new(resolve_names(ret, r, scope)),
            suspend: *suspend,
            nullable: *nullable,
        },
        TypeRef::Platform(inner) => TypeRef::Platform(Box::new(resolve_names(inner, r, scope))),
        other => other.clone(),
    }
}

// ------------------------------------------------------------ enumeration (completion)

impl<'s> Engine<'s> {
    /// Every instance member of `t` (nearest declaration of each signature first), with
    /// Java getters also offered as properties (`getName()` → `name`).
    pub fn all_members(&self, t: &TypeRef) -> Vec<(Symbol, Subst)> {
        let mut out: Vec<(Symbol, Subst)> = Vec::new();
        let mut seen: HashSet<(String, bool, usize)> = HashSet::new();
        let mut push = |m: Symbol, s: &Subst, out: &mut Vec<(Symbol, Subst)>| {
            let key = (m.name.clone(), m.kind == SymbolKind::Function, m.params.len());
            if seen.insert(key) {
                out.push((m, s.clone()));
            }
        };
        for st in self.supertype_closure(t) {
            let Some(fq) = fq_of(&st) else { continue };
            if let Some(members) = self.anon_members(fq) {
                for m in members {
                    push(m, &Subst::new(), &mut out);
                }
                continue;
            }
            let Some((c, alias_args)) = self.classifier_for(fq) else { continue };
            let args = if alias_args.is_empty() { args_of(&st).to_vec() } else { alias_args };
            let s = self.type_subst(&c, &args);
            let java = matches!(&c, Classifier::Library(lc) if !lc.is_kotlin);
            for m in self.members_of(&c) {
                if m.kind == SymbolKind::Constructor || m.kind.is_classifier() || m.modifiers.has(Modifiers::STATIC) || !self.accessible(&m) {
                    continue;
                }
                if java && m.kind == SymbolKind::Function && m.params.is_empty() && m.type_params.is_empty()
                    && let Some(prop) = java_property_name(&m.name)
                {
                    let mut p = m.clone();
                    p.kind = SymbolKind::Property;
                    p.name = prop;
                    push(p, &s, &mut out);
                }
                push(m, &s, &mut out);
            }
        }
        out
    }

    /// Private members are only visible from the file that declares them.
    fn accessible(&self, m: &Symbol) -> bool {
        m.visibility != crate::symbols::Visibility::Private || matches!(&m.origin, Origin::Source { file, .. } if *file == self.path)
    }

    /// Extension callables applicable to a receiver of type `t` at `at`: member extensions
    /// of implicit receivers, explicitly imported ones, and those of the packages in scope.
    pub fn all_extensions(&self, at: &SyntaxNode, t: &TypeRef) -> Vec<(Symbol, Subst)> {
        let candidates = self.extension_candidates(at);
        self.applicable_to(candidates, t)
    }

    /// Extensions callable at `at` before checking receivers (compute once, then filter
    /// per receiver with [`Engine::applicable_to`]).
    pub fn extension_candidates(&self, at: &SyntaxNode) -> Vec<(Symbol, Subst)> {
        let mut candidates: Vec<(Symbol, Subst)> = Vec::new();
        for (r, _) in self.implicit_receivers(at) {
            candidates.extend(self.all_members(&r).into_iter().filter(|(m, _)| m.receiver.is_some()));
        }
        for sym in self.imported_symbols().1 {
            if sym.receiver.is_some() {
                candidates.push((sym, Subst::new()));
            }
        }
        for pkg in self.callable_packages() {
            let (_, callables) = self.res.package_contents(&pkg);
            candidates.extend(callables.into_iter().filter(|s| s.receiver.is_some()).map(|s| (s, Subst::new())));
        }
        candidates.retain(|(s, _)| self.accessible(s));
        candidates
    }

    /// The candidates whose receiver accepts `t`, most specific first.
    pub fn applicable_to(&self, candidates: Vec<(Symbol, Subst)>, t: &TypeRef) -> Vec<(Symbol, Subst)> {
        self.applicable_extensions(candidates, t)
    }

    /// Constructors of the classifier `fq`.
    pub fn constructors(&self, fq: &str) -> Vec<Symbol> {
        let Some((c, _)) = self.classifier_for(fq) else { return Vec::new() };
        self.members_of(&c).into_iter().filter(|m| m.kind == SymbolKind::Constructor).collect()
    }

    /// Members of `t`'s supertypes that a subclass may override and does not yet:
    /// abstract and open members, interface members, non-final Java methods.
    pub fn overridable_members(&self, t: &TypeRef) -> Vec<(Symbol, Subst)> {
        let closure = self.supertype_closure(t);
        let own: Vec<Symbol> = closure
            .first()
            .and_then(fq_of)
            .and_then(|fq| self.anon_members(fq).or_else(|| self.classifier_for(fq).map(|(c, _)| self.members_of(&c))))
            .unwrap_or_default();
        let declared = |m: &Symbol| own.iter().any(|o| o.name == m.name && o.kind == m.kind && o.params.len() == m.params.len());
        let mut out: Vec<(Symbol, Subst)> = Vec::new();
        let mut seen: HashSet<(String, usize)> = HashSet::new();
        for st in closure.iter().skip(1) {
            let Some(fq) = fq_of(st) else { continue };
            let Some((c, alias_args)) = self.classifier_for(fq) else { continue };
            let args = if alias_args.is_empty() { args_of(st).to_vec() } else { alias_args };
            let s = self.type_subst(&c, &args);
            let interface = c.symbol().kind == SymbolKind::Interface;
            let java = matches!(&c, Classifier::Library(lc) if !lc.is_kotlin);
            for m in self.members_of(&c) {
                if !matches!(m.kind, SymbolKind::Function | SymbolKind::Property) || m.modifiers.has(Modifiers::STATIC) || m.modifiers.has(Modifiers::FINAL) {
                    continue;
                }
                if m.visibility == crate::symbols::Visibility::Private || m.receiver.is_some() && !interface && !m.modifiers.has(Modifiers::OPEN | Modifiers::ABSTRACT) {
                    continue;
                }
                let open = interface || java || m.modifiers.has(Modifiers::OPEN) || m.modifiers.has(Modifiers::ABSTRACT) || m.modifiers.has(Modifiers::OVERRIDE);
                if !open || declared(&m) || !seen.insert((m.name.clone(), m.params.len())) {
                    continue;
                }
                out.push((m, s.clone()));
            }
        }
        out
    }

    /// Explicitly imported names: classifiers `(name, FQ)` and callables (aliases applied).
    pub fn imported_symbols(&self) -> (Vec<(String, String)>, Vec<Symbol>) {
        let (mut classes, mut callables) = (Vec::new(), Vec::new());
        let Some(s) = self.res.summary() else { return (classes, callables) };
        for imp in &s.imports {
            let Some(name) = imp.imported_name() else { continue };
            match self.res.resolve_import(imp) {
                Ok(Resolved::Classifier(c)) => classes.push((name.to_string(), c.fq_name().to_string())),
                Ok(Resolved::Callables(cs)) => callables.extend(cs.into_iter().map(|mut c| {
                    c.name = name.to_string();
                    c
                })),
                _ => {}
            }
        }
        (classes, callables)
    }

    /// Packages whose top-level declarations are in scope without an import, in priority
    /// order: this file's package, star imports, default imports.
    pub fn scope_packages(&self) -> Vec<String> {
        self.callable_packages()
    }

    /// What a classifier name gives access to: nested classifiers (`(name, FQ)`) and
    /// enum entries, object members, Java statics and companion members.
    pub fn all_static_members(&self, c: &Classifier) -> (Vec<(String, String)>, Vec<(Symbol, Subst)>) {
        let sym = c.symbol();
        let mut nested: Vec<(String, String)> = Vec::new();
        let mut members: Vec<(Symbol, Subst)> = Vec::new();
        match c {
            Classifier::Library(lc) => nested.extend(lc.nested.iter().map(|n| (n.clone(), format!("{}.{n}", sym.fq_name)))),
            Classifier::Source(_) => {
                nested.extend(self.members_of(c).into_iter().filter(|m| m.kind.is_classifier()).map(|m| (m.name.clone(), m.fq_name.clone())))
            }
            Classifier::LibraryAlias(_) => {}
        }
        for m in self.members_of(c) {
            if !m.kind.is_classifier() && (m.kind == SymbolKind::EnumEntry || m.modifiers.has(Modifiers::STATIC)) && self.accessible(&m) {
                members.push((m, Subst::new()));
            }
        }
        if sym.kind == SymbolKind::Object {
            members.extend(self.all_members(&class_type(sym)));
        } else {
            let companion = match c {
                Classifier::Library(lc) => lc.companion.clone(),
                _ => None,
            }
            .unwrap_or_else(|| "Companion".into());
            if let Some(comp) = self.res.find_classifier(&format!("{}.{companion}", sym.fq_name)) {
                members.extend(self.all_members(&class_type(comp.symbol())));
                nested.retain(|(n, _)| *n != companion);
            }
        }
        (nested, members)
    }

    /// Names of locals (variables, parameters, local functions and classes) declared
    /// syntactically before `at` in an enclosing scope, innermost first, resolved with
    /// the same rules as name lookup (shadowing respected).
    pub fn locals_at(&self, at: &SyntaxNode) -> Vec<LocalDef> {
        let mut names: Vec<String> = Vec::new();
        let before = at.text_range().start();
        let ident = |n: &SyntaxNode| token_at(n, SyntaxKind::IDENT).map(|t| ast::ident_text(&t));
        let params = |list: &SyntaxNode, names: &mut Vec<String>| {
            for p in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) {
                names.extend(ident(&p));
                for d in p.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                    names.extend(d.children().filter(|e| e.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|e| ident(&e)));
                }
            }
        };
        for anc in at.ancestors().skip(1) {
            match anc.kind() {
                SyntaxKind::BLOCK => {
                    for st in anc.children().filter(|s| s.text_range().end() <= before) {
                        match st.kind() {
                            SyntaxKind::PROPERTY => {
                                names.extend(ident(&st));
                                for d in st.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                                    names.extend(d.children().filter(|e| e.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|e| ident(&e)));
                                }
                            }
                            SyntaxKind::FUN | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => names.extend(ident(&st)),
                            _ => {}
                        }
                    }
                }
                SyntaxKind::LAMBDA_EXPR => match anc.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) {
                    Some(list) => params(&list, &mut names),
                    None => names.push("it".into()),
                },
                SyntaxKind::FUN | SyntaxKind::SECONDARY_CONSTRUCTOR | SyntaxKind::CATCH_CLAUSE | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::ANONYMOUS_FUNCTION => {
                    if let Some(list) = anc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) {
                        params(&list, &mut names);
                    }
                    if anc.kind() == SyntaxKind::PROPERTY_ACCESSOR {
                        names.push("field".into());
                    }
                }
                SyntaxKind::FOR_EXPR => {
                    for p in anc.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) {
                        names.extend(ident(&p));
                    }
                    for d in anc.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
                        names.extend(d.children().filter(|e| e.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|e| ident(&e)));
                    }
                }
                SyntaxKind::WHEN_EXPR => {
                    if let Some(subject) = anc.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT)
                        && let Some(p) = subject.children().find(|c| c.kind() == SyntaxKind::PROPERTY)
                    {
                        names.extend(ident(&p));
                    }
                }
                // Constructor parameters are visible in initializers and property initializers.
                SyntaxKind::CLASS => {
                    if let Some(list) = anc
                        .children()
                        .find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR)
                        .and_then(|c| c.children().find(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST))
                    {
                        let in_init = at.ancestors().any(|a| a.kind() == SyntaxKind::CLASS_INITIALIZER)
                            || at.ancestors().take_while(|a| a != &anc).any(|a| a.kind() == SyntaxKind::PROPERTY && a.parent().is_some_and(|p| p.kind() == SyntaxKind::CLASS_BODY));
                        if in_init {
                            params(&list, &mut names);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut seen = HashSet::new();
        names
            .into_iter()
            .filter(|n| !n.is_empty() && n != "_" && seen.insert(n.clone()))
            .filter_map(|n| self.find_local(at, &n))
            .collect()
    }
}

/// `getName` → `name`, `isEnabled` → `isEnabled` (Kotlin's synthetic Java properties).
fn java_property_name(method: &str) -> Option<String> {
    let rest = method.strip_prefix("get").filter(|r| r.chars().next().is_some_and(char::is_uppercase));
    if let Some(r) = rest {
        let mut c = r.chars();
        let first = c.next()?;
        return Some(first.to_lowercase().chain(c).collect());
    }
    method.strip_prefix("is").filter(|r| r.chars().next().is_some_and(char::is_uppercase)).map(|_| method.to_string())
}

/// Members the compiler generates for a source class: `copy`/`componentN` of data
/// classes, `values`/`valueOf`/`entries` of enums.
fn synthesized_members(class: &Symbol, members: &[Symbol]) -> Vec<Symbol> {
    let mut out = Vec::new();
    if class.kind == SymbolKind::Enum {
        out.extend(enum_statics(class, members));
    }
    if class.modifiers.has(Modifiers::DATA) {
        let ctor = members.iter().find(|m| m.kind == SymbolKind::Constructor);
        let props: Vec<&Symbol> = match ctor.map(|c| &c.origin) {
            Some(Origin::Source { range: ctor_range, .. }) => members
                .iter()
                .filter(|m| m.kind == SymbolKind::Property && matches!(&m.origin, Origin::Source { range, .. } if ctor_range.contains_range(*range)))
                .collect(),
            _ => Vec::new(),
        };
        let synth = |name: &str, kind: SymbolKind| {
            let mut s = Symbol::new(kind, name, format!("{}.{name}", class.fq_name), class.origin.clone());
            s.modifiers.set(Modifiers::SYNTHETIC);
            s
        };
        if !members.iter().any(|m| m.name == "copy") {
            let mut copy = synth("copy", SymbolKind::Function);
            copy.params = props.iter().map(|p| crate::symbols::Param { name: p.name.clone(), ty: p.ty.clone().unwrap_or(TypeRef::Error), has_default: true, vararg: false }).collect();
            copy.ty = Some(class_type(class));
            out.push(copy);
        }
        for (i, p) in props.iter().enumerate() {
            let name = format!("component{}", i + 1);
            if !members.iter().any(|m| m.name == name) {
                let mut c = synth(&name, SymbolKind::Function);
                c.modifiers.set(Modifiers::OPERATOR);
                c.ty = p.ty.clone();
                out.push(c);
            }
        }
    }
    out
}

fn enum_statics(class: &Symbol, members: &[Symbol]) -> Vec<Symbol> {
    let t = class_type(class);
    let arg = |ty: TypeRef| TypeArg::Type { variance: crate::symbols::Variance::Invariant, ty };
    let mut out = Vec::new();
    let mut add = |name: &str, kind: SymbolKind, ty: TypeRef, params: Vec<crate::symbols::Param>| {
        if !members.iter().any(|m| m.name == name) {
            let mut s = Symbol::new(kind, name, format!("{}.{name}", class.fq_name), class.origin.clone());
            s.modifiers.set(Modifiers::STATIC);
            s.modifiers.set(Modifiers::SYNTHETIC);
            s.ty = Some(ty);
            s.params = params;
            out.push(s);
        }
    };
    add("values", SymbolKind::Function, TypeRef::with_args("kotlin.Array", vec![arg(t.clone())]), Vec::new());
    add(
        "valueOf",
        SymbolKind::Function,
        t.clone(),
        vec![crate::symbols::Param { name: "value".into(), ty: TypeRef::named("kotlin.String"), has_default: false, vararg: false }],
    );
    add("entries", SymbolKind::Property, TypeRef::with_args("kotlin.enums.EnumEntries", vec![arg(t)]), Vec::new());
    out
}

// ------------------------------------------------------------ checks (diagnostics)

/// A member a concrete subclass must implement.
pub struct AbstractMember {
    pub symbol: Symbol,
    pub subst: Subst,
    /// Declared in an interface (rather than an abstract class).
    pub from_interface: bool,
}

impl<'s> Engine<'s> {
    pub fn classifier(&self, fq: &str) -> Option<Classifier> {
        self.classifier_for(fq).map(|(c, _)| c)
    }

    /// Supertype closure of `t` without `t` itself, or `None` if some supertype declared in
    /// sources does not resolve (members would be unknown).
    fn known_supertypes(&self, t: &TypeRef) -> Option<Vec<TypeRef>> {
        let closure = self.supertype_closure(t);
        for st in &closure {
            let fq = fq_of(st)?;
            if self.anon_members(fq).is_some() {
                continue;
            }
            let (c, _) = self.classifier_for(fq)?;
            if let Classifier::Source(sym) = &c {
                // Every declared supertype must be found.
                for d in &sym.supertypes {
                    let r = self.resolve_declared(sym, d);
                    if fq_of(&r).is_none_or(|f| self.classifier_for(f).is_none()) {
                        return None;
                    }
                }
            }
        }
        Some(closure.into_iter().skip(1).collect())
    }

    /// Abstract members of `t`'s supertypes that neither `t` nor any supertype implements.
    /// Members of `java.lang`/`java.util` types are skipped (Kotlin maps them to its own
    /// collection and string types, with other names). `None`: the hierarchy is not fully known.
    pub fn unimplemented_members(&self, t: &TypeRef) -> Option<Vec<AbstractMember>> {
        let supers = self.known_supertypes(t)?;
        // Delegation (`: I by d`) and Java classes (implementing Kotlin's mapped collection
        // members under Java names) make implementations invisible here.
        for st in &supers {
            match fq_of(st).and_then(|fq| self.classifier_for(fq)).map(|(c, _)| c) {
                Some(Classifier::Library(lc)) if !lc.is_kotlin && lc.symbol.fq_name != "java.lang.Object" => return None,
                Some(Classifier::Source(sym)) if self.delegates(&sym) => return None,
                _ => {}
            }
        }
        let own_fq = fq_of(t)?.to_string();
        let own: Vec<Symbol> = match self.anon_members(&own_fq) {
            Some(m) => m,
            None => self.members_of(&self.classifier_for(&own_fq)?.0),
        };
        let mut implemented: HashSet<String> = own.iter().map(|m| m.name.clone()).collect();
        implemented.extend(["equals", "hashCode", "toString"].map(String::from));
        let mut abstract_members: Vec<AbstractMember> = Vec::new();
        for st in &supers {
            let Some(fq) = fq_of(st) else { continue };
            let Some((c, alias_args)) = self.classifier_for(fq) else { continue };
            let args = if alias_args.is_empty() { args_of(st).to_vec() } else { alias_args };
            let subst = self.type_subst(&c, &args);
            let interface = c.symbol().kind == SymbolKind::Interface;
            let mapped = fq.starts_with("java.lang.") || fq.starts_with("java.util.");
            for m in self.members_of(&c) {
                if !matches!(m.kind, SymbolKind::Function | SymbolKind::Property) || m.modifiers.has(Modifiers::STATIC) {
                    continue;
                }
                if m.modifiers.has(Modifiers::ABSTRACT) {
                    if !mapped {
                        abstract_members.push(AbstractMember { symbol: m, subst: subst.clone(), from_interface: interface });
                    }
                } else {
                    implemented.insert(m.name.clone());
                }
            }
        }
        let mut seen = HashSet::new();
        Some(abstract_members.into_iter().filter(|a| !implemented.contains(&a.symbol.name) && seen.insert(a.symbol.name.clone())).collect())
    }

    /// A source class with a delegated supertype (`: I by d`).
    fn delegates(&self, sym: &Symbol) -> bool {
        let Origin::Source { file, range, .. } = &sym.origin else { return false };
        let Some(f) = self.snap.file(file) else { return false };
        let root = f.parse().syntax();
        let Some(decl) = crate::syntax::covering(&root, *range) else { return false };
        let node = match decl {
            rowan::NodeOrToken::Node(n) => n,
            rowan::NodeOrToken::Token(t) => match t.parent() {
                Some(p) => p,
                None => return false,
            },
        };
        node.ancestors()
            .find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION))
            .is_some_and(|c| c.children().filter(|l| l.kind() == SyntaxKind::SUPER_TYPE_LIST).any(|l| l.children().any(|e| e.kind() == SyntaxKind::DELEGATED_SUPER_TYPE_ENTRY)))
    }

    /// Names of all members of `t`'s supertypes, or `None` if the hierarchy is not fully known.
    pub fn supertype_member_names(&self, t: &TypeRef) -> Option<HashSet<String>> {
        let supers = self.known_supertypes(t)?;
        let mut out = HashSet::new();
        for st in &supers {
            let Some(fq) = fq_of(st) else { continue };
            if let Some(members) = self.anon_members(fq) {
                out.extend(members.into_iter().map(|m| m.name));
                continue;
            }
            let (c, _) = self.classifier_for(fq)?;
            out.extend(self.members_of(&c).into_iter().map(|m| m.name));
        }
        Some(out)
    }

    /// Every function a call through `callee` (the call's NAME_REF) could resolve to, over
    /// all resolution levels (Kotlin falls through to outer levels when no inner candidate
    /// applies). `None` when the candidates are not fully known: local functions, values
    /// that may be invoked, unknown receivers, SAM constructors.
    pub fn call_candidates(&self, callee: &SyntaxNode) -> Option<Vec<(Symbol, Subst)>> {
        let tok = token_at(callee, SyntaxKind::IDENT)?;
        let name = ast::ident_text(&tok);
        let call = callee.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR)?;
        let mut out: Vec<(Symbol, Subst)> = Vec::new();
        let functions = |v: Vec<(Symbol, Subst)>| -> Option<Vec<(Symbol, Subst)>> {
            // A property of that name could be invoked (`invoke` operator).
            if v.iter().any(|(s, _)| !matches!(s.kind, SymbolKind::Function | SymbolKind::Constructor)) {
                return None;
            }
            Some(v)
        };
        match call.parent() {
            Some(outer) if matches!(outer.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && first_expr_child(&outer).as_ref() != Some(&call) => {
                let recv = first_expr_child(&outer)?;
                match self.resolve_qualifier(&recv) {
                    Qualifier::Value(t) => {
                        fq_of(&t)?;
                        out.extend(functions(self.members_named(&t, &name))?);
                        out.extend(functions(self.member_extensions(&recv, &t, &name))?);
                        out.extend(functions(self.extensions(&t, &name))?);
                    }
                    Qualifier::Static(c) => {
                        // Nested class constructors and companion/static functions of that name.
                        let (nested, members) = self.all_static_members(&c);
                        for (_, fq) in nested.iter().filter(|(n, _)| *n == name) {
                            let cls = self.res.find_classifier(fq)?;
                            out.extend(self.constructor_candidates(cls.symbol())?);
                        }
                        for (m, s) in members.into_iter().filter(|(m, _)| m.name == name) {
                            if !matches!(m.kind, SymbolKind::Function | SymbolKind::Constructor) {
                                return None;
                            }
                            out.push((m, s));
                        }
                    }
                    Qualifier::Package(p) => {
                        let fq = format!("{p}.{name}");
                        if let Some(c) = self.res.find_classifier(&fq) {
                            out.extend(self.constructor_candidates(c.symbol())?);
                        } else {
                            out.extend(functions(self.res.top_level_callables(&p, &name).into_iter().map(|s| (s, Subst::new())).collect())?);
                        }
                    }
                    Qualifier::Unknown => return None,
                }
            }
            Some(outer) if outer.kind() == SyntaxKind::CALLABLE_REFERENCE => return None,
            _ => {
                if self.find_local(callee, &name).is_some() {
                    return None;
                }
                for (recv, cls) in self.implicit_receivers(callee) {
                    out.extend(functions(self.members_named(&recv, &name))?);
                    if let Some(c) = &cls {
                        for d in self.static_members(c, &name) {
                            match d {
                                Def::Symbol { sym, subst } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) => out.push((sym, subst)),
                                _ => return None,
                            }
                        }
                    }
                    out.extend(functions(self.member_extensions(callee, &recv, &name))?);
                    out.extend(functions(self.extensions(&recv, &name))?);
                }
                let mut top = self.imported_callables(&name);
                for pkg in self.callable_packages() {
                    top.extend(self.res.top_level_callables(&pkg, &name));
                }
                out.extend(functions(top.into_iter().filter(|s| s.receiver.is_none()).map(|s| (s, Subst::new())).collect())?);
                let scope = scope_at(callee, &self.package(), &mut self.locals_cache.borrow_mut());
                match self.res.resolve_type_name(&[name.as_str()], &scope) {
                    Ok(Resolved::Classifier(c)) => out.extend(self.constructor_candidates(c.symbol())?),
                    Ok(Resolved::Local(_)) | Ok(Resolved::TypeParameter(_)) => return None,
                    _ => {}
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Constructors of a class used as a call; `None` for interfaces (SAM constructors),
    /// aliases and classes whose constructors are not known.
    fn constructor_candidates(&self, sym: &Symbol) -> Option<Vec<(Symbol, Subst)>> {
        if !matches!(sym.kind, SymbolKind::Class | SymbolKind::Enum) {
            return None;
        }
        // `Foo { }` may call the companion's `operator fun invoke`.
        let c = self.res.find_classifier(&sym.fq_name)?;
        if self.all_static_members(&c).1.iter().any(|(m, _)| m.name == "invoke") {
            return None;
        }
        let ctors = self.constructors(&sym.fq_name);
        if ctors.is_empty() {
            return None;
        }
        Some(ctors.into_iter().map(|c| (c, Subst::new())).collect())
    }
}

/// Whether `at` (inside `class`) sees the primary constructor's plain parameters.
fn ctor_params_visible(at: &SyntaxNode, class: &SyntaxNode) -> bool {
    let path: Vec<SyntaxNode> = at.ancestors().take_while(|a| a != class).collect();
    // The class member containing `at`, if `at` is in the body.
    let Some(i) = path.iter().position(|a| a.parent().is_some_and(|p| p.kind() == SyntaxKind::CLASS_BODY && p.parent().as_ref() == Some(class))) else { return true };
    let member = &path[i];
    match member.kind() {
        SyntaxKind::CLASS_INITIALIZER => true,
        SyntaxKind::PROPERTY => !path[..i].iter().any(|a| a.kind() == SyntaxKind::PROPERTY_ACCESSOR),
        _ => false,
    }
}
