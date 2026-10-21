//! Changing a function's parameters together with every call in the project: move a
//! parameter left / right, remove a parameter, and add a parameter from a call that passes
//! too many arguments or an unknown named one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::assists::type_text;
use super::infer::Engine;
use super::inlay::value_type;
use super::{NavTarget, references};
use crate::analysis::Snapshot;
use crate::analysis::lints::{Edit, Fix, has_modifier};
use crate::symbols::{Origin, Symbol};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange};

/// The function's signature may change: a project function with no override family.
fn changeable(f: &SyntaxNode) -> bool {
    let fixed = ["override", "open", "abstract", "expect", "actual", "operator", "external"].iter().any(|m| has_modifier(f, m));
    let in_interface = f.ancestors().skip(1).find(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION)).is_some_and(|c| c.children_with_tokens().any(|t| t.kind() == SyntaxKind::INTERFACE_KW));
    !fixed && !in_interface && ast::token(f, SyntaxKind::IDENT).is_some()
}

/// A call of the function: its argument nodes and whether a trailing lambda follows.
struct Call {
    path: PathBuf,
    /// The whole call expression.
    range: TextRange,
    /// Its parent's kind (for parentheses when inlining).
    context: Option<SyntaxKind>,
    args: Vec<SyntaxNode>,
    trailing_lambda: bool,
}

/// Every call of the function named by `name` (in `path`). `None` if a reference is not
/// a plain call (`::f`, an infix use), which a signature change would break.
fn calls(snap: &Snapshot, path: &Path, name: &SyntaxToken) -> Option<Vec<Call>> {
    let refs = references(snap, path, name.text_range().start(), false, &|| false)?;
    let mut trees: HashMap<PathBuf, SyntaxNode> = HashMap::new();
    let mut out = Vec::new();
    for NavTarget { path, range } in refs {
        let root = trees.entry(path.clone()).or_insert_with(|| snap.file(&path).map(|f| f.parse().syntax()).unwrap());
        let tok = root.token_at_offset(range.start()).right_biased()?;
        // KDoc links and imports are not calls and need no change.
        if tok.kind() == SyntaxKind::KDOC || tok.parent_ancestors().any(|a| a.kind() == SyntaxKind::IMPORT_DIRECTIVE) {
            continue;
        }
        let nr = tok.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF)?;
        let call = nr.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR && c.first_child().as_ref() == Some(&nr))?;
        let args: Vec<SyntaxNode> = call.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).flat_map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect::<Vec<_>>()).collect();
        if args.iter().any(|a| ast::token(a, SyntaxKind::STAR).is_some()) {
            return None;
        }
        out.push(Call { path, range: call.text_range(), context: call.parent().map(|p| p.kind()), args, trailing_lambda: call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) });
    }
    Some(out)
}

fn arg_name(a: &SyntaxNode) -> Option<String> {
    a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t))
}

fn value_of(a: &SyntaxNode) -> Option<SyntaxNode> {
    a.children().filter(|c| c.kind() != SyntaxKind::VALUE_ARGUMENT_NAME).last()
}

/// Removing item `idx` of a comma-separated list, with its separator.
fn list_item_removal(items: &[SyntaxNode], idx: usize) -> TextRange {
    let item = &items[idx];
    if idx + 1 < items.len() {
        TextRange::new(item.text_range().start(), items[idx + 1].text_range().start())
    } else if idx > 0 {
        TextRange::new(items[idx - 1].text_range().end(), item.text_range().end())
    } else {
        item.text_range()
    }
}

/// Collects edits for the declaration's file and the other files.
struct Edits {
    here: PathBuf,
    fix: Fix,
}

impl Edits {
    fn new(here: &Path, title: String) -> Edits {
        Edits { here: here.to_path_buf(), fix: Fix::new(title, Vec::new()) }
    }

    fn push(&mut self, path: &Path, range: TextRange, text: String) {
        let e = Edit { range, text };
        if path == self.here {
            self.fix.edits.push(e);
        } else {
            self.fix.elsewhere.push((path.to_path_buf(), e));
        }
    }
}

