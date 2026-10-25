//! Declaration shapes the compiler rejects: accessors and delegates, `expect` bodies,
//! `fun interface`s, value classes, sealed hierarchies, enum entries with abstract members,
//! repeated supertypes; and statements whose value is unused.

use super::super::*;
use crate::analysis::ide::infer::{Def, Engine, LocalKind, fq_of};
use crate::symbols::{Modifiers, Origin, SymbolKind, TypeRef};

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    for n in cx.root.descendants() {
        if in_error(&n) {
            continue;
        }
        match n.kind() {
            SyntaxKind::PROPERTY => {
                property(&n, out);
                accessor_types(cx, &n, out);
            }
            SyntaxKind::FUN => function(&n, out),
            SyntaxKind::CLASS => {
                fun_interface(&n, out);
                value_class(cx, &n, out);
                sealed_constructor(&n, out);
                enum_entries(&n, out);
            }
            SyntaxKind::LAMBDA_EXPR | SyntaxKind::LITERAL | SyntaxKind::NAME_REF | SyntaxKind::THIS_EXPR | SyntaxKind::IS_EXPR | SyntaxKind::BINARY_EXPR | SyntaxKind::STRING_TEMPLATE | SyntaxKind::CLASS_LITERAL | SyntaxKind::CALLABLE_REFERENCE => {
                unused_expression(cx, &n, out)
            }
            SyntaxKind::CALL_EXPR => implicit_invoke(cx, &n, out),
            _ => {}
        }
        if matches!(n.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL) {
            supertypes(cx, &n, out);
        }
    }
}

/// `get` / `set` of an accessor.
fn accessor_kind(acc: &SyntaxNode) -> Option<String> {
    ast::token(acc, SyntaxKind::IDENT).map(|t| t.text().to_string()).filter(|t| t == "get" || t == "set")
}

/// An accessor with a body (`get() = …`, `set(v) { … }`), not just a visibility change.
fn has_accessor_body(acc: &SyntaxNode) -> bool {
    ast::token(acc, SyntaxKind::EQ).is_some() || acc.children().any(|c| c.kind() == SyntaxKind::BLOCK)
}

const VISIBILITIES: &[&str] = &["private", "protected", "internal", "public"];

/// The visibility written on a declaration or accessor.
fn visibility(n: &SyntaxNode) -> Option<String> {
    modifiers(n).and_then(|m| m.modifiers().map(|t| t.text().to_string()).find(|t| VISIBILITIES.contains(&t.as_str())))
}

fn rank(v: &str) -> u8 {
    match v {
        "private" => 0,
        "protected" | "internal" => 1,
        _ => 2,
    }
}

