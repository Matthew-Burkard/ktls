//! Conversions from analysis types to LSP types.

use crate::analysis::diagnostics::{Diagnostic, Severity};
use crate::server::line_index::{Encoding, LineIndex};
use crate::symbols::{Modifiers, SymbolKind};

pub fn diagnostic(text: &str, index: &LineIndex, d: &Diagnostic, enc: Encoding) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: index.range(text, d.range, enc),
        severity: Some(match d.severity {
            Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
            Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
            Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
            Severity::Hint => lsp_types::DiagnosticSeverity::HINT,
        }),
        code: Some(lsp_types::NumberOrString::String(d.code.to_string())),
        code_description: None,
        source: Some("kotlin".into()),
        message: d.message.clone(),
        related_information: None,
        tags: if d.code == "DEPRECATION" {
            Some(vec![lsp_types::DiagnosticTag::DEPRECATED])
        } else {
            d.unnecessary.then(|| vec![lsp_types::DiagnosticTag::UNNECESSARY])
        },
        data: None,
    }
}

pub fn symbol_kind(kind: SymbolKind, modifiers: Modifiers, member: bool) -> lsp_types::SymbolKind {
    use lsp_types::SymbolKind as K;
    match kind {
        SymbolKind::Class => K::CLASS,
        SymbolKind::Interface | SymbolKind::Annotation => K::INTERFACE,
        SymbolKind::Object => K::OBJECT,
        SymbolKind::Enum => K::ENUM,
        SymbolKind::EnumEntry => K::ENUM_MEMBER,
        SymbolKind::TypeAlias => K::TYPE_PARAMETER,
        SymbolKind::Function if member => K::METHOD,
        SymbolKind::Function => K::FUNCTION,
        SymbolKind::Constructor => K::CONSTRUCTOR,
        SymbolKind::Property if modifiers.has(Modifiers::CONST) => K::CONSTANT,
        SymbolKind::Property if member => K::PROPERTY,
        SymbolKind::Property => K::VARIABLE,
        SymbolKind::Field => K::FIELD,
        SymbolKind::TypeParameter => K::TYPE_PARAMETER,
        SymbolKind::Package => K::PACKAGE,
    }
}