/// On a parameter of a project function: move it left / right, or remove it if unused.
pub fn parameter_assists(snap: &Snapshot, path: &Path, tok: &SyntaxToken) -> Vec<Fix> {
    let Some(p) = tok.parent().filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER && ast::token(p, SyntaxKind::IDENT).as_ref() == Some(tok)) else { return Vec::new() };
    let Some(list) = p.parent().filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST) else { return Vec::new() };
    let Some(f) = list.parent().filter(|f| f.kind() == SyntaxKind::FUN && changeable(f)) else { return Vec::new() };
    let params: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).collect();
    let Some(index) = params.iter().position(|x| *x == p) else { return Vec::new() };
    if params.iter().any(|x| has_modifier(x, "vararg")) {
        return Vec::new();
    }
    let name = ast::token(&f, SyntaxKind::IDENT).unwrap();
    let Some(calls) = calls(snap, path, &name) else { return Vec::new() };
    let mut out = Vec::new();
    if index > 0 {
        out.extend(swap(path, &params, index - 1, &calls, "left"));
    }
    if index + 1 < params.len() {
        out.extend(swap(path, &params, index, &calls, "right"));
    }
    out.extend(remove(snap, path, &f, &params, index, &calls));
    out
}

/// Swap parameters `i` and `i + 1` and the arguments passed for them.
fn swap(path: &Path, params: &[SyntaxNode], i: usize, calls: &[Call], dir: &str) -> Option<Fix> {
    let moved = if dir == "left" { i + 1 } else { i };
    let pname = |k: usize| ast::token(&params[k], SyntaxKind::IDENT).map(|t| ast::ident_text(&t));
    let mut edits = Edits::new(path, format!("Move parameter '{}' {dir}", pname(moved)?));
    edits.push(path, params[i].text_range(), params[i + 1].text().to_string());
    edits.push(path, params[i + 1].text_range(), params[i].text().to_string());
    for c in calls {
        let positional = c.args.iter().take_while(|a| arg_name(a).is_none()).count();
        // A trailing lambda is always the last parameter.
        if c.trailing_lambda && i + 2 >= params.len() {
            return None;
        }
        if positional > i + 1 {
            edits.push(&c.path, c.args[i].text_range(), c.args[i + 1].text().to_string());
            edits.push(&c.path, c.args[i + 1].text_range(), c.args[i].text().to_string());
        } else if positional == i + 1 {
            // Only the first of the two is passed: name it so that it keeps its parameter.
            edits.push(&c.path, TextRange::empty(c.args[i].text_range().start()), format!("{} = ", pname(i)?));
        }
    }
    Some(edits.fix)
}

/// Remove parameter `index` (unused in the body) and its argument from every call.
fn remove(snap: &Snapshot, path: &Path, f: &SyntaxNode, params: &[SyntaxNode], index: usize, calls: &[Call]) -> Option<Fix> {
    let p = &params[index];
    let pname = ast::ident_text(&ast::token(p, SyntaxKind::IDENT)?);
    let e = Engine::new(snap, path)?;
    let used = f.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == pname.as_str()).any(|r| {
        matches!(e.resolve_name_ref(&r).as_slice(), [super::infer::Def::Local(l)] if ast::token(p, SyntaxKind::IDENT).is_some_and(|t| t.text_range() == l.name_range))
    });
    if used {
        return None;
    }
    let mut edits = Edits::new(path, format!("Remove parameter '{pname}'"));
    edits.push(path, list_item_removal(params, index), String::new());
    for c in calls {
        let target = c.args.iter().position(|a| arg_name(a).as_deref() == Some(pname.as_str())).or_else(|| (index < c.args.len() && arg_name(&c.args[index]).is_none()).then_some(index));
        match target {
            Some(i) => edits.push(&c.path, list_item_removal(&c.args, i), String::new()),
            None if index + 1 == params.len() && c.trailing_lambda => return None,
            None => {}
        }
    }
    Some(edits.fix)
}

