//! Type references: the number of type arguments of a class or type alias.

use super::super::*;
use crate::analysis::ide::infer::Engine;
use crate::symbols::TypeRef;

pub(super) fn check(cx: &Cx, out: &mut Sink) {
    let Some(e) = cx.engine() else { return };
    for n in cx.root.descendants().filter(|n| n.kind() == SyntaxKind::USER_TYPE) {
        if !in_error(&n) {
            type_arguments(e, &n, out);
        }
    }
}

/// `Map<String>`, `List<Int, Int>`, or `List` without arguments where they cannot be
/// inferred.
fn type_arguments(e: &Engine, ut: &SyntaxNode, out: &mut Sink) {
    // Outermost user type only (`Map.Entry`), not a qualifier inside one.
    if ut.parent().is_some_and(|p| p.kind() == SyntaxKind::USER_TYPE) {
        return;
    }
    let segments: Vec<SyntaxNode> = ut.children().filter(|c| c.kind() == SyntaxKind::USER_TYPE_SEGMENT).collect();
    let with_args = segments.iter().filter(|s| s.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST)).count();
    let Some(last) = segments.last() else { return };
    if with_args > 1 || with_args == 1 && !last.children().any(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST) {
        return;
    }
    // Arguments, star projections included: the elements between `<`, `,` and `>`.
    let given = last
        .children()
        .filter(|c| c.kind() == SyntaxKind::TYPE_ARGUMENT_LIST)
        .flat_map(|l| l.children_with_tokens().filter(|t| !t.kind().is_trivia() && !matches!(t.kind(), SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::COMMA)).collect::<Vec<_>>())
        .count();
    // Where arguments may be left out: `is`/`as` (inferred), class literals, annotations,
    // supertype calls and constructor-like positions, imports.
    let context = ut.ancestors().skip(1).find(|a| !matches!(a.kind(), SyntaxKind::TYPE_REFERENCE | SyntaxKind::NULLABLE_TYPE));
    let omissible = context.as_ref().is_some_and(|c| {
        matches!(c.kind(), SyntaxKind::IS_EXPR | SyntaxKind::BINARY_WITH_TYPE | SyntaxKind::WHEN_CONDITION | SyntaxKind::ANNOTATION | SyntaxKind::CALLABLE_REFERENCE | SyntaxKind::SUPER_TYPE_CALL_ENTRY | SyntaxKind::SUPER_EXPR | SyntaxKind::CLASS_LITERAL | SyntaxKind::TYPE_CONSTRAINT)
    }) || ut.ancestors().any(|a| matches!(a.kind(), SyntaxKind::IMPORT_DIRECTIVE | SyntaxKind::ANNOTATION | SyntaxKind::KDOC));
    if given == 0 && omissible {
        return;
    }
    let Some(tr) = ut.ancestors().find_map(ast::TypeRef::cast) else { return };
    let TypeRef::Named { fq: Some(fq), .. } = crate::analysis::ide::infer::strip(&e.resolve_syntax_type(&tr)).clone() else { return };
    let Some(c) = e.res.find_classifier(&fq) else { return };
    let sym = c.symbol();
    // Inner classes also take their outer class's arguments.
    if sym.modifiers.has(crate::symbols::Modifiers::INNER) {
        return;
    }
    let expected = sym.type_params.len();
    if given == expected {
        return;
    }
    let shown = sym.name.clone();
    let kind = match sym.kind {
        crate::symbols::SymbolKind::Interface => "interface",
        crate::symbols::SymbolKind::TypeAlias => "type alias",
        _ => "class",
    };
    let params: Vec<String> = sym.type_params.iter().map(|t| t.name.clone()).collect();
    let msg = match expected {
        0 => format!("No type arguments expected for {kind} '{shown}'."),
        1 => format!("One type argument expected for {kind} '{shown}<{}>'.", params.join(", ")),
        n => format!("{n} type arguments expected for {kind} '{shown}<{}>'.", params.join(", ")),
    };
    out.report(&WRONG_NUMBER_OF_TYPE_ARGUMENTS, last.text_range(), msg, None);
}
