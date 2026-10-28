//! Unused imports, variables, parameters and private declarations; `var` that can be `val`.

use std::collections::HashSet;

use super::*;

/// Names called through operator syntax (or delegation), never by name.
const CONVENTION_NAMES: &[&str] = &[
    "plus", "minus", "times", "div", "rem", "mod", "rangeTo", "rangeUntil", "unaryPlus", "unaryMinus", "not", "inc", "dec",
    "contains", "get", "set", "invoke", "compareTo", "equals", "iterator", "next", "hasNext", "getValue", "setValue",
    "provideDelegate", "plusAssign", "minusAssign", "timesAssign", "divAssign", "remAssign",
];

/// Java serialization hooks, called reflectively.
const SERIALIZATION_NAMES: &[&str] = &["serialVersionUID", "readResolve", "writeReplace", "readObject", "writeObject", "readObjectNoData"];

fn is_convention_name(name: &str) -> bool {
    CONVENTION_NAMES.contains(&name) || name.strip_prefix("component").is_some_and(|n| n.parse::<u32>().is_ok())
}

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    if cx.on(&UNUSED_IMPORT) {
        unused_imports(cx, out);
    }
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::PROPERTY if is_local(&n) => local_variable(cx, &n, out),
            SyntaxKind::FUN => {
                unused_parameters(cx, &n, out);
                unused_declaration(cx, &n, out);
                if n.children().any(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) {
                    unused_receiver(cx, &n, out);
                }
            }
            SyntaxKind::PRIMARY_CONSTRUCTOR | SyntaxKind::SECONDARY_CONSTRUCTOR => unused_constructor_parameters(cx, &n, out),
            SyntaxKind::LAMBDA_EXPR => unused_lambda_parameters(cx, &n, out),
            SyntaxKind::CLASS => {
                unused_declaration(cx, &n, out);
                can_be_parameter(cx, &n, out);
            }
            SyntaxKind::PROPERTY | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS => unused_declaration(cx, &n, out),
            SyntaxKind::VALUE_PARAMETER => unused_constructor_property(cx, &n, out),
            _ => {}
        }
    }
}

pub(super) fn unused_imports(cx: &Cx, out: &mut Sink) {
    let Some(file) = cx.snap.file(cx.path) else { return };
    let summary = file.summary();
    // Names used anywhere outside the import list (identifiers and KDoc links).
    let mut used: HashSet<String> = HashSet::new();
    let mut kdoc = String::new();
    for t in tokens(&cx.root) {
        if t.kind() == SyntaxKind::IDENT && !t.parent_ancestors().any(|a| a.kind() == SyntaxKind::IMPORT_LIST) {
            used.insert(ast::ident_text(&t));
        } else if t.kind() == SyntaxKind::KDOC {
            kdoc.push_str(t.text());
        }
    }
    let resolver = cx.engine().map(|_| cx.snap.resolver(cx.path));
    let mut seen = HashSet::new();
    for imp in &summary.imports {
        if imp.star || imp.segments.is_empty() {
            continue;
        }
        let name = imp.alias.clone().unwrap_or_else(|| imp.segments.last().unwrap().0.clone());
        let duplicate = !seen.insert((imp.path(), imp.alias.clone()));
        if !duplicate {
            if used.contains(&name) || is_convention_name(&imp.segments.last().unwrap().0) || kdoc_mentions(&kdoc, &name) {
                continue;
            }
            // Unresolved imports are already errors; with no types available, stay quiet.
            match &resolver {
                Some(r) if r.resolve_import(imp).is_ok() => {}
                _ => continue,
            }
        }
        let range = imp.range;
        out.report(&UNUSED_IMPORT, range, "Unused import directive", Some(Fix::delete("Remove unused import", deletion_range(cx.text, range))));
    }
}