fn property(p: &SyntaxNode, out: &mut Sink) {
    let Some(name) = name_token(p) else { return };
    let decl = ast::PropertyDecl::cast(p.clone()).unwrap();
    let n = name.text();
    let at = name.text_range();
    let accessors: Vec<SyntaxNode> = p.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR).collect();
    let getter = accessors.iter().find(|a| accessor_kind(a).as_deref() == Some("get"));
    let setter = accessors.iter().find(|a| accessor_kind(a).as_deref() == Some("set"));
    let owner = owner(p);
    let interface = owner.as_ref().is_some_and(is_interface);
    let init = decl.has_initializer();
    let delegate = decl.has_delegate();
    if !decl.is_var()
        && let Some(s) = setter
    {
        out.report(&VAL_WITH_SETTER, s.text_range(), "A 'val' property cannot have a setter.", None);
    }
    if delegate {
        for a in accessors.iter().filter(|a| has_accessor_body(a)) {
            out.report(&ACCESSOR_FOR_DELEGATED_PROPERTY, a.text_range(), "Delegated property cannot have accessors with non-default implementations.", None);
        }
        if interface {
            out.report(&DELEGATED_PROPERTY_IN_INTERFACE, at, "Delegated properties are not allowed in interfaces.", None);
        } else if has_modifier(p, "abstract") {
            out.report(&ABSTRACT_DELEGATED_PROPERTY, at, "Delegated property cannot be abstract.", None);
        }
    }
    if decl.receiver().is_some() && init {
        out.report(&EXTENSION_PROPERTY_WITH_BACKING_FIELD, at, "Extension property cannot be initialized because it has no backing field.", None);
    }
    if has_modifier(p, "inline") && init {
        out.report(&INLINE_PROPERTY_WITH_BACKING_FIELD, at, "Inline property cannot have a backing field.", None);
    }
    if has_modifier(p, "expect") {
        if init {
            out.report(&EXPECTED_PROPERTY_INITIALIZER, at, "Expected property cannot have an initializer.", None);
        }
        if delegate {
            out.report(&EXPECTED_DELEGATED_PROPERTY, at, "Expected property cannot be delegated.", None);
        }
    }
    // A type is needed: written, from an initializer, a delegate or a getter.
    let typeless = decl.type_ref().is_none() && !init && !delegate && !getter.is_some_and(|g| ast::token(g, SyntaxKind::EQ).is_some()) && !decl.is_destructuring();
    if typeless {
        if is_local(p) {
            out.report(&VARIABLE_WITH_NO_TYPE_NO_INITIALIZER, at, "This variable must either have an explicit type or be initialized.", None);
        } else if !getter.is_some_and(has_accessor_body) {
            out.report(&PROPERTY_WITH_NO_TYPE_NO_INITIALIZER, at, "This property must have an explicit type, be initialized, or be delegated.", None);
        }
    }
    // Accessor visibility.
    let prop_vis = visibility(p).unwrap_or_else(|| "public".into());
    if let Some(g) = getter
        && let Some(v) = visibility(g)
        && v != prop_vis
    {
        out.report(&GETTER_VISIBILITY_DIFFERS_FROM_PROPERTY_VISIBILITY, g.text_range(), "Getter visibility must be the same as property visibility.", None);
    }
    if let Some(s) = setter
        && let Some(v) = visibility(s)
    {
        let abstract_ = has_modifier(p, "abstract") || interface && !init && !accessors.iter().any(has_accessor_body);
        if v == "private" && abstract_ {
            out.report(&PRIVATE_SETTER_FOR_ABSTRACT_PROPERTY, s.text_range(), "Private setters are not allowed for abstract properties.", None);
        } else if v == "private" && has_modifier(p, "open") {
            out.report(&PRIVATE_SETTER_FOR_OPEN_PROPERTY, s.text_range(), "Private setters are not allowed for open properties.", None);
        } else if rank(&v) > rank(&prop_vis) || rank(&v) == 1 && rank(&prop_vis) == 1 && v != prop_vis {
            out.report(&SETTER_VISIBILITY_INCONSISTENT_WITH_PROPERTY_VISIBILITY, s.text_range(), "Setter visibility must be the same or less permissive than property visibility.", None);
        }
    }
    if let Some(o) = &owner
        && o.kind() == SyntaxKind::CLASS
        && (has_modifier(o, "value") || has_modifier(o, "inline"))
    {
        if delegate {
            out.report(&DELEGATED_PROPERTY_INSIDE_VALUE_CLASS, at, "Value class cannot have delegated properties.", None);
        } else if init || has_modifier(p, "lateinit") {
            out.report(&PROPERTY_WITH_BACKING_FIELD_INSIDE_VALUE_CLASS, at, "Value class cannot have properties with backing fields.", None);
        }
    }
    let _ = n;
}

/// `get(): T` / `set(v: T)` with a written type other than the property's.
fn accessor_types(cx: &Cx, p: &SyntaxNode, out: &mut Sink) {
    let Some(declared) = ast::child::<ast::TypeRef>(p) else { return };
    let Some(e) = cx.engine() else { return };
    let resolved = |t: &ast::TypeRef| match e.resolve_syntax_type(t) {
        TypeRef::Named { fq: Some(fq), nullable, .. } => Some((fq, nullable)),
        _ => None,
    };
    let Some(want) = resolved(&declared) else { return };
    for acc in p.children().filter(|c| c.kind() == SyntaxKind::PROPERTY_ACCESSOR) {
        let (written, rule, what) = match accessor_kind(&acc).as_deref() {
            Some("get") => (ast::child::<ast::TypeRef>(&acc), &WRONG_GETTER_RETURN_TYPE, "Getter return type"),
            Some("set") => (acc.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).and_then(|l| l.children().find(|c| c.kind() == SyntaxKind::VALUE_PARAMETER)).and_then(|v| ast::child::<ast::TypeRef>(&v)), &WRONG_SETTER_PARAMETER_TYPE, "Setter parameter type"),
            _ => continue,
        };
        let Some(written) = written else { continue };
        if resolved(&written).is_some_and(|got| got != want) {
            let fix = Fix::replace(format!("Change to '{}'", declared.syntax().text()), written.syntax().text_range(), declared.syntax().text().to_string());
            out.report(rule, written.syntax().text_range(), format!("{what} must be '{}'.", declared.syntax().text()), Some(fix));
        }
    }
}

