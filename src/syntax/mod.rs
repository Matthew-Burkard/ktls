//! Kotlin syntax: lexer, error tolerant parser and typed AST accessors.

pub mod ast;
mod grammar;
pub mod kind;
pub mod lexer;
mod parser;

use std::sync::Arc;

pub use kind::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
pub use parser::SyntaxError;
use rowan::GreenNode;
pub use rowan::{TextRange, TextSize};

/// Result of parsing one file. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Parse {
    green: GreenNode,
    errors: Arc<[SyntaxError]>,
}

impl Parse {
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn file(&self) -> ast::KtFile {
        ast::KtFile { syntax: self.syntax() }
    }

    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }

    /// Indented tree dump, for tests and debugging.
    pub fn debug_tree(&self) -> String {
        debug_tree(&self.syntax())
    }
}

pub fn debug_tree(node: &SyntaxNode) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut indent = 0usize;
    for event in node.preorder_with_tokens() {
        match event {
            rowan::WalkEvent::Enter(el) => {
                match &el {
                    rowan::NodeOrToken::Node(n) => {
                        writeln!(out, "{:indent$}{:?}@{:?}", "", n.kind(), n.text_range(), indent = indent).unwrap();
                    }
                    rowan::NodeOrToken::Token(t) => {
                        if !t.kind().is_trivia() {
                            writeln!(out, "{:indent$}{:?} {:?}", "", t.kind(), t.text(), indent = indent).unwrap();
                        }
                    }
                }
                indent += 2;
            }
            rowan::WalkEvent::Leave(_) => indent -= 2,
        }
    }
    out
}

/// The smallest element covering `range`, or `None` if `range` is outside `root` (e.g. a
/// range from another file, or from a Java file whose Kotlin tree is empty).
pub fn covering(root: &SyntaxNode, range: TextRange) -> Option<SyntaxElement> {
    root.text_range().contains_range(range).then(|| root.covering_element(range))
}

/// Parse a `.kt` file (`script = false`) or a `.kts` script.
pub fn parse(text: &str, script: bool) -> Parse {
    let tokens = lexer::lex(text);
    let mut p = parser::Parser::new(text, &tokens);
    grammar::kotlin_file(&mut p, script);
    let events = p.finish();
    let (green, errors) = parser::build_tree(text, &tokens, events);
    Parse { green, errors: errors.into() }
}

#[cfg(test)]
mod tests;
