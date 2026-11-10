//! Modifiers that constrain a function's shape (`operator`, `infix`, `inline` on virtual
//! members), `this`/`super` outside any class, and how arguments are passed (twice,
//! spread into a non-vararg, named to Java).

use super::super::*;
use crate::analysis::ide::infer::Engine;
use crate::analysis::resolve::Classifier;
use crate::symbols::Symbol;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let script = cx.root.children().any(|c| c.kind() == SyntaxKind::SCRIPT_BODY);
    for n in cx.root.descendants() {
        if cx.in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::FUN => {
                operator(&n, out);
                infix(&n, out);
                inline_virtual(&n, out);
            }
            SyntaxKind::THIS_EXPR | SyntaxKind::SUPER_EXPR if !script => outside_class(&n, out),
            SyntaxKind::VALUE_ARGUMENT_LIST => duplicate_names(&n, out),
            _ => {}
        }
    }
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::CALL_EXPR) {
        if !cx.in_error(&n) {
            arguments(e, &n, out);
        }
    }
}

/// Unary operators and other conventions without value parameters.
const NULLARY: &[&str] = &["unaryPlus", "unaryMinus", "not", "inc", "dec", "iterator", "next", "hasNext"];
/// Binary operators and other conventions with one value parameter.
const UNARY_PARAM: &[&str] = &["plus", "minus", "times", "div", "rem", "mod", "rangeTo", "rangeUntil", "contains", "compareTo", "equals", "plusAssign", "minusAssign", "timesAssign", "divAssign", "remAssign", "modAssign"];

fn is_member_or_extension(f: &SyntaxNode) -> bool {
    f.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) || f.parent().is_some_and(|p| p.kind() == SyntaxKind::CLASS_BODY)
}

fn return_type_text(f: &SyntaxNode) -> Option<String> {
    let d = ast::FunDecl::cast(f.clone())?;
    match d.return_type() {
        Some(t) => Some(t.syntax().text().to_string().replace(' ', "")),
        None if d.body_block().is_some() => Some("Unit".into()),
        None => None,
    }
}

fn operator(f: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = modifiers(f).and_then(|m| m.modifiers().find(|t| t.text() == "operator")) else { return };
    let Some(name) = name_token(f).map(|t| ast::ident_text(&t)) else { return };
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let params = decl.parameters();
    let vararg = params.iter().any(|p| p.is_vararg());
    let n = params.len();
    let component = name.strip_prefix("component").is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) && !d.starts_with('0'));
    let problem = if !is_member_or_extension(f) {
        Some("must be a member or an extension function".to_string())
    } else if NULLARY.contains(&name.as_str()) || component {
        (n != 0).then(|| "must have no value parameters".to_string())
    } else if UNARY_PARAM.contains(&name.as_str()) {
        (n != 1 || vararg).then(|| "must have a single value parameter".to_string())
    } else {
        match name.as_str() {
            "get" => (n == 0).then(|| "must have at least one value parameter".to_string()),
            "set" => (n < 2).then(|| "must have at least two value parameters".to_string()),
            "getValue" | "provideDelegate" => (n != 2).then(|| "must have two value parameters".to_string()),
            "setValue" => (n != 3).then(|| "must have three value parameters".to_string()),
            "invoke" => None,
            _ => Some("illegal function name".to_string()),
        }
    };
    if let Some(why) = problem {
        out.report(&INAPPLICABLE_OPERATOR_MODIFIER, kw.text_range(), format!("'operator' modifier is inapplicable on this function: {why}."), None);
        return;
    }
    let ret = return_type_text(f);
    match name.as_str() {
        "inc" | "dec" if ret.as_deref() == Some("Unit") => {
            out.report(&INC_DEC_SHOULD_NOT_RETURN_UNIT, name_token(f).map_or(kw.text_range(), |t| t.text_range()), "Functions inc(), dec() shouldn't return Unit to be used by operators ++, --.", None);
        }
        "compareTo" if decl.return_type().is_some() && ret.as_deref().is_some_and(|r| r != "Int" && r != "kotlin.Int") => {
            out.report(&COMPARE_TO_TYPE_MISMATCH, decl.return_type().map_or(kw.text_range(), |t| t.syntax().text_range()), "'compareTo()' must return 'Int', but returns another type.", None);
        }
        n if n.ends_with("Assign") && decl.return_type().is_some() && ret.as_deref().is_some_and(|r| r != "Unit" && r != "kotlin.Unit") => {
            out.report(&ASSIGNMENT_OPERATOR_SHOULD_RETURN_UNIT, decl.return_type().map_or(kw.text_range(), |t| t.syntax().text_range()), format!("Function '{n}' should return Unit to be used by the corresponding assignment operator."), None);
        }
        _ => {}
    }
}

fn infix(f: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = modifiers(f).and_then(|m| m.modifiers().find(|t| t.text() == "infix")) else { return };
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let params = decl.parameters();
    let why = if !is_member_or_extension(f) {
        Some("must be a member or an extension function")
    } else if params.len() != 1 {
        Some("must have a single value parameter")
    } else if params[0].is_vararg() || ast::token(params[0].syntax(), SyntaxKind::EQ).is_some() {
        Some("the parameter must not be vararg or have a default value")
    } else {
        None
    };
    if let Some(why) = why {
        out.report(&INAPPLICABLE_INFIX_MODIFIER, kw.text_range(), format!("'infix' modifier is inapplicable on this function: {why}."), None);
    }
}

