//! Project-wide index of source declarations.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use super::source::{FileSummary, SourceDecl};
use crate::symbols::{SymbolKind, TypeRef};
use super::{FileContext, FileData, Workspace};

/// Location of a declaration: file + index path into `FileSummary::decls`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclLoc {
    pub path: Arc<Path>,
    pub decl: Vec<u32>,
    pub ctx: Option<FileContext>,
}

impl DeclLoc {
    pub fn resolve<'s>(&self, summary: &'s FileSummary) -> Option<&'s SourceDecl> {
        let mut it = self.decl.iter();
        let mut d = summary.decls.get(*it.next()? as usize)?;
        for &i in it {
            d = d.children.get(i as usize)?;
        }
        Some(d)
    }
}

#[derive(Default)]
pub struct SourceIndex {
    classifiers: HashMap<String, Vec<DeclLoc>>,
    /// package → top-level declarations
    top_level: HashMap<String, Vec<DeclLoc>>,
    /// package → contexts declaring it (including parent packages)
    packages: HashMap<String, HashSet<Option<FileContext>>>,
    shapes: HashMap<PathBuf, u64>,
    /// Every name declared in project sources (declarations, members, parameters).
    names: HashSet<String>,
    /// Names that may be used after `x.` on some receiver other than their declaring
    /// class's: extensions, and properties and parameters that may hold a function type.
    receiver_names: HashSet<String>,
    pub file_count: usize,
}

impl SourceIndex {
    pub fn build(files: &[Arc<FileData>], ws: &Workspace) -> SourceIndex {
        let summaries: Vec<(Arc<FileSummary>, Option<FileContext>)> = files.par_iter().map(|f| (f.summary(), ws.context_of(&f.path))).collect();
        let mut idx = SourceIndex { file_count: summaries.len(), ..Default::default() };
        for (s, ctx) in &summaries {
            let path: Arc<Path> = s.path.as_path().into();
            idx.shapes.insert(s.path.clone(), s.shape_hash());
            let mut pkg = s.package.as_str();
            loop {
                idx.packages.entry(pkg.to_string()).or_default().insert(*ctx);
                match pkg.rfind('.') {
                    Some(i) => pkg = &pkg[..i],
                    None => break,
                }
            }
            if !s.package.is_empty() {
                idx.packages.entry(String::new()).or_default().insert(*ctx);
            }
            fn names(d: &SourceDecl, out: &mut HashSet<String>) {
                out.insert(d.symbol.name.clone());
                out.extend(d.symbol.params.iter().map(|p| p.name.clone()));
                d.children.iter().for_each(|c| names(c, out));
            }
            s.decls.iter().for_each(|d| names(d, &mut idx.names));
            for (i, d) in s.decls.iter().enumerate() {
                let loc = DeclLoc { path: path.clone(), decl: vec![i as u32], ctx: *ctx };
                idx.top_level.entry(s.package.clone()).or_default().push(loc.clone());
                idx.add_classifiers(d, loc);
            }
        }
        // Second pass: whether a declared type may be a function type depends on the type
        // aliases of every file.
        fn aliases(d: &SourceDecl, out: &mut HashSet<String>) {
            if d.symbol.kind == SymbolKind::TypeAlias {
                out.insert(d.symbol.name.clone());
            }
            d.children.iter().for_each(|c| aliases(c, out));
        }
        fn receiver_names(d: &SourceDecl, aliases: &HashSet<String>, out: &mut HashSet<String>) {
            let s = &d.symbol;
            let property = matches!(s.kind, SymbolKind::Property | SymbolKind::Field);
            if s.receiver.is_some() || (property && s.ty.as_ref().is_none_or(|t| may_be_function(t, aliases))) {
                out.insert(s.name.clone());
            }
            out.extend(s.params.iter().filter(|p| may_be_function(&p.ty, aliases)).map(|p| p.name.clone()));
            d.children.iter().for_each(|c| receiver_names(c, aliases, out));
        }
        let mut alias_names = HashSet::new();
        summaries.iter().flat_map(|(s, _)| &s.decls).for_each(|d| aliases(d, &mut alias_names));
        summaries.iter().flat_map(|(s, _)| &s.decls).for_each(|d| receiver_names(d, &alias_names, &mut idx.receiver_names));
        idx
    }

    fn add_classifiers(&mut self, d: &SourceDecl, loc: DeclLoc) {
        if d.symbol.kind.is_classifier() {
            self.classifiers.entry(d.symbol.fq_name.clone()).or_default().push(loc.clone());
        }
        for (i, c) in d.children.iter().enumerate() {
            if c.symbol.kind.is_classifier() {
                let mut l = loc.clone();
                l.decl.push(i as u32);
                self.add_classifiers(c, l);
            }
        }
    }

    /// Some project source declares something called `name`.
    pub fn declares_name(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Some project source declares `name` so that `x.name` may resolve on a receiver that
    /// does not declare it as a member (see `receiver_names`).
    pub fn declares_receiver_name(&self, name: &str) -> bool {
        self.receiver_names.contains(name)
    }

    pub fn shape(&self, path: &Path) -> Option<u64> {
        self.shapes.get(path).copied()
    }

    pub fn classifier<'a>(&'a self, fq: &str, visible: &'a dyn Fn(Option<FileContext>) -> bool) -> impl Iterator<Item = &'a DeclLoc> + 'a {
        self.classifiers.get(fq).into_iter().flatten().filter(move |l| visible(l.ctx))
    }

    pub fn top_level<'a>(&'a self, pkg: &str, visible: &'a dyn Fn(Option<FileContext>) -> bool) -> impl Iterator<Item = &'a DeclLoc> + 'a {
        self.top_level.get(pkg).into_iter().flatten().filter(move |l| visible(l.ctx))
    }

    pub fn has_package(&self, pkg: &str, visible: &dyn Fn(Option<FileContext>) -> bool) -> bool {
        self.packages.get(pkg).is_some_and(|ctxs| ctxs.iter().any(|c| visible(*c)))
    }

    /// Direct subpackages of `pkg` visible through `visible`, as simple names.
    pub fn subpackages(&self, pkg: &str, visible: &dyn Fn(Option<FileContext>) -> bool) -> Vec<String> {
        let prefix = if pkg.is_empty() { String::new() } else { format!("{pkg}.") };
        let mut out: Vec<String> = self
            .packages
            .iter()
            .filter(|(_, ctxs)| ctxs.iter().any(|c| visible(*c)))
            .filter_map(|(p, _)| p.strip_prefix(prefix.as_str()))
            .filter(|rest| !rest.is_empty())
            .map(|rest| rest.split('.').next().unwrap().to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// All classifiers (for workspace symbol search).
    pub fn all_classifiers(&self) -> impl Iterator<Item = (&String, &Vec<DeclLoc>)> {
        self.classifiers.iter()
    }

    pub fn all_top_level(&self) -> impl Iterator<Item = (&String, &Vec<DeclLoc>)> {
        self.top_level.iter()
    }
}

/// `ty` may be a function type, whose value can be invoked as `x.name()`. Named types
/// count when a project type alias has their name (library aliases of function types are
/// rare enough to ignore).
fn may_be_function(ty: &TypeRef, aliases: &HashSet<String>) -> bool {
    match ty {
        TypeRef::Named { name, .. } => aliases.contains(name.rsplit('.').next().unwrap_or(name)),
        TypeRef::Platform(t) => may_be_function(t, aliases),
        _ => true,
    }
}