fn function(f: &SyntaxNode, out: &mut Sink) {
    let Some(name) = name_token(f) else { return };
    let body = f.children().any(|c| c.kind() == SyntaxKind::BLOCK) || ast::token(f, SyntaxKind::EQ).is_some();
    if has_modifier(f, "expect") && body {
        out.report(&EXPECTED_DECLARATION_WITH_BODY, name.text_range(), "Expected declaration must not have a body.", None);
    }
    if owner(f).as_ref().is_some_and(is_interface) && !body && has_modifier(f, "private") {
        out.report(&PRIVATE_FUNCTION_WITH_NO_BODY, name.text_range(), format!("Function '{}' without body cannot be private.", name.text()), Some(add_body_fix(f)));
    }
}

// ------------------------------------------------------------ fun interfaces

/// A `fun interface` declares exactly one abstract function (checked when it has no
/// supertypes, which could contribute others), without type parameters or defaults, and no
/// abstract properties.
fn fun_interface(c: &SyntaxNode, out: &mut Sink) {
    if ast::token(c, SyntaxKind::FUN_KW).is_none() || !is_interface(c) {
        return;
    }
    let Some(name) = name_token(c) else { return };
    let members: Vec<SyntaxNode> = c.children().filter(|x| x.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children().collect::<Vec<_>>()).collect();
    let mut abstract_funs = Vec::new();
    for m in &members {
        match m.kind() {
            SyntaxKind::FUN if !m.children().any(|c| c.kind() == SyntaxKind::BLOCK) && ast::token(m, SyntaxKind::EQ).is_none() => abstract_funs.push(m.clone()),
            SyntaxKind::PROPERTY if !m.children().any(|a| a.kind() == SyntaxKind::PROPERTY_ACCESSOR && has_accessor_body(&a)) => {
                if let Some(t) = name_token(m) {
                    out.report(&FUN_INTERFACE_WITH_ABSTRACT_PROPERTY, t.text_range(), "Fun interfaces cannot have abstract properties.", None);
                }
            }
            _ => {}
        }
    }
    let has_supertypes = c.children().any(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST);
    if !has_supertypes && abstract_funs.len() != 1 {
        out.report(&FUN_INTERFACE_WRONG_COUNT_OF_ABSTRACT_MEMBERS, name.text_range(), "Fun interfaces must have exactly one abstract method.", None);
    }
    for f in &abstract_funs {
        let Some(fname) = name_token(f) else { continue };
        if f.children().any(|c| c.kind() == SyntaxKind::TYPE_PARAMETER_LIST) {
            out.report(&FUN_INTERFACE_ABSTRACT_METHOD_WITH_TYPE_PARAMETERS, fname.text_range(), "Single abstract member cannot declare type parameters.", None);
        }
        let defaults = f.children().filter(|c| c.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).any(|p| ast::token(&p, SyntaxKind::EQ).is_some());
        if defaults {
            out.report(&FUN_INTERFACE_ABSTRACT_METHOD_WITH_DEFAULT_VALUE, fname.text_range(), "Single abstract member cannot declare default values.", None);
        }
    }
}

// ------------------------------------------------------------ value classes

