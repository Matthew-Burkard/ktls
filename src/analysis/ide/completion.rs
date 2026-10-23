//! Code completion.
//!
//! A marker identifier is inserted at the cursor so that half-typed code (`foo.`, `bar(`)
//! parses into the shape it will have, then the syntax around the marker decides the
//! context: members of a receiver, names in scope, types, import paths, declarations
//! (keywords and `override`s), or named arguments.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use super::infer::{Def, Engine, LocalKind, Qualifier, Subst, class_type, substitute};
use crate::analysis::diagnostics::{LocalsCache, scope_at};
use crate::analysis::resolve::Resolved;
use crate::analysis::source::Language;
use crate::analysis::{FileData, Snapshot};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, TypeRef};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

/// Inserted at the cursor before parsing.
const MARKER: &str = "zzKlsCompletionMarker";
/// Upper bound on returned items (the list is then marked incomplete).
const MAX_ITEMS: usize = 1000;
/// Upper bound on not-yet-imported classes offered.
const MAX_AUTO_IMPORTS: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Variable,
    Function,
    Method,
    Property,
    Field,
    Constructor,
    Class,
    Interface,
    Object,
    Enum,
    EnumMember,
    TypeParameter,
    Keyword,
    Package,
    Snippet,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    /// Full signature.
    pub detail: Option<String>,
    /// Shown right after the label: `(a: Int, b: String)`.
    pub label_detail: Option<String>,
    /// Shown right-aligned: the type, or the package of a class to import.
    pub description: Option<String>,
    /// Text inserted instead of the label (a snippet if `snippet`).
    pub insert: Option<String>,
    pub snippet: bool,
    /// Lower sorts first: 0 named arguments, locals, 1 members, 2 extensions, 3 this
    /// package and imports, 4 default imports, 5 keywords, 6 needs an import.
    pub rank: u8,
    /// Extra edits (an import), in the original text.
    pub additional: Vec<(TextRange, String)>,
    pub deprecated: bool,
    pub doc: Option<String>,
    /// Matched instead of the label (e.g. the member name of an `override` item).
    pub filter: Option<String>,
    /// Replaces this range instead of the identifier prefix.
    pub range: Option<TextRange>,
}

#[derive(Clone, Debug)]
pub struct Completion {
    /// The identifier prefix the items replace.
    pub range: TextRange,
    pub items: Vec<Item>,
    /// More items may match as the user types (the client should ask again).
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// The client understands `$0`-style snippets.
    pub snippets: bool,
}

/// Where the cursor is.
enum Context {
    /// `recv.|`, `recv?.|`, `Type::|`.
    Member { recv: SyntaxNode, callable_ref: bool },
    /// A name in an expression; `statement`: it starts a statement.
    Scope { at: SyntaxNode, statement: bool, callable_ref: bool },
    /// A type reference; `qualifier`: segments before the cursor (`Map.|`).
    Type { at: SyntaxNode, qualifier: Vec<String> },
    /// An `import` (or `package`) path.
    Import { qualifier: Vec<String>, package: bool },
    /// A member or top-level declaration position.
    Declaration { at: SyntaxNode, in_class: bool, after_override: Option<TextSize> },
    /// The name of an `override fun` / `override val` being written.
    Override { at: SyntaxNode, start: TextSize },
}

pub fn complete(snap: &Snapshot, path: &Path, offset: TextSize, opts: Options) -> Option<Completion> {
    let file = snap.file(path)?;
    if file.language == Language::Java {
        return None;
    }
    let o = usize::from(offset);
    if o > file.text.len() || !file.text.is_char_boundary(o) {
        return None;
    }
    if let Some(c) = kdoc_completion(snap, file, offset, opts) {
        return Some(c);
    }
    if let Some(c) = label_completion(file, offset) {
        return Some(c);
    }
    // Parse the text with the marker; the rest of the snapshot is unchanged.
    let text = format!("{}{MARKER}{}", &file.text[..o], &file.text[o..]);
    let data = FileData::new(path.to_path_buf(), Arc::from(text.as_str()), file.version)?;
    let mut files = (*snap.files).clone();
    files.insert(path.to_path_buf(), Arc::new(data));
    let marked = Snapshot { workspace: snap.workspace.clone(), files: Arc::new(files), index: snap.index.clone(), lints: snap.lints.clone() };
    let e = Engine::new(&marked, path)?;
    let marker_range = TextRange::at(offset, TextSize::from(MARKER.len() as u32));
    let tok = match e.root.covering_element(marker_range) {
        rowan::NodeOrToken::Token(t) if t.kind() == SyntaxKind::IDENT => t,
        _ => return None,
    };
    let start = tok.text_range().start() + TextSize::from(u32::from(tok.text().starts_with('`')));
    let prefix = text[usize::from(start)..o].to_string();
    let next_char = file.text[o..].chars().find(|c| !c.is_alphanumeric() && *c != '_');
    let context = classify(&tok, &text)?;
    let mut c = Collector { e: &e, file, prefix, opts, next_char, offset, items: Vec::new(), seen: HashSet::new(), truncated: false };
    match context {
        Context::Member { recv, callable_ref } => c.members(&recv, &tok, callable_ref),
        Context::Scope { at, statement, callable_ref } => c.scope(&at, statement, callable_ref),
        Context::Type { at, qualifier } => c.types(&at, &qualifier),
        Context::Import { qualifier, package } => c.import(&qualifier, package),
        Context::Declaration { at, in_class, after_override } => c.declaration(&at, in_class, after_override),
        Context::Override { at, start } => c.overrides(&at, Some(start)),
    }
    let empty_prefix = c.prefix.is_empty();
    let truncated = c.truncated || c.items.len() > MAX_ITEMS;
    let first = c.prefix.chars().next();
    let mut items = c.items;
    // Items whose first letter has the case the user typed come first (`Jo` → `Join`).
    let key = |i: &Item| {
        let name = i.filter.as_deref().unwrap_or(&i.label);
        let case_miss = first.is_some_and(|f| !name.starts_with(f));
        (case_miss, i.rank, i.label.to_lowercase())
    };
    items.sort_by_cached_key(key);
    items.truncate(MAX_ITEMS);
    Some(Completion { range: TextRange::new(start, offset), items, incomplete: truncated || empty_prefix })
}