/// `inline` on an open, abstract or interface member.
fn inline_virtual(f: &SyntaxNode, out: &mut Sink) {
    let Some(kw) = modifiers(f).and_then(|m| m.modifiers().find(|t| t.text() == "inline")) else { return };
    let Some(class) = f.parent().filter(|b| b.kind() == SyntaxKind::CLASS_BODY).and_then(|b| b.parent()) else { return };
    if has_modifier(f, "private") || has_modifier(f, "final") {
        return;
    }
    let interface = class.kind() == SyntaxKind::CLASS && ast::token(&class, SyntaxKind::INTERFACE_KW).is_some();
    let open_class = class.kind() == SyntaxKind::CLASS && ["open", "abstract", "sealed"].iter().any(|m| has_modifier(&class, m));
    let virtual_member = has_modifier(f, "open") || has_modifier(f, "abstract") || interface || has_modifier(f, "override") && open_class;
    if virtual_member {
        out.report(&DECLARATION_CANT_BE_INLINED, kw.text_range(), "Declaration can't be inlined: it is open, abstract or an interface member.", None);
    }
}

/// `this` or `super` with nothing around that provides one.
fn outside_class(n: &SyntaxNode, out: &mut Sink) {
    if n.children().any(|c| c.kind() == SyntaxKind::LABEL_QUALIFIER) {
        return;
    }
    let this = n.kind() == SyntaxKind::THIS_EXPR;
    let provided = n.ancestors().any(|a| match a.kind() {
        SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL => true,
        // Receivers of lambdas and extensions (no `super` there).
        SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION => this,
        SyntaxKind::FUN | SyntaxKind::PROPERTY => this && a.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE),
        _ => false,
    });
    if provided {
        return;
    }
    if this {
        out.report(&NO_THIS, n.text_range(), "'this' is not defined in this context.", None);
    } else {
        out.report(&SUPER_NOT_AVAILABLE, n.text_range(), "No supertypes are accessible in this context.", None);
    }
}

fn argument_name(a: &SyntaxNode) -> Option<SyntaxToken> {
    a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT))
}

/// `f(a = 1, a = 2)`.
fn duplicate_names(list: &SyntaxNode, out: &mut Sink) {
    let mut seen = std::collections::HashSet::new();
    for a in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT) {
        if let Some(t) = argument_name(&a)
            && !seen.insert(ast::ident_text(&t))
        {
            out.report(&ARGUMENT_PASSED_TWICE, t.text_range(), "An argument is already passed for this parameter.", None);
        }
    }
}

fn java_owner(e: &Engine, sym: &Symbol) -> bool {
    let owner = sym.fq_name.trim_end_matches(".<init>");
    let owner = if sym.kind == crate::symbols::SymbolKind::Constructor { owner } else { owner.rsplit_once('.').map_or("", |(o, _)| o) };
    matches!(e.res.find_classifier(owner), Some(Classifier::Library(lc)) if !lc.is_kotlin)
}

/// Named arguments to Java, a positional argument and a named one for one parameter, and
/// `*spread` into a parameter that is not `vararg`.
fn arguments(e: &Engine, call: &SyntaxNode, out: &mut Sink) {
    let Some(list) = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST) else { return };
    let args: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT).collect();
    let named: Vec<SyntaxToken> = args.iter().filter_map(argument_name).collect();
    let spread = args.iter().any(|a| ast::token(a, SyntaxKind::STAR).is_some());
    if named.is_empty() && !spread {
        return;
    }
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(cands) = e.call_candidates(&callee) else { return };
    if cands.is_empty() {
        return;
    }
    if !named.is_empty() && cands.iter().all(|(s, _)| java_owner(e, s)) {
        for t in &named {
            out.report(&NAMED_ARGUMENTS_NOT_ALLOWED, t.text_range(), "Named arguments are not allowed for non-Kotlin functions.", None);
        }
        return;
    }
    let [(sym, _)] = cands.as_slice() else { return };
    let vararg = sym.params.iter().position(|p| p.vararg);
    for (i, a) in args.iter().enumerate() {
        let index = match argument_name(a) {
            Some(t) => match sym.params.iter().position(|p| p.name == ast::ident_text(&t)) {
                Some(j) => j,
                None => continue,
            },
            None if named.iter().any(|t| t.text_range().start() < a.text_range().start()) => break,
            None => match vararg {
                Some(v) if i >= v => v,
                _ => i,
            },
        };
        if argument_name(a).is_none() && named.iter().any(|t| sym.params.get(index).is_some_and(|p| p.name == ast::ident_text(t)) && !sym.params[index].vararg) {
            out.report(&ARGUMENT_PASSED_TWICE, a.text_range(), "An argument is already passed for this parameter.", None);
        }
        if let Some(star) = ast::token(a, SyntaxKind::STAR)
            && sym.params.get(index).is_some_and(|p| !p.vararg)
        {
            out.report(&NON_VARARG_SPREAD, star.text_range(), "The spread operator (*foo) may only be applied in a vararg position.", Some(Fix::delete("Remove '*'", star.text_range())));
        }
    }
}
