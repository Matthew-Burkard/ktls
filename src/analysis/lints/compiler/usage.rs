//! Checks on how declarations are used: instantiating abstract classes and interfaces,
//! access to invisible declarations, infix calls of non-`infix` functions, and
//! assignments to something that is not a variable.

use super::super::*;
use crate::analysis::ide::infer::{Def, Engine, fq_of};
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind, Visibility};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        if n.kind() == SyntaxKind::BINARY_EXPR {
            variable_expected(&n, out);
        }
    }
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::CALL_EXPR => instantiation(e, &n, out),
            SyntaxKind::NAME_REF => visibility(cx, e, &n, out),
            SyntaxKind::OPERATION_REFERENCE => {
                infix(e, &n, out);
                operator(cx, e, &n, out);
            }
            _ => {}
        }
    }
}

/// `Shape()` for an abstract or sealed class, an interface or an enum.
fn instantiation(e: &Engine, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let defs = e.resolve_name_ref(&callee);
    let classes: Vec<Symbol> = defs
        .iter()
        .filter_map(|d| match d {
            Def::Symbol { sym, .. } if sym.kind == SymbolKind::Constructor => e.classifier(sym.fq_name.trim_end_matches(".<init>")).map(|c| c.symbol().clone()),
            Def::Symbol { sym, .. } if sym.kind.is_classifier() => Some(sym.clone()),
            _ => None,
        })
        .collect();
    let [class] = classes.as_slice() else { return };
    if defs.len() != classes.len() {
        return;
    }
    // A function of the same name (`Mutex()`, `CoroutineScope(ctx)`) is called instead.
    let fun = |s: &Symbol| s.kind == SymbolKind::Function;
    if e.top_level_named(&class.name).iter().any(fun) || e.implicit_receivers(&callee).iter().any(|(t, _)| e.members_named(t, &class.name).iter().any(|(m, _)| fun(m))) {
        return;
    }
    // `X(args)` calling the companion's `operator fun invoke`.
    if e.classifier(&class.fq_name).is_some_and(|c| e.all_static_members(&c).1.iter().any(|(m, _)| m.name == "invoke")) {
        return;
    }
    // Importing a class also imports same-named functions of its package (`Random(seed)`).
    let package = class.fq_name.rsplit_once('.').map_or("", |(p, _)| p);
    if e.top_level_in(package, &class.name).iter().any(|s| s.kind == SymbolKind::Function) {
        return;
    }
    // `Runnable { }`, `Comparator { a, b -> }`: SAM constructors.
    let lambda = call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT)
        || call.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST).flat_map(|l| l.children()).any(|a| a.children().any(|c| c.kind() == SyntaxKind::LAMBDA_EXPR));
    let name = &class.name;
    let msg = match class.kind {
        SymbolKind::Interface if !lambda => format!("Interface '{name}' does not have constructors."),
        SymbolKind::Enum => format!("Enum class '{name}' cannot be instantiated."),
        SymbolKind::Class if class.modifiers.has(Modifiers::SEALED) => format!("Sealed class '{name}' cannot be instantiated."),
        SymbolKind::Class if class.modifiers.has(Modifiers::ABSTRACT) => format!("Cannot create an instance of an abstract class '{name}'."),
        _ => return,
    };
    if class.modifiers.has(Modifiers::EXPECT) {
        return;
    }
    out.report(&CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS, callee.text_range(), msg, None);
}