/// Names in a KDoc link (`[Na|`, `[Type.me|`, `@param |`): the declaration's parameters,
/// names in its scope, or members of the qualifier. `None` outside a link.
fn kdoc_completion(snap: &Snapshot, file: &Arc<FileData>, offset: TextSize, opts: Options) -> Option<Completion> {
    use super::kdoc;
    let root = file.parse().syntax();
    let tok = kdoc::kdoc_at(&root, offset)?;
    let link = kdoc::link_at(&tok, offset, true)?;
    let e = Engine::new(snap, &file.path)?;
    // The engine has its own tree: find the same declaration there.
    let decl = e.root.covering_element(link.decl.text_range()).ancestors().find(|a| a.text_range() == link.decl.text_range() && a.kind() == link.decl.kind())?;
    let (prefix, range) = link.segments.last()?.clone();
    let range = TextRange::new(range.start(), offset);
    let mut c = Collector { e: &e, file, prefix: prefix[..usize::from(range.len()).min(prefix.len())].to_string(), opts, next_char: None, offset, items: Vec::new(), seen: HashSet::new(), truncated: false };
    let at = kdoc::anchor(&decl);
    if link.segments.len() == 1 {
        for (name, _) in kdoc::parameters(&decl) {
            let mut item = Collector::item(&name, ItemKind::Variable, 0);
            item.detail = Some("parameter".into());
            c.push(item);
        }
        c.scope(&at, false, true);
        c.types(&at, &[]);
    } else {
        let qualifier = kdoc::Link { segments: link.segments[..link.segments.len() - 1].to_vec(), decl };
        match kdoc::resolve(&e, &qualifier).first() {
            Some(Def::Package(p)) => c.package_contents(p, 3, false, false),
            Some(Def::Symbol { sym, .. }) if sym.kind.is_classifier() => {
                let cls = e.classifier(&sym.fq_name)?;
                let (nested, statics) = e.all_static_members(&cls);
                for (n, fq) in nested {
                    c.classifier(&n, &fq, 1, false);
                }
                for (m, s) in statics.into_iter().chain(e.all_members(&class_type(cls.symbol()))) {
                    c.symbol(&m, &s, 1, true, false);
                }
            }
            _ => {}
        }
    }
    // Names only: no imports, parentheses or snippets.
    let mut items: Vec<Item> = c.items.into_iter().filter(|i| i.additional.is_empty()).collect();
    for i in &mut items {
        i.insert = None;
        i.snippet = false;
        i.range = None;
    }
    items.sort_by_cached_key(|i| (i.rank, i.label.to_lowercase()));
    items.dedup_by(|a, b| a.label == b.label);
    Some(Completion { range, items, incomplete: c.truncated })
}

fn classify(tok: &SyntaxToken, text: &str) -> Option<Context> {
    let parent = tok.parent()?;
    match parent.kind() {
        SyntaxKind::NAME_REF => {
            let expr = match parent.parent() {
                Some(p) if p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(&parent) => p,
                _ => parent.clone(),
            };
            let outer = expr.parent()?;
            match outer.kind() {
                SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR if outer.first_child().as_ref() != Some(&expr) => {
                    Some(Context::Member { recv: outer.first_child()?, callable_ref: false })
                }
                SyntaxKind::CALLABLE_REFERENCE => match outer.first_child().filter(|r| *r != parent) {
                    Some(recv) => Some(Context::Member { recv, callable_ref: true }),
                    None => Some(Context::Scope { at: parent, statement: false, callable_ref: true }),
                },
                k => Some(Context::Scope { at: parent, statement: matches!(k, SyntaxKind::BLOCK | SyntaxKind::SCRIPT_BODY), callable_ref: false }),
            }
        }
        SyntaxKind::USER_TYPE_SEGMENT => {
            let ut = parent.parent()?;
            let qualifier = ut
                .children()
                .filter(|s| s.kind() == SyntaxKind::USER_TYPE_SEGMENT)
                .take_while(|s| *s != parent)
                .filter_map(|s| ast::token(&s, SyntaxKind::IDENT))
                .map(|t| ast::ident_text(&t))
                .collect();
            Some(Context::Type { at: parent, qualifier })
        }
        SyntaxKind::DOT_QUALIFIED_NAME => {
            let directive = parent.ancestors().find(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE))?;
            let qualifier = parent
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SyntaxKind::IDENT && t.text_range().end() <= tok.text_range().start())
                .map(|t| ast::ident_text(&t))
                .collect();
            Some(Context::Import { qualifier, package: directive.kind() == SyntaxKind::PACKAGE_DIRECTIVE })
        }
        SyntaxKind::ERROR | SyntaxKind::CLASS_BODY | SyntaxKind::KOTLIN_FILE => {
            let in_class = tok.parent_ancestors().any(|a| a.kind() == SyntaxKind::CLASS_BODY);
            // `override na|` on one line: the item replaces from `override`.
            let line_start = text[..usize::from(tok.text_range().start())].rfind('\n').map_or(0, |i| i + 1);
            let before = &text[line_start..usize::from(tok.text_range().start())];
            let after_override = before.trim_end().ends_with("override").then(|| TextSize::from((line_start + before.find("override").unwrap()) as u32));
            Some(Context::Declaration { at: parent, in_class, after_override })
        }
        SyntaxKind::FUN | SyntaxKind::PROPERTY if ast::token(&parent, SyntaxKind::IDENT).as_ref() == Some(tok) => {
            let m = ast::child::<ast::ModifierList>(&parent)?;
            let ov = m.modifiers().find(|t| t.text() == "override")?;
            Some(Context::Override { at: parent.clone(), start: ov.text_range().start() })
        }
        _ => None,
    }
}

struct Collector<'a, 's> {
    e: &'a Engine<'s>,
    /// The file as it is (without the marker).
    file: &'a Arc<FileData>,
    prefix: String,
    opts: Options,
    /// The first non-identifier character after the cursor (`(` means: no parentheses).
    next_char: Option<char>,
    /// The cursor.
    offset: TextSize,
    items: Vec<Item>,
    seen: HashSet<(String, Option<String>, Option<String>)>,
    truncated: bool,
}

/// `prefix` matches `name` (case-insensitively): the first letters agree, and every
/// further letter continues the current word or, if it is uppercase (or the whole prefix
/// is lowercase), starts a later word ("camel humps"). So `fBN` and `fbn` find
/// `findByName`, but `Jo` does not find `JvmOverloads`.
pub fn matches(prefix: &str, name: &str) -> bool {
    let all_lower = !prefix.chars().any(char::is_uppercase);
    let jumps: Vec<bool> = prefix.chars().map(|c| all_lower || c.is_uppercase()).collect();
    let p: Vec<char> = prefix.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    if p.is_empty() {
        return true;
    }
    let n: Vec<char> = name.chars().collect();
    let lower: Vec<char> = n.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    // Word starts: the first character, uppercase letters, and characters after `_`.
    let hump = |i: usize| i == 0 || n[i].is_uppercase() || n[i - 1] == '_' || n[i].is_ascii_digit() && !n[i - 1].is_ascii_digit();
    fn go(p: &[char], jumps: &[bool], lower: &[char], i: usize, hump: &dyn Fn(usize) -> bool) -> bool {
        let Some((&c, rest)) = p.split_first() else { return true };
        // Continue the current word...
        if i < lower.len() && lower[i] == c && go(rest, &jumps[1..], lower, i + 1, hump) {
            return true;
        }
        // ...or jump to a later word start.
        jumps[0] && (i + 1..lower.len()).any(|j| hump(j) && lower[j] == c && go(rest, &jumps[1..], lower, j + 1, hump))
    }
    !lower.is_empty() && lower[0] == p[0] && go(&p[1..], &jumps[1..], &lower, 1, &hump)
}

