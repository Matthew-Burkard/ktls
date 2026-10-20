//! Signature help: the overloads of the call around the cursor and the argument being typed.

use std::path::Path;

use super::infer::{Def, Engine, Subst, fq_of, substitute};
use super::type_segment;
use crate::analysis::Snapshot;
use crate::symbols::{Modifiers, Symbol, SymbolKind, TypeRef};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

#[derive(Clone, Debug)]
pub struct Signature {
    /// `name(a: Int, b: String = …): R`
    pub label: String,
    /// Byte ranges of the parameters in `label`.
    pub params: Vec<(TextRange, Option<String>)>,
    pub doc: Option<String>,
    /// The parameter the cursor's argument maps to.
    pub active_param: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct SignatureHelp {
    pub signatures: Vec<Signature>,
    pub active: u32,
}

/// The argument list around `offset`: inside its parentheses, or after the `(` of an
/// unclosed one (`foo(a, |`).
fn argument_list(root: &SyntaxNode, offset: TextSize) -> Option<SyntaxNode> {
    let mut tok = root.token_at_offset(offset).left_biased()?;
    while tok.kind().is_trivia() && tok.text_range().start() > TextSize::from(0) {
        tok = tok.prev_token()?;
    }
    tok.parent_ancestors().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).find(|list| {
        let Some(open) = ast::token(list, SyntaxKind::L_PAREN) else { return false };
        offset >= open.text_range().end()
            && match ast::token(list, SyntaxKind::R_PAREN) {
                Some(close) => offset <= close.text_range().start(),
                None => true,
            }
    })
}

/// The functions the argument list may call, and the one resolution picked (if known).
fn candidates(e: &Engine, list: &SyntaxNode) -> Option<(Vec<(Symbol, Subst)>, Option<Symbol>)> {
    let owner = list.parent()?;
    match owner.kind() {
        SyntaxKind::CALL_EXPR => {
            let callee = owner.first_child()?;
            if callee.kind() != SyntaxKind::NAME_REF {
                let t = e.type_of(&callee)?;
                return function_type(&callee.text().to_string(), &t).map(|s| (vec![(s, Subst::new())], None));
            }
            let resolved = e.resolve_name_ref(&callee);
            let picked = match resolved.as_slice() {
                [Def::Symbol { sym, .. }] => Some(sym.clone()),
                _ => None,
            };
            let mut all = e.call_candidates(&callee).unwrap_or_default();
            if all.is_empty() {
                for d in resolved {
                    match d {
                        Def::Symbol { sym, subst } if matches!(sym.kind, SymbolKind::Function | SymbolKind::Constructor) => all.push((sym, subst)),
                        Def::Local(l) => all.extend(l.ty.as_ref().and_then(|t| function_type(&l.name, t)).map(|s| (s, Subst::new()))),
                        Def::Symbol { sym, subst } => all.extend(e.symbol_type(&sym).and_then(|t| function_type(&sym.name, &substitute(&t, &subst))).map(|s| (s, Subst::new()))),
                        Def::Package(_) => {}
                    }
                }
            }
            Some((all, picked))
        }
        // `: Base(…)`, `@Ann(…)`
        SyntaxKind::SUPER_TYPE_CALL_ENTRY | SyntaxKind::ANNOTATION => {
            let seg = owner.descendants().filter(|d| d.kind() == SyntaxKind::USER_TYPE_SEGMENT).last()?;
            let fq = type_segment(e, &seg).into_iter().find_map(|d| match d {
                Def::Symbol { sym, .. } => Some(sym.fq_name),
                _ => None,
            })?;
            Some((e.constructors(&fq).into_iter().map(|c| (c, Subst::new())).collect(), None))
        }
        // `this(…)` / `super(…)` in a secondary constructor.
        SyntaxKind::CONSTRUCTOR_DELEGATION_CALL => {
            let class = owner.ancestors().find(|a| a.kind() == SyntaxKind::CLASS)?;
            let name = ast::token(&class, SyntaxKind::IDENT)?;
            let Some(Def::Symbol { sym: this, .. }) = super::declaration_def(e, &class, &name) else { return None };
            let fq = if ast::token(&owner, SyntaxKind::SUPER_KW).is_some() || owner.first_token().is_some_and(|t| t.text() == "super") {
                this.supertypes.iter().map(|t| e.resolve_declared(&this, t)).find_map(|t| fq_of(&t).filter(|fq| e.classifier(fq).is_some_and(|c| c.symbol().kind == SymbolKind::Class)).map(str::to_string))?
            } else {
                this.fq_name.clone()
            };
            let skip = owner.ancestors().find(|a| a.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR).map(|c| c.text_range());
            let ctors = e.constructors(&fq).into_iter().filter(|c| !matches!(&c.origin, crate::symbols::Origin::Source { range, .. } if Some(*range) == skip));
            Some((ctors.map(|c| (c, Subst::new())).collect(), None))
        }
        _ => None,
    }
}

