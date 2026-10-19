//! Token and node kinds of the Kotlin concrete syntax tree.

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // ---- trivia ----
    WHITESPACE = 0,
    NEWLINE,
    LINE_COMMENT,
    BLOCK_COMMENT,
    KDOC,
    SHEBANG,

    // ---- literals & names ----
    IDENT,
    INT_LITERAL,
    FLOAT_LITERAL,
    CHAR_LITERAL,

    // ---- string pieces ----
    /// `"` / `"""`, optionally prefixed with `$`s for multi-dollar interpolation.
    STRING_OPEN,
    STRING_CLOSE,
    STRING_TEXT,
    STRING_ESCAPE,
    /// `$` (or `$$…`) introducing `$name`.
    SHORT_TEMPLATE_START,
    /// `${` (or `$${…`).
    LONG_TEMPLATE_START,
    /// `}` closing a long template entry.
    LONG_TEMPLATE_END,

    // ---- hard keywords ----
    AS_KW,
    AS_SAFE_KW,
    BREAK_KW,
    CLASS_KW,
    CONTINUE_KW,
    DO_KW,
    ELSE_KW,
    FALSE_KW,
    FOR_KW,
    FUN_KW,
    IF_KW,
    IN_KW,
    NOT_IN_KW,
    INTERFACE_KW,
    IS_KW,
    NOT_IS_KW,
    NULL_KW,
    OBJECT_KW,
    PACKAGE_KW,
    RETURN_KW,
    SUPER_KW,
    THIS_KW,
    THROW_KW,
    TRUE_KW,
    TRY_KW,
    TYPEALIAS_KW,
    TYPEOF_KW,
    VAL_KW,
    VAR_KW,
    WHEN_KW,
    WHILE_KW,

    // ---- punctuation & operators ----
    L_PAREN,
    R_PAREN,
    L_BRACK,
    R_BRACK,
    L_CURLY,
    R_CURLY,
    COMMA,
    DOT,
    SEMI,
    COLON,
    COLON2,
    QUESTION,
    SAFE_DOT,
    ELVIS,
    ARROW,
    DOUBLE_ARROW,
    RANGE,
    RANGE_UNTIL,
    AT,
    HASH,
    DOLLAR,
    EQ,
    PLUS_EQ,
    MINUS_EQ,
    STAR_EQ,
    SLASH_EQ,
    PERCENT_EQ,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    PERCENT,
    PLUS2,
    MINUS2,
    AMP,
    AMP2,
    PIPE2,
    EXCL,
    EXCL2,
    EQ2,
    EQ3,
    NOT_EQ,
    NOT_EQ2,
    LT,
    GT,
    LT_EQ,
    GT_EQ,
    UNDERSCORE_FALLBACK,

    /// A character the lexer does not understand.
    ERROR_TOKEN,
    EOF,

    // ---- nodes ----
    KOTLIN_FILE,
    SCRIPT_BODY,
    FILE_ANNOTATION_LIST,
    PACKAGE_DIRECTIVE,
    IMPORT_LIST,
    IMPORT_DIRECTIVE,
    IMPORT_ALIAS,
    DOT_QUALIFIED_NAME,
    NAME,

    MODIFIER_LIST,
    MODIFIER,
    ANNOTATION,
    ANNOTATION_TARGET,
    ANNOTATION_GROUP,

    CLASS,
    OBJECT_DECLARATION,
    FUN,
    PROPERTY,
    TYPEALIAS,
    PRIMARY_CONSTRUCTOR,
    SECONDARY_CONSTRUCTOR,
    CONSTRUCTOR_DELEGATION_CALL,
    CLASS_INITIALIZER,
    CLASS_BODY,
    ENUM_ENTRY,
    COMPANION_OBJECT,
    SUPER_TYPE_LIST,
    SUPER_TYPE_ENTRY,
    SUPER_TYPE_CALL_ENTRY,
    DELEGATED_SUPER_TYPE_ENTRY,
    TYPE_PARAMETER_LIST,
    TYPE_PARAMETER,
    TYPE_CONSTRAINT_LIST,
    TYPE_CONSTRAINT,
    VALUE_PARAMETER_LIST,
    VALUE_PARAMETER,
    CONTEXT_RECEIVER_LIST,
    RECEIVER_TYPE,
    PROPERTY_ACCESSOR,
    PROPERTY_DELEGATE,
    BACKING_FIELD,
    DESTRUCTURING_DECLARATION,
    DESTRUCTURING_ENTRY,

    // types
    TYPE_REFERENCE,
    USER_TYPE,
    USER_TYPE_SEGMENT,
    NULLABLE_TYPE,
    FUNCTION_TYPE,
    FUNCTION_TYPE_PARAMETER_LIST,
    FUNCTION_TYPE_RECEIVER,
    PARENTHESIZED_TYPE,
    DEFINITELY_NON_NULL_TYPE,
    DYNAMIC_TYPE,
    TYPE_ARGUMENT_LIST,
    TYPE_PROJECTION,
    STAR_PROJECTION,

    // statements & blocks
    BLOCK,
    LOCAL_DECLARATION,

    // expressions
    LITERAL,
    STRING_TEMPLATE,
    LITERAL_STRING_ENTRY,
    ESCAPE_STRING_ENTRY,
    SHORT_STRING_TEMPLATE_ENTRY,
    LONG_STRING_TEMPLATE_ENTRY,
    NAME_REF,
    PAREN_EXPR,
    THIS_EXPR,
    SUPER_EXPR,
    LABEL,
    LABEL_QUALIFIER,
    LABELED_EXPR,
    ANNOTATED_EXPR,
    BINARY_EXPR,
    BINARY_WITH_TYPE,
    IS_EXPR,
    PREFIX_EXPR,
    POSTFIX_EXPR,
    CALL_EXPR,
    INDEX_EXPR,
    DOT_QUALIFIED_EXPR,
    SAFE_QUALIFIED_EXPR,
    CALLABLE_REFERENCE,
    CLASS_LITERAL,
    VALUE_ARGUMENT_LIST,
    VALUE_ARGUMENT,
    VALUE_ARGUMENT_NAME,
    LAMBDA_ARGUMENT,
    LAMBDA_EXPR,
    LAMBDA_PARAMETER_LIST,
    ANONYMOUS_FUNCTION,
    OBJECT_LITERAL,
    COLLECTION_LITERAL,
    IF_EXPR,
    ELSE_BRANCH,
    CONDITION,
    THEN_BRANCH,
    WHEN_EXPR,
    WHEN_SUBJECT,
    WHEN_ENTRY,
    WHEN_CONDITION,
    WHEN_GUARD,
    TRY_EXPR,
    CATCH_CLAUSE,
    FINALLY_CLAUSE,
    FOR_EXPR,
    WHILE_EXPR,
    DO_WHILE_EXPR,
    LOOP_RANGE,
    BODY,
    RETURN_EXPR,
    THROW_EXPR,
    BREAK_EXPR,
    CONTINUE_EXPR,
    OPERATION_REFERENCE,

    ERROR,
    // Must stay last.
    __LAST,
}