impl Collector<'_, '_> {
    fn push(&mut self, item: Item) {
        let key = item.filter.clone().unwrap_or_else(|| item.label.clone());
        if !matches(&self.prefix, &key) {
            return;
        }
        // Overloads of package-level functions collapse into one item; members keep theirs.
        let overload = if item.rank >= 3 && item.kind == ItemKind::Function { None } else { item.label_detail.clone() };
        if self.seen.insert((item.label.clone(), overload, item.additional.first().map(|(_, t)| t.clone()))) {
            self.items.push(item);
        }
    }

    /// Whether `sym` can be used from this file.
    fn visible(&self, sym: &Symbol) -> bool {
        use crate::symbols::Visibility as V;
        let source = matches!(&sym.origin, Origin::Source { .. });
        match sym.visibility {
            V::Private => matches!(&sym.origin, Origin::Source { file, .. } if *file == self.e.path),
            // Internal declarations of this project (modules are not distinguished).
            V::Internal => source,
            V::PackagePrivate => source && package_of(&sym.fq_name) == self.e.package(),
            V::Public | V::Protected => true,
        }
    }

    fn item(label: &str, kind: ItemKind, rank: u8) -> Item {
        Item {
            label: label.to_string(),
            kind,
            detail: None,
            label_detail: None,
            description: None,
            insert: None,
            snippet: false,
            rank,
            additional: Vec::new(),
            deprecated: false,
            doc: None,
            filter: None,
            range: None,
        }
    }

    fn type_text(&self, sym: &Symbol, t: &TypeRef, subst: &Subst) -> String {
        substitute(&self.e.resolve_declared(sym, t), subst).display()
    }

    /// An item for a declaration. `call`: complete with parentheses / a lambda.
    fn symbol(&mut self, sym: &Symbol, subst: &Subst, rank: u8, member: bool, call: bool) {
        if !self.visible(sym) || sym.name.is_empty() || sym.name.starts_with('<') {
            return;
        }
        let kind = match sym.kind {
            SymbolKind::Function if member || sym.receiver.is_some() => ItemKind::Method,
            SymbolKind::Function => ItemKind::Function,
            SymbolKind::Constructor => ItemKind::Constructor,
            SymbolKind::Property if member || sym.receiver.is_some() => ItemKind::Property,
            SymbolKind::Property => ItemKind::Variable,
            SymbolKind::Field => ItemKind::Field,
            SymbolKind::Interface | SymbolKind::Annotation => ItemKind::Interface,
            SymbolKind::Object => ItemKind::Object,
            SymbolKind::Enum => ItemKind::Enum,
            SymbolKind::EnumEntry => ItemKind::EnumMember,
            SymbolKind::TypeAlias | SymbolKind::Class => ItemKind::Class,
            _ => ItemKind::Variable,
        };
        let mut item = Self::item(&sym.name, kind, rank);
        item.detail = Some(sym.signature());
        item.deprecated = sym.modifiers.has(Modifiers::DEPRECATED);
        item.doc = sym.doc.as_deref().map(super::render_doc);
        match sym.kind {
            SymbolKind::Function | SymbolKind::Constructor => {
                let params: Vec<String> = sym.params.iter().map(|p| format!("{}{}: {}", if p.vararg { "vararg " } else { "" }, p.name, self.type_text(sym, &p.ty, subst))).collect();
                item.label_detail = Some(format!("({})", params.join(", ")));
                item.description = sym.ty.as_ref().map(|t| self.type_text(sym, t, subst));
                if let Some(r) = &sym.receiver {
                    item.label_detail = Some(format!("({}) for {}", params.join(", "), self.type_text(sym, r, subst)));
                }
                if call {
                    self.call_insertion(&mut item, sym);
                }
            }
            SymbolKind::Property | SymbolKind::Field | SymbolKind::EnumEntry => {
                item.description = sym.ty.as_ref().map(|t| self.type_text(sym, t, subst));
                if let Some(r) = sym.receiver.as_ref().filter(|_| sym.kind == SymbolKind::Property) {
                    item.label_detail = Some(format!(" for {}", self.type_text(sym, r, subst)));
                }
            }
            _ => item.description = Some(package_of(&sym.fq_name)),
        }
        self.push(item);
    }

    /// `name()`, `name($0)` or `name { $0 }`.
    fn call_insertion(&self, item: &mut Item, sym: &Symbol) {
        if matches!(self.next_char, Some('(') | Some('{') | Some('<')) {
            return;
        }
        let name = &item.label;
        let required: Vec<_> = sym.params.iter().filter(|p| !p.has_default && !p.vararg).collect();
        let trailing_lambda = sym.params.last().is_some_and(|p| is_function_type(&p.ty)) && required.len() == 1 && required[0].name == sym.params.last().unwrap().name;
        let (snippet, plain) = if sym.params.is_empty() {
            (format!("{name}()$0"), format!("{name}()"))
        } else if trailing_lambda {
            (format!("{name} {{ $0 }}"), format!("{name} {{ }}"))
        } else {
            (format!("{name}($0)"), format!("{name}("))
        };
        if self.opts.snippets {
            item.insert = Some(snippet);
            item.snippet = true;
        } else {
            item.insert = Some(plain);
        }
    }

    /// A classifier known by FQ name (parsed only when its name matches).
    fn classifier(&mut self, name: &str, fq: &str, rank: u8, import: bool) {
        if !matches(&self.prefix, name) {
            return;
        }
        let Some(c) = self.e.res.find_classifier(fq) else { return };
        let sym = c.symbol();
        if import && sym.visibility != crate::symbols::Visibility::Public || !self.visible(sym) {
            return;
        }
        let mut sym = sym.clone();
        sym.name = name.to_string();
        let before = self.items.len();
        self.symbol(&sym, &Subst::new(), rank, false, false);
        if import && self.items.len() > before {
            let item = self.items.last_mut().unwrap();
            item.additional = vec![import_edit(self.file, fq)];
            item.description = Some(package_of(fq));
        }
    }

    fn keywords(&mut self, words: &[&str]) {
        for w in words {
            let mut item = Self::item(w, ItemKind::Keyword, 5);
            if !matches!(*w, "true" | "false" | "null" | "this" | "super" | "break" | "continue" | "return" | "init" | "else") {
                item.insert = Some(format!("{w} "));
            }
            self.push(item);
        }
    }

    // ---------------------------------------------------------------- contexts

    fn members(&mut self, recv: &SyntaxNode, at: &SyntaxToken, callable_ref: bool) {
        let call = !callable_ref;
        match self.e.resolve_qualifier(recv) {
            Qualifier::Value(t) => {
                for (m, s) in self.e.all_members(&t) {
                    self.symbol(&m, &s, 1, true, call);
                }
                if call && self.opts.snippets {
                    self.postfix(recv, at, &t);
                }
                let at = at.parent().unwrap();
                for (m, s) in self.e.all_extensions(&at, &t) {
                    self.symbol(&m, &s, 2, true, call);
                }
                if call {
                    self.unimported_extensions(&t);
                }
            }
            Qualifier::Static(c) => {
                if callable_ref {
                    self.keywords(&["class"]);
                }
                let (nested, members) = self.e.all_static_members(&c);
                for (n, fq) in nested {
                    self.classifier(&n, &fq, 1, false);
                }
                for (m, s) in members {
                    self.symbol(&m, &s, 1, true, call);
                }
                // `Type::member` also refers to instance members.
                if callable_ref {
                    for (m, s) in self.e.all_members(&class_type(c.symbol())) {
                        self.symbol(&m, &s, 1, true, false);
                    }
                }
            }
            Qualifier::Package(p) => self.package_contents(&p, 3, call, false),
            Qualifier::Unknown => {}
        }
    }

