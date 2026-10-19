//! Typed views over the untyped CST.

use super::kind::SyntaxKind::{self, *};
use super::kind::{SyntaxNode, SyntaxToken};
use rowan::TextRange;

pub trait AstNode: Sized {
    fn can_cast(kind: SyntaxKind) -> bool;
    fn cast(syntax: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;

    fn range(&self) -> TextRange {
        self.syntax().text_range()
    }
}

macro_rules! ast_node {
    ($name:ident, $($kind:ident)|+) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name {
            pub(crate) syntax: SyntaxNode,
        }
        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                matches!(kind, $($kind)|+)
            }
            fn cast(syntax: SyntaxNode) -> Option<Self> {
                Self::can_cast(syntax.kind()).then(|| Self { syntax })
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.syntax
            }
        }
    };
}

pub fn child<N: AstNode>(node: &SyntaxNode) -> Option<N> {
    node.children().find_map(N::cast)
}

pub fn children<N: AstNode + 'static>(node: &SyntaxNode) -> impl Iterator<Item = N> + '_ {
    node.children().filter_map(N::cast)
}

pub fn token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens().filter_map(|e| e.into_token()).find(|t| t.kind() == kind)
}

/// Strip backticks from an identifier.
pub fn ident_text(token: &SyntaxToken) -> String {
    let t = token.text();
    t.strip_prefix('`').and_then(|t| t.strip_suffix('`')).unwrap_or(t).to_string()
}

ast_node!(KtFile, KOTLIN_FILE);
ast_node!(PackageDirective, PACKAGE_DIRECTIVE);
ast_node!(ImportDirective, IMPORT_DIRECTIVE);
ast_node!(QualifiedName, DOT_QUALIFIED_NAME);
ast_node!(ModifierList, MODIFIER_LIST);
ast_node!(Annotation, ANNOTATION);
ast_node!(ClassDecl, CLASS);
ast_node!(ObjectDecl, OBJECT_DECLARATION);
ast_node!(FunDecl, FUN);
ast_node!(PropertyDecl, PROPERTY);
ast_node!(TypeAliasDecl, TYPEALIAS);
ast_node!(SecondaryConstructor, SECONDARY_CONSTRUCTOR);
ast_node!(EnumEntry, ENUM_ENTRY);
ast_node!(ClassBody, CLASS_BODY);
ast_node!(PrimaryConstructor, PRIMARY_CONSTRUCTOR);
ast_node!(ValueParameterList, VALUE_PARAMETER_LIST);
ast_node!(ValueParameter, VALUE_PARAMETER);
ast_node!(TypeParameterList, TYPE_PARAMETER_LIST);
ast_node!(TypeParameter, TYPE_PARAMETER);
ast_node!(TypeConstraint, TYPE_CONSTRAINT);
ast_node!(SuperTypeList, SUPER_TYPE_LIST);
ast_node!(SuperTypeEntry, SUPER_TYPE_ENTRY | SUPER_TYPE_CALL_ENTRY | DELEGATED_SUPER_TYPE_ENTRY);
ast_node!(TypeRef, TYPE_REFERENCE);
ast_node!(UserType, USER_TYPE);
ast_node!(UserTypeSegment, USER_TYPE_SEGMENT);
ast_node!(TypeArgumentList, TYPE_ARGUMENT_LIST);
ast_node!(FunctionType, FUNCTION_TYPE);
ast_node!(ReceiverType, RECEIVER_TYPE);
ast_node!(Block, BLOCK);
ast_node!(PropertyAccessor, PROPERTY_ACCESSOR);

/// Any declaration that can appear at top level or in a class body.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Decl {
    Class(ClassDecl),
    Object(ObjectDecl),
    Fun(FunDecl),
    Property(PropertyDecl),
    TypeAlias(TypeAliasDecl),
    Constructor(SecondaryConstructor),
    EnumEntry(EnumEntry),
}

impl Decl {
    pub fn cast(node: SyntaxNode) -> Option<Decl> {
        Some(match node.kind() {
            CLASS => Decl::Class(ClassDecl { syntax: node }),
            OBJECT_DECLARATION => Decl::Object(ObjectDecl { syntax: node }),
            FUN => Decl::Fun(FunDecl { syntax: node }),
            PROPERTY => Decl::Property(PropertyDecl { syntax: node }),
            TYPEALIAS => Decl::TypeAlias(TypeAliasDecl { syntax: node }),
            SECONDARY_CONSTRUCTOR => Decl::Constructor(SecondaryConstructor { syntax: node }),
            ENUM_ENTRY => Decl::EnumEntry(EnumEntry { syntax: node }),
            _ => return None,
        })
    }

    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Decl::Class(d) => &d.syntax,
            Decl::Object(d) => &d.syntax,
            Decl::Fun(d) => &d.syntax,
            Decl::Property(d) => &d.syntax,
            Decl::TypeAlias(d) => &d.syntax,
            Decl::Constructor(d) => &d.syntax,
            Decl::EnumEntry(d) => &d.syntax,
        }
    }

    pub fn name_token(&self) -> Option<SyntaxToken> {
        token(self.syntax(), IDENT)
    }

    pub fn name(&self) -> Option<String> {
        self.name_token().map(|t| ident_text(&t))
    }

    pub fn modifiers(&self) -> Option<ModifierList> {
        child(self.syntax())
    }

    pub fn has_modifier(&self, m: &str) -> bool {
        self.modifiers().is_some_and(|l| l.has(m))
    }

    pub fn kdoc(&self) -> Option<String> {
        self.syntax()
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == KDOC)
            .map(|t| t.text().to_string())
    }
}

