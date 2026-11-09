//! Event based, error tolerant parser infrastructure (in the style of rust-analyzer).
//!
//! The grammar functions in [`super::grammar`] drive a [`Parser`] that only sees
//! non-trivia tokens. The parser emits a flat list of [`Event`]s which
//! [`build_tree`] turns into a lossless rowan tree, re-inserting trivia.

use std::cell::Cell;

use rowan::{GreenNode, GreenNodeBuilder, TextRange, TextSize};

use super::kind::SyntaxKind::{self, *};
use super::lexer::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ParseToken<'t> {
    pub kind: SyntaxKind,
    pub text: &'t str,
    /// A newline (or comment containing one) precedes this token.
    pub nl_before: bool,
    /// No trivia at all between this and the previous token.
    pub joint: bool,
}

#[derive(Debug)]
pub(crate) enum Event {
    Start { kind: SyntaxKind, forward_parent: Option<u32> },
    Finish,
    Token,
    Error { message: String, at_prev_end: bool },
    Tombstone,
}

pub(crate) struct Parser<'t> {
    tokens: Vec<ParseToken<'t>>,
    pos: usize,
    events: Vec<Event>,
    steps: Cell<u32>,
    /// When set, `{` after an expression is not parsed as a trailing lambda
    /// (used for `class A : I by impl { ... }`, `when (x) {`, etc).
    pub(crate) no_trailing_lambda: bool,
    /// Whether newlines are significant: off inside `(...)`/`[...]`, on again inside
    /// `{...}` bodies (mirrors the Kotlin compiler's newline handling).
    pub(crate) newlines: Vec<bool>,
    /// Markers started but not yet completed or abandoned: the depth of the node
    /// under construction.
    open: u32,
    /// Links of the left-recursive chains (`a.b.c`, `1 + 2 + 3`) being parsed; each
    /// link nests the tree one level deeper without leaving a marker open.
    chain: u32,
}

/// Deepest tree the parser builds. Anything nested deeper becomes one ERROR node, so
/// that the parser and every recursive pass over the tree run in bounded stack space.
/// Real code stays far below this (deep Compose UIs reach a few hundred).
pub(crate) const MAX_DEPTH: u32 = 1_500;

/// Lookahead calls without consuming a token before the parser is considered stuck in
/// a loop. Unwinding a tree [`MAX_DEPTH`] deep at end of input legitimately peeks a
/// few times per level.
const STEP_LIMIT: u32 = 10_000 + 16 * MAX_DEPTH;

impl<'t> Parser<'t> {
    pub(crate) fn new(text: &'t str, raw: &[Token]) -> Self {
        let mut tokens = Vec::with_capacity(raw.len() / 2);
        let mut offset = 0u32;
        let mut nl = false;
        let mut joint = true;
        for t in raw {
            let s = &text[offset as usize..(offset + t.len) as usize];
            if t.kind.is_trivia() {
                if t.kind == NEWLINE || (t.kind != WHITESPACE && s.contains('\n')) {
                    nl = true;
                }
                joint = false;
            } else {
                tokens.push(ParseToken { kind: t.kind, text: s, nl_before: nl, joint });
                nl = false;
                joint = true;
            }
            offset += t.len;
        }
        Parser { tokens, pos: 0, events: Vec::new(), steps: Cell::new(0), no_trailing_lambda: false, newlines: Vec::new(), open: 0, chain: 0 }
    }

    pub(crate) fn finish(self) -> Vec<Event> {
        self.events
    }