    /// `a, b ->` at the start of a lambda without parameters, from its expected type.
    fn lambda_parameters(&mut self, at: &SyntaxNode) {
        let Some(body) = at.parent().filter(|b| b.kind() == SyntaxKind::BLOCK && b.first_child().as_ref() == Some(at)) else { return };
        let Some(lambda) = body.parent().filter(|l| l.kind() == SyntaxKind::LAMBDA_EXPR) else { return };
        if lambda.children_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::ARROW | SyntaxKind::VALUE_PARAMETER_LIST)) {
            return;
        }
        let Some((_, params)) = self.e.lambda_signature(&lambda) else { return };
        if params.is_empty() {
            return;
        }
        // The called function, for conventional names (`index`, `acc`).
        let callee = lambda
            .ancestors()
            .skip(1)
            .find(|a| a.kind() == SyntaxKind::CALL_EXPR)
            .and_then(|c| c.first_child())
            .filter(|c| c.kind() == SyntaxKind::NAME_REF)
            .map(|c| c.text().to_string())
            .unwrap_or_default();
        let acc = match callee.as_str() {
            "fold" | "reduce" | "runningFold" | "runningReduce" | "scan" => Some(0),
            "foldIndexed" | "reduceIndexed" | "runningFoldIndexed" | "runningReduceIndexed" | "scanIndexed" => Some(1),
            "foldRight" | "reduceRight" => Some(1),
            "foldRightIndexed" | "reduceRightIndexed" => Some(2),
            _ => None,
        };
        let mut names: Vec<String> = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let int = super::infer::strip(p).fq_name() == Some("kotlin.Int");
            let base = if acc == Some(i) {
                "acc".to_string()
            } else if i == 0 && int && callee.ends_with("Indexed") {
                "index".to_string()
            } else {
                suggested_name(super::infer::strip(p))
            };
            let mut name = base.clone();
            let mut i = 2;
            while names.contains(&name) {
                name = format!("{base}{i}");
                i += 1;
            }
            names.push(name);
        }
        let types: Vec<String> = params.iter().map(|p| p.display()).collect();
        let mut offer = |label: String, snippet: String, rank: u8| {
            let mut item = Self::item(&label, ItemKind::Snippet, rank);
            item.description = Some(format!("({}) ->", types.join(", ")));
            item.detail = Some("lambda parameters".into());
            if self.opts.snippets {
                item.insert = Some(snippet);
                item.snippet = true;
            } else {
                item.insert = Some(format!("{label} "));
            }
            self.push(item);
        };
        let tabstops: Vec<String> = names.iter().enumerate().map(|(i, n)| format!("${{{}:{n}}}", i + 1)).collect();
        offer(format!("{} ->", names.join(", ")), format!("{} -> ", tabstops.join(", ")), 0);
        // Destructuring for one pair-like parameter.
        if let [p] = params.as_slice() {
            let parts = match super::infer::strip(p).fq_name() {
                Some("kotlin.collections.Map.Entry" | "kotlin.collections.MutableMap.MutableEntry") => Some(["key", "value"]),
                Some("kotlin.Pair") => Some(["first", "second"]),
                _ => None,
            };
            if let Some([a, b]) = parts {
                offer(format!("({a}, {b}) ->"), format!("(${{1:{a}}}, ${{2:{b}}}) -> "), 0);
            }
        }
    }

    /// Postfix templates (`cond.if`, `xs.for`, `expr.val`): the snippet replaces the
    /// template name, and an extra edit removes `recv.` before it.
    fn postfix(&mut self, recv: &SyntaxNode, at: &SyntaxToken, t: &TypeRef) {
        let Some(outer) = recv.parent().filter(|p| p.kind() == SyntaxKind::DOT_QUALIFIED_EXPR) else { return };
        let marker = match at.parent().and_then(|n| n.parent()) {
            Some(c) if c.kind() == SyntaxKind::CALL_EXPR => c,
            _ => at.parent().unwrap(),
        };
        // `recv.name` exactly, not `recv.name()` or `recv.name.more`.
        if marker.kind() == SyntaxKind::CALL_EXPR || outer.children().last().as_ref() != Some(&marker) {
            return;
        }
        let statement = outer.parent().is_some_and(|p| matches!(p.kind(), SyntaxKind::BLOCK | SyntaxKind::SCRIPT_BODY));
        let expr = snippet_escape(&recv.text().to_string());
        let simple = matches!(recv.kind(), SyntaxKind::NAME_REF | SyntaxKind::CALL_EXPR | SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR | SyntaxKind::PAREN_EXPR | SyntaxKind::THIS_EXPR)
            || format!("{:?}", recv.kind()).ends_with("LITERAL");
        let stripped = super::infer::strip(t);
        let fq = stripped.fq_name().unwrap_or_default();
        let nullable = matches!(t, TypeRef::Platform(_)) || matches!(stripped, TypeRef::Named { nullable: true, .. } | TypeRef::TypeParam { nullable: true, .. } | TypeRef::Function { nullable: true, .. });
        let boolean = fq == "kotlin.Boolean" && !nullable;
        let unit = matches!(fq, "kotlin.Unit" | "kotlin.Nothing");
        let iterable = !nullable && self.e.supertype_closure(t).iter().filter_map(|s| s.fq_name()).any(|f| {
            matches!(f, "kotlin.collections.Iterable" | "kotlin.collections.Map" | "kotlin.sequences.Sequence" | "kotlin.CharSequence" | "kotlin.Array")
                || f.starts_with("kotlin.") && f.ends_with("Array") && !f[7..].contains('.')
        });
        let name = suggested_name(stripped);
        let mut templates: Vec<(&str, String, &str)> = Vec::new();
        if statement && !unit {
            templates.push(("val", format!("val ${{1:{name}}} = {expr}$0"), "val name = expr"));
            templates.push(("var", format!("var ${{1:{name}}} = {expr}$0"), "var name = expr"));
        }
        if statement {
            templates.push(("return", format!("return {expr}$0"), "return expr"));
        }
        if boolean {
            if statement {
                templates.push(("if", format!("if ({expr}) {{\n    $0\n}}"), "if (expr) {}"));
                templates.push(("while", format!("while ({expr}) {{\n    $0\n}}"), "while (expr) {}"));
            }
            let not = if simple { format!("!{expr}$0") } else { format!("!({expr})$0") };
            templates.push(("not", not, "!expr"));
        }
        if nullable && statement {
            templates.push(("notnull", format!("if ({expr} != null) {{\n    $0\n}}"), "if (expr != null) {}"));
            templates.push(("null", format!("if ({expr} == null) {{\n    $0\n}}"), "if (expr == null) {}"));
        }
        if iterable && statement {
            let element = match stripped {
                TypeRef::Named { args, .. } if args.len() == 1 => match &args[0] {
                    crate::symbols::TypeArg::Type { ty, .. } => Some(suggested_name(super::infer::strip(ty))),
                    _ => None,
                },
                _ => None,
            };
            let item = if fq == "kotlin.collections.Map" || fq.ends_with("Map") {
                "(key, value)".to_string()
            } else {
                format!("${{1:{}}}", element.filter(|e| e != "value").unwrap_or_else(|| "item".into()))
            };
            templates.push(("for", format!("for ({item} in {expr}) {{\n    $0\n}}"), "for (item in expr) {}"));
        }
        if !unit {
            templates.push(("when", format!("when ({expr}) {{\n    $0\n}}"), "when (expr) {}"));
            templates.push(("par", format!("({expr})$0"), "(expr)"));
            templates.push(("arg", format!("${{1:call}}({expr})$0"), "call(expr)"));
        }
        if statement && !unit {
            templates.push(("println", format!("println({expr})$0"), "println(expr)"));
        }
        if statement {
            templates.push(("try", format!("try {{\n    {expr}$0\n}} catch (e: Exception) {{\n    \n}}"), "try { expr } catch"));
        }
        let remove = TextRange::new(recv.text_range().start(), at.text_range().start());
        for (label, insert, shown) in templates {
            if !matches(&self.prefix, label) {
                continue;
            }
            let mut item = Self::item(label, ItemKind::Snippet, 6);
            item.description = Some(shown.to_string());
            item.detail = Some(format!("postfix: {shown}"));
            item.insert = Some(insert);
            item.snippet = true;
            item.additional = vec![(remove, String::new())];
            self.items.push(item);
        }
    }

    /// Subpackages, classes and top-level callables of `pkg`.
    fn package_contents(&mut self, pkg: &str, rank: u8, call: bool, extensions: bool) {
        for sub in self.e.res.subpackages(pkg) {
            let mut item = Self::item(&sub, ItemKind::Package, rank);
            item.description = Some(if pkg.is_empty() { sub.clone() } else { format!("{pkg}.{sub}") });
            self.push(item);
        }
        let (classes, callables) = self.e.res.package_contents(pkg);
        for (n, fq) in classes {
            self.classifier(&n, &fq, rank, false);
        }
        for s in callables.iter().filter(|s| extensions || s.receiver.is_none()) {
            self.symbol(s, &Subst::new(), rank, false, call);
        }
    }

    fn scope(&mut self, at: &SyntaxNode, statement: bool, callable_ref: bool) {
        let call = !callable_ref;
        self.named_arguments(at);
        self.when_branches(at);
        if statement {
            self.lambda_parameters(at);
        }
        for l in self.e.locals_at(at) {
            let kind = match l.kind {
                LocalKind::Function => ItemKind::Function,
                LocalKind::Class => ItemKind::Class,
                _ => ItemKind::Variable,
            };
            let mut item = Self::item(&l.name, kind, 0);
            item.description = l.ty.as_ref().map(|t| t.display());
            item.detail = Some(format!("{} {}{}", match l.kind {
                LocalKind::Var => "var",
                LocalKind::Function => "fun",
                LocalKind::Class => "class",
                LocalKind::Val => "val",
                _ => "parameter",
            }, l.name, l.ty.as_ref().map(|t| format!(": {}", t.display())).unwrap_or_default()));
            self.push(item);
        }
        // Members of implicit receivers (`this`, lambda receivers) and their extensions.
        let receivers = self.e.implicit_receivers(at);
        let candidates = self.e.extension_candidates(at);
        for (i, (t, cls)) in receivers.iter().enumerate() {
            let rank = if i == 0 { 1 } else { 2 };
            for (m, s) in self.e.all_members(t) {
                self.symbol(&m, &s, rank, true, call);
            }
            if let Some(c) = cls {
                let (nested, statics) = self.e.all_static_members(c);
                for (n, fq) in nested {
                    self.classifier(&n, &fq, rank, false);
                }
                for (m, s) in statics {
                    self.symbol(&m, &s, rank, true, call);
                }
            }
            for (m, s) in self.e.applicable_to(candidates.clone(), t) {
                self.symbol(&m, &s, 2, true, call);
            }
        }
        // Imported and package-level declarations.
        let (classes, callables) = self.e.imported_symbols();
        for (n, fq) in classes {
            self.classifier(&n, &fq, 3, false);
        }
        for s in callables.iter().filter(|s| s.receiver.is_none()) {
            self.symbol(s, &Subst::new(), 3, false, call);
        }
        let own = self.e.package();
        for pkg in self.e.scope_packages() {
            let rank = if pkg == own || !crate::analysis::resolve::KOTLIN_DEFAULT_IMPORTS.contains(&pkg.as_str()) { 3 } else { 4 };
            let (classes, callables) = self.e.res.package_contents(&pkg);
            for (n, fq) in classes {
                self.classifier(&n, &fq, rank, false);
            }
            for s in callables.iter().filter(|s| s.receiver.is_none()) {
                self.symbol(s, &Subst::new(), rank, false, call);
            }
        }
        // Root packages start qualified names (`java.util.UUID`).
        if !self.prefix.is_empty() {
            for p in self.e.res.subpackages("") {
                let mut item = Self::item(&p, ItemKind::Package, 5);
                item.description = Some(p.clone());
                self.push(item);
            }
        }
        if !callable_ref {
            if statement {
                self.keywords(&["val", "var", "fun", "if", "when", "for", "while", "do", "return", "try", "throw", "break", "continue", "object", "class", "this", "super", "null", "true", "false"]);
            } else {
                self.keywords(&["if", "when", "try", "object", "this", "super", "null", "true", "false", "throw", "return"]);
            }
            // Labeled jumps and receivers: `return@forEach`, `break@outer`, `this@Outer`.
            for (keyword, label) in labels(at) {
                if !statement && matches!(keyword, "break" | "continue") {
                    continue;
                }
                let mut item = Self::item(&format!("{keyword}@{label}"), ItemKind::Keyword, 5);
                item.insert = Some(format!("{keyword}@{label}"));
                self.push(item);
            }
        }
        self.auto_import(true);
    }

    /// In a `when` branch condition: the conditions still missing for an exhaustive `when`
    /// (enum entries, sealed subclasses, `true`/`false`, `null`), one by one or all at once.
    fn when_branches(&mut self, at: &SyntaxNode) {
        let Some(entry) = at.parent().filter(|p| p.kind() == SyntaxKind::WHEN_CONDITION).and_then(|c| c.parent()).filter(|e| e.kind() == SyntaxKind::WHEN_ENTRY) else { return };
        let Some(when) = entry.parent().filter(|w| w.kind() == SyntaxKind::WHEN_EXPR) else { return };
        let Some(subject) = when.children().find(|c| c.kind() == SyntaxKind::WHEN_SUBJECT) else { return };
        let Some(missing) = crate::analysis::lints::missing_branches(self.e, &when, &subject, Some(&entry)) else { return };
        if missing.is_empty() {
            return;
        }
        let indent = crate::analysis::lints::indent_at(&self.file.text, at.text_range().start());
        if missing.len() > 1 {
            let mut item = Self::item("Add remaining branches", ItemKind::Snippet, 0);
            item.filter = Some(self.prefix.clone());
            item.detail = Some(missing.join(", "));
            item.insert = Some(missing.iter().map(|m| format!("{m} -> TODO()")).collect::<Vec<_>>().join(&format!("\n{indent}")));
            self.push(item);
        }
        for m in &missing {
            let mut item = Self::item(m, ItemKind::Keyword, 0);
            item.insert = Some(format!("{m} -> "));
            self.push(item);
        }
    }

    /// `name =` items for the parameters of the call around `at`.
    fn named_arguments(&mut self, at: &SyntaxNode) {
        let Some(arg) = at.parent().filter(|p| p.kind() == SyntaxKind::VALUE_ARGUMENT) else { return };
        if arg.children().any(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME) {
            return;
        }
        let Some(list) = arg.parent() else { return };
        let Some(call) = list.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR) else { return };
        let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
        let used: HashSet<String> = list
            .children()
            .filter_map(|a| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME))
            .filter_map(|n| ast::token(&n, SyntaxKind::IDENT))
            .map(|t| ast::ident_text(&t))
            .collect();
        let mut callables = Vec::new();
        for d in self.e.resolve_name_ref(&callee) {
            let Def::Symbol { sym, subst } = d else { continue };
            if sym.kind.is_classifier() {
                callables.extend(self.e.constructors(&sym.fq_name).into_iter().map(|c| (c, subst.clone())));
            } else if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) {
                callables.push((sym, subst));
            }
        }
        for (sym, subst) in callables {
            for p in sym.params.iter().filter(|p| !used.contains(&p.name)) {
                let mut item = Self::item(&format!("{} =", p.name), ItemKind::Variable, 0);
                item.filter = Some(p.name.clone());
                item.description = Some(self.type_text(&sym, &p.ty, &subst));
                item.insert = Some(format!("{} = ", p.name));
                self.push(item);
            }
        }
    }

    fn types(&mut self, at: &SyntaxNode, qualifier: &[String]) {
        let package = self.e.package();
        let scope = scope_at(at, &package, &mut LocalsCache::default());
        if !qualifier.is_empty() {
            let segs: Vec<&str> = qualifier.iter().map(String::as_str).collect();
            match self.e.res.resolve_type_name(&segs, &scope) {
                Ok(Resolved::Classifier(c)) => {
                    for (n, fq) in self.e.all_static_members(&c).0 {
                        self.classifier(&n, &fq, 1, false);
                    }
                }
                Ok(Resolved::Package(p)) => {
                    for sub in self.e.res.subpackages(&p) {
                        self.push(Self::item(&sub, ItemKind::Package, 3));
                    }
                    for (n, fq) in self.e.res.package_contents(&p).0 {
                        self.classifier(&n, &fq, 3, false);
                    }
                }
                _ => {
                    // An unresolved first segment may still be a package (`java.ut|`).
                    let p = qualifier.join(".");
                    for sub in self.e.res.subpackages(&p) {
                        self.push(Self::item(&sub, ItemKind::Package, 3));
                    }
                    for (n, fq) in self.e.res.package_contents(&p).0 {
                        self.classifier(&n, &fq, 3, false);
                    }
                }
            }
            return;
        }
        for tp in &scope.type_params {
            let mut item = Self::item(tp, ItemKind::TypeParameter, 0);
            item.detail = Some(format!("type parameter {tp}"));
            self.push(item);
        }
        for l in &scope.locals {
            self.push(Self::item(l, ItemKind::Class, 0));
        }
        for fq in scope.enclosing.iter().rev() {
            if let Some(c) = self.e.res.find_classifier(fq) {
                for (n, nfq) in self.e.all_static_members(&c).0 {
                    self.classifier(&n, &nfq, 1, false);
                }
            }
        }
        for (n, fq) in self.e.imported_symbols().0 {
            self.classifier(&n, &fq, 3, false);
        }
        let own = self.e.package();
        for pkg in self.e.scope_packages() {
            let rank = if pkg == own || !crate::analysis::resolve::KOTLIN_DEFAULT_IMPORTS.contains(&pkg.as_str()) { 3 } else { 4 };
            for (n, fq) in self.e.res.package_contents(&pkg).0 {
                self.classifier(&n, &fq, rank, false);
            }
        }
        if !self.prefix.is_empty() {
            for p in self.e.res.subpackages("") {
                self.push(Self::item(&p, ItemKind::Package, 5));
            }
        }
        self.auto_import(false);
    }

    fn import(&mut self, qualifier: &[String], package: bool) {
        let pkg = qualifier.join(".");
        if package {
            for sub in self.e.res.subpackages(&pkg) {
                self.push(Self::item(&sub, ItemKind::Package, 3));
            }
            return;
        }
        if let Some(c) = (!pkg.is_empty()).then(|| self.e.res.find_classifier(&pkg)).flatten() {
            let (nested, members) = self.e.all_static_members(&c);
            for (n, fq) in nested {
                self.classifier(&n, &fq, 1, false);
            }
            for (m, s) in members {
                self.symbol(&m, &s, 1, true, false);
            }
            return;
        }
        self.package_contents(&pkg, 3, false, true);
        if !qualifier.is_empty() {
            self.keywords(&["*"]);
        }
    }

    fn declaration(&mut self, at: &SyntaxNode, in_class: bool, after_override: Option<TextSize>) {
        if after_override.is_some() {
            self.overrides(at, after_override);
            return;
        }
        if in_class {
            self.overrides(at, None);
            self.keywords(&[
                "fun", "val", "var", "class", "interface", "object", "enum", "data", "sealed", "abstract", "open", "override", "private", "protected", "internal", "public", "companion", "init", "constructor", "inner", "lateinit", "const", "suspend", "operator", "infix", "inline", "value", "annotation", "typealias",
            ]);
        } else {
            self.keywords(&[
                "fun", "val", "var", "class", "interface", "object", "enum", "data", "sealed", "abstract", "open", "private", "internal", "public", "const", "lateinit", "suspend", "operator", "infix", "inline", "value", "annotation", "typealias", "expect", "actual",
            ]);
        }
    }

    /// `override fun name(...)` items for the class around `at`. `start`: replace from here
    /// to the cursor (an already typed `override` / `override fun`).
    fn overrides(&mut self, at: &SyntaxNode, start: Option<TextSize>) {
        let Some(class) = at.ancestors().find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL)) else { return };
        let t = if class.kind() == SyntaxKind::OBJECT_LITERAL {
            self.e.anon_type(&class)
        } else {
            let Some(body) = class.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY) else { return };
            let Some((t, _)) = self.e.implicit_receivers(&body.first_child().unwrap_or(body.clone())).into_iter().next() else { return };
            t
        };
        let indent = crate::analysis::lints::indent_at(&self.file.text, start.unwrap_or(self.offset));
        let range = start.map(|s| TextRange::new(s, self.offset));
        for (m, s) in self.e.overridable_members(&t) {
            let label = override_signature(self.e, &m, &s);
            let text = override_stub(self.e, &m, &s, &indent);
            let mut item = Self::item(&label, ItemKind::Snippet, 1);
            item.filter = Some(m.name.clone());
            item.detail = Some(m.signature());
            item.doc = m.doc.as_deref().map(super::render_doc);
            item.insert = Some(text);
            item.range = range;
            self.push(item);
        }
    }

    /// Classes (and, in expressions, project top-level functions) that need an import.
    fn auto_import(&mut self, callables: bool) {
        let Some(first) = self.prefix.chars().next() else { return };
        let prefix = self.prefix.clone();
        if first.is_uppercase() {
            let in_scope: HashSet<String> = self.items.iter().map(|i| i.label.clone()).collect();
            let keep = |simple: &str| matches(&prefix, simple) && !in_scope.contains(simple);
            let mut fqs: Vec<String> = self.e.res.classpath.top_level_classes_where(&keep);
            fqs.extend(self.e.res.source_classifiers_where(&keep).into_iter().map(|(_, fq)| fq));
            fqs.retain(|fq| importable(fq));
            fqs.sort_by_key(|fq| (fq.rsplit('.').next().unwrap_or(fq).len(), fq.clone()));
            fqs.dedup();
            if fqs.len() > MAX_AUTO_IMPORTS {
                self.truncated = true;
                fqs.truncate(MAX_AUTO_IMPORTS);
            }
            for fq in fqs {
                let simple = fq.rsplit('.').next().unwrap_or(&fq).to_string();
                self.classifier(&simple, &fq, 6, true);
            }
        } else if callables && prefix.chars().count() >= 2 {
            let in_scope: HashSet<String> = self.items.iter().map(|i| i.label.clone()).collect();
            let mut syms: Vec<Symbol> = self.e.res.source_top_level_where(&|n| matches(&prefix, n) && !in_scope.contains(n)).into_iter().filter(|s| matches!(&s.origin, Origin::Source { .. })).collect();
            // Library functions and properties too (not of default-imported packages).
            syms.extend(self.e.res.classpath.top_level_callables_where(&|n| matches(&prefix, n) && !in_scope.contains(n)).into_iter().filter(|s| {
                let pkg = s.fq_name.rsplit_once('.').map_or("", |(p, _)| p);
                !crate::analysis::resolve::KOTLIN_DEFAULT_IMPORTS.contains(&pkg) && importable(&s.fq_name)
            }));
            if syms.len() > MAX_AUTO_IMPORTS {
                self.truncated = true;
            }
            let mut seen = HashSet::new();
            for s in syms.into_iter().filter(|s| s.receiver.is_none()).take(MAX_AUTO_IMPORTS) {
                if s.visibility != crate::symbols::Visibility::Public && s.visibility != crate::symbols::Visibility::Internal || !seen.insert(s.fq_name.clone()) {
                    continue;
                }
                let fq = s.fq_name.clone();
                let before = self.items.len();
                self.symbol(&s, &Subst::new(), 6, false, true);
                if self.items.len() > before {
                    let item = self.items.last_mut().unwrap();
                    item.additional = vec![import_edit(self.file, &fq)];
                    item.description = Some(package_of(&fq));
                }
            }
        }
    }
}

