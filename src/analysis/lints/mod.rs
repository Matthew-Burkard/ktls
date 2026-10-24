//! Lint rules (IntelliJ-style inspections beyond the compiler's checks).
//!
//! Every rule has a default level that settings can override per rule id or per category,
//! honours `@Suppress("Id")`, `@file:Suppress("Id")` and `//noinspection Id`, and most
//! offer a quick fix. Rules that need types only run when the file is fully analysable;
//! all of them stay silent rather than guess.

mod bugs;
mod compiler;

pub(crate) use compiler::{certain_type, missing_branches};
mod naming;
mod redundant;
mod style;
mod unused;

use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::Snapshot;
use super::diagnostics::{Diagnostic, Severity};
use super::ide::infer::Engine;
use super::source::Language;
use crate::project::Module;
use crate::syntax::ast::{self, AstNode};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
    #[serde(alias = "information")]
    Info,
    Hint,
    /// No diagnostic; the fix is offered as a refactoring when the cursor is on it.
    Action,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// Compiler errors and warnings (always run; `lint.enabled` does not apply).
    Compiler,
    Unused,
    Redundancy,
    Style,
    Bugs,
    Naming,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Compiler => "compiler",
            Category::Unused => "unused",
            Category::Redundancy => "redundancy",
            Category::Style => "style",
            Category::Bugs => "bugs",
            Category::Naming => "naming",
        }
    }
}