/// A value of function type being invoked (`onClick(…)`, `block(…)`), as a pseudo-function.
fn function_type(name: &str, t: &TypeRef) -> Option<Symbol> {
    let (params, ret): (Vec<TypeRef>, TypeRef) = match super::infer::strip(t) {
        TypeRef::Function { params, ret, .. } => (params.clone(), (**ret).clone()),
        TypeRef::Named { fq: Some(fq), args, .. } => {
            fq.strip_prefix("kotlin.Function").or_else(|| fq.strip_prefix("kotlin.coroutines.SuspendFunction"))?.parse::<u32>().ok()?;
            let arg = |a: &crate::symbols::TypeArg| match a {
                crate::symbols::TypeArg::Type { ty, .. } => ty.clone(),
                crate::symbols::TypeArg::Star => TypeRef::named("kotlin.Any"),
            };
            let (ret, params) = args.split_last()?;
            (params.iter().map(arg).collect(), arg(ret))
        }
        _ => return None,
    };
    let mut sym = Symbol::new(SymbolKind::Function, name, name.to_string(), crate::symbols::Origin::Builtin);
    sym.params = params.into_iter().enumerate().map(|(i, ty)| crate::symbols::Param { name: format!("p{}", i + 1), ty, has_default: false, vararg: false }).collect();
    sym.ty = Some(ret);
    Some(sym)
}

/// `@param` descriptions of a KDoc comment.
fn param_docs(doc: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in doc.lines() {
        let l = line.trim().trim_start_matches('*').trim();
        if let Some(rest) = l.strip_prefix("@param ") {
            let (name, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            out.push((name.trim_matches(['[', ']']).to_string(), desc.trim().to_string()));
        } else if l.starts_with('@') || l.ends_with("*/") && l.len() <= 2 {
            out.push((String::new(), String::new()));
        } else if let Some(last) = out.last_mut().filter(|(n, _)| !n.is_empty() && !l.is_empty()) {
            last.1.push(' ');
            last.1.push_str(l);
        }
    }
    out.retain(|(n, _)| !n.is_empty());
    out
}

fn render(e: &Engine, sym: &Symbol, subst: &Subst, doc: Option<&str>) -> Signature {
    let ty = |t: &TypeRef| substitute(&e.resolve_declared(sym, t), subst).display().replace('!', "");
    let docs = doc.map(param_docs).unwrap_or_default();
    let mut label = match sym.kind {
        SymbolKind::Constructor => sym.fq_name.trim_end_matches(".<init>").rsplit('.').next().unwrap_or_default().to_string(),
        _ => sym.name.clone(),
    };
    label.push('(');
    let mut params = Vec::new();
    for (i, p) in sym.params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let start = label.len();
        if p.vararg {
            label.push_str("vararg ");
        }
        label.push_str(&format!("{}: {}", p.name, ty(&p.ty)));
        if p.has_default {
            label.push_str(" = …");
        }
        let range = TextRange::new(TextSize::from(start as u32), TextSize::from(label.len() as u32));
        params.push((range, docs.iter().find(|(n, _)| *n == p.name).map(|(_, d)| d.clone()).filter(|d| !d.is_empty())));
    }
    label.push(')');
    if sym.kind == SymbolKind::Function
        && let Some(r) = sym.ty.clone().or_else(|| e.symbol_type(sym)).as_ref().map(ty)
    {
        label.push_str(": ");
        label.push_str(&r);
    }
    let summary = doc.map(super::render_doc).map(|d| d.split("\n\n**Parameters**").next().unwrap_or_default().trim().to_string()).filter(|d| !d.is_empty());
    Signature { label, params, doc: summary, active_param: None }
}

fn commas_before(list: &SyntaxNode, offset: TextSize) -> usize {
    list.children_with_tokens().filter_map(|t| t.into_token()).filter(|t| t.kind() == SyntaxKind::COMMA && t.text_range().end() <= offset).count()
}