impl Collector<'_, '_> {
    /// Extensions of libraries and other packages applicable to a receiver of type `t`
    /// that need an import (`flow.collectLatest`, `"…".toInstant()`).
    fn unimported_extensions(&mut self, t: &TypeRef) {
        if self.prefix.is_empty() {
            return;
        }
        let prefix = self.prefix.clone();
        let in_scope: HashSet<String> = self.items.iter().map(|i| i.label.split('(').next().unwrap_or(&i.label).trim().to_string()).collect();
        let keep = |n: &str| matches(&prefix, n) && !in_scope.contains(n);
        let mut cands: Vec<Symbol> = self.e.res.classpath.top_level_callables_where(&keep);
        cands.extend(self.e.res.source_top_level_where(&keep).into_iter().filter(|s| matches!(&s.origin, Origin::Source { .. })));
        cands.retain(|s| {
            let pkg = s.fq_name.rsplit_once('.').map_or("", |(p, _)| p);
            s.receiver.is_some() && s.visibility != crate::symbols::Visibility::Private && importable(&s.fq_name) && !crate::analysis::resolve::KOTLIN_DEFAULT_IMPORTS.contains(&pkg)
        });
        let applicable = self.e.applicable(cands, t);
        let mut seen = HashSet::new();
        for (s, subst) in applicable.into_iter().take(MAX_AUTO_IMPORTS) {
            if !seen.insert(s.fq_name.clone()) {
                continue;
            }
            let before = self.items.len();
            self.symbol(&s, &subst, 6, true, true);
            if self.items.len() > before {
                let item = self.items.last_mut().unwrap();
                item.additional = vec![import_edit(self.file, &s.fq_name)];
                item.description = Some(package_of(&s.fq_name));
            }
        }
    }
}