#[derive(Debug)]
pub struct Rule {
    /// IntelliJ inspection id (or compiler diagnostic id), also accepted by `@Suppress`.
    pub id: &'static str,
    pub category: Category,
    pub level: Level,
    /// The flagged code is unused or redundant (rendered faded).
    pub unnecessary: bool,
    /// Other names accepted by `@Suppress` (e.g. `unused`).
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

macro_rules! rules {
    ($($name:ident = $id:literal, $cat:ident, $level:ident, $unnecessary:literal, [$($alias:literal),*], $desc:literal;)*) => {
        $(pub static $name: Rule = Rule {
            id: $id,
            category: Category::$cat,
            level: Level::$level,
            unnecessary: $unnecessary,
            aliases: &[$($alias),*],
            description: $desc,
        };)*
        /// Every lint rule.
        pub static RULES: &[&Rule] = &[$(&$name),*];
    };
}

rules! {
    // Compiler errors.
    UNRESOLVED_REFERENCE_EXPR = "UNRESOLVED_REFERENCE", Compiler, Error, false, [], "Name that does not resolve to any declaration";
    VAL_REASSIGNMENT = "VAL_REASSIGNMENT", Compiler, Error, false, [], "Assignment to a 'val' or parameter";
    UNSAFE_CALL = "UNSAFE_CALL", Compiler, Error, false, [], "Member access with '.' on a nullable local or parameter that is never null-checked";
    NO_RETURN = "NO_RETURN_IN_FUNCTION_WITH_BLOCK_BODY", Compiler, Error, false, [], "Function with a return type whose block body has no 'return'";
    ABSTRACT_MEMBER_NOT_IMPLEMENTED = "ABSTRACT_MEMBER_NOT_IMPLEMENTED", Compiler, Error, false, [], "Concrete class missing an implementation of an interface member";
    ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED = "ABSTRACT_CLASS_MEMBER_NOT_IMPLEMENTED", Compiler, Error, false, [], "Concrete class missing an implementation of an abstract class member";
    NOTHING_TO_OVERRIDE = "NOTHING_TO_OVERRIDE", Compiler, Error, false, [], "'override' of a member no supertype declares";
    VIRTUAL_MEMBER_HIDDEN = "VIRTUAL_MEMBER_HIDDEN", Compiler, Error, false, [], "Member with the signature of a supertype member but no 'override'";
    OVERRIDING_FINAL_MEMBER = "OVERRIDING_FINAL_MEMBER", Compiler, Error, false, [], "'override' of a final member";
    NO_ACTUAL_FOR_EXPECT = "NO_ACTUAL_FOR_EXPECT", Compiler, Error, false, [], "Top-level 'expect' declaration with no 'actual' anywhere in the project";
    ACTUAL_WITHOUT_EXPECT = "ACTUAL_WITHOUT_EXPECT", Compiler, Error, false, [], "'actual' declaration with no matching 'expect'";
    NO_ELSE_IN_WHEN = "NO_ELSE_IN_WHEN", Compiler, Error, false, [], "'when' that is not exhaustive";
    REPEATED_MODIFIER = "REPEATED_MODIFIER", Compiler, Error, false, [], "The same modifier twice";
    INCOMPATIBLE_MODIFIERS = "INCOMPATIBLE_MODIFIERS", Compiler, Error, false, [], "Modifiers that cannot be combined";
    INAPPLICABLE_LATEINIT_MODIFIER = "INAPPLICABLE_LATEINIT_MODIFIER", Compiler, Error, false, [], "'lateinit' on a val, nullable or primitive type, or with an initializer";
    WRONG_MODIFIER_TARGET = "WRONG_MODIFIER_TARGET", Compiler, Error, false, [], "Modifier not applicable to this declaration";
    CONST_VAL_NOT_TOP_LEVEL_OR_OBJECT = "CONST_VAL_NOT_TOP_LEVEL_OR_OBJECT", Compiler, Error, false, [], "'const val' inside a class";
    CONST_VAL_WITHOUT_INITIALIZER = "CONST_VAL_WITHOUT_INITIALIZER", Compiler, Error, false, [], "'const val' without a value";
    CONST_VAL_WITH_GETTER = "CONST_VAL_WITH_GETTER", Compiler, Error, false, [], "'const val' with a getter";
    ABSTRACT_FUNCTION_WITH_BODY = "ABSTRACT_FUNCTION_WITH_BODY", Compiler, Error, false, [], "Abstract function with a body";
    NON_ABSTRACT_FUNCTION_WITH_NO_BODY = "NON_ABSTRACT_FUNCTION_WITH_NO_BODY", Compiler, Error, false, [], "Member function without a body that is not abstract";
    NON_MEMBER_FUNCTION_NO_BODY = "NON_MEMBER_FUNCTION_NO_BODY", Compiler, Error, false, [], "Top-level or local function without a body";
    ABSTRACT_PROPERTY_WITH_INITIALIZER = "ABSTRACT_PROPERTY_WITH_INITIALIZER", Compiler, Error, false, [], "Abstract property with an initializer";
    ABSTRACT_PROPERTY_WITH_GETTER = "ABSTRACT_PROPERTY_WITH_GETTER", Compiler, Error, false, [], "Abstract property with a getter body";
    ABSTRACT_FUNCTION_IN_NON_ABSTRACT_CLASS = "ABSTRACT_FUNCTION_IN_NON_ABSTRACT_CLASS", Compiler, Error, false, [], "Abstract function in a concrete class or object";
    ABSTRACT_PROPERTY_IN_NON_ABSTRACT_CLASS = "ABSTRACT_PROPERTY_IN_NON_ABSTRACT_CLASS", Compiler, Error, false, [], "Abstract property in a concrete class or object";
    MUST_BE_INITIALIZED = "MUST_BE_INITIALIZED", Compiler, Error, false, [], "Top-level property without an initializer";
    MUST_BE_INITIALIZED_OR_BE_ABSTRACT = "MUST_BE_INITIALIZED_OR_BE_ABSTRACT", Compiler, Error, false, [], "Member property that is never initialized";
    DATA_CLASS_WITHOUT_PARAMETERS = "DATA_CLASS_WITHOUT_PARAMETERS", Compiler, Error, false, [], "Data class with no constructor parameters";
    DATA_CLASS_NOT_PROPERTY_PARAMETER = "DATA_CLASS_NOT_PROPERTY_PARAMETER", Compiler, Error, false, [], "Data class constructor parameter without 'val'/'var'";
    CONSTRUCTOR_IN_INTERFACE = "CONSTRUCTOR_IN_INTERFACE", Compiler, Error, false, [], "Interface with a constructor";
    CONSTRUCTOR_IN_OBJECT = "CONSTRUCTOR_IN_OBJECT", Compiler, Error, false, [], "Object with a constructor";
    ANONYMOUS_INITIALIZER_IN_INTERFACE = "ANONYMOUS_INITIALIZER_IN_INTERFACE", Compiler, Error, false, [], "Interface with an 'init' block";
    PROPERTY_INITIALIZER_IN_INTERFACE = "PROPERTY_INITIALIZER_IN_INTERFACE", Compiler, Error, false, [], "Interface property with an initializer";
    SUPERTYPE_INITIALIZED_IN_INTERFACE = "SUPERTYPE_INITIALIZED_IN_INTERFACE", Compiler, Error, false, [], "Interface calling a supertype constructor";
    INTERFACE_WITH_SUPERCLASS = "INTERFACE_WITH_SUPERCLASS", Compiler, Error, false, [], "Interface extending a class";
    SUPERTYPE_NOT_INITIALIZED = "SUPERTYPE_NOT_INITIALIZED", Compiler, Error, false, [], "Superclass constructor not called";
    MANY_CLASSES_IN_SUPERTYPE_LIST = "MANY_CLASSES_IN_SUPERTYPE_LIST", Compiler, Error, false, [], "More than one superclass";
    FINAL_SUPERTYPE = "FINAL_SUPERTYPE", Compiler, Error, false, [], "Extending a final class";
    REDECLARATION_LOCAL = "REDECLARATION", Compiler, Error, false, [], "Two declarations with the same name in one scope";
    CONFLICTING_OVERLOADS = "CONFLICTING_OVERLOADS", Compiler, Error, false, [], "Two functions with the same signature in one scope";
    INITIALIZER_TYPE_MISMATCH = "INITIALIZER_TYPE_MISMATCH", Compiler, Error, false, [], "Value of the wrong type (literal, typed local/parameter, constructor call) as an initializer or default value";
    RETURN_TYPE_MISMATCH = "RETURN_TYPE_MISMATCH", Compiler, Error, false, [], "Value of the wrong type returned";
    CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS = "CREATING_AN_INSTANCE_OF_ABSTRACT_CLASS", Compiler, Error, false, [], "Constructor call of an abstract or sealed class, an interface or an enum";
    INVISIBLE_REFERENCE = "INVISIBLE_REFERENCE", Compiler, Error, false, [], "Use of a private, protected or internal declaration where it is not visible";
    OPERATOR_MODIFIER_REQUIRED = "OPERATOR_MODIFIER_REQUIRED", Compiler, Error, false, [], "Operator use of a function that is not 'operator'";
    UPPER_BOUND_VIOLATED = "UPPER_BOUND_VIOLATED", Compiler, Error, false, [], "Type argument outside its parameter's upper bound";
    ASSIGNMENT_TYPE_MISMATCH = "ASSIGNMENT_TYPE_MISMATCH", Compiler, Error, false, [], "Assigned value of the wrong type (literal, typed local/parameter, constructor call)";
    CONDITION_TYPE_MISMATCH = "CONDITION_TYPE_MISMATCH", Compiler, Error, false, [], "Condition that is not a Boolean";
    INFIX_MODIFIER_REQUIRED = "INFIX_MODIFIER_REQUIRED", Compiler, Error, false, [], "Infix call of a function that is not 'infix'";
    VARIABLE_EXPECTED = "VARIABLE_EXPECTED", Compiler, Error, false, [], "Assignment to something that is not a variable";
    RETURN_NOT_ALLOWED = "RETURN_NOT_ALLOWED", Compiler, Error, false, [], "'return' in an initializer block, or a value returned from a constructor";
    BREAK_OR_CONTINUE_OUTSIDE_A_LOOP = "BREAK_OR_CONTINUE_OUTSIDE_A_LOOP", Compiler, Error, false, [], "'break' or 'continue' outside a loop";
    UNINITIALIZED_VARIABLE = "UNINITIALIZED_VARIABLE", Compiler, Error, false, [], "Local variable read before it is assigned";
    WRONG_NUMBER_OF_TYPE_ARGUMENTS = "WRONG_NUMBER_OF_TYPE_ARGUMENTS", Compiler, Error, false, [], "Type with the wrong number of type arguments";
    VAR_OVERRIDDEN_BY_VAL = "VAR_OVERRIDDEN_BY_VAL", Compiler, Error, false, [], "'val' overriding a 'var'";
    CANNOT_WEAKEN_ACCESS_PRIVILEGE = "CANNOT_WEAKEN_ACCESS_PRIVILEGE", Compiler, Error, false, [], "Override with less visibility than the overridden member";
    RETURN_TYPE_MISMATCH_ON_OVERRIDE = "RETURN_TYPE_MISMATCH_ON_OVERRIDE", Compiler, Error, false, [], "Override whose return type is not a subtype of the overridden one";
    PROPERTY_TYPE_MISMATCH_ON_OVERRIDE = "PROPERTY_TYPE_MISMATCH_ON_OVERRIDE", Compiler, Error, false, [], "'val' override whose type is not a subtype of the overridden one";
    VAR_TYPE_MISMATCH_ON_OVERRIDE = "VAR_TYPE_MISMATCH_ON_OVERRIDE", Compiler, Error, false, [], "'var' override whose type differs from the overridden one";
    MULTIPLE_VARARG_PARAMETERS = "MULTIPLE_VARARG_PARAMETERS", Compiler, Error, false, [], "More than one 'vararg' parameter";
    DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE = "DEFAULT_VALUE_NOT_ALLOWED_IN_OVERRIDE", Compiler, Error, false, [], "Default parameter value in an 'override fun'";
    REIFIED_TYPE_PARAMETER_NO_INLINE = "REIFIED_TYPE_PARAMETER_NO_INLINE", Compiler, Error, false, [], "'reified' type parameter of a non-inline declaration";
    TYPE_PARAMETER_AS_REIFIED = "TYPE_PARAMETER_AS_REIFIED", Compiler, Error, false, [], "'T::class' of a type parameter that is not reified";
    EXPOSED_FUNCTION_RETURN_TYPE = "EXPOSED_FUNCTION_RETURN_TYPE", Compiler, Error, false, [], "Function whose return type is less visible than the function";
    EXPOSED_PARAMETER_TYPE = "EXPOSED_PARAMETER_TYPE", Compiler, Error, false, [], "Function or constructor whose parameter type is less visible than it";
    EXPOSED_PROPERTY_TYPE = "EXPOSED_PROPERTY_TYPE", Compiler, Error, false, [], "Property whose type is less visible than the property";
    EXPOSED_RECEIVER_TYPE = "EXPOSED_RECEIVER_TYPE", Compiler, Error, false, [], "Extension whose receiver type is less visible than the extension";
    EXPOSED_SUPER_CLASS = "EXPOSED_SUPER_CLASS", Compiler, Error, false, [], "Class whose superclass is less visible than the class";
    EXPOSED_SUPER_INTERFACE = "EXPOSED_SUPER_INTERFACE", Compiler, Error, false, [], "Class whose super-interface is less visible than the class";
    EQUALITY_NOT_APPLICABLE = "EQUALITY_NOT_APPLICABLE", Compiler, Error, false, [], "'==' between values of unrelated classes";
    INCOMPATIBLE_TYPES = "INCOMPATIBLE_TYPES", Compiler, Error, false, [], "'when' branch value whose class is unrelated to the subject's";
    INVALID_IF_AS_EXPRESSION = "INVALID_IF_AS_EXPRESSION", Compiler, Error, false, [], "'if' without 'else' used as a value";
    ELSE_MISPLACED_IN_WHEN = "ELSE_MISPLACED_IN_WHEN", Compiler, Error, false, [], "'else' branch that is not the last one in a 'when'";
    UNSAFE_OPERATOR_CALL = "UNSAFE_OPERATOR_CALL", Compiler, Error, false, [], "Operator on a nullable value without an operator for nullable receivers";
    ITERATOR_ON_NULLABLE = "ITERATOR_ON_NULLABLE", Compiler, Error, false, [], "'for' over a nullable value";
    DEPRECATION_ERROR = "DEPRECATION_ERROR", Compiler, Error, false, [], "Use of a project declaration deprecated with level ERROR or HIDDEN";
    OPT_IN_USAGE_ERROR = "OPT_IN_USAGE_ERROR", Compiler, Error, false, [], "Use of a declaration that requires opt-in (marker with level ERROR) without opting in";
    CYCLIC_INHERITANCE_HIERARCHY = "CYCLIC_INHERITANCE_HIERARCHY", Compiler, Error, false, [], "Class that inherits from itself through its supertypes";
    MANY_IMPL_MEMBER_NOT_IMPLEMENTED = "MANY_IMPL_MEMBER_NOT_IMPLEMENTED", Compiler, Error, false, [], "Member inherited with implementations from two interfaces and not overridden";
    ITERATOR_MISSING = "ITERATOR_MISSING", Compiler, Error, false, [], "'for' over a value without 'iterator()'";
    DELEGATE_SPECIAL_FUNCTION_MISSING = "DELEGATE_SPECIAL_FUNCTION_MISSING", Compiler, Error, false, [], "Property delegate without 'getValue' (or 'setValue' for a 'var')";
    ILLEGAL_SUSPEND_FUNCTION_CALL = "ILLEGAL_SUSPEND_FUNCTION_CALL", Compiler, Error, false, [], "Call to a 'suspend' function outside a coroutine or 'suspend' function";
    VAL_WITH_SETTER = "VAL_WITH_SETTER", Compiler, Error, false, [], "'val' property with a setter";
    ACCESSOR_FOR_DELEGATED_PROPERTY = "ACCESSOR_FOR_DELEGATED_PROPERTY", Compiler, Error, false, [], "Delegated property with an accessor body";
    DELEGATED_PROPERTY_IN_INTERFACE = "DELEGATED_PROPERTY_IN_INTERFACE", Compiler, Error, false, [], "Delegated property in an interface";
    ABSTRACT_DELEGATED_PROPERTY = "ABSTRACT_DELEGATED_PROPERTY", Compiler, Error, false, [], "Abstract delegated property";
    EXTENSION_PROPERTY_WITH_BACKING_FIELD = "EXTENSION_PROPERTY_WITH_BACKING_FIELD", Compiler, Error, false, [], "Extension property with an initializer";
    INLINE_PROPERTY_WITH_BACKING_FIELD = "INLINE_PROPERTY_WITH_BACKING_FIELD", Compiler, Error, false, [], "'inline' property with an initializer";
    EXPECTED_PROPERTY_INITIALIZER = "EXPECTED_PROPERTY_INITIALIZER", Compiler, Error, false, [], "'expect' property with an initializer";
    EXPECTED_DELEGATED_PROPERTY = "EXPECTED_DELEGATED_PROPERTY", Compiler, Error, false, [], "'expect' property with a delegate";
    EXPECTED_DECLARATION_WITH_BODY = "EXPECTED_DECLARATION_WITH_BODY", Compiler, Error, false, [], "'expect' function with a body";
    PROPERTY_WITH_NO_TYPE_NO_INITIALIZER = "PROPERTY_WITH_NO_TYPE_NO_INITIALIZER", Compiler, Error, false, [], "Property without a type, initializer, delegate or getter";
    VARIABLE_WITH_NO_TYPE_NO_INITIALIZER = "VARIABLE_WITH_NO_TYPE_NO_INITIALIZER", Compiler, Error, false, [], "Local variable without a type or initializer";
    GETTER_VISIBILITY_DIFFERS_FROM_PROPERTY_VISIBILITY = "GETTER_VISIBILITY_DIFFERS_FROM_PROPERTY_VISIBILITY", Compiler, Error, false, [], "Getter with a visibility other than the property's";
    SETTER_VISIBILITY_INCONSISTENT_WITH_PROPERTY_VISIBILITY = "SETTER_VISIBILITY_INCONSISTENT_WITH_PROPERTY_VISIBILITY", Compiler, Error, false, [], "Setter more visible than its property";
    PRIVATE_SETTER_FOR_ABSTRACT_PROPERTY = "PRIVATE_SETTER_FOR_ABSTRACT_PROPERTY", Compiler, Error, false, [], "'private set' on an abstract property";
    PRIVATE_SETTER_FOR_OPEN_PROPERTY = "PRIVATE_SETTER_FOR_OPEN_PROPERTY", Compiler, Error, false, [], "'private set' on an open property";
    PRIVATE_FUNCTION_WITH_NO_BODY = "PRIVATE_FUNCTION_WITH_NO_BODY", Compiler, Error, false, [], "Private interface function without a body";
    FUN_INTERFACE_WRONG_COUNT_OF_ABSTRACT_MEMBERS = "FUN_INTERFACE_WRONG_COUNT_OF_ABSTRACT_MEMBERS", Compiler, Error, false, [], "'fun interface' without exactly one abstract function";
    FUN_INTERFACE_WITH_ABSTRACT_PROPERTY = "FUN_INTERFACE_WITH_ABSTRACT_PROPERTY", Compiler, Error, false, [], "'fun interface' with an abstract property";
    FUN_INTERFACE_ABSTRACT_METHOD_WITH_TYPE_PARAMETERS = "FUN_INTERFACE_ABSTRACT_METHOD_WITH_TYPE_PARAMETERS", Compiler, Error, false, [], "'fun interface' method with type parameters";
    FUN_INTERFACE_ABSTRACT_METHOD_WITH_DEFAULT_VALUE = "FUN_INTERFACE_ABSTRACT_METHOD_WITH_DEFAULT_VALUE", Compiler, Error, false, [], "'fun interface' method with default parameter values";
    VALUE_CLASS_EMPTY_CONSTRUCTOR = "VALUE_CLASS_EMPTY_CONSTRUCTOR", Compiler, Error, false, [], "Value class without a constructor parameter";
    INLINE_CLASS_CONSTRUCTOR_WRONG_PARAMETERS_SIZE = "INLINE_CLASS_CONSTRUCTOR_WRONG_PARAMETERS_SIZE", Compiler, Error, false, [], "Value class with more than one constructor parameter";
    VALUE_CLASS_CONSTRUCTOR_NOT_FINAL_READ_ONLY_PARAMETER = "VALUE_CLASS_CONSTRUCTOR_NOT_FINAL_READ_ONLY_PARAMETER", Compiler, Error, false, [], "Value class parameter that is not a 'val'";
    PROPERTY_WITH_BACKING_FIELD_INSIDE_VALUE_CLASS = "PROPERTY_WITH_BACKING_FIELD_INSIDE_VALUE_CLASS", Compiler, Error, false, [], "Value class property with a backing field";
    DELEGATED_PROPERTY_INSIDE_VALUE_CLASS = "DELEGATED_PROPERTY_INSIDE_VALUE_CLASS", Compiler, Error, false, [], "Value class with a delegated property";
    VALUE_CLASS_NOT_FINAL = "VALUE_CLASS_NOT_FINAL", Compiler, Error, false, [], "Open, abstract or sealed value class";
    VALUE_CLASS_NOT_TOP_LEVEL = "VALUE_CLASS_NOT_TOP_LEVEL", Compiler, Error, false, [], "Local or inner value class";
    VALUE_CLASS_CANNOT_EXTEND_CLASSES = "VALUE_CLASS_CANNOT_EXTEND_CLASSES", Compiler, Error, false, [], "Value class extending a class";
    VALUE_CLASS_WITHOUT_JVM_INLINE_ANNOTATION = "VALUE_CLASS_WITHOUT_JVM_INLINE_ANNOTATION", Compiler, Error, false, [], "Value class without '@JvmInline' on the JVM";
    NON_PRIVATE_OR_PROTECTED_CONSTRUCTOR_IN_SEALED = "NON_PRIVATE_OR_PROTECTED_CONSTRUCTOR_IN_SEALED", Compiler, Error, false, [], "Public or internal constructor of a sealed class";
    SEALED_INHERITOR_IN_DIFFERENT_PACKAGE = "SEALED_INHERITOR_IN_DIFFERENT_PACKAGE", Compiler, Error, false, [], "Subclass of a sealed class in another package";
    SEALED_INHERITOR_IN_DIFFERENT_MODULE = "SEALED_INHERITOR_IN_DIFFERENT_MODULE", Compiler, Error, false, [], "Subclass of a sealed class from another module or library";
    SEALED_SUPERTYPE_IN_LOCAL_CLASS = "SEALED_SUPERTYPE_IN_LOCAL_CLASS", Compiler, Error, false, [], "Local class or anonymous object extending a sealed type";
    SUPERTYPE_APPEARS_TWICE = "SUPERTYPE_APPEARS_TWICE", Compiler, Error, false, [], "The same supertype listed twice";
    ABSTRACT_MEMBER_NOT_IMPLEMENTED_BY_ENUM_ENTRY = "ABSTRACT_MEMBER_NOT_IMPLEMENTED_BY_ENUM_ENTRY", Compiler, Error, false, [], "Enum entry that does not implement the enum's abstract members";
    UNSAFE_IMPLICIT_INVOKE_CALL = "UNSAFE_IMPLICIT_INVOKE_CALL", Compiler, Error, false, [], "Call of a nullable function-typed local or parameter";
    CONFLICTING_JVM_DECLARATIONS = "CONFLICTING_JVM_DECLARATIONS", Compiler, Error, false, [], "Declarations of one class or file that compile to the same JVM method";
    NON_LOCAL_RETURN_NOT_ALLOWED = "NON_LOCAL_RETURN_NOT_ALLOWED", Compiler, Error, false, [], "'return' in a 'crossinline' lambda, or an inline parameter called where it is not inlined";
    USAGE_IS_NOT_INLINABLE = "USAGE_IS_NOT_INLINABLE", Compiler, Error, false, [], "Inline parameter used as a value";
    NON_PUBLIC_CALL_FROM_PUBLIC_INLINE = "NON_PUBLIC_CALL_FROM_PUBLIC_INLINE", Compiler, Error, false, [], "Public inline function using a private or internal declaration";
    NON_LOCAL_SUSPENSION_POINT = "NON_LOCAL_SUSPENSION_POINT", Compiler, Error, false, [], "Suspend call in a lambda passed to a 'crossinline' parameter";
    UNRESOLVED_LABEL = "UNRESOLVED_LABEL", Compiler, Error, false, [], "Label that names nothing around it";
    NOT_A_LOOP_LABEL = "NOT_A_LOOP_LABEL", Compiler, Error, false, [], "'break' or 'continue' with a label that is not on a loop";
    CONFLICTING_PROJECTION = "CONFLICTING_PROJECTION", Compiler, Error, false, [], "'in' projection of an 'out' type parameter, or the reverse";
    PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE = "PROJECTION_IN_IMMEDIATE_ARGUMENT_TO_SUPERTYPE", Compiler, Error, false, [], "'in', 'out' or '*' type argument of a supertype";
    TYPE_VARIANCE_CONFLICT = "TYPE_VARIANCE_CONFLICT", Compiler, Error, false, [], "'out' type parameter in an 'in' position (or the reverse) of a member";
    ARGUMENT_TYPE_MISMATCH = "ARGUMENT_TYPE_MISMATCH", Compiler, Error, false, [], "Argument of the wrong type (literal, typed local/parameter, constructor call) for a function without overloads";
    NULL_FOR_NONNULL_TYPE = "NULL_FOR_NONNULL_TYPE", Compiler, Error, false, [], "'null' for a non-null type";
    TOO_MANY_ARGUMENTS = "TOO_MANY_ARGUMENTS", Compiler, Error, false, [], "More arguments than parameters";
    NO_VALUE_FOR_PARAMETER = "NO_VALUE_FOR_PARAMETER", Compiler, Error, false, [], "Required parameter without an argument";
    NAMED_PARAMETER_NOT_FOUND = "NAMED_PARAMETER_NOT_FOUND", Compiler, Error, false, [], "Named argument for a parameter that does not exist";
    NONE_APPLICABLE = "NONE_APPLICABLE", Compiler, Error, false, [], "No overload accepts these arguments";
    // Compiler warnings.
    USELESS_CAST = "USELESS_CAST", Compiler, Warning, true, [], "Cast to the type the value already has";
    USELESS_IS_CHECK = "USELESS_IS_CHECK", Compiler, Warning, false, [], "'is' check that is always true or always false";
    CAST_NEVER_SUCCEEDS = "CAST_NEVER_SUCCEEDS", Compiler, Warning, false, [], "Cast between unrelated classes";
    NOTHING_TO_INLINE = "NOTHING_TO_INLINE", Compiler, Warning, false, [], "'inline' function without parameters of function type";
    REDUNDANT_NULLABLE = "REDUNDANT_NULLABLE", Compiler, Warning, true, [], "Type marked nullable twice ('T??')";
    DUPLICATE_BRANCH_CONDITION_IN_WHEN = "DUPLICATE_BRANCH_CONDITION_IN_WHEN", Compiler, Warning, false, [], "'when' branch condition repeating an earlier one";
    UNUSED_VALUE = "UNUSED_VALUE", Compiler, Warning, true, [], "Value assigned to a local that no path reads";
    VARIABLE_WITH_REDUNDANT_INITIALIZER = "VARIABLE_WITH_REDUNDANT_INITIALIZER", Compiler, Warning, true, [], "Local initializer that is always overwritten before it is read";
    ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE = "ASSIGNED_BUT_NEVER_ACCESSED_VARIABLE", Compiler, Warning, true, [], "Local 'var' that is assigned but never read";
    OPT_IN_USAGE = "OPT_IN_USAGE", Compiler, Warning, false, [], "Use of a declaration that requires opt-in (marker with level WARNING) without opting in";
    UNCHECKED_CAST = "UNCHECKED_CAST", Compiler, Warning, false, [], "Cast to a generic type or type parameter the runtime cannot check";
    EXTENSION_SHADOWED_BY_MEMBER = "EXTENSION_SHADOWED_BY_MEMBER", Compiler, Warning, false, [], "Extension with the signature of a member of its receiver";
    REDUNDANT_PROJECTION = "REDUNDANT_PROJECTION", Compiler, Warning, true, [], "'out' projection of an 'out' type parameter (or 'in' of an 'in' one)";
    PLATFORM_CLASS_MAPPED_TO_KOTLIN = "PLATFORM_CLASS_MAPPED_TO_KOTLIN", Compiler, Warning, false, [], "Java class used where its Kotlin counterpart should be ('java.lang.Integer', 'java.util.List')";
    FINAL_UPPER_BOUND = "FINAL_UPPER_BOUND", Compiler, Warning, false, [], "Type parameter bounded by a final class";
    UNNECESSARY_SAFE_CALL = "UNNECESSARY_SAFE_CALL", Compiler, Warning, true, [], "'?.' on a non-null receiver";
    UNNECESSARY_NOT_NULL_ASSERTION = "UNNECESSARY_NOT_NULL_ASSERTION", Compiler, Warning, true, [], "'!!' on a non-null value";
    USELESS_ELVIS = "USELESS_ELVIS", Compiler, Warning, true, [], "'?:' after a non-null value";
    SENSELESS_COMPARISON = "SENSELESS_COMPARISON", Compiler, Warning, false, [], "Comparing a non-null value with null";
    DEPRECATION = "DEPRECATION", Compiler, Warning, false, [], "Use of a deprecated declaration";
    REDUNDANT_ELSE_IN_WHEN = "REDUNDANT_ELSE_IN_WHEN", Compiler, Warning, true, [], "'else' in a 'when' that is already exhaustive";
    NAME_SHADOWING = "NAME_SHADOWING", Compiler, Warning, false, [], "Local declaration hiding an outer one";
    UNREACHABLE_CODE = "UNREACHABLE_CODE", Compiler, Warning, true, [], "Code after 'return', 'throw', 'break' or 'continue'";
    NON_FINAL_MEMBER_IN_FINAL_CLASS = "NON_FINAL_MEMBER_IN_FINAL_CLASS", Compiler, Warning, false, [], "'open' member of a final class";
    NON_FINAL_MEMBER_IN_OBJECT = "NON_FINAL_MEMBER_IN_OBJECT", Compiler, Warning, false, [], "'open' member of an object";
    UNUSED_EXPRESSION = "UNUSED_EXPRESSION", Compiler, Warning, true, [], "Statement whose value is never used and that has no effect";
    UNUSED_LAMBDA_EXPRESSION = "UNUSED_LAMBDA_EXPRESSION", Compiler, Warning, true, [], "Lambda literal used as a statement (never called)";

    UNUSED_IMPORT = "UnusedImport", Unused, Warning, true, ["unused"], "Import directive that nothing in the file uses";
    UNUSED_VARIABLE = "UNUSED_VARIABLE", Unused, Warning, true, ["unused", "UnusedVariable"], "Local variable that is never read";
    UNUSED_PARAMETER = "UNUSED_PARAMETER", Unused, Warning, true, ["unused", "UnusedParameter"], "Function or constructor parameter that is never used";
    UNUSED_ANONYMOUS_PARAMETER = "UNUSED_ANONYMOUS_PARAMETER", Unused, Info, true, ["unused"], "Lambda parameter that is never used (can be '_')";
    UNUSED_SYMBOL = "UnusedSymbol", Unused, Warning, true, ["unused", "UnusedPrivateMember", "UnusedPrivateProperty", "UnusedPrivateClass"], "Private or local declaration that is never used";
    CAN_BE_VAL = "CanBeVal", Unused, Warning, false, ["CAN_BE_VAL"], "Local 'var' that is never reassigned";

    REDUNDANT_SEMICOLON = "RedundantSemicolon", Redundancy, Warning, true, [], "Semicolon that is not needed";
    REDUNDANT_CURLY_BRACES = "RemoveCurlyBracesFromTemplate", Redundancy, Warning, true, [], "'${x}' in a string template where '$x' suffices";
    REDUNDANT_VISIBILITY = "RedundantVisibilityModifier", Redundancy, Warning, true, [], "'public' modifier (the default visibility)";
    REDUNDANT_MODALITY = "RedundantModalityModifier", Redundancy, Warning, true, [], "'final', 'open' or 'abstract' that is implied anyway";
    REDUNDANT_UNIT_RETURN_TYPE = "RedundantUnitReturnType", Redundancy, Warning, true, [], "': Unit' on a function with a block body";
    REDUNDANT_IF = "RedundantIf", Redundancy, Warning, false, [], "'if (c) true else false' and similar";
    REDUNDANT_EMPTY_CLASS_BODY = "RemoveEmptyClassBody", Redundancy, Warning, true, [], "Empty '{}' class body";
    REDUNDANT_EMPTY_CONSTRUCTOR = "RemoveEmptyPrimaryConstructor", Redundancy, Warning, true, [], "Empty '()' primary constructor";
    REDUNDANT_LAMBDA_PARENS = "RemoveEmptyParenthesesFromLambdaCall", Redundancy, Info, true, [], "'foo() { }' where 'foo { }' suffices";
    REDUNDANT_GETTER = "RedundantGetter", Redundancy, Warning, true, [], "Getter that only returns the backing field";
    REDUNDANT_SETTER = "RedundantSetter", Redundancy, Warning, true, [], "Setter that only assigns the backing field";
    REDUNDANT_INIT_BLOCK = "RedundantEmptyInitializerBlock", Redundancy, Warning, true, [], "Empty 'init {}' block";
    REDUNDANT_BACKTICKS = "RemoveRedundantBackticks", Redundancy, Warning, true, [], "Backticks around a name that does not need them";
    REDUNDANT_UNIT = "RedundantUnitExpression", Redundancy, Warning, true, [], "'Unit' expression that has no effect";
    UNNECESSARY_VARIABLE = "UnnecessaryVariable", Redundancy, Info, false, [], "Variable that is only returned on the next line";
    REDUNDANT_STRING_TEMPLATE = "RemoveSingleExpressionStringTemplate", Redundancy, Info, false, [], "'\"$s\"' where 's' is already a String";
    REDUNDANT_LAMBDA_ARROW = "RedundantLambdaArrow", Redundancy, Warning, true, [], "'->' in a lambda without parameters, or 'it ->'";
    REDUNDANT_TYPE_ARGUMENTS = "RemoveExplicitTypeArguments", Redundancy, Warning, true, [], "Explicit type arguments that the arguments already determine";
    REDUNDANT_EXPLICIT_TYPE = "RedundantExplicitType", Redundancy, Warning, true, [], "Local variable type that its initializer (literal or constructor call) states anyway";
    REDUNDANT_QUALIFIER = "RemoveRedundantQualifierName", Redundancy, Warning, true, [], "Package or class qualifier of a name that resolves the same without it";
    USELESS_CALL_ON_NOT_NULL = "UselessCallOnNotNull", Redundancy, Warning, true, [], "'orEmpty()', 'isNullOrEmpty()' or 'isNullOrBlank()' on a value that is never null";

    USE_EXPRESSION_BODY = "UseExpressionBody", Style, Action, false, [], "Block body consisting of a single 'return'";
    MAY_BE_CONSTANT = "MayBeConstant", Style, Info, false, [], "'val' with a constant initializer that could be 'const'";
    RANGE_UNTIL = "ReplaceRangeToWithRangeUntil", Style, Info, false, ["ReplaceRangeToWithUntil"], "'a..b - 1' instead of 'a..<b'";
    MANUAL_INDICES = "ReplaceManualRangeWithIndicesCalls", Style, Info, false, [], "'0 until xs.size' instead of 'xs.indices'";
    SIZE_CHECK_IS_EMPTY = "ReplaceSizeZeroCheckWithIsEmpty", Style, Info, false, [], "'xs.size == 0' instead of 'xs.isEmpty()'";
    SIZE_CHECK_IS_NOT_EMPTY = "ReplaceSizeCheckWithIsNotEmpty", Style, Info, false, [], "'xs.size > 0' instead of 'xs.isNotEmpty()'";
    REPLACE_GET_OR_SET = "ReplaceGetOrSet", Style, Info, false, [], "Explicit 'get'/'set' operator call instead of indexing";
    REPLACE_CALL_WITH_OPERATOR = "ReplaceCallWithBinaryOperator", Style, Info, false, [], "Explicit 'plus'/'minus'/... operator call instead of the operator";
    EMPTY_CONTROL_FLOW = "ControlFlowWithEmptyBody", Style, Info, false, [], "'if', 'else', 'for' or 'while' with an empty body";
    OPERATOR_ASSIGNMENT = "ReplaceWithOperatorAssignment", Style, Info, false, [], "'x = x + y' instead of 'x += y'";
    PUT_ASSIGNMENT = "ReplacePutWithAssignment", Style, Info, false, [], "'map.put(k, v)' instead of 'map[k] = v'";
    SIMPLIFIABLE_CALL_CHAIN = "SimplifiableCallChain", Style, Info, false, [], "Call chain that one call replaces ('filter { }.first()' instead of 'first { }')";

    NAN_EQUALITY = "ConvertNaNEquality", Bugs, Warning, false, [], "Comparison with NaN using '==' (always false)";
    SELF_ASSIGNMENT = "SelfAssignment", Bugs, Warning, false, [], "Variable assigned to itself";
    EQUALS_OR_HASH_CODE = "EqualsOrHashCode", Bugs, Warning, false, [], "Class overrides only one of 'equals()' and 'hashCode()'";
    ARRAY_IN_DATA_CLASS = "ArrayInDataClass", Bugs, Warning, false, [], "Array property in a data class without custom 'equals()'/'hashCode()'";
    THROWABLE_NOT_THROWN = "ThrowableNotThrown", Bugs, Warning, false, [], "Exception created but neither thrown nor used";
    RECURSIVE_ACCESSOR = "RecursivePropertyAccessor", Bugs, Warning, false, [], "Property accessor that reads (getter) or assigns (setter) its own property";
    NESTED_IT = "NestedLambdaShadowedImplicitParameter", Bugs, Warning, false, [], "Implicit 'it' of a nested lambda shadowing the enclosing lambda's 'it'";

    CLASS_NAME = "ClassName", Naming, Info, false, [], "Class, object and type alias names are UpperCamelCase";
    FUNCTION_NAME = "FunctionName", Naming, Info, false, ["TestFunctionName"], "Function names are lowerCamelCase";
    PROPERTY_NAME = "PropertyName", Naming, Info, false, ["PrivatePropertyName", "ObjectPropertyName"], "Property names are lowerCamelCase (constants may be SCREAMING_CASE)";
    CONST_PROPERTY_NAME = "ConstPropertyName", Naming, Info, false, [], "'const val' names are SCREAMING_SNAKE_CASE";
    LOCAL_VARIABLE_NAME = "LocalVariableName", Naming, Info, false, [], "Local variable names are lowerCamelCase";
    ENUM_ENTRY_NAME = "EnumEntryName", Naming, Info, false, [], "Enum entries are UPPER_CASE or UpperCamelCase";
    PACKAGE_NAME = "PackageName", Naming, Info, false, [], "Package names are lowercase";
    PACKAGE_DIRECTORY_MISMATCH = "PackageDirectoryMismatch", Naming, Info, false, [], "Package does not match the directory";
}

/// `lint` settings: `{ "enabled": true, "rules": { "UnusedImport": "error", "naming": "off" } }`.
/// Levels: `error`, `warning`, `info`, `hint`, `action` (only a code action at the cursor), `off`.
/// Keys of `rules` are rule ids, category names (`unused`, `redundancy`, `style`, `bugs`,
/// `naming`) or `all`; the most specific one wins.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LintConfig {
    pub enabled: bool,
    pub rules: HashMap<String, Level>,
}

