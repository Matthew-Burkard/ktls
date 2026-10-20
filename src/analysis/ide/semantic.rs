//! Semantic tokens: identifiers classified by what they resolve to (parameter, local,
//! property, method, type...), soft keywords, and the lexical tokens (keywords, strings,
//! numbers, comments) so that clients without another highlighter get full coloring.

use std::path::Path;

use super::infer::{Def, Engine, LocalKind};
use super::{DECLARATIONS, declaration_def, qualified_name_segment, type_segment};
use crate::analysis::Snapshot;
use crate::symbols::{Modifiers, Origin, Symbol, SymbolKind};
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxToken, TextRange};

/// Token types, in legend order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenType {
    Namespace,
    Type,
    Class,
    Enum,
    Interface,
    TypeParameter,
    Parameter,
    Variable,
    Property,
    EnumMember,
    Function,
    Method,
    Keyword,
    Modifier,
    Comment,
    String,
    Number,
    Operator,
    Decorator,
    Label,
}

pub const TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "function",
    "method",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "operator",
    "decorator",
    "label",
];

/// Token modifiers, as bits in legend order.
pub mod modifier {
    pub const DECLARATION: u32 = 1 << 0;
    pub const READONLY: u32 = 1 << 1;
    pub const STATIC: u32 = 1 << 2;
    pub const DEPRECATED: u32 = 1 << 3;
    pub const ABSTRACT: u32 = 1 << 4;
    /// `suspend` functions.
    pub const ASYNC: u32 = 1 << 5;
    /// From a library or the Kotlin built-ins.
    pub const DEFAULT_LIBRARY: u32 = 1 << 6;
    pub const DOCUMENTATION: u32 = 1 << 7;
    /// Extension functions and properties.
    pub const EXTENSION: u32 = 1 << 8;
    /// Mutable locals and `var` properties.
    pub const MUTABLE: u32 = 1 << 9;
}

pub const TOKEN_MODIFIERS: &[&str] = &["declaration", "readonly", "static", "deprecated", "abstract", "async", "defaultLibrary", "documentation", "extension", "mutable"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub range: TextRange,
    pub ty: TokenType,
    pub modifiers: u32,
}

/// Identifiers that are keywords only in some places.
const SOFT_KEYWORDS: &[&str] = &[
    "by", "catch", "constructor", "delegate", "dynamic", "field", "file", "finally", "get", "import", "init", "param", "property", "receiver", "set", "setparam", "value", "where",
    "actual", "abstract", "annotation", "companion", "const", "crossinline", "data", "enum", "expect", "external", "final", "infix", "inline", "inner", "internal", "lateinit",
    "noinline", "open", "operator", "out", "override", "private", "protected", "public", "reified", "sealed", "suspend", "tailrec", "vararg", "context",
];

pub fn semantic_tokens(snap: &Snapshot, path: &Path, range: Option<TextRange>) -> Vec<Token> {
    let Some(e) = Engine::new(snap, path) else { return Vec::new() };
    let mut out = Vec::new();
    for el in e.root.descendants_with_tokens() {
        let Some(tok) = el.into_token() else { continue };
        if range.is_some_and(|r| tok.text_range().intersect(r).is_none_or(|i| i.is_empty())) {
            continue;
        }
        let lexical = match tok.kind() {
            SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT | SyntaxKind::SHEBANG => Some((TokenType::Comment, 0)),
            SyntaxKind::KDOC => Some((TokenType::Comment, modifier::DOCUMENTATION)),
            SyntaxKind::INT_LITERAL | SyntaxKind::FLOAT_LITERAL => Some((TokenType::Number, 0)),
            SyntaxKind::STRING_OPEN | SyntaxKind::STRING_CLOSE | SyntaxKind::STRING_TEXT | SyntaxKind::CHAR_LITERAL => Some((TokenType::String, 0)),
            k if k.is_keyword() => Some((TokenType::Keyword, 0)),
            _ => None,
        };
        if let Some((ty, modifiers)) = lexical {
            out.push(Token { range: tok.text_range(), ty, modifiers });
            continue;
        }
        if tok.kind() == SyntaxKind::IDENT
            && let Some((ty, modifiers)) = identifier(&e, &tok)
        {
            out.push(Token { range: tok.text_range(), ty, modifiers });
        }
    }
    out
}