/// The labels usable at `at`, as (keyword, label): `return@` lambdas (named by their label
/// or the function they are passed to), `break@`/`continue@` labeled loops, `this@` classes
/// and labeled lambdas.
fn labels(at: &SyntaxNode) -> Vec<(&'static str, String)> {
    let label_of = |n: &SyntaxNode| n.parent().filter(|p| p.kind() == SyntaxKind::LABELED_EXPR).and_then(|p| p.children().find(|c| c.kind() == SyntaxKind::LABEL)).map(|l| l.text().to_string().trim_end_matches('@').to_string());
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut push = |k: &'static str, l: String| {
        if !l.is_empty() && !out.iter().any(|(ok, ol)| *ok == k && *ol == l) {
            out.push((k, l));
        }
    };
    for a in at.ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::LAMBDA_EXPR => {
                let callee = a
                    .ancestors()
                    .skip(1)
                    .find(|c| c.kind() == SyntaxKind::CALL_EXPR)
                    .filter(|c| c.children().any(|x| matches!(x.kind(), SyntaxKind::LAMBDA_ARGUMENT | SyntaxKind::VALUE_ARGUMENT_LIST) && x.text_range().contains_range(a.text_range())))
                    .and_then(|c| c.first_child())
                    .filter(|n| n.kind() == SyntaxKind::NAME_REF)
                    .map(|n| n.text().to_string());
                if let Some(l) = label_of(&a).or(callee) {
                    push("return", l.clone());
                    push("this", l);
                }
            }
            SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR => {
                if let Some(l) = label_of(&a) {
                    push("break", l.clone());
                    push("continue", l);
                }
            }
            SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION => {
                if let Some(n) = ast::token(&a, SyntaxKind::IDENT) {
                    push("this", ast::ident_text(&n));
                }
            }
            // Labels do not reach out of named functions (a plain `return` leaves them).
            SyntaxKind::FUN if ast::token(&a, SyntaxKind::IDENT).is_some() => {
                // Classes around the function still give `this@Class`.
                for c in a.ancestors().filter(|c| matches!(c.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)) {
                    if let Some(n) = ast::token(&c, SyntaxKind::IDENT) {
                        push("this", ast::ident_text(&n));
                    }
                }
                break;
            }
            _ => {}
        }
    }
    out
}