    fn tok(&self, n: usize) -> Option<&ParseToken<'t>> {
        self.tokens.get(self.pos + n)
    }

    pub(crate) fn nth(&self, n: usize) -> SyntaxKind {
        let steps = self.steps.get();
        assert!(steps < STEP_LIMIT, "parser seems stuck at token {}", self.pos);
        self.steps.set(steps + 1);
        self.tok(n).map_or(EOF, |t| t.kind)
    }

    pub(crate) fn current(&self) -> SyntaxKind {
        self.nth(0)
    }

    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    pub(crate) fn nth_at(&self, n: usize, kind: SyntaxKind) -> bool {
        self.nth(n) == kind
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.at(EOF)
    }

    pub(crate) fn nth_text(&self, n: usize) -> &'t str {
        self.tok(n).map_or("", |t| t.text)
    }

    pub(crate) fn current_text(&self) -> &'t str {
        self.nth_text(0)
    }

    /// Current token is the soft keyword `kw` (lexed as an identifier).
    pub(crate) fn at_soft(&self, kw: &str) -> bool {
        self.at(IDENT) && self.current_text() == kw
    }

    pub(crate) fn nth_at_soft(&self, n: usize, kw: &str) -> bool {
        self.nth_at(n, IDENT) && self.nth_text(n) == kw
    }

    /// A newline precedes the n-th token.
    pub(crate) fn nth_nl_before(&self, n: usize) -> bool {
        self.tok(n).is_some_and(|t| t.nl_before)
    }

    pub(crate) fn nl_before(&self) -> bool {
        self.newlines.last().copied().unwrap_or(true) && self.nth_nl_before(0)
    }

    /// Run `f` with newline significance set to `significant`.
    pub(crate) fn with_newlines<R>(&mut self, significant: bool, f: impl FnOnce(&mut Self) -> R) -> R {
        self.newlines.push(significant);
        let r = f(self);
        self.newlines.pop();
        r
    }

    /// The n-th token directly follows the previous one with no trivia in between.
    pub(crate) fn nth_joint(&self, n: usize) -> bool {
        self.tok(n).is_some_and(|t| t.joint)
    }

    pub(crate) fn bump(&mut self) {
        if self.at_eof() {
            return;
        }
        self.pos += 1;
        self.steps.set(0);
        self.events.push(Event::Token);
    }

    pub(crate) fn bump_if(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.bump_if(kind) {
            return true;
        }
        self.error_expecting(kind.describe());
        false
    }

    pub(crate) fn error_expecting(&mut self, what: &str) {
        self.events.push(Event::Error { message: format!("Expecting {what}"), at_prev_end: true });
    }

    /// Error located at the current token.
    pub(crate) fn error(&mut self, message: impl Into<String>) {
        self.events.push(Event::Error { message: message.into(), at_prev_end: false });
    }

    /// Wrap the current token in an ERROR node with `message`.
    pub(crate) fn err_and_bump(&mut self, message: impl Into<String>) {
        if self.at_eof() {
            self.error(message);
            return;
        }
        let m = self.start();
        self.error(message);
        self.bump();
        m.complete(self, ERROR);
    }

    /// Skip tokens until one in `recovery` (or EOF) and wrap them in an ERROR node.
    pub(crate) fn err_recover_until(&mut self, message: impl Into<String>, stop: impl Fn(&Parser) -> bool) -> Option<CompletedMarker> {
        if self.at_eof() || stop(self) {
            self.error(message);
            return None;
        }
        let m = self.start();
        self.error(message);
        let mut depth = 0i32;
        // Always make progress, even if the first token is a stray closing bracket.
        match self.current() {
            L_CURLY | L_PAREN | L_BRACK => depth += 1,
            _ => {}
        }
        self.bump();
        while !self.at_eof() {
            if depth == 0 && stop(self) {
                break;
            }
            match self.current() {
                L_CURLY | L_PAREN | L_BRACK => depth += 1,
                R_CURLY | R_PAREN | R_BRACK => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            self.bump();
        }
        Some(m.complete(self, ERROR))
    }

    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len() as u32;
        self.events.push(Event::Tombstone);
        self.open += 1;
        Marker { pos, completed: false }
    }

    /// The tree is as deep as [`MAX_DEPTH`] allows: callers about to nest further
    /// should [`skip_too_deep`](Self::skip_too_deep) instead.
    pub(crate) fn too_deep(&self) -> bool {
        self.open + self.chain >= MAX_DEPTH
    }

    /// Wrap the current token (a whole bracketed group if it opens one) in a single
    /// ERROR node, without descending into it.
    pub(crate) fn skip_too_deep(&mut self) -> Option<CompletedMarker> {
        const MESSAGE: &str = "Code is nested too deeply to be analysed";
        if self.at_eof() {
            self.error(MESSAGE);
            return None;
        }
        let m = self.start();
        self.error(MESSAGE);
        let mut depth = 0u32;
        loop {
            match self.current() {
                L_CURLY | L_PAREN | L_BRACK => depth += 1,
                R_CURLY | R_PAREN | R_BRACK => depth = depth.saturating_sub(1),
                EOF => break,
                _ => {}
            }
            self.bump();
            if depth == 0 {
                break;
            }
        }
        Some(m.complete(self, ERROR))
    }

    /// Note one more link of a left-recursive chain (see [`Parser::chain`]).
    pub(crate) fn enter_link(&mut self) {
        self.chain += 1;
    }

    /// The chain whose links were noted with [`enter_link`](Self::enter_link) ended.
    pub(crate) fn leave_links(&mut self, links: u32) {
        self.chain -= links;
    }

    /// Stop extending a chain that reached [`MAX_DEPTH`]: the rest of the expression
    /// (up to the end of the statement or enclosing bracket) becomes one ERROR node.
    pub(crate) fn skip_rest_of_chain(&mut self) {
        let continues = |p: &Parser| matches!(p.current(), DOT | QUESTION | ELVIS | COLON2 | AMP2 | PIPE2);
        self.err_recover_until("Expression is nested too deeply to be analysed", |p| {
            p.at(SEMI) || (p.nl_before() && !continues(p))
        });
    }
}

pub(crate) struct Marker {
    pos: u32,
    completed: bool,
}

impl Marker {
    pub(crate) fn complete(mut self, p: &mut Parser, kind: SyntaxKind) -> CompletedMarker {
        self.completed = true;
        match &mut p.events[self.pos as usize] {
            ev @ Event::Tombstone => *ev = Event::Start { kind, forward_parent: None },
            _ => unreachable!(),
        }
        p.open -= 1;
        p.events.push(Event::Finish);
        CompletedMarker { pos: self.pos }
    }