impl KtFile {
    pub fn package(&self) -> Option<PackageDirective> {
        child(&self.syntax)
    }

    pub fn imports(&self) -> impl Iterator<Item = ImportDirective> + '_ {
        self.syntax
            .children()
            .filter(|n| n.kind() == IMPORT_LIST)
            .flat_map(|l| l.children().filter_map(ImportDirective::cast).collect::<Vec<_>>())
    }

    /// Top level declarations (for scripts this includes declarations among statements).
    pub fn declarations(&self) -> impl Iterator<Item = Decl> + '_ {
        self.syntax.children().filter_map(Decl::cast)
    }

    pub fn file_annotations(&self) -> Vec<Annotation> {
        self.syntax
            .children()
            .filter(|n| n.kind() == FILE_ANNOTATION_LIST)
            .flat_map(|n| n.children().filter_map(Annotation::cast).collect::<Vec<_>>())
            .collect()
    }
}

impl PackageDirective {
    pub fn name(&self) -> Option<QualifiedName> {
        child(&self.syntax)
    }
}

impl QualifiedName {
    pub fn segments(&self) -> Vec<SyntaxToken> {
        self.syntax.children_with_tokens().filter_map(|e| e.into_token()).filter(|t| t.kind() == IDENT).collect()
    }

    pub fn text(&self) -> String {
        self.segments().iter().map(ident_text).collect::<Vec<_>>().join(".")
    }

    pub fn has_star(&self) -> bool {
        token(&self.syntax, STAR).is_some()
    }
}

impl ImportDirective {
    pub fn path(&self) -> Option<QualifiedName> {
        child(&self.syntax)
    }

    pub fn is_star(&self) -> bool {
        self.path().is_some_and(|p| p.has_star())
    }

    pub fn alias(&self) -> Option<SyntaxToken> {
        self.syntax.children().find(|n| n.kind() == IMPORT_ALIAS).and_then(|n| token(&n, IDENT))
    }
}

impl ModifierList {
    pub fn modifiers(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        self.syntax.children().filter(|n| n.kind() == MODIFIER).filter_map(|n| n.first_token())
    }

    pub fn has(&self, m: &str) -> bool {
        self.modifiers().any(|t| t.text() == m)
    }

    pub fn annotations(&self) -> impl Iterator<Item = Annotation> + '_ {
        self.syntax.descendants().filter_map(Annotation::cast)
    }
}

impl Annotation {
    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    pub fn use_site_target(&self) -> Option<String> {
        self.syntax
            .children()
            .find(|n| n.kind() == ANNOTATION_TARGET)
            .and_then(|n| token(&n, IDENT))
            .map(|t| t.text().to_string())
    }
}

impl ClassDecl {
    pub fn is_interface(&self) -> bool {
        token(&self.syntax, INTERFACE_KW).is_some()
    }

    pub fn is_fun_interface(&self) -> bool {
        token(&self.syntax, FUN_KW).is_some()
    }

    pub fn type_parameters(&self) -> Vec<TypeParameter> {
        type_params(&self.syntax)
    }

    pub fn primary_constructor(&self) -> Option<PrimaryConstructor> {
        child(&self.syntax)
    }

    pub fn super_types(&self) -> Vec<SuperTypeEntry> {
        super_types(&self.syntax)
    }

    pub fn body(&self) -> Option<ClassBody> {
        child(&self.syntax)
    }

    pub fn type_constraints(&self) -> Vec<TypeConstraint> {
        type_constraints(&self.syntax)
    }
}

impl ObjectDecl {
    pub fn super_types(&self) -> Vec<SuperTypeEntry> {
        super_types(&self.syntax)
    }

    pub fn body(&self) -> Option<ClassBody> {
        child(&self.syntax)
    }
}

impl PrimaryConstructor {
    pub fn parameters(&self) -> Vec<ValueParameter> {
        child::<ValueParameterList>(&self.syntax).map(|l| l.parameters()).unwrap_or_default()
    }
}

impl SecondaryConstructor {
    pub fn parameters(&self) -> Vec<ValueParameter> {
        child::<ValueParameterList>(&self.syntax).map(|l| l.parameters()).unwrap_or_default()
    }
}

impl ClassBody {
    pub fn members(&self) -> impl Iterator<Item = Decl> + '_ {
        self.syntax.children().filter_map(Decl::cast)
    }
}