fn kdoc_mentions(kdoc: &str, name: &str) -> bool {
    kdoc.match_indices(name).any(|(i, _)| {
        let before = kdoc[..i].chars().next_back();
        let after = kdoc[i + name.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// `IDENT`s named `name` in `scope` after `after`, excluding `except`.
fn mentions(scope: &SyntaxNode, name: &str, after: TextSize, except: &SyntaxToken) -> Vec<SyntaxToken> {
    idents(scope).filter(|t| t.text_range().start() >= after && t != except && ast::ident_text(t) == name).collect()
}

fn local_variable(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    let Some(name_tok) = name_token(n) else { return };
    let name = ast::ident_text(&name_tok);
    if name == "_" {
        return;
    }
    let Some(block) = n.parent() else { return };
    let uses = mentions(&block, &name, n.text_range().end(), &name_tok);
    if uses.is_empty() && cx.on(&UNUSED_VARIABLE) {
        let init = initializer(n);
        let decl_range = deletion_range(cx.text, n.text_range());
        let fix = match &init {
            Some(e) if !is_pure(e) => Fix::replace(format!("Remove variable '{name}'"), n.text_range(), e.text().to_string()),
            _ => Fix::delete(format!("Remove variable '{name}'"), decl_range),
        };
        let fix = (!has_comments(n) && !has_annotations(n)).then_some(fix);
        out.report(&UNUSED_VARIABLE, name_tok.text_range(), format!("Variable '{name}' is never used"), fix);
        return;
    }
    if !cx.on(&CAN_BE_VAL) {
        return;
    }
    let Some(var) = ast::token(n, SyntaxKind::VAR_KW) else { return };
    let decl = ast::PropertyDecl::cast(n.clone()).unwrap();
    if !decl.has_initializer() || decl.has_delegate() {
        return;
    }
    if !uses.iter().any(is_write) {
        out.report(
            &CAN_BE_VAL,
            var.text_range(),
            "Variable is never modified, so it can be declared using 'val'",
            Some(Fix::replace("Change to 'val'", var.text_range(), "val")),
        );
    }
}

/// The token is assigned, incremented, or referenced as a (mutable) property.
fn is_write(t: &SyntaxToken) -> bool {
    let Some(nr) = t.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF) else { return false };
    let Some(parent) = nr.parent() else { return false };
    match parent.kind() {
        SyntaxKind::BINARY_EXPR => assignment_op(&parent).is_some() && parent.first_child().as_ref() == Some(&nr),
        SyntaxKind::PREFIX_EXPR | SyntaxKind::POSTFIX_EXPR => {
            binary_op(&parent).is_some_and(|op| matches!(op.kind(), SyntaxKind::PLUS2 | SyntaxKind::MINUS2))
        }
        SyntaxKind::CALLABLE_REFERENCE => true,
        _ => false,
    }
}

/// Evaluating the expression has no side effects.
fn is_pure(e: &SyntaxNode) -> bool {
    match e.kind() {
        SyntaxKind::LITERAL | SyntaxKind::NAME_REF | SyntaxKind::THIS_EXPR | SyntaxKind::CLASS_LITERAL | SyntaxKind::CALLABLE_REFERENCE => true,
        SyntaxKind::STRING_TEMPLATE => e.children().all(|c| matches!(c.kind(), SyntaxKind::LITERAL_STRING_ENTRY | SyntaxKind::ESCAPE_STRING_ENTRY)),
        _ => false,
    }
}

/// Uses of the parameter declared by `tok` inside `scope`. With types, a use must resolve to
/// the parameter; without, any same-named reference counts (shadowing hides unused ones).
fn parameter_used(cx: &Cx, scope: &SyntaxNode, tok: &SyntaxToken) -> bool {
    let name = ast::ident_text(tok);
    let engine = cx.engine();
    idents(scope).any(|t| {
        if t == *tok || t.text_range().start() < tok.text_range().end() || ast::ident_text(&t) != name {
            return false;
        }
        let Some(nr) = t.parent().filter(|p| p.kind() == SyntaxKind::NAME_REF) else { return false };
        // `other.name` is a member of something else.
        let selector = nr.parent().is_some_and(|p| {
            matches!(p.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && p.first_child().as_ref() != Some(&nr)
                || p.kind() == SyntaxKind::CALL_EXPR
                    && p.parent().is_some_and(|q| matches!(q.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && q.first_child().as_ref() != Some(&p))
        });
        match engine {
            // `recv.name()` may call a function-typed parameter with a receiver.
            // (members win; otherwise it is the local function value).
            Some(e) if selector => {
                let defs = e.resolve_name_ref(&nr);
                defs.iter().any(|d| matches!(d, crate::analysis::ide::infer::Def::Local(l) if l.name_range == tok.text_range()))
                    || defs.is_empty() && e.find_local(&nr, &name).is_some_and(|l| l.name_range == tok.text_range())
            }
            Some(e) => e.find_local(&nr, &name).is_none_or(|l| l.name_range == tok.text_range()),
            None => true,
        }
    })
}

/// A parameter whose name is part of a fixed contract (frameworks, overrides, references).
fn signature_is_fixed(cx: &Cx, f: &SyntaxNode) -> bool {
    if ["override", "open", "abstract", "actual", "expect", "operator", "external"].iter().any(|m| has_modifier(f, m)) {
        return true;
    }
    // Annotated functions are usually called by frameworks (tests, web handlers, DI);
    // deprecated ones keep their signature.
    let annotated = modifiers(f).is_some_and(|m| {
        m.annotations().any(|a| !matches!(a.type_ref().map(|t| t.syntax().text().to_string()).as_deref(), Some("Suppress" | "kotlin.Suppress")))
    });
    if annotated {
        return true;
    }
    if owner(f).is_some_and(|o| is_interface(&o) || has_modifier(&o, "expect")) {
        return true;
    }
    let Some(name) = name_token(f).map(|t| ast::ident_text(&t)) else { return true };
    if name == "main" {
        return true;
    }
    // Passed as a function reference: the signature must match a function type.
    let reference = format!("::{name}");
    if is_local(f) || has_modifier(f, "private") {
        cx.text.contains(&reference)
    } else {
        cx.snap.files.values().any(|file| file.text.contains(&reference))
    }
}

/// Bodies that intentionally ignore their parameters: `{}`, `TODO()`, `throw ...`.
fn is_stub(body: &SyntaxNode) -> bool {
    if is_empty_block(body) || is_todo(body) {
        return true;
    }
    let stmt = if body.kind() == SyntaxKind::BLOCK { only_statement(body) } else { Some(body.clone()) };
    stmt.is_some_and(|s| s.kind() == SyntaxKind::THROW_EXPR || s.kind() == SyntaxKind::RETURN_EXPR && s.first_child().is_some_and(|c| c.kind() == SyntaxKind::THROW_EXPR))
}

fn unused_parameters(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_PARAMETER) {
        return;
    }
    let decl = ast::FunDecl::cast(f.clone()).unwrap();
    let body = decl.body_block().map(|b| b.syntax().clone()).or_else(|| initializer(f));
    let Some(body) = body else { return };
    if is_stub(&body) {
        return;
    }
    let unused: Vec<ast::ValueParameter> = decl
        .parameters()
        .into_iter()
        .filter(|p| p.name_token().is_some_and(|t| ast::ident_text(&t) != "_" && !has_annotations(p.syntax()) && !parameter_used(cx, f, &t)))
        .collect();
    if unused.is_empty() || signature_is_fixed(cx, f) {
        return;
    }
    for p in unused {
        let tok = p.name_token().unwrap();
        let name = ast::ident_text(&tok);
        let fix = remove_parameter_fix(cx, f, &p);
        out.report(&UNUSED_PARAMETER, tok.text_range(), format!("Parameter '{name}' is never used"), fix);
    }
}

/// Plain (non-property) constructor parameters never used in initializers.
fn unused_constructor_parameters(cx: &Cx, ctor: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_PARAMETER) {
        return;
    }
    let Some(class) = ctor.ancestors().find(|a| a.kind() == SyntaxKind::CLASS) else { return };
    if has_annotations(&class) || ["data", "value", "inline", "expect", "actual", "annotation", "external"].iter().any(|m| has_modifier(&class, m)) {
        return;
    }
    let scope = if ctor.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR { ctor.clone() } else { class.clone() };
    // `@Inject constructor(...)`: the framework decides the parameters.
    if has_annotations(ctor) {
        return;
    }
    if ctor.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR && ( ctor.children().find(|c| c.kind() == SyntaxKind::BLOCK).is_none_or(|b| is_stub(&b)) && !ctor.children().any(|c| c.kind() == SyntaxKind::CONSTRUCTOR_DELEGATION_CALL)) {
        return;
    }
    let Some(list) = ctor.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST) else { return };
    for p in list.children().filter_map(ast::ValueParameter::cast) {
        let Some(tok) = p.name_token() else { continue };
        let name = ast::ident_text(&tok);
        if p.property_keyword().is_some() || name == "_" || has_annotations(p.syntax()) || parameter_used(cx, &scope, &tok) {
            continue;
        }
        out.report(&UNUSED_PARAMETER, tok.text_range(), format!("Constructor parameter '{name}' is never used"), None);
    }
}

/// Lambda parameters that are never used can be `_`.
fn unused_lambda_parameters(cx: &Cx, lambda: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_ANONYMOUS_PARAMETER) {
        return;
    }
    let Some(list) = lambda.children().find(|c| c.kind() == SyntaxKind::LAMBDA_PARAMETER_LIST) else { return };
    for p in list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER) {
        let mut names: Vec<SyntaxToken> = name_token(&p).into_iter().collect();
        for d in p.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_DECLARATION) {
            names.extend(d.children().filter(|c| c.kind() == SyntaxKind::DESTRUCTURING_ENTRY).filter_map(|c| name_token(&c)));
        }
        for tok in names {
            let name = ast::ident_text(&tok);
            if name == "_" || parameter_used(cx, lambda, &tok) {
                continue;
            }
            out.report(
                &UNUSED_ANONYMOUS_PARAMETER,
                tok.text_range(),
                format!("Parameter '{name}' is never used, could be renamed to _"),
                Some(Fix::replace("Rename to _", tok.text_range(), "_")),
            );
        }
    }
}