impl Default for LintConfig {
    fn default() -> Self {
        LintConfig { enabled: true, rules: HashMap::new() }
    }
}

impl LintConfig {
    pub fn level(&self, rule: &Rule) -> Level {
        self.rules
            .get(rule.id)
            .or_else(|| self.rules.get(rule.category.name()))
            .or_else(|| self.rules.get("all"))
            .copied()
            .unwrap_or(rule.level)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: TextRange,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<Edit>,
    /// Edits in other files (e.g. making a supertype member `open`).
    pub elsewhere: Vec<(std::path::PathBuf, Edit)>,
}

impl Fix {
    pub fn new(title: impl Into<String>, edits: Vec<Edit>) -> Fix {
        Fix { title: title.into(), edits, elsewhere: Vec::new() }
    }

    pub fn replace(title: impl Into<String>, range: TextRange, text: impl Into<String>) -> Fix {
        Fix::new(title, vec![Edit { range, text: text.into() }])
    }

    pub fn delete(title: impl Into<String>, range: TextRange) -> Fix {
        Fix::replace(title, range, "")
    }
}

#[derive(Clone, Debug)]
pub struct Lint {
    pub rule: &'static Rule,
    pub level: Level,
    pub range: TextRange,
    pub message: String,
    pub fix: Option<Fix>,
    /// Further fixes, offered after `fix` (which `source.fixAll` applies).
    pub alternatives: Vec<Fix>,
}

impl Lint {
    pub fn diagnostic(&self) -> Diagnostic {
        let severity = match self.level {
            Level::Error => Severity::Error,
            Level::Warning => Severity::Warning,
            Level::Info => Severity::Info,
            Level::Hint | Level::Action | Level::Off => Severity::Hint,
        };
        Diagnostic { range: self.range, severity, message: self.message.clone(), code: self.rule.id, unnecessary: self.rule.unnecessary }
    }
}

/// What the rules see.
pub(crate) struct Cx<'a> {
    pub snap: &'a Snapshot,
    pub path: &'a Path,
    pub text: &'a str,
    pub root: SyntaxNode,
    pub module: Option<&'a Module>,
    /// The file belongs to a test source set.
    pub test_source: bool,
    config: &'a LintConfig,
    /// Types are available (dependencies resolved, standard library present).
    full: bool,
    engine: OnceCell<Option<Engine<'a>>>,
}