fn value_class(cx: &Cx, c: &SyntaxNode, out: &mut Sink) {
    let Some(modifier) = modifiers(c).and_then(|m| m.modifiers().find(|t| t.text() == "value" || t.text() == "inline")) else { return };
    let Some(name) = name_token(c) else { return };
    let at = name.text_range();
    if has_modifier(c, "expect") || ast::token(c, SyntaxKind::CLASS_KW).is_none() {
        return;
    }
    if let Some(m) = modifiers(c).and_then(|m| m.modifiers().find(|t| matches!(t.text(), "open" | "abstract" | "sealed"))) {
        out.report(&VALUE_CLASS_NOT_FINAL, m.text_range(), "Value classes can be only final.", Some(Fix::delete(format!("Remove '{}'", m.text()), m.text_range())));
    }
    if has_modifier(c, "inner") || is_local(c) {
        out.report(&VALUE_CLASS_NOT_TOP_LEVEL, at, "Value classes cannot be local or inner.", None);
    }
    let params: Vec<SyntaxNode> = c
        .children()
        .filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR)
        .flat_map(|p| p.children().filter(|l| l.kind() == SyntaxKind::VALUE_PARAMETER_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).collect::<Vec<_>>())
        .filter(|p| p.kind() == SyntaxKind::VALUE_PARAMETER)
        .collect();
    match params.len() {
        0 => out.report(&VALUE_CLASS_EMPTY_CONSTRUCTOR, at, "Value class must have exactly one primary constructor parameter.", None),
        1 => {
            let vp = ast::ValueParameter::cast(params[0].clone()).unwrap();
            let read_only = vp.property_keyword().is_some_and(|k| k.kind() == SyntaxKind::VAL_KW);
            if !read_only && let Some(t) = vp.name_token() {
                out.report(&VALUE_CLASS_CONSTRUCTOR_NOT_FINAL_READ_ONLY_PARAMETER, t.text_range(), "Value class primary constructor must only have final read-only (val) property parameter.", None);
            }
        }
        _ => out.report(&INLINE_CLASS_CONSTRUCTOR_WRONG_PARAMETERS_SIZE, at, "Value class must have exactly one primary constructor parameter.", None),
    }
    // Only interfaces as supertypes.
    for call in c.children().filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).filter(|e| e.kind() == SyntaxKind::SUPER_TYPE_CALL_ENTRY) {
        out.report(&VALUE_CLASS_CANNOT_EXTEND_CLASSES, call.text_range(), "Value class cannot extend classes.", None);
    }
    // `@JvmInline` on the JVM (`inline class` is the deprecated spelling without it).
    let jvm = cx.module.is_some_and(|m| !m.kotlin.multiplatform);
    let annotated = modifiers(c).is_some_and(|m| m.annotations().any(|a| a.type_ref().is_some_and(|t| t.syntax().text().to_string().ends_with("JvmInline"))));
    if modifier.text() == "value" && jvm && !annotated {
        let fix = Fix::replace("Add '@JvmInline' annotation", TextRange::empty(c.text_range().start()), "@JvmInline\n");
        out.report(&VALUE_CLASS_WITHOUT_JVM_INLINE_ANNOTATION, modifier.text_range(), "Value classes without '@JvmInline' annotation are not yet supported.", Some(fix));
    }
}

// ------------------------------------------------------------ sealed and enum classes

/// `sealed class S public constructor()`: constructors of sealed classes are protected or
/// private.
fn sealed_constructor(c: &SyntaxNode, out: &mut Sink) {
    if !has_modifier(c, "sealed") || is_interface(c) {
        return;
    }
    let primary = c.children().filter(|x| x.kind() == SyntaxKind::PRIMARY_CONSTRUCTOR);
    let secondary = c.children().filter(|x| x.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children().filter(|m| m.kind() == SyntaxKind::SECONDARY_CONSTRUCTOR).collect::<Vec<_>>());
    for ctor in primary.chain(secondary) {
        if let Some(v) = modifiers(&ctor).and_then(|m| m.modifiers().find(|t| t.text() == "public" || t.text() == "internal")) {
            out.report(&NON_PRIVATE_OR_PROTECTED_CONSTRUCTOR_IN_SEALED, v.text_range(), "Constructor must be private or protected in sealed class.", Some(Fix::replace("Make constructor protected", v.text_range(), "protected")));
        }
    }
}