/// The range that removes `item` and one adjacent comma from its list.
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

/// "Remove parameter": for private and local functions, whose calls are all in this file,
/// the parameter and the matching argument of every call.
fn remove_parameter_fix(cx: &Cx, f: &SyntaxNode, p: &ast::ValueParameter) -> Option<Fix> {
    if !(is_local(f) || has_modifier(f, "private")) || p.is_vararg() {
        return None;
    }
    let e = cx.engine()?;
    let fname = name_token(f)?;
    let pname = ast::ident_text(&p.name_token()?);
    let list = f.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST)?;
    let params: Vec<SyntaxNode> = list.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER).collect();
    let index = params.iter().position(|x| x == p.syntax())?;
    let mut edits = vec![Edit { range: list_item_removal(&params, index), text: String::new() }];
    let name = ast::ident_text(&fname);
    for t in idents(&cx.root).filter(|t| *t != fname && ast::ident_text(t) == name) {
        let nr = t.parent().filter(|n| n.kind() == SyntaxKind::NAME_REF)?;
        // Only direct calls that resolve to this function.
        let defs = e.resolve_name_ref(&nr);
        let ours = defs.iter().any(|d| match d {
            crate::analysis::ide::infer::Def::Symbol { sym, .. } => matches!(&sym.origin, crate::symbols::Origin::Source { name_range, .. } if *name_range == fname.text_range()),
            crate::analysis::ide::infer::Def::Local(l) => l.name_range == fname.text_range(),
            _ => false,
        });
        if !ours {
            continue;
        }
        let call = nr.parent().filter(|c| c.kind() == SyntaxKind::CALL_EXPR && c.first_child().as_ref() == Some(&nr))?;
        let args_list = call.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_LIST);
        let args: Vec<SyntaxNode> = args_list.iter().flat_map(|l| l.children().filter(|a| a.kind() == SyntaxKind::VALUE_ARGUMENT).collect::<Vec<_>>()).collect();
        if args.iter().any(|a| ast::token(a, SyntaxKind::STAR).is_some()) {
            return None;
        }
        let arg_name = |a: &SyntaxNode| a.children().find(|c| c.kind() == SyntaxKind::VALUE_ARGUMENT_NAME).and_then(|n| ast::token(&n, SyntaxKind::IDENT)).map(|t| ast::ident_text(&t));
        let target = args.iter().position(|a| arg_name(a).as_deref() == Some(pname.as_str())).or_else(|| (index < args.len() && arg_name(&args[index]).is_none()).then_some(index));
        match target {
            Some(i) => edits.push(Edit { range: list_item_removal(&args, i), text: String::new() }),
            // A trailing lambda for the last parameter cannot just disappear.
            None if index + 1 == params.len() && call.children().any(|c| c.kind() == SyntaxKind::LAMBDA_ARGUMENT) => return None,
            None => {}
        }
    }
    Some(Fix::new(format!("Remove parameter '{pname}'"), edits))
}