impl<'a> Cx<'a> {
    pub fn on(&self, rule: &Rule) -> bool {
        self.config.level(rule) != Level::Off
    }

    /// The type engine, when the file is fully analysable.
    pub fn engine(&self) -> Option<&Engine<'a>> {
        if !self.full {
            return None;
        }
        self.engine.get_or_init(|| Engine::new(self.snap, self.path)).as_ref()
    }

    /// The Kotlin language version (`major * 100 + minor`), if known.
    pub fn language_version(&self) -> Option<u32> {
        let k = &self.module?.kotlin;
        let v = k.language_version.as_ref().or(k.version.as_ref())?;
        let mut parts = v.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().ok());
        Some(parts.next()?? * 100 + parts.next().flatten().unwrap_or(0))
    }
}

pub(crate) struct Sink<'a> {
    config: &'a LintConfig,
    pub lints: Vec<Lint>,
}

impl Sink<'_> {
    pub fn report(&mut self, rule: &'static Rule, range: TextRange, message: impl Into<String>, fix: Option<Fix>) {
        self.report_fixes(rule, range, message, fix.into_iter().collect());
    }

    /// Report with several fixes; the first is the preferred one.
    pub fn report_fixes(&mut self, rule: &'static Rule, range: TextRange, message: impl Into<String>, mut fixes: Vec<Fix>) {
        let level = self.config.level(rule);
        if level != Level::Off {
            let fix = (!fixes.is_empty()).then(|| fixes.remove(0));
            self.lints.push(Lint { rule, level, range, message: message.into(), fix, alternatives: fixes });
        }
    }
}