/// Entries of an enum class that declares abstract members must implement them all.
fn enum_entries(c: &SyntaxNode, out: &mut Sink) {
    if !has_modifier(c, "enum") {
        return;
    }
    let Some(body) = c.children().find(|x| x.kind() == SyntaxKind::CLASS_BODY) else { return };
    let abstract_members: Vec<String> = body.children().filter(|m| matches!(m.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY) && has_modifier(m, "abstract")).filter_map(|m| name_token(&m).map(|t| t.text().to_string())).collect();
    if abstract_members.is_empty() {
        return;
    }
    for entry in body.children().filter(|e| e.kind() == SyntaxKind::ENUM_ENTRY) {
        let Some(name) = name_token(&entry) else { continue };
        let implemented: Vec<String> = entry.children().filter(|x| x.kind() == SyntaxKind::CLASS_BODY).flat_map(|b| b.children().collect::<Vec<_>>()).filter_map(|m| name_token(&m).map(|t| t.text().to_string())).collect();
        if let Some(missing) = abstract_members.iter().find(|m| !implemented.contains(m)) {
            out.report(&ABSTRACT_MEMBER_NOT_IMPLEMENTED_BY_ENUM_ENTRY, name.text_range(), format!("Enum entry '{}' must implement abstract member '{missing}'.", name.text()), None);
        }
    }
}