/// `TODO()` / `{ TODO() }`.
fn is_todo(body: &SyntaxNode) -> bool {
    let e = if body.kind() == SyntaxKind::BLOCK {
        let Some(s) = only_statement(body) else { return false };
        s
    } else {
        body.clone()
    };
    e.kind() == SyntaxKind::CALL_EXPR && e.first_child().is_some_and(|c| c.text() == "TODO")
}

fn unused_declaration(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_SYMBOL) {
        return;
    }
    let local = is_local(n);
    if n.kind() == SyntaxKind::PROPERTY && local {
        return; // UNUSED_VARIABLE
    }
    if !(local || has_modifier(n, "private")) || has_annotations(n) {
        return;
    }
    if ["override", "operator", "expect", "actual", "external", "companion"].iter().any(|m| has_modifier(n, m)) {
        return;
    }
    let Some(name_tok) = name_token(n) else { return };
    let name = ast::ident_text(&name_tok);
    if SERIALIZATION_NAMES.contains(&name.as_str()) || name == "main" || name == "_" {
        return;
    }
    if n.ancestors().skip(1).any(|a| matches!(a.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) && reflective(&a)) {
        return;
    }
    if n.kind() == SyntaxKind::PROPERTY && ast::PropertyDecl::cast(n.clone()).unwrap().is_destructuring() {
        return;
    }
    let scope = if local { n.parent().unwrap() } else { private_scope(cx, n) };
    let after = if local { n.text_range().end() } else { TextSize::from(0) };
    // Recursive uses inside the declaration itself do not count.
    let used = idents(&scope).any(|t| {
        t.text_range().start() >= after && !n.text_range().contains_range(t.text_range()) && ast::ident_text(&t) == name && !is_declared_name(&t) && !refers_elsewhere(cx, &t, name_tok.text_range(), true)
    });
    if used || named_in_string(&cx.root, &name) {
        return;
    }
    let what = match n.kind() {
        SyntaxKind::FUN if local => "Local function",
        SyntaxKind::FUN => "Private function",
        SyntaxKind::PROPERTY => "Private property",
        SyntaxKind::TYPEALIAS => "Type alias",
        SyntaxKind::OBJECT_DECLARATION => "Object",
        _ if local => "Local class",
        _ => "Private class",
    };
    let fix = Fix::delete(format!("Remove '{name}'"), deletion_range(cx.text, n.text_range()));
    out.report(&UNUSED_SYMBOL, name_tok.text_range(), format!("{what} '{name}' is never used"), Some(fix));
}