/// A name that only resolves to declarations not visible here (private members of other
/// classes, private declarations of other files, protected members outside subclasses,
/// internal library declarations).
fn visibility(cx: &Cx, e: &Engine, nr: &SyntaxNode, out: &mut Sink) {
    let Some(tok) = nr.first_token().filter(|t| t.kind() == SyntaxKind::IDENT) else { return };
    if nr.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::PACKAGE_DIRECTIVE | SyntaxKind::KDOC)) {
        return;
    }
    let defs = e.resolve_name_ref(nr);
    let syms: Vec<&Symbol> = defs.iter().filter_map(|d| if let Def::Symbol { sym, .. } = d { Some(sym) } else { None }).collect();
    if syms.is_empty() || syms.len() != defs.len() {
        return;
    }
    let mut why = None;
    for s in &syms {
        match hidden(e, s, nr) {
            Some(reason) => why = Some(reason),
            None => return,
        }
    }
    let Some(reason) = why else { return };
    // "Make 'x' public": drop the visibility modifier of a single project declaration.
    let make_visible = || -> Option<Fix> {
        let [s] = syms.as_slice() else { return None };
        declaration_fix(cx, s, format!("Make '{}' public", s.name), |d| {
            modifiers(d).and_then(|m| m.modifiers().find(|t| matches!(t.text(), "private" | "protected" | "internal"))).map(|t| modifier_removal(&t))
        })
    };
    // `receiver.name`, or a bare name of private top-level declarations of other files (a
    // bare member name may come from an outer scope once invisible members are skipped).
    // And only when no member or extension of that name is visible.
    let name = ast::ident_text(&tok);
    let expr = nr.parent().filter(|p| p.kind() == SyntaxKind::CALL_EXPR && p.first_child().as_ref() == Some(nr)).unwrap_or_else(|| nr.clone());
    let qualified = expr.parent().filter(|p| matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(&expr));
    let Some(q) = qualified else {
        let top_level = syms.iter().all(|s| s.fq_name.rsplit_once('.').is_none_or(|(c, _)| e.classifier(c).is_none()) && s.visibility == Visibility::Private);
        let in_scope = e.top_level_named(&name).iter().any(|s| hidden(e, s, nr).is_none());
        if top_level && !in_scope && nr.parent().is_none_or(|p| p.kind() != SyntaxKind::CALLABLE_REFERENCE) {
            out.report(&INVISIBLE_REFERENCE, tok.text_range(), format!("Cannot access '{name}': it is {reason}."), make_visible());
        }
        return;
    };
    let Some(t) = q.first_child().and_then(|r| e.type_of(&r)) else { return };
    if !e.extensions(&t, &name).is_empty() || e.members_named(&t, &name).iter().any(|(m, _)| hidden(e, m, nr).is_none()) {
        return;
    }
    out.report(&INVISIBLE_REFERENCE, tok.text_range(), format!("Cannot access '{name}': it is {reason}."), make_visible());
}

/// Why `s` is not visible at `at`; `None` if it is (or that is not certain).
fn hidden(e: &Engine, s: &Symbol, at: &SyntaxNode) -> Option<String> {
    let container = s.fq_name.trim_end_matches(".<init>").rsplit_once('.').map(|(c, _)| c.to_string())?;
    let class = e.classifier(&container);
    let class_name = container.rsplit('.').next().unwrap_or(&container).to_string();
    // Java (and Java-like) members: package visibility and fields shadowed by getters.
    let kotlin = match (&s.origin, &class) {
        (Origin::Source { file, .. }, _) => file.extension().is_some_and(|x| x == "kt" || x == "kts"),
        (_, Some(crate::analysis::resolve::Classifier::Library(lc))) => lc.is_kotlin,
        _ => false,
    };
    if !kotlin {
        return None;
    }
    match s.visibility {
        Visibility::Private => match &s.origin {
            Origin::Source { file, .. } => {
                if *file != e.path {
                    return Some(match class { Some(_) => format!("private in '{class_name}'"), None => "private in file".into() });
                }
                // Same file: visible inside the outermost class around the declaration.
                class.as_ref()?;
                let mut outer = container.clone();
                while let Some((p, _)) = outer.rsplit_once('.') {
                    if e.classifier(p).is_some_and(|c| matches!(c.symbol().origin, Origin::Source { .. })) {
                        outer = p.to_string();
                    } else {
                        break;
                    }
                }
                let range = match e.classifier(&outer)?.symbol().origin.clone() {
                    Origin::Source { range, .. } => range,
                    _ => return None,
                };
                (!range.contains_range(at.text_range())).then(|| format!("private in '{class_name}'"))
            }
            // Library private members of Kotlin classes (not top-level, whose facade is unknown).
            _ => class.is_some().then(|| format!("private in '{class_name}'")),
        },
        Visibility::Protected => {
            class.as_ref()?;
            // Inside a subclass (or the class itself), or a class nested in / around it.
            let inside = e.implicit_receivers(at).iter().any(|(t, _)| {
                let Some(f) = fq_of(t) else { return true };
                e.supertype_closure(t).iter().any(|st| fq_of(st) == Some(container.as_str())) || container.starts_with(&format!("{f}.")) || f.starts_with(&format!("{container}.")) || f.contains("anon_")
            });
            (!inside).then(|| format!("protected in '{class_name}'"))
        }
        Visibility::Internal => matches!(s.origin, Origin::Library { .. }).then(|| format!("internal in '{}'", if class.is_some() { class_name } else { container })),
        _ => None,
    }
}