    pub(crate) fn abandon(mut self, p: &mut Parser) {
        self.completed = true;
        p.open -= 1;
        let idx = self.pos as usize;
        if idx == p.events.len() - 1 {
            p.events.pop();
        }
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            panic!("Marker must be completed or abandoned");
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CompletedMarker {
    pos: u32,
}

impl CompletedMarker {
    /// Create a new node that starts before this one and will become its parent.
    pub(crate) fn precede(self, p: &mut Parser) -> Marker {
        let new = p.start();
        match &mut p.events[self.pos as usize] {
            Event::Start { forward_parent, .. } => *forward_parent = Some(new.pos - self.pos),
            _ => unreachable!(),
        }
        new
    }
}

/// Kinds that own the KDoc / comments immediately preceding them.
fn attaches_leading_docs(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        CLASS | OBJECT_DECLARATION | FUN | PROPERTY | TYPEALIAS | ENUM_ENTRY | SECONDARY_CONSTRUCTOR | COMPANION_OBJECT
    )
}

pub(crate) fn build_tree(text: &str, raw: &[Token], mut events: Vec<Event>) -> (GreenNode, Vec<SyntaxError>) {
    let mut builder = GreenNodeBuilder::new();
    let mut errors = Vec::new();
    let mut raw_pos = 0usize;
    let mut offset = 0u32;
    let mut depth = 0usize;
    // End offset of the last non-trivia token emitted.
    let mut prev_end = 0u32;

    let eat_trivia = |builder: &mut GreenNodeBuilder, raw_pos: &mut usize, offset: &mut u32, limit: usize| {
        while *raw_pos < limit && raw[*raw_pos].kind.is_trivia() {
            let t = raw[*raw_pos];
            builder.token(t.kind.into(), &text[*offset as usize..(*offset + t.len) as usize]);
            *offset += t.len;
            *raw_pos += 1;
        }
    };

    // Number of trailing trivia tokens (from raw_pos) that should go into a doc-owning node.
    let leading_docs = |raw_pos: usize| -> usize {
        let mut end = raw_pos;
        while end < raw.len() && raw[end].kind.is_trivia() {
            end += 1;
        }
        // Walk backwards from the first non-trivia token over whitespace and plain
        // comments; if we reach a KDoc, the node owns everything from it onwards.
        let mut i = end;
        while i > raw_pos {
            match raw[i - 1].kind {
                KDOC => return end - (i - 1),
                WHITESPACE | NEWLINE | LINE_COMMENT | BLOCK_COMMENT => i -= 1,
                _ => break,
            }
        }
        0
    };

    let mut i = 0;
    while i < events.len() {
        match std::mem::replace(&mut events[i], Event::Tombstone) {
            Event::Start { kind, forward_parent } => {
                // Collect the forward-parent chain.
                let mut kinds = vec![kind];
                let mut idx = i;
                let mut fp = forward_parent;
                while let Some(delta) = fp {
                    idx += delta as usize;
                    fp = match std::mem::replace(&mut events[idx], Event::Tombstone) {
                        Event::Start { kind, forward_parent } => {
                            kinds.push(kind);
                            forward_parent
                        }
                        _ => unreachable!(),
                    };
                }
                for kind in kinds.into_iter().rev() {
                    if depth > 0 {
                        let mut trivia_end = raw_pos;
                        while trivia_end < raw.len() && raw[trivia_end].kind.is_trivia() {
                            trivia_end += 1;
                        }
                        let docs = if attaches_leading_docs(kind) { leading_docs(raw_pos) } else { 0 };
                        eat_trivia(&mut builder, &mut raw_pos, &mut offset, trivia_end - docs);
                    }
                    builder.start_node(kind.into());
                    depth += 1;
                }
            }
            Event::Finish => {
                depth -= 1;
                if depth == 0 {
                    eat_trivia(&mut builder, &mut raw_pos, &mut offset, raw.len());
                }
                builder.finish_node();
            }
            Event::Token => {
                eat_trivia(&mut builder, &mut raw_pos, &mut offset, raw.len());
                let t = raw[raw_pos];
                builder.token(t.kind.into(), &text[offset as usize..(offset + t.len) as usize]);
                offset += t.len;
                prev_end = offset;
                raw_pos += 1;
            }
            Event::Error { message, at_prev_end } => {
                let range = if at_prev_end {
                    TextRange::empty(TextSize::from(prev_end))
                } else {
                    // Range of the next non-trivia token.
                    let mut j = raw_pos;
                    let mut o = offset;
                    while j < raw.len() && raw[j].kind.is_trivia() {
                        o += raw[j].len;
                        j += 1;
                    }
                    let len = raw.get(j).map_or(0, |t| t.len);
                    TextRange::at(TextSize::from(o), TextSize::from(len))
                };
                errors.push(SyntaxError { message, range });
            }
            Event::Tombstone => {}
        }
        i += 1;
    }
    (builder.finish(), errors)
}