/// Class annotations of frameworks that read or write members by reflection (serialization,
/// ORM, configuration binding). Others (`@Service`, `@Component`) leave private members unused.
const REFLECTIVE_ANNOTATIONS: &[&str] = &[
    "Serializable", "Entity", "Embeddable", "MappedSuperclass", "Table", "Document", "Node", "RelationshipProperties", "RedisHash",
    "JsonAutoDetect", "JsonClass", "JsonSerializable", "XmlRootElement", "XmlAccessorType", "XmlType", "Root", "Parcelize",
    "ConfigurationProperties", "DynamoDbBean", "DynamoDBTable", "Keep",
];

/// Whether the members of `class` may be used by a framework through reflection.
fn reflective(class: &SyntaxNode) -> bool {
    modifiers(class).is_some_and(|l| {
        l.annotations().any(|a| {
            let name = a.type_ref().map(|t| t.syntax().text().to_string()).unwrap_or_default();
            let name = name.split('<').next().unwrap_or_default();
            REFLECTIVE_ANNOTATIONS.contains(&name.rsplit('.').next().unwrap_or_default())
        })
    })
}

/// Where a private declaration can be used: the file for top-level ones, otherwise the
/// top-level declaration around it (companions and outer classes see nested private members
/// only in some cases; the whole top-level declaration covers all of them).
fn private_scope(cx: &Cx, n: &SyntaxNode) -> SyntaxNode {
    n.ancestors().skip(1).find(|a| a.parent().is_some_and(|p| p.kind() == SyntaxKind::KOTLIN_FILE)).unwrap_or_else(|| cx.root.clone())
}