/// `a foo b` where `foo` is not declared `infix`.
fn infix(e: &Engine, op: &SyntaxNode, out: &mut Sink) {
    let Some(tok) = op.first_token().filter(|t| t.kind() == SyntaxKind::IDENT) else { return };
    if op.parent().is_none_or(|p| p.kind() != SyntaxKind::BINARY_EXPR) {
        return;
    }
    let defs = e.resolve_infix(op);
    let funs: Vec<&Symbol> = defs.iter().filter_map(|d| if let Def::Symbol { sym, .. } = d { Some(sym) } else { None }).collect();
    if funs.is_empty() || funs.len() != defs.len() || funs.iter().any(|f| f.kind != SymbolKind::Function || f.modifiers.has(Modifiers::INFIX)) {
        return;
    }
    let name = ast::ident_text(&tok);
    // Another candidate of that name (an extension) may be the infix one.
    let Some(left) = op.parent().and_then(|b| b.first_child()).and_then(|l| e.type_of(&l)) else { return };
    if e.members_named(&left, &name).iter().chain(e.extensions(&left, &name).iter()).any(|(m, _)| m.modifiers.has(Modifiers::INFIX)) {
        return;
    }
    out.report(&INFIX_MODIFIER_REQUIRED, tok.text_range(), format!("'infix' modifier is required on '{name}'."), None);
}

/// `a + b` (also `-`, `*`, `/`, `%`, `..`, `..<` and compound assignments) where no
/// candidate function of the convention's name is declared `operator`.
fn operator(cx: &Cx, e: &Engine, op: &SyntaxNode, out: &mut Sink) {
    let Some(tok) = op.first_token() else { return };
    let names: &[&str] = match tok.kind() {
        SyntaxKind::PLUS => &["plus"],
        SyntaxKind::MINUS => &["minus"],
        SyntaxKind::STAR => &["times"],
        SyntaxKind::SLASH => &["div"],
        SyntaxKind::PERCENT => &["rem"],
        SyntaxKind::RANGE => &["rangeTo"],
        SyntaxKind::RANGE_UNTIL => &["rangeUntil"],
        _ => return,
    };
    let Some(bin) = op.parent().filter(|p| p.kind() == SyntaxKind::BINARY_EXPR) else { return };
    let Some(left) = bin.first_child().and_then(|l| e.type_of(&l)) else { return };
    let defs = e.operator_defs(&tok).unwrap_or_default();
    let funs: Vec<&Symbol> = defs.iter().filter_map(|d| if let Def::Symbol { sym, .. } = d { Some(sym) } else { None }).collect();
    if funs.is_empty() || funs.len() != defs.len() || funs.iter().any(|f| f.modifiers.has(Modifiers::OPERATOR)) {
        return;
    }
    // Only Kotlin declarations need the modifier (Java methods qualify by signature).
    let kotlin = |s: &Symbol| match &s.origin {
        Origin::Source { file, .. } => file.extension().is_some_and(|x| x == "kt" || x == "kts"),
        _ => s.fq_name.rsplit_once('.').and_then(|(c, _)| e.classifier(c)).is_some_and(|c| matches!(c, crate::analysis::resolve::Classifier::Library(lc) if lc.is_kotlin)),
    };
    if !funs.iter().all(|f| kotlin(f)) {
        return;
    }
    // Any `operator` candidate (member or extension) is used instead.
    if names.iter().any(|n| e.members_named(&left, n).iter().chain(e.extensions(&left, n).iter()).any(|(m, _)| m.modifiers.has(Modifiers::OPERATOR))) {
        return;
    }
    let name = names[0];
    let fix = match funs.as_slice() {
        [f] => declaration_fix(cx, f, format!("Add 'operator' modifier to '{name}'"), |d| {
            let kw = d.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::FUN_KW)?;
            Some(Edit { range: TextRange::empty(kw.text_range().start()), text: "operator ".into() })
        }),
        _ => None,
    };
    out.report(&OPERATOR_MODIFIER_REQUIRED, tok.text_range(), format!("'operator' modifier is required on '{name}' in '{}'.", left.display()), fix);
}

/// `f() = 1`, `"a" = b`: the left side of `=` is not a variable.
fn variable_expected(n: &SyntaxNode, out: &mut Sink) {
    if !assignment_op(n).is_some_and(|op| op.kind() == SyntaxKind::EQ) {
        return;
    }
    let Some(target) = n.first_child() else { return };
    let assignable = |t: &SyntaxNode| match t.kind() {
        SyntaxKind::NAME_REF | SyntaxKind::INDEX_EXPR => true,
        SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR => t.children().last().is_some_and(|s| s.kind() == SyntaxKind::NAME_REF),
        _ => false,
    };
    let mut inner = target.clone();
    while inner.kind() == SyntaxKind::PAREN_EXPR {
        let Some(c) = inner.children().last() else { return };
        inner = c;
    }
    if !assignable(&inner) && !matches!(inner.kind(), SyntaxKind::ERROR | SyntaxKind::ANNOTATED_EXPR | SyntaxKind::LABELED_EXPR) {
        out.report(&VARIABLE_EXPECTED, target.text_range(), "Variable expected.", None);
    }
}