/// Run every enabled rule on `path`. `full`: the file is fully analysable (types available).
pub fn lint_file(snap: &Snapshot, path: &Path, full: bool) -> Vec<Lint> {
    let Some(file) = snap.file(path) else { return Vec::new() };
    if file.language != Language::Kotlin {
        return Vec::new();
    }
    let ctx = snap.context_of(path);
    let module = ctx.map(|c| snap.workspace.module(c.module));
    let root_path = snap.workspace.source_root_of(path);
    let generated = module.is_some_and(|m| m.source_roots.iter().any(|r| r.generated && Some(r.path.as_path()) == root_path));
    if generated {
        return Vec::new();
    }
    let cx = Cx {
        snap,
        path,
        text: &file.text,
        root: file.parse().syntax(),
        module,
        test_source: ctx.is_some_and(|c| c.kind == crate::project::SourceKind::Test),
        config: &snap.lints,
        full,
        engine: OnceCell::new(),
    };
    let mut sink = Sink { config: &snap.lints, lints: Vec::new() };
    compiler::check(&cx, &mut sink);
    if snap.lints.enabled {
        unused::check(&cx, &mut sink);
        redundant::check(&cx, &mut sink);
        style::check(&cx, &mut sink);
        bugs::check(&cx, &mut sink);
        naming::check(&cx, &mut sink);
    }
    let suppressions = Suppressions::collect(&cx.root);
    let mut lints = sink.lints;
    lints.retain(|l| !suppressions.suppressed(l));
    lints.sort_by_key(|l| (l.range.start(), l.range.end(), l.rule.id));
    lints.dedup_by(|a, b| a.range == b.range && a.rule.id == b.rule.id);
    lints
}