/// The parameter the argument at `offset` maps to.
fn active_parameter(sym: &Symbol, list: &SyntaxNode, offset: TextSize) -> Option<u32> {
    // The argument being typed: the number of top-level commas before the cursor.
    let commas = commas_before(list, offset);
    // Its name, if named (an incomplete `name = ` is an error node).
    let named = list
        .children()
        .filter(|c| matches!(c.kind(), SyntaxKind::VALUE_ARGUMENT | SyntaxKind::ERROR) && commas_before(list, c.text_range().start()) == commas)
        .find_map(|a| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME))
        .and_then(|n| ast::token(&n, SyntaxKind::IDENT))
        .map(|t| ast::ident_text(&t));
    if let Some(name) = named {
        return sym.params.iter().position(|p| p.name == name).map(|i| i as u32);
    }
    let last = sym.params.len().checked_sub(1)?;
    // Positional after named arguments keeps the position (Kotlin 1.4+ when in order).
    let i = commas.min(if sym.params[last].vararg { last } else { usize::MAX });
    (i <= last).then_some(i as u32)
}

/// Whether `sym` can take the arguments written so far (count and names).
fn fits(e: &Engine, sym: &Symbol, subst: &Subst, list: &SyntaxNode, offset: TextSize) -> bool {
    let args: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let commas = commas_before(list, offset);
    let count = args.len().max(commas + 1);
    let vararg = sym.params.iter().any(|p| p.vararg);
    let name_of = |a: &SyntaxNode| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
    let names_ok = args.iter().filter_map(name_of).all(|n| sym.params.iter().any(|p| p.name == n));
    if !(names_ok && (vararg || count <= sym.params.len() || args.is_empty() && sym.params.is_empty())) {
        return false;
    }
    // Leading positional arguments whose types are known must be subtypes of the parameter's.
    for (a, p) in args.iter().take_while(|a| name_of(a).is_none()).zip(&sym.params).take_while(|(_, p)| !p.vararg) {
        if !argument_fits(e, sym, subst, a, &p.ty) {
            return false;
        }
    }
    true
}

fn argument_fits(e: &Engine, sym: &Symbol, subst: &Subst, arg: &SyntaxNode, param: &TypeRef) -> bool {
    let Some(value) = arg.children().last() else { return true };
    let declared = substitute(&e.resolve_declared(sym, param), subst);
    let Some(want) = fq_of(&declared).map(str::to_string) else { return true };
    // Integer literals also fit the other integral types.
    let int_literal = value.kind() == SyntaxKind::LITERAL && value.first_token().is_some_and(|t| t.kind() == SyntaxKind::INT_LITERAL);
    if int_literal && matches!(want.as_str(), "kotlin.Long" | "kotlin.Short" | "kotlin.Byte" | "kotlin.Int" | "kotlin.Double" | "kotlin.Float") {
        return want != "kotlin.Double" && want != "kotlin.Float";
    }
    let Some(have) = e.type_of(&value) else { return true };
    let Some(have_fq) = fq_of(&have) else { return true };
    if have_fq == "kotlin.Nothing" || want == "kotlin.Any" {
        return true;
    }
    e.supertype_closure(&have).iter().any(|t| fq_of(t) == Some(want.as_str()))
}

pub fn signature_help(snap: &Snapshot, path: &Path, offset: TextSize) -> Option<SignatureHelp> {
    let e = Engine::new(snap, path)?;
    let list = argument_list(&e.root, offset)?;
    let (mut cands, picked) = candidates(&e, &list)?;
    // Synthetic and duplicate overloads (same signature reached twice) are dropped.
    let mut seen = std::collections::HashSet::new();
    cands.retain(|(s, _)| !s.modifiers.has(Modifiers::SYNTHETIC) && seen.insert((s.fq_name.clone(), s.signature())));
    if cands.is_empty() {
        return None;
    }
    // Overloads that fit the arguments first, then by parameter count.
    cands.sort_by_cached_key(|(s, subst)| (!fits(&e, s, subst, &list, offset), s.params.len()));
    let active = picked
        .as_ref()
        .and_then(|p| cands.iter().position(|(s, subst)| s.fq_name == p.fq_name && s.signature() == p.signature() && fits(&e, s, subst, &list, offset)))
        .unwrap_or(0);
    let mut signatures: Vec<Signature> = Vec::new();
    let mut active_out = 0;
    for (i, (sym, subst)) in cands.iter().enumerate() {
        let doc = match &sym.origin {
            crate::symbols::Origin::Source { .. } => sym.doc.clone(),
            _ => super::library::locate_in_sources(snap, &e.res.classpath, sym, false).and_then(|t| t.doc),
        };
        let mut s = render(&e, sym, subst, doc.as_deref());
        s.active_param = active_parameter(sym, &list, offset);
        // A member and an extension can render the same (`List.indexOf`).
        match signatures.iter().position(|o| o.label == s.label) {
            Some(j) if i == active => active_out = j,
            Some(_) => {}
            None => {
                if i == active {
                    active_out = signatures.len();
                }
                signatures.push(s);
            }
        }
    }
    Some(SignatureHelp { signatures, active: active_out as u32 })
}