/// The name of a declaration (another class's property, a parameter), not a use.
fn is_declared_name(t: &SyntaxToken) -> bool {
    t.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            SyntaxKind::PROPERTY | SyntaxKind::FUN | SyntaxKind::VALUE_PARAMETER | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS
        ) && name_token(&p).as_ref() == Some(t)
    })
}

/// An occurrence of a declaration's name that is certainly something else: a named argument
/// (`f(name = 1)`, unless it may name the constructor parameter `decl` itself) or a reference
/// that is not a call and resolves only to other declarations (`other.name`, a local `name`).
fn refers_elsewhere(cx: &Cx, t: &SyntaxToken, decl: TextRange, named_args: bool) -> bool {
    let Some(p) = t.parent() else { return false };
    if p.kind() == SyntaxKind::VALUE_ARGUMENT_NAME {
        return named_args;
    }
    // Calls may pick another overload (or a property of the same name) than the compiler.
    if p.kind() != SyntaxKind::NAME_REF || p.parent().is_some_and(|c| c.kind() == SyntaxKind::CALL_EXPR && c.first_child().as_ref() == Some(&p)) {
        return false;
    }
    let Some(e) = cx.engine() else { return false };
    let defs = e.resolve_name_ref(&p);
    !defs.is_empty()
        && defs.iter().all(|d| match d {
            crate::analysis::ide::infer::Def::Symbol { sym, .. } => {
                !matches!(&sym.origin, crate::symbols::Origin::Source { file, name_range, .. } if file.as_path() == cx.path && *name_range == decl)
            }
            crate::analysis::ide::infer::Def::Local(l) => l.name_range != decl,
            crate::analysis::ide::infer::Def::Package(_) => true,
        })
}

/// Members named in strings are used by reflection (`getDeclaredMethod("name")`) or by
/// frameworks through annotation arguments (`@MethodSource("cases")`, `@EnabledIf("Class#method")`).
fn named_in_string(root: &SyntaxNode, name: &str) -> bool {
    tokens(root).any(|t| {
        t.kind() == SyntaxKind::STRING_TEXT
            && (t.text() == name || t.text().rsplit(['#', '.']).next() == Some(name) && t.parent_ancestors().any(|a| a.kind() == SyntaxKind::ANNOTATION))
    })
}

/// `class A(private val x: Int)` where `x` is never used.
fn unused_constructor_property(cx: &Cx, p: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_SYMBOL) || !has_modifier(p, "private") || has_annotations(p) {
        return;
    }
    let vp = ast::ValueParameter::cast(p.clone()).unwrap();
    if vp.property_keyword().is_none() {
        return;
    }
    let Some(class) = p.ancestors().find(|a| a.kind() == SyntaxKind::CLASS) else { return };
    if !p.ancestors().any(|a| a.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) {
        return;
    }
    if reflective(&class) || ["data", "value", "inline", "expect", "actual", "annotation"].iter().any(|m| has_modifier(&class, m)) {
        return;
    }
    let Some(tok) = vp.name_token() else { return };
    let name = ast::ident_text(&tok);
    let used = idents(&private_scope(cx, &class)).any(|t| t != tok && ast::ident_text(&t) == name && !is_declared_name(&t) && !refers_elsewhere(cx, &t, tok.text_range(), false));
    if used || named_in_string(&cx.root, &name) {
        return;
    }
    out.report(&UNUSED_SYMBOL, tok.text_range(), format!("Private property '{name}' is never used"), None);
}