/// Ranges of unused (or duplicate) import directives, regardless of the lint settings.
pub fn unused_imports(snap: &Snapshot, path: &Path, full: bool) -> Vec<TextRange> {
    let Some(file) = snap.file(path) else { return Vec::new() };
    let config = LintConfig::default();
    let cx = Cx {
        snap,
        path,
        text: &file.text,
        root: file.parse().syntax(),
        module: None,
        test_source: false,
        config: &config,
        full,
        engine: OnceCell::new(),
    };
    let mut sink = Sink { config: &config, lints: Vec::new() };
    unused::unused_imports(&cx, &mut sink);
    sink.lints.into_iter().map(|l| l.range).collect()
}

// ---------------------------------------------------------------- suppression

struct Suppressions(Vec<(TextRange, Vec<String>)>);

impl Suppressions {
    fn collect(root: &SyntaxNode) -> Suppressions {
        let mut out = Vec::new();
        for ann in root.descendants().filter_map(ast::Annotation::cast) {
            let name = ann
                .type_ref()
                .and_then(|t| t.inner())
                .and_then(ast::UserType::cast)
                .and_then(|ut| ut.segments().last().and_then(|s| s.name_token()))
                .map(|t| ast::ident_text(&t));
            if name.as_deref() != Some("Suppress") {
                continue;
            }
            let ids: Vec<String> = ann
                .syntax()
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SyntaxKind::STRING_TEXT)
                .map(|t| t.text().to_string())
                .collect();
            let mut scope = ann.syntax().parent();
            while let Some(s) = scope.clone().filter(|s| matches!(s.kind(), SyntaxKind::MODIFIER_LIST | SyntaxKind::ANNOTATION_GROUP)) {
                scope = s.parent();
            }
            let range = match scope {
                Some(s) if s.kind() == SyntaxKind::FILE_ANNOTATION_LIST => root.text_range(),
                Some(s) => s.text_range(),
                None => continue,
            };
            out.push((range, ids));
        }
        for tok in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
            let Some(rest) = (tok.kind() == SyntaxKind::LINE_COMMENT).then(|| tok.text().strip_prefix("//noinspection")).flatten() else { continue };
            let ids: Vec<String> = rest.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::to_string).collect();
            let Some(parent) = tok.parent() else { continue };
            // A leading comment is either part of the declaration node or its previous sibling.
            let scope = if parent.text_range().start() == tok.text_range().start() {
                Some(parent)
            } else {
                tok.siblings_with_tokens(rowan::Direction::Next).find_map(|e| e.into_node())
            };
            if let Some(s) = scope {
                out.push((s.text_range(), ids));
            }
        }
        Suppressions(out)
    }

    fn suppressed(&self, l: &Lint) -> bool {
        self.0.iter().any(|(range, ids)| {
            range.contains_range(l.range) && ids.iter().any(|id| id == l.rule.id || id == "ALL" || l.rule.aliases.contains(&id.as_str()))
        })
    }
}