impl ValueParameterList {
    pub fn parameters(&self) -> Vec<ValueParameter> {
        children(&self.syntax).collect()
    }
}

impl ValueParameter {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        token(&self.syntax, IDENT)
    }

    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    pub fn has_default(&self) -> bool {
        token(&self.syntax, EQ).is_some()
    }

    /// `val`/`var` constructor property.
    pub fn property_keyword(&self) -> Option<SyntaxToken> {
        token(&self.syntax, VAL_KW).or_else(|| token(&self.syntax, VAR_KW))
    }

    pub fn modifiers(&self) -> Option<ModifierList> {
        child(&self.syntax)
    }

    pub fn is_vararg(&self) -> bool {
        self.modifiers().is_some_and(|m| m.has("vararg"))
    }
}

impl TypeParameter {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        token(&self.syntax, IDENT)
    }

    pub fn bound(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

impl TypeConstraint {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        token(&self.syntax, IDENT)
    }

    pub fn bound(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

impl SuperTypeEntry {
    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    pub fn is_constructor_call(&self) -> bool {
        self.syntax.kind() == SUPER_TYPE_CALL_ENTRY
    }
}

impl FunDecl {
    pub fn type_parameters(&self) -> Vec<TypeParameter> {
        type_params(&self.syntax)
    }

    pub fn receiver(&self) -> Option<TypeRef> {
        child::<ReceiverType>(&self.syntax).and_then(|r| child(&r.syntax))
    }

    pub fn parameters(&self) -> Vec<ValueParameter> {
        child::<ValueParameterList>(&self.syntax).map(|l| l.parameters()).unwrap_or_default()
    }

    pub fn return_type(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    pub fn body_block(&self) -> Option<Block> {
        child(&self.syntax)
    }

    pub fn has_expression_body(&self) -> bool {
        token(&self.syntax, EQ).is_some()
    }

    pub fn type_constraints(&self) -> Vec<TypeConstraint> {
        type_constraints(&self.syntax)
    }
}

impl PropertyDecl {
    pub fn is_var(&self) -> bool {
        token(&self.syntax, VAR_KW).is_some()
    }

    pub fn type_parameters(&self) -> Vec<TypeParameter> {
        type_params(&self.syntax)
    }

    pub fn receiver(&self) -> Option<TypeRef> {
        child::<ReceiverType>(&self.syntax).and_then(|r| child(&r.syntax))
    }

    pub fn type_ref(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }

    pub fn has_initializer(&self) -> bool {
        token(&self.syntax, EQ).is_some()
    }

    pub fn has_delegate(&self) -> bool {
        self.syntax.children().any(|n| n.kind() == PROPERTY_DELEGATE)
    }

    pub fn accessors(&self) -> impl Iterator<Item = PropertyAccessor> + '_ {
        children(&self.syntax)
    }

    pub fn is_destructuring(&self) -> bool {
        self.syntax.children().any(|n| n.kind() == DESTRUCTURING_DECLARATION)
    }
}

impl TypeAliasDecl {
    pub fn type_parameters(&self) -> Vec<TypeParameter> {
        type_params(&self.syntax)
    }

    pub fn aliased(&self) -> Option<TypeRef> {
        child(&self.syntax)
    }
}

impl TypeRef {
    /// The actual type node inside the reference (USER_TYPE, NULLABLE_TYPE, FUNCTION_TYPE...).
    pub fn inner(&self) -> Option<SyntaxNode> {
        self.syntax.children().find(|n| n.kind() != MODIFIER_LIST)
    }

    pub fn modifiers(&self) -> Option<ModifierList> {
        child(&self.syntax)
    }
}

impl UserType {
    pub fn segments(&self) -> Vec<UserTypeSegment> {
        children(&self.syntax).collect()
    }
}

impl UserTypeSegment {
    pub fn name_token(&self) -> Option<SyntaxToken> {
        token(&self.syntax, IDENT)
    }

    pub fn type_arguments(&self) -> Option<TypeArgumentList> {
        child(&self.syntax)
    }
}

impl TypeArgumentList {
    /// Each argument is either a TYPE_PROJECTION or a STAR_PROJECTION node.
    pub fn arguments(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax.children().filter(|n| matches!(n.kind(), TYPE_PROJECTION | STAR_PROJECTION))
    }
}

fn type_params(node: &SyntaxNode) -> Vec<TypeParameter> {
    child::<TypeParameterList>(node).map(|l| children(&l.syntax).collect()).unwrap_or_default()
}

fn type_constraints(node: &SyntaxNode) -> Vec<TypeConstraint> {
    node.children()
        .filter(|n| n.kind() == TYPE_CONSTRAINT_LIST)
        .flat_map(|l| l.children().filter_map(TypeConstraint::cast).collect::<Vec<_>>())
        .collect()
}

fn super_types(node: &SyntaxNode) -> Vec<SuperTypeEntry> {
    child::<SuperTypeList>(node).map(|l| children(&l.syntax).collect()).unwrap_or_default()
}