/// `class A(private val x: Int) { val y = x * 2 }`: `x` is only read while constructing,
/// so it need not be a property.
pub(super) fn can_be_parameter(cx: &Cx, class: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&CAN_BE_PARAMETER) || ["data", "value", "inline", "annotation", "expect", "actual"].iter().any(|m| has_modifier(class, m)) {
        return;
    }
    let Some(ctor) = class.children().find(|c| c.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR) else { return };
    let body = class.children().find(|c| c.kind() == SyntaxKind::CLASS_BODY);
    // What runs during construction (lambdas and objects in it may run later).
    let construction = |n: &SyntaxNode| -> bool {
        let Some(body) = &body else { return false };
        for a in n.ancestors() {
            if matches!(a.kind(), SyntaxKind::LAMBDA_EXPR | SyntaxKind::ANONYMOUS_FUNCTION | SyntaxKind::OBJECT_LITERAL | SyntaxKind::FUN | SyntaxKind::PROPERTY_ACCESSOR | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION) {
                return false;
            }
            if a.parent().as_ref() == Some(body) {
                return matches!(a.kind(), SyntaxKind::CLASS_INITIALIZER | SyntaxKind::PROPERTY);
            }
        }
        false
    };
    for p in ctor.descendants().filter(|d| d.kind() == SyntaxKind::VALUE_PARAMETER) {
        let Some(kw) = ast::token(&p, SyntaxKind::VAL_KW).or_else(|| ast::token(&p, SyntaxKind::VAR_KW)) else { continue };
        let Some(m) = modifiers(&p) else { continue };
        if !m.modifiers().any(|t| t.text() == "private") || m.modifiers().count() != 1 || m.annotations().next().is_some() {
            continue;
        }
        let Some(name) = name_token(&p).map(|t| ast::ident_text(&t)) else { continue };
        let uses: Vec<SyntaxNode> = class.descendants().filter(|d| d.kind() == SyntaxKind::NAME_REF && d.text() == name.as_str()).collect();
        // `this.x` reads the property; plain names during construction read the parameter.
        let property_use = uses.iter().any(|u| {
            let qualified = u.parent().is_some_and(|q| matches!(q.kind(), SyntaxKind::DOT_QUALIFIED_EXPR | SyntaxKind::SAFE_QUALIFIED_EXPR) && q.first_child().as_ref() != Some(u));
            qualified || !construction(u)
        });
        if property_use {
            continue;
        }
        // Remove `private val ` (up to the name).
        let Some(name_tok) = name_token(&p) else { continue };
        let start = m.syntax().text_range().start();
        out.report(&CAN_BE_PARAMETER, kw.text_range(), format!("Constructor parameter '{name}' is never used as a property"), Some(Fix::delete("Remove 'val' or 'var' from parameter", TextRange::new(start, name_tok.text_range().start()))));
    }
}

/// `fun String.f() = 1`: the receiver is never used.
pub(super) fn unused_receiver(cx: &Cx, f: &SyntaxNode, out: &mut Sink) {
    if !cx.on(&UNUSED_RECEIVER_PARAMETER) || ["override", "abstract", "open", "expect", "actual", "external", "operator", "infix"].iter().any(|m| has_modifier(f, m)) {
        return;
    }
    let Some(recv) = f.children().find(|c| c.kind() == SyntaxKind::RECEIVER_TYPE) else { return };
    let Some(decl) = ast::FunDecl::cast(f.clone()) else { return };
    let body = match decl.body_block() {
        Some(b) => b.syntax().clone(),
        None => match initializer(f) {
            Some(v) => v,
            None => return,
        },
    };
    // Annotated (`@Suppress`, test DSLs) or in an interface: the receiver is part of a contract.
    if modifiers(f).is_some_and(|m| m.annotations().next().is_some()) || f.parent().and_then(|b| b.parent()).is_some_and(|c| c.kind() == SyntaxKind::CLASS && ast::token(&c, SyntaxKind::INTERFACE_KW).is_some()) {
        return;
    }
    // Generic receivers (`T.f()`) select overloads by type.
    if !decl.type_parameters().is_empty() {
        return;
    }
    let Some(e) = cx.engine() else { return };
    let Some(tr) = recv.children().find_map(ast::TypeRef::cast) else { return };
    let t = e.resolve_syntax_type(&tr);
    if crate::analysis::ide::infer::fq_of(&t).is_none() {
        return;
    }
    if uses_receiver(e, &body, &t) == Some(false) {
        out.report(&UNUSED_RECEIVER_PARAMETER, recv.text_range(), "Receiver parameter is never used", None);
    }
}