/// After `return@`, `break@`, `continue@` or `this@`: the labels for that keyword.
fn label_completion(file: &FileData, offset: TextSize) -> Option<Completion> {
    let o = usize::from(offset);
    let before = &file.text[..o];
    let partial_len = before.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').map(char::len_utf8).sum::<usize>();
    let head = before[..o - partial_len].strip_suffix('@')?;
    let keyword = ["return", "break", "continue", "this"].into_iter().find(|k| head.ends_with(k) && !head[..head.len() - k.len()].ends_with(|c: char| c.is_alphanumeric() || c == '_'))?;
    let root = file.parse().syntax();
    let at = root.token_at_offset(TextSize::from((o - partial_len - 1) as u32)).left_biased()?.parent()?;
    let partial = &before[o - partial_len..];
    let items: Vec<Item> = labels(&at)
        .into_iter()
        .filter(|(k, l)| *k == keyword && matches(partial, l))
        .map(|(_, l)| Collector::item(&l, ItemKind::Keyword, 1))
        .collect();
    (!items.is_empty()).then(|| Completion { range: TextRange::new(TextSize::from((o - partial_len) as u32), offset), items, incomplete: false })
}

fn package_of(fq: &str) -> String {
    let parts: Vec<&str> = fq.split('.').collect();
    let first_upper = parts.iter().position(|p| p.chars().next().is_some_and(char::is_uppercase)).unwrap_or(parts.len().saturating_sub(1));
    parts[..first_upper].join(".")
}