/// Code actions that suppress `lint` on the innermost declaration and on the whole file.
pub fn suppress_fixes(root: &SyntaxNode, text: &str, lint: &Lint) -> Vec<Fix> {
    let id = lint.rule.id;
    let mut out = Vec::new();
    let decl = root
        .covering_element(lint.range)
        .ancestors()
        .find(|n| matches!(n.kind(), SyntaxKind::FUN | SyntaxKind::PROPERTY | SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::TYPEALIAS | SyntaxKind::SECONDARY_CONSTRUCTOR));
    if let Some(decl) = decl {
        let what = match decl.kind() {
            SyntaxKind::FUN => "function",
            SyntaxKind::PROPERTY => "property",
            SyntaxKind::TYPEALIAS => "type alias",
            SyntaxKind::SECONDARY_CONSTRUCTOR => "constructor",
            _ => "class",
        };
        let name = ast::token(&decl, SyntaxKind::IDENT).map(|t| format!(" '{}'", ast::ident_text(&t))).unwrap_or_default();
        let existing = ast::child::<ast::ModifierList>(&decl).and_then(|m| m.annotations().find(is_suppress));
        let edit = match existing {
            Some(a) => add_to_suppress(&a, id),
            None => {
                // Insert before the first modifier/annotation or keyword, after any KDoc.
                let start = decl
                    .children_with_tokens()
                    .find(|e| !e.kind().is_trivia())
                    .map(|e| e.text_range().start())
                    .unwrap_or(decl.text_range().start());
                Some(Edit { range: TextRange::empty(start), text: format!("@Suppress(\"{id}\")\n{}", indent_at(text, start)) })
            }
        };
        if let Some(edit) = edit {
            out.push(Fix::new(format!("Suppress '{id}' for {what}{name}"), vec![edit]));
        }
    }
    let file_ann = root
        .children()
        .find(|n| n.kind() == SyntaxKind::FILE_ANNOTATION_LIST)
        .and_then(|l| l.children().filter_map(ast::Annotation::cast).find(is_suppress));
    let edit = match file_ann {
        Some(a) => add_to_suppress(&a, id),
        None => {
            // After a shebang / leading comments, before the package directive.
            let start = root
                .children_with_tokens()
                .find(|e| !e.kind().is_trivia() && !e.text_range().is_empty())
                .map(|e| e.text_range().start())
                .unwrap_or_default();
            Some(Edit { range: TextRange::empty(start), text: format!("@file:Suppress(\"{id}\")\n\n") })
        }
    };
    if let Some(edit) = edit {
        out.push(Fix::new(format!("Suppress '{id}' for file"), vec![edit]));
    }
    out
}

fn is_suppress(a: &ast::Annotation) -> bool {
    a.type_ref().is_some_and(|t| {
        let s = t.syntax().text().to_string();
        s == "Suppress" || s == "kotlin.Suppress"
    })
}

fn add_to_suppress(a: &ast::Annotation, id: &str) -> Option<Edit> {
    let args = a.syntax().children().find(|n| n.kind() == SyntaxKind::VALUE_ARGUMENT_LIST)?;
    let close = ast::token(&args, SyntaxKind::R_PAREN)?;
    let empty = !args.children().any(|n| n.kind() == SyntaxKind::VALUE_ARGUMENT);
    let text = if empty { format!("\"{id}\"") } else { format!(", \"{id}\"") };
    Some(Edit { range: TextRange::empty(close.text_range().start()), text })
}

// ---------------------------------------------------------------- helpers

pub(crate) fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + use<> {
    n.descendants_with_tokens().filter_map(|e| e.into_token())
}