use SyntaxKind::*;

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, WHITESPACE | NEWLINE | LINE_COMMENT | BLOCK_COMMENT | KDOC | SHEBANG)
    }

    pub fn is_keyword(self) -> bool {
        (AS_KW as u16..=WHILE_KW as u16).contains(&(self as u16))
    }

    pub fn from_keyword(text: &str) -> Option<SyntaxKind> {
        Some(match text {
            "as" => AS_KW,
            "break" => BREAK_KW,
            "class" => CLASS_KW,
            "continue" => CONTINUE_KW,
            "do" => DO_KW,
            "else" => ELSE_KW,
            "false" => FALSE_KW,
            "for" => FOR_KW,
            "fun" => FUN_KW,
            "if" => IF_KW,
            "in" => IN_KW,
            "interface" => INTERFACE_KW,
            "is" => IS_KW,
            "null" => NULL_KW,
            "object" => OBJECT_KW,
            "package" => PACKAGE_KW,
            "return" => RETURN_KW,
            "super" => SUPER_KW,
            "this" => THIS_KW,
            "throw" => THROW_KW,
            "true" => TRUE_KW,
            "try" => TRY_KW,
            "typealias" => TYPEALIAS_KW,
            "typeof" => TYPEOF_KW,
            "val" => VAL_KW,
            "var" => VAR_KW,
            "when" => WHEN_KW,
            "while" => WHILE_KW,
            _ => return None,
        })
    }

    /// Human readable description used in parser error messages.
    pub fn describe(self) -> &'static str {
        match self {
            IDENT => "identifier",
            L_PAREN => "'('",
            R_PAREN => "')'",
            L_BRACK => "'['",
            R_BRACK => "']'",
            L_CURLY => "'{'",
            R_CURLY => "'}'",
            COMMA => "','",
            DOT => "'.'",
            SEMI => "';'",
            COLON => "':'",
            ARROW => "'->'",
            EQ => "'='",
            GT => "'>'",
            LT => "'<'",
            STRING_CLOSE => "closing quote",
            LONG_TEMPLATE_END => "'}'",
            IN_KW => "'in'",
            EOF => "end of file",
            _ => "token",
        }
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        rowan::SyntaxKind(kind as u16)
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum KotlinLanguage {}

impl rowan::Language for KotlinLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        assert!(raw.0 < __LAST as u16);
        // SAFETY: SyntaxKind is repr(u16) and every value below __LAST is a variant.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        kind.into()
    }
}

pub type SyntaxNode = rowan::SyntaxNode<KotlinLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<KotlinLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<KotlinLanguage>;

/// Soft keywords that act as modifiers in declaration position.
pub const MODIFIER_KEYWORDS: &[&str] = &[
    "abstract",
    "actual",
    "annotation",
    "companion",
    "const",
    "crossinline",
    "data",
    "enum",
    "expect",
    "external",
    "final",
    "infix",
    "inline",
    "inner",
    "internal",
    "lateinit",
    "noinline",
    "open",
    "operator",
    "out",
    "override",
    "private",
    "protected",
    "public",
    "reified",
    "sealed",
    "suspend",
    "tailrec",
    "vararg",
    "value",
    "fun", // only for `fun interface`, handled specially
];

pub fn is_modifier_keyword(text: &str) -> bool {
    text != "fun" && MODIFIER_KEYWORDS.contains(&text)
}