fn identifier(e: &Engine, tok: &SyntaxToken) -> Option<(TokenType, u32)> {
    let parent = tok.parent()?;
    match parent.kind() {
        SyntaxKind::NAME_REF => {
            let defs = e.resolve_name_ref(&parent);
            match defs.first() {
                Some(d) => Some(def_token(d, false)),
                None => soft_keyword(tok),
            }
        }
        SyntaxKind::USER_TYPE_SEGMENT => {
            let defs = type_segment(e, &parent);
            let annotation = parent.ancestors().take_while(|a| a.kind() != SyntaxKind::VALUE_ARGUMENT_LIST).any(|a| a.kind() == SyntaxKind::ANNOTATION);
            let (ty, m) = defs.first().map(|d| def_token(d, false))?;
            Some(if annotation && ty != TokenType::Namespace { (TokenType::Decorator, m) } else { (ty, m) })
        }
        SyntaxKind::DOT_QUALIFIED_NAME => {
            let defs = qualified_name_segment(e, &parent, tok);
            match defs.first() {
                Some(d) => Some(def_token(d, false)),
                None => Some((TokenType::Namespace, 0)),
            }
        }
        SyntaxKind::LABEL => Some((TokenType::Label, 0)),
        SyntaxKind::MODIFIER_LIST => Some((TokenType::Modifier, 0)),
        k if DECLARATIONS.contains(&k) && ast::token(&parent, SyntaxKind::IDENT).as_ref() == Some(tok) => {
            let d = declaration_def(e, &parent, tok)?;
            let (ty, m) = def_token(&d, true);
            Some((ty, m | modifier::DECLARATION))
        }
        _ => soft_keyword(tok).or_else(|| label_reference(tok)),
    }
}

fn soft_keyword(tok: &SyntaxToken) -> Option<(TokenType, u32)> {
    SOFT_KEYWORDS.contains(&tok.text()).then_some((TokenType::Keyword, 0))
}

/// `return@label`, `this@Outer`.
fn label_reference(tok: &SyntaxToken) -> Option<(TokenType, u32)> {
    let prev = tok.prev_token()?;
    (prev.kind() == SyntaxKind::AT && prev.text_range().end() == tok.text_range().start()).then_some((TokenType::Label, 0))
}

fn def_token(d: &Def, declaration: bool) -> (TokenType, u32) {
    match d {
        Def::Package(_) => (TokenType::Namespace, 0),
        Def::Local(l) => match l.kind {
            LocalKind::Val => (TokenType::Variable, modifier::READONLY),
            LocalKind::Var => (TokenType::Variable, modifier::MUTABLE),
            LocalKind::Parameter | LocalKind::LambdaParameter | LocalKind::It => (TokenType::Parameter, modifier::READONLY),
            LocalKind::Function => (TokenType::Function, 0),
            LocalKind::Class => (TokenType::Class, 0),
        },
        Def::Symbol { sym, .. } => symbol_token(sym, declaration),
    }
}

fn symbol_token(sym: &Symbol, declaration: bool) -> (TokenType, u32) {
    let m = sym.modifiers;
    let mut mods = 0;
    if m.has(Modifiers::DEPRECATED) {
        mods |= modifier::DEPRECATED;
    }
    if m.has(Modifiers::ABSTRACT) {
        mods |= modifier::ABSTRACT;
    }
    if m.has(Modifiers::SUSPEND) {
        mods |= modifier::ASYNC;
    }
    if m.has(Modifiers::STATIC) {
        mods |= modifier::STATIC;
    }
    if sym.receiver.is_some() {
        mods |= modifier::EXTENSION;
    }
    if !matches!(sym.origin, Origin::Source { .. }) && !declaration {
        mods |= modifier::DEFAULT_LIBRARY;
    }
    // Members of a class (not top-level or local declarations).
    let member = sym.fq_name.rsplit_once('.').is_some_and(|(c, _)| c.rsplit('.').next().is_some_and(|s| s.starts_with(char::is_uppercase)));
    let ty = match sym.kind {
        SymbolKind::Class | SymbolKind::Object => TokenType::Class,
        SymbolKind::Interface => TokenType::Interface,
        SymbolKind::Annotation => TokenType::Decorator,
        SymbolKind::Enum => TokenType::Enum,
        SymbolKind::EnumEntry => {
            mods |= modifier::STATIC | modifier::READONLY;
            TokenType::EnumMember
        }
        SymbolKind::TypeAlias => TokenType::Type,
        SymbolKind::TypeParameter => TokenType::TypeParameter,
        SymbolKind::Constructor => TokenType::Class,
        SymbolKind::Function if member || sym.receiver.is_some() => TokenType::Method,
        SymbolKind::Function => TokenType::Function,
        SymbolKind::Property | SymbolKind::Field => {
            mods |= if m.has(Modifiers::VAR) || sym.kind == SymbolKind::Field && !m.has(Modifiers::FINAL) { modifier::MUTABLE } else { modifier::READONLY };
            TokenType::Property
        }
        SymbolKind::Package => TokenType::Namespace,
    };
    (ty, mods)
}