/// "Add parameter": the declaration of `sym` (a project function) gains parameters for
/// `extra` arguments of a call in `call_path`, typed from the arguments.
pub fn add_parameter_fix(snap: &Snapshot, call_engine: &Engine, sym: &Symbol, extra: &[SyntaxNode]) -> Option<Fix> {
    let Origin::Source { file, name_range, .. } = &sym.origin else { return None };
    let decl_engine = Engine::new(snap, file)?;
    let f = crate::syntax::covering(&decl_engine.root, *name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::FUN)?;
    if !changeable(&f) {
        return None;
    }
    let list = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?;
    let close = list.last_token().filter(|t| t.kind() == SyntaxKind::R_PAREN)?;
    let file_data = snap.file(file)?;
    let mut taken: Vec<String> = sym.params.iter().map(|p| p.name.clone()).collect();
    let mut new_params = Vec::new();
    let mut fix = Fix::new(String::new(), Vec::new());
    let push = |fix: &mut Fix, e: Edit| {
        if file.as_path() == call_engine.path.as_path() {
            fix.edits.push(e);
        } else {
            fix.elsewhere.push((file.to_path_buf(), e));
        }
    };
    for a in extra {
        let value = value_of(a)?;
        let t = value_type(call_engine, &value)?;
        let (ty, imports) = type_text(&decl_engine, file_data, &t, &close)?;
        for i in imports {
            push(&mut fix, i);
        }
        let base = arg_name(a).unwrap_or_else(|| match value.kind() {
            SyntaxKind::NAME_REF => value.text().to_string(),
            _ => super::completion::suggested_name(super::infer::strip(&t)),
        });
        let mut name = base.clone();
        let mut i = 2;
        while taken.contains(&name) {
            name = format!("{base}{i}");
            i += 1;
        }
        taken.push(name.clone());
        new_params.push(format!("{name}: {ty}"));
    }
    let sep = if sym.params.is_empty() { "" } else { ", " };
    // After the last parameter (keeping a trailing comma's position).
    let last = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).last();
    let at = last.map_or(close.text_range().start(), |l| l.text_range().end());
    push(&mut fix, Edit { range: TextRange::empty(at), text: format!("{sep}{}", new_params.join(", ")) });
    let fname = ast::token(&f, SyntaxKind::IDENT).map(|t| ast::ident_text(&t))?;
    fix.title = format!("Add parameter{} '{}' to '{fname}'", if new_params.len() > 1 { "s" } else { "" }, new_params.join(", "));
    Some(fix)
}