pub(crate) fn idents(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + use<> {
    tokens(n).filter(|t| t.kind() == SyntaxKind::IDENT)
}

pub(crate) fn is_comment(k: SyntaxKind) -> bool {
    matches!(k, SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT | SyntaxKind::KDOC)
}

pub(crate) fn has_comments(n: &SyntaxNode) -> bool {
    tokens(n).any(|t| is_comment(t.kind()))
}

/// The next token that is not whitespace or a comment (newlines count unless `skip_newlines`).
pub(crate) fn next_significant(t: &SyntaxToken, skip_newlines: bool) -> Option<SyntaxToken> {
    let mut cur = t.next_token();
    while let Some(c) = cur {
        let skip = c.kind() == SyntaxKind::WHITESPACE || is_comment(c.kind()) || skip_newlines && c.kind() == SyntaxKind::NEWLINE;
        if !skip {
            return Some(c);
        }
        cur = c.next_token();
    }
    None
}

pub(crate) fn prev_significant(t: &SyntaxToken) -> Option<SyntaxToken> {
    let mut cur = t.prev_token();
    while let Some(c) = cur {
        if !c.kind().is_trivia() {
            return Some(c);
        }
        cur = c.prev_token();
    }
    None
}

/// `{ }` with nothing (not even a comment) inside.
pub(crate) fn is_empty_block(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::BLOCK
        && n.children().next().is_none()
        && n.children_with_tokens().all(|e| matches!(e.kind(), SyntaxKind::L_CURLY | SyntaxKind::R_CURLY | SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE))
}

pub(crate) fn modifiers(n: &SyntaxNode) -> Option<ast::ModifierList> {
    ast::child(n)
}

pub(crate) fn has_modifier(n: &SyntaxNode, m: &str) -> bool {
    modifiers(n).is_some_and(|l| l.has(m))
}

/// Annotated with something other than `@Suppress` / `@Deprecated` (frameworks may use it).
pub(crate) fn has_annotations(n: &SyntaxNode) -> bool {
    modifiers(n).is_some_and(|l| {
        l.annotations().any(|a| {
            let name = a.type_ref().map(|t| t.syntax().text().to_string()).unwrap_or_default();
            !matches!(name.as_str(), "Suppress" | "kotlin.Suppress" | "Deprecated" | "kotlin.Deprecated")
        })
    })
}

/// The declared name (`IDENT` child) of a declaration.
pub(crate) fn name_token(n: &SyntaxNode) -> Option<SyntaxToken> {
    ast::token(n, SyntaxKind::IDENT)
}

/// The node is inside, or contains, a syntax error.
pub(crate) fn in_error(n: &SyntaxNode) -> bool {
    n.ancestors().any(|a| a.kind() == SyntaxKind::ERROR) || n.descendants().any(|d| d.kind() == SyntaxKind::ERROR)
}

/// Expressions that never need parentheses as an operand.
pub(crate) fn is_primary(n: &SyntaxNode) -> bool {
    matches!(
        n.kind(),
        SyntaxKind::NAME_REF
            | SyntaxKind::LITERAL
            | SyntaxKind::STRING_TEMPLATE
            | SyntaxKind::CALL_EXPR
            | SyntaxKind::DOT_QUALIFIED_EXPR
            | SyntaxKind::SAFE_QUALIFIED_EXPR
            | SyntaxKind::PAREN_EXPR
            | SyntaxKind::THIS_EXPR
            | SyntaxKind::INDEX_EXPR
            | SyntaxKind::POSTFIX_EXPR
            | SyntaxKind::CLASS_LITERAL
            | SyntaxKind::CALLABLE_REFERENCE
    )
}

pub(crate) fn operand(n: &SyntaxNode) -> String {
    if is_primary(n) { n.text().to_string() } else { format!("({})", n.text()) }
}

/// The assignment operator of a `BINARY_EXPR` (`=`, `+=`, ...), if it is one.
pub(crate) fn assignment_op(n: &SyntaxNode) -> Option<SyntaxToken> {
    if n.kind() != SyntaxKind::BINARY_EXPR {
        return None;
    }
    n.children_with_tokens().filter_map(|e| e.into_token()).find(|t| {
        matches!(t.kind(), SyntaxKind::EQ | SyntaxKind::PLUS_EQ | SyntaxKind::MINUS_EQ | SyntaxKind::STAR_EQ | SyntaxKind::SLASH_EQ | SyntaxKind::PERCENT_EQ)
    })
}

/// The operator token of a binary expression (inside its `OPERATION_REFERENCE`).
pub(crate) fn binary_op(n: &SyntaxNode) -> Option<SyntaxToken> {
    n.children().find(|c| c.kind() == SyntaxKind::OPERATION_REFERENCE)?.first_token()
}

/// Operands of a binary expression.
pub(crate) fn binary_operands(n: &SyntaxNode) -> Option<(SyntaxNode, SyntaxNode)> {
    let mut it = n.children().filter(|c| c.kind() != SyntaxKind::OPERATION_REFERENCE);
    Some((it.next()?, it.next()?))
}

/// Extend `range` so that deleting it also removes its line when nothing else is on it,
/// or else the whitespace before it.
pub(crate) fn deletion_range(text: &str, range: TextRange) -> TextRange {
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..start];
    let rest = &text[end..];
    let line_end = rest.find('\n').map_or(text.len(), |i| end + i);
    let after = &text[end..line_end];
    if before.trim().is_empty() && after.trim().is_empty() {
        let stop = if line_end < text.len() { line_end + 1 } else { line_end };
        return TextRange::new(TextSize::from(line_start as u32), TextSize::from(stop as u32));
    }
    let ws = before.len() - before.trim_end().len();
    TextRange::new(TextSize::from((start - ws) as u32), range.end())
}

/// A value for a written type, for quick fixes that need one (`0`, `""`, `null`,
/// `emptyList()`, …); `TODO()` when there is no obvious one.
pub(crate) fn default_value(ty: &str) -> String {
    let ty = ty.trim();
    if ty.ends_with('?') {
        return "null".into();
    }
    let base = ty.split('<').next().unwrap_or(ty).trim_start_matches("kotlin.").trim_start_matches("collections.");
    match base {
        "Int" | "Short" | "Byte" => "0",
        "Long" => "0L",
        "Double" => "0.0",
        "Float" => "0f",
        "UInt" | "UShort" | "UByte" => "0u",
        "ULong" => "0uL",
        "Boolean" => "false",
        "Char" => "' '",
        "String" | "CharSequence" => "\"\"",
        "Unit" => "Unit",
        "List" | "Collection" | "Iterable" => "emptyList()",
        "MutableList" | "ArrayList" => "mutableListOf()",
        "Set" => "emptySet()",
        "MutableSet" | "HashSet" | "LinkedHashSet" => "mutableSetOf()",
        "Map" => "emptyMap()",
        "MutableMap" | "HashMap" | "LinkedHashMap" => "mutableMapOf()",
        "Sequence" => "emptySequence()",
        "Array" => "emptyArray()",
        _ => "TODO()",
    }
    .into()
}

/// The indentation of the line containing `offset`.
pub(crate) fn indent_at(text: &str, offset: TextSize) -> String {
    let o = usize::from(offset);
    let line_start = text[..o].rfind('\n').map_or(0, |i| i + 1);
    text[line_start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// The initializer expression of a property (`= e`).
pub(crate) fn initializer(n: &SyntaxNode) -> Option<SyntaxNode> {
    let eq = ast::token(n, SyntaxKind::EQ)?;
    eq.siblings_with_tokens(rowan::Direction::Next).find_map(|e| e.into_node())
}

/// The only statement of a block.
pub(crate) fn only_statement(b: &SyntaxNode) -> Option<SyntaxNode> {
    let mut stmts = b.children();
    match (stmts.next(), stmts.next()) {
        (Some(s), None) => Some(s),
        _ => None,
    }
}

/// Declarations whose parent is a block are local.
pub(crate) fn is_local(n: &SyntaxNode) -> bool {
    n.parent().is_some_and(|p| p.kind() == SyntaxKind::BLOCK)
}

/// The class or object a member is declared in.
pub(crate) fn owner(n: &SyntaxNode) -> Option<SyntaxNode> {
    let body = n.parent().filter(|p| p.kind() == SyntaxKind::CLASS_BODY)?;
    body.parent().filter(|p| matches!(p.kind(), SyntaxKind::CLASS | SyntaxKind::OBJECT_DECLARATION | SyntaxKind::OBJECT_LITERAL))
}

pub(crate) fn is_interface(n: &SyntaxNode) -> bool {
    n.kind() == SyntaxKind::CLASS && ast::token(n, SyntaxKind::INTERFACE_KW).is_some()
}

#[cfg(test)]
mod tests;