fn is_function_type(t: &TypeRef) -> bool {
    match t {
        TypeRef::Function { .. } => true,
        TypeRef::Named { name, fq, .. } => {
            let n = fq.as_deref().unwrap_or(name);
            n.starts_with("kotlin.Function") || n.starts_with("kotlin.coroutines.SuspendFunction")
        }
        TypeRef::Platform(inner) => is_function_type(inner),
        _ => false,
    }
}

fn strip_platform(s: &str) -> String {
    s.replace('!', "")
}

/// Not offered for auto-import: JDK and library internals.
pub fn importable(fq: &str) -> bool {
    const HIDDEN: &[&str] = &["sun.", "jdk.", "com.sun.", "kotlin.jvm.internal.", "kotlin.internal.", "kotlin.coroutines.jvm.internal."];
    !HIDDEN.iter().any(|h| fq.starts_with(h)) && !fq.contains(".internal.") && fq.contains('.')
}

/// Where and what to insert to import `fq` (sorted among the existing imports).
pub fn import_edit(file: &FileData, fq: &str) -> (TextRange, String) {
    let root = file.parse().syntax();
    let imports: Vec<SyntaxNode> = root.descendants().filter(|n| n.kind() == SyntaxKind::IMPORT_DIRECTIVE).collect();
    let line = format!("import {fq}");
    if !imports.is_empty() {
        let path = |n: &SyntaxNode| n.children().find(|c| c.kind() == SyntaxKind::DOT_QUALIFIED_NAME).map(|q| q.text().to_string()).unwrap_or_default();
        if let Some(next) = imports.iter().find(|i| path(i).as_str() > fq) {
            return (TextRange::empty(next.text_range().start()), format!("{line}\n"));
        }
        let last = imports.last().unwrap();
        return (TextRange::empty(last.text_range().end()), format!("\n{line}"));
    }
    if let Some(pkg) = root.children().find(|n| n.kind() == SyntaxKind::PACKAGE_DIRECTIVE && !n.text_range().is_empty()) {
        return (TextRange::empty(pkg.text_range().end()), format!("\n\n{line}"));
    }
    (TextRange::empty(TextSize::from(0)), format!("{line}\n\n"))
}

/// `override fun name(p: T): R` / `override val name: T` for implementing `m`.
pub fn override_signature(e: &Engine, m: &Symbol, s: &Subst) -> String {
    let ty = |t: &TypeRef| strip_platform(&substitute(&e.resolve_declared(m, t), s).display());
    match m.kind {
        SymbolKind::Function => {
            let params: Vec<String> = m.params.iter().map(|p| format!("{}{}: {}", if p.vararg { "vararg " } else { "" }, p.name, ty(&p.ty))).collect();
            let ret = m.ty.as_ref().map(ty).filter(|r| r != "Unit").map(|r| format!(": {r}")).unwrap_or_default();
            let suspend = if m.modifiers.has(Modifiers::SUSPEND) { "suspend " } else { "" };
            format!("override {suspend}fun {}({}){ret}", m.name, params.join(", "))
        }
        _ => {
            let kw = if m.modifiers.has(Modifiers::VAR) { "var" } else { "val" };
            format!("override {kw} {}: {}", m.name, m.ty.as_ref().map(ty).unwrap_or_else(|| "Any".into()))
        }
    }
}

/// An implementation stub of `m` (`TODO()` body); `indent` is the member's indentation.
pub fn override_stub(e: &Engine, m: &Symbol, s: &Subst, indent: &str) -> String {
    let head = override_signature(e, m, s);
    match m.kind {
        SymbolKind::Function => format!("{head} {{\n{indent}    TODO(\"Not yet implemented\")\n{indent}}}"),
        _ => format!("{head}\n{indent}    get() = TODO(\"Not yet implemented\")"),
    }
}

/// Escape `$`, `}` and `\` for a snippet.
fn snippet_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}")
}

/// A variable name for a value of type `t`: `User` → `user`, `List<User>` → `users`.
pub(super) fn suggested_name(t: &TypeRef) -> String {
    let simple = |t: &TypeRef| t.fq_name().map(|f| f.rsplit('.').next().unwrap_or(f).to_string());
    let decap = |s: &str| {
        let mut c = s.chars();
        c.next().map(|f| f.to_lowercase().chain(c).collect::<String>()).unwrap_or_default()
    };
    let name = match t {
        TypeRef::Named { args, .. } if args.len() == 1 && simple(t).is_some_and(|s| matches!(s.as_str(), "List" | "MutableList" | "Set" | "MutableSet" | "Collection" | "Iterable" | "Sequence" | "Array")) => {
            match &args[0] {
                crate::symbols::TypeArg::Type { ty, .. } => simple(super::infer::strip(ty)).map(|s| format!("{}s", decap(&s))),
                _ => None,
            }
        }
        _ => simple(t).map(|s| decap(&s)),
    };
    match name.as_deref() {
        None | Some("") | Some("int") | Some("long") | Some("double") | Some("float") | Some("short") | Some("byte") | Some("char") | Some("boolean") | Some("any") => "value".to_string(),
        Some("string") => "s".to_string(),
        Some(n @ ("object" | "class" | "interface" | "in" | "is" | "as" | "fun" | "val" | "var" | "when" | "if" | "for" | "do" | "try" | "throw" | "return" | "package" | "typealias" | "this" | "super" | "null")) => format!("{n}Value"),
        Some(n) => n.to_string(),
    }
}