/// "Inline function": a top-level or local function whose body is one expression replaces
/// each of its calls (parameters substituted by the arguments), and is then removed. On the
/// declaration's name or a call.
pub fn inline_function(snap: &Snapshot, path: &Path, tok: &SyntaxToken) -> Option<Fix> {
    let e = Engine::new(snap, path)?;
    let parent = tok.parent()?;
    let (decl_path, name_range) = match parent.kind() {
        SyntaxKind::FUN if ast::token(&parent, SyntaxKind::IDENT).as_ref() == Some(tok) => (path.to_path_buf(), tok.text_range()),
        SyntaxKind::NAME_REF => match e.resolve_name_ref(&parent).as_slice() {
            [super::infer::Def::Symbol { sym, .. }] => match &sym.origin {
                Origin::Source { file, name_range, .. } if sym.kind == crate::symbols::SymbolKind::Function => (file.clone(), *name_range),
                _ => return None,
            },
            _ => return None,
        },
        _ => return None,
    };
    let de = Engine::new(snap, &decl_path)?;
    let decl_text = &snap.file(&decl_path)?.text;
    let f = crate::syntax::covering(&de.root, name_range).into_iter().flat_map(|el| el.ancestors()).find(|a| a.kind() == SyntaxKind::FUN)?;
    let in_class = f.ancestors().skip(1).any(|a| matches!(a.kind(), SyntaxKind::CLASS_BODY | SyntaxKind::OBJECT_LITERAL));
    if !changeable(&f) || in_class || f.children().any(|c| matches!(c.kind(), SyntaxKind::RECEIVER_TYPE | SyntaxKind::TYPE_PARAMETER_LIST)) {
        return None;
    }
    // `= expr` or `{ return expr }`.
    let body = match crate::analysis::lints::initializer(&f) {
        Some(b) => b,
        None => {
            let block = f.children().find(|c| c.kind() == SyntaxKind::BLOCK)?;
            let kids: Vec<SyntaxNode> = block.children().collect();
            let [ret] = kids.as_slice() else { return None };
            if ret.kind() != SyntaxKind::RETURN_EXPR || ret.children().any(|c| c.kind() == SyntaxKind::LABEL) {
                return None;
            }
            ret.first_child()?
        }
    };
    if body.descendants_with_tokens().any(|t| matches!(t.kind(), SyntaxKind::RETURN_KW | SyntaxKind::THIS_KW | SyntaxKind::SUPER_KW)) {
        return None;
    }
    let list = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?;
    let params: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).collect();
    if params.iter().any(|p| has_modifier(p, "vararg")) {
        return None;
    }
    let pnames: Vec<(String, TextRange)> = params.iter().filter_map(|p| ast::token(p, SyntaxKind::IDENT).map(|t| (ast::ident_text(&t), t.text_range()))).collect();
    // Uses of each parameter in the body.
    let mut uses: Vec<(usize, TextRange)> = Vec::new();
    for r in body.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF) {
        if let [super::infer::Def::Local(l)] = de.resolve_name_ref(&r).as_slice()
            && let Some(i) = pnames.iter().position(|(_, nr)| *nr == l.name_range)
        {
            uses.push((i, r.text_range()));
        }
    }
    let fname = ast::token(&f, SyntaxKind::IDENT)?;
    let calls = calls(snap, &decl_path, &fname)?;
    if calls.is_empty() {
        return None;
    }
    // Recursion, or a call nested in another's arguments, would overlap.
    let overlapping = calls.iter().any(|c| c.path == decl_path && f.text_range().contains_range(c.range))
        || calls.iter().enumerate().any(|(i, a)| calls.iter().skip(i + 1).any(|b| a.path == b.path && a.range.intersect(b.range).is_some()));
    if overlapping {
        return None;
    }
    let mut edits = Edits::new(path, format!("Inline function '{}'", fname.text()));
    let body_start = usize::from(body.text_range().start());
    for c in &calls {
        if c.trailing_lambda {
            return None;
        }
        // The argument text for each parameter (or its default value).
        let mut values: Vec<Option<(String, bool)>> = vec![None; params.len()];
        for (i, a) in c.args.iter().enumerate() {
            let slot = match arg_name(a) {
                Some(n) => pnames.iter().position(|(p, _)| *p == n)?,
                None => i,
            };
            let v = value_of(a)?;
            values[slot] = Some((v.text().to_string(), super::rewrites_atomic(&v)));
        }
        for (i, p) in params.iter().enumerate() {
            if values[i].is_none() {
                let d = crate::analysis::lints::initializer(p)?;
                values[i] = Some((d.text().to_string(), super::rewrites_atomic(&d)));
            }
        }
        // A non-trivial argument used more than once would be evaluated more than once.
        for (i, v) in values.iter().enumerate() {
            let (_, atomic) = v.as_ref()?;
            let n = uses.iter().filter(|(k, _)| *k == i).count();
            if n > 1 && !atomic {
                return None;
            }
        }
        let mut text = body.text().to_string();
        let mut sorted = uses.clone();
        sorted.sort_by_key(|(_, r)| std::cmp::Reverse(r.start()));
        for (i, r) in sorted {
            let (v, atomic) = values[i].as_ref()?;
            let s = usize::from(r.start()) - body_start;
            let e2 = usize::from(r.end()) - body_start;
            text.replace_range(s..e2, &if *atomic { v.clone() } else { format!("({v})") });
        }
        let wrap = !super::rewrites_atomic(&body) && c.context.is_some_and(|k| !matches!(k, SyntaxKind::VALUE_ARGUMENT | SyntaxKind::BLOCK | SyntaxKind::PROPERTY | SyntaxKind::PAREN_EXPR | SyntaxKind::RETURN_EXPR | SyntaxKind::FUN));
        edits.push(&c.path, c.range, if wrap { format!("({text})") } else { text });
    }
    // Remove the declaration with its lines.
    let start = usize::from(f.text_range().start());
    let line_start = decl_text[..start].rfind('\n').map_or(0, |i| i + 1);
    let end = usize::from(f.text_range().end());
    let end = if decl_text[end..].starts_with("\n\n") { end + 2 } else if decl_text[end..].starts_with('\n') { end + 1 } else { end };
    edits.push(&decl_path, TextRange::new((line_start as u32).into(), (end as u32).into()), String::new());
    Some(edits.fix)
}