/// Supertypes: none twice; sealed ones only from the same package and module, and not by
/// local classes or objects.
fn supertypes(cx: &Cx, c: &SyntaxNode, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    let entries: Vec<SyntaxNode> = c.children().filter(|x| x.kind() == SyntaxKind::SUPER_TYPE_LIST).flat_map(|l| l.children().collect::<Vec<_>>()).collect();
    if entries.is_empty() || has_modifier(c, "expect") || has_modifier(c, "actual") {
        return;
    }
    let mut seen: Vec<String> = Vec::new();
    let local = c.kind() == SyntaxKind::OBJECT_LITERAL || c.ancestors().skip(1).any(|a| matches!(a.kind(), SyntaxKind::BLOCK | SyntaxKind::FUN));
    let module_of = |p: &std::path::Path| cx.snap.context_of(p).map(|ctx| ctx.module);
    for entry in &entries {
        let Some(tr) = entry.descendants().find_map(ast::TypeRef::cast) else { continue };
        if let Some(q) = tr.syntax().children().find(|c| c.kind() == SyntaxKind::NULLABLE_TYPE).and_then(|n| ast::token(&n, SyntaxKind::QUESTION)) {
            out.report(&NULLABLE_SUPERTYPE, tr.syntax().text_range(), "A supertype cannot be nullable.", Some(Fix::delete("Remove '?'", q.text_range())));
        }
        let t = e.resolve_syntax_type(&tr);
        let Some(fq) = fq_of(&t).map(str::to_string) else { continue };
        if seen.contains(&fq) {
            out.report(&SUPERTYPE_APPEARS_TWICE, tr.syntax().text_range(), "A supertype appears twice.", None);
            continue;
        }
        seen.push(fq.clone());
        let Some(cls) = e.classifier(&fq) else { continue };
        let sym = cls.symbol();
        if !sym.modifiers.has(Modifiers::SEALED) || !matches!(sym.kind, SymbolKind::Class | SymbolKind::Interface) {
            continue;
        }
        let at = tr.syntax().text_range();
        if local {
            out.report(&SEALED_SUPERTYPE_IN_LOCAL_CLASS, at, format!("{} cannot extend a sealed class or interface.", if c.kind() == SyntaxKind::OBJECT_LITERAL { "Anonymous object" } else { "Local class" }), None);
            continue;
        }
        match &sym.origin {
            Origin::Source { file, .. } => {
                if module_of(file) != module_of(cx.path) {
                    out.report(&SEALED_INHERITOR_IN_DIFFERENT_MODULE, at, "Inheritor of sealed class or interface declared in a different module.", None);
                } else if let Some(other) = cx.snap.file(file)
                    && other.summary().package != e.package()
                {
                    let msg = format!("Inheritor of sealed class or interface declared in package '{}' but it must be in package '{}' where base class is declared.", e.package(), other.summary().package);
                    out.report(&SEALED_INHERITOR_IN_DIFFERENT_PACKAGE, at, msg, None);
                }
            }
            // Kotlin library classes (Java's sealed classes have their own rules).
            _ if matches!(cls, crate::analysis::resolve::Classifier::Library(ref lc) if lc.is_kotlin) => {
                out.report(&SEALED_INHERITOR_IN_DIFFERENT_MODULE, at, "Inheritor of sealed class or interface declared in a different module.", None);
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------ unused values

/// `n` is a statement whose value nobody uses: not the last statement of a block whose
/// value counts (lambdas, `if`/`when`/`try` branches).
fn unused_position(n: &SyntaxNode) -> bool {
    let Some(block) = n.parent().filter(|p| p.kind() == SyntaxKind::BLOCK) else { return false };
    if block.last_child().as_ref() != Some(n) {
        return true;
    }
    match block.parent() {
        Some(p) if matches!(p.kind(), SyntaxKind::FUN | SyntaxKind::CLASS_INITIALIZER | SyntaxKind::SECONDARY_CONSTRUCTOR) => true,
        Some(p) if p.kind() == SyntaxKind::BODY => p.parent().is_some_and(|l| matches!(l.kind(), SyntaxKind::FOR_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::DO_WHILE_EXPR)),
        _ => false,
    }
}

/// An expression without effects: literals, names of locals, `this`, checks and
/// comparisons of those.
fn pure(e: &Engine, n: &SyntaxNode) -> bool {
    match n.kind() {
        SyntaxKind::LITERAL | SyntaxKind::THIS_EXPR | SyntaxKind::CLASS_LITERAL => true,
        SyntaxKind::STRING_TEMPLATE => n.children().all(|c| matches!(c.kind(), SyntaxKind::LITERAL_STRING_ENTRY | SyntaxKind::ESCAPE_STRING_ENTRY)),
        SyntaxKind::PAREN_EXPR => n.children().last().is_some_and(|c| pure(e, &c)),
        SyntaxKind::NAME_REF => matches!(e.resolve_name_ref(n).as_slice(), [Def::Local(l)] if matches!(l.kind, LocalKind::Val | LocalKind::Var | LocalKind::Parameter | LocalKind::LambdaParameter | LocalKind::It)),
        SyntaxKind::IS_EXPR => n.first_child().is_some_and(|c| pure(e, &c)),
        SyntaxKind::BINARY_EXPR => {
            binary_op(n).is_some_and(|o| matches!(o.kind(), SyntaxKind::EQ2 | SyntaxKind::NOT_EQ | SyntaxKind::EQ3 | SyntaxKind::NOT_EQ2))
                && binary_operands(n).is_some_and(|(l, r)| pure(e, &l) && pure(e, &r))
        }
        _ => false,
    }
}

fn unused_expression(cx: &Cx, n: &SyntaxNode, out: &mut Sink) {
    if !unused_position(n) {
        return;
    }
    if n.kind() == SyntaxKind::LAMBDA_EXPR {
        let fix = Fix::replace("Wrap in 'run'", TextRange::empty(n.text_range().start()), "run ");
        out.report(&UNUSED_LAMBDA_EXPRESSION, n.first_token().map_or(n.text_range(), |t| t.text_range()), "The lambda expression is unused. If you mean a block, you can use 'run { ... }'.", Some(fix));
        return;
    }
    let Some(e) = cx.engine() else { return };
    if pure(e, n) {
        out.report(&UNUSED_EXPRESSION, n.text_range(), "The expression is unused.", None);
    }
}

// ------------------------------------------------------------ implicit invoke

/// `f()` where `f` is a local or parameter of nullable function type.
fn implicit_invoke(cx: &Cx, call: &SyntaxNode, out: &mut Sink) {
    let Some(callee) = call.first_child().filter(|c| c.kind() == SyntaxKind::NAME_REF) else { return };
    let Some(e) = cx.engine() else { return };
    let Some(shown) = super::expressions::nullable_local(e, &callee) else { return };
    let fix = Fix::replace("Replace with safe (?.invoke) call", TextRange::empty(callee.text_range().end()), "?.invoke");
    out.report(&UNSAFE_IMPLICIT_INVOKE_CALL, callee.text_range(), format!("Reference has a nullable type '{shown}', use explicit '?.invoke()' to make a function-like call instead."), Some(fix));
}
