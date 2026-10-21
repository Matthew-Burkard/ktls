//! Syntax-only structure: folding ranges and selection ranges.

use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldKind {
    Imports,
    Comment,
    Region,
    Code,
}

/// A foldable span from `start` to `end`. `keep_end_line`: the last line (with the
/// closing brace) stays visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub start: TextSize,
    pub end: TextSize,
    pub kind: FoldKind,
    pub keep_end_line: bool,
}

const BRACED: &[SyntaxKind] = &[SyntaxKind::BLOCK, SyntaxKind::CLASS_BODY, SyntaxKind::WHEN_EXPR, SyntaxKind::LAMBDA_EXPR];
const LISTS: &[SyntaxKind] = &[SyntaxKind::VALUE_ARGUMENT_LIST, SyntaxKind::VALUE_PARAMETER_LIST, SyntaxKind::STRING_TEMPLATE];

fn multiline(text: &str, r: TextRange) -> bool {
    text[usize::from(r.start())..usize::from(r.end())].contains('\n')
}

pub fn folding_ranges(root: &SyntaxNode, text: &str) -> Vec<Fold> {
    let mut out = Vec::new();
    for n in root.descendants() {
        let k = n.kind();
        if k == SyntaxKind::IMPORT_LIST {
            let imports: Vec<SyntaxNode> = n.children().filter(|c| c.kind() == SyntaxKind::IMPORT_DIRECTIVE).collect();
            if let (Some(first), Some(last)) = (imports.first(), imports.last())
                && imports.len() > 1
            {
                // From the end of `import` so that the first line stays readable.
                let start = first.first_token().map_or(first.text_range().start(), |t| t.text_range().end());
                out.push(Fold { start, end: last.text_range().end(), kind: FoldKind::Imports, keep_end_line: false });
            }
        } else if BRACED.contains(&k) || LISTS.contains(&k) {
            let (Some(open), Some(close)) = (n.first_token(), n.last_token()) else { continue };
            if !matches!(close.kind(), SyntaxKind::R_CURLY | SyntaxKind::R_PAREN | SyntaxKind::STRING_CLOSE) || !multiline(text, n.text_range()) {
                continue;
            }
            // A lambda's or `when`'s braces: fold from the `{`.
            let open = if matches!(open.kind(), SyntaxKind::L_CURLY | SyntaxKind::L_PAREN | SyntaxKind::STRING_OPEN) {
                open
            } else {
                match n.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == SyntaxKind::L_CURLY) {
                    Some(t) => t,
                    None => continue,
                }
            };
            out.push(Fold { start: open.text_range().end(), end: close.text_range().start(), kind: FoldKind::Code, keep_end_line: close.kind() != SyntaxKind::STRING_CLOSE });
        }
    }
    comments(root, text, &mut out);
    out.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
    out
}

fn comments(root: &SyntaxNode, text: &str, out: &mut Vec<Fold>) {
    let mut regions: Vec<TextSize> = Vec::new();
    // A run of `//` comments on consecutive lines.
    let mut run: Option<(TextSize, TextSize, usize)> = None;
    let flush = |run: &mut Option<(TextSize, TextSize, usize)>, out: &mut Vec<Fold>| {
        if let Some((s, e, n)) = run.take()
            && n > 1
        {
            out.push(Fold { start: s, end: e, kind: FoldKind::Comment, keep_end_line: false });
        }
    };
    // Newlines since the last line comment.
    let mut newlines = 0;
    for t in root.descendants_with_tokens().filter_map(|el| el.into_token()) {
        match t.kind() {
            SyntaxKind::BLOCK_COMMENT | SyntaxKind::KDOC if multiline(text, t.text_range()) => {
                flush(&mut run, out);
                out.push(Fold { start: t.text_range().start(), end: t.text_range().end(), kind: FoldKind::Comment, keep_end_line: false });
            }
            SyntaxKind::LINE_COMMENT => {
                let body = t.text().trim_start_matches('/').trim();
                if body.starts_with("region") || body.starts_with("<editor-fold") {
                    flush(&mut run, out);
                    regions.push(t.text_range().start());
                } else if body.starts_with("endregion") || body.starts_with("</editor-fold") {
                    flush(&mut run, out);
                    if let Some(s) = regions.pop() {
                        out.push(Fold { start: s, end: t.text_range().end(), kind: FoldKind::Region, keep_end_line: false });
                    }
                } else {
                    match &mut run {
                        Some((_, e, n)) if newlines == 1 => {
                            *e = t.text_range().end();
                            *n += 1;
                        }
                        _ => {
                            flush(&mut run, out);
                            run = Some((t.text_range().start(), t.text_range().end(), 1));
                        }
                    }
                }
                newlines = 0;
            }
            SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE => newlines += t.text().matches('\n').count(),
            _ => {
                flush(&mut run, out);
                newlines = 2;
            }
        }
    }
    flush(&mut run, out);
}

/// Nested ranges around `offset` for expand/shrink selection, innermost first.
pub fn selection_ranges(root: &SyntaxNode, text: &str, offset: TextSize) -> Vec<TextRange> {
    let mut out: Vec<TextRange> = Vec::new();
    let tok = root.token_at_offset(offset).max_by_key(|t| match t.kind() {
        SyntaxKind::IDENT => 3,
        k if k.is_trivia() => 0,
        _ => 1,
    });
    let Some(tok) = tok else { return out };
    if !tok.kind().is_trivia() || matches!(tok.kind(), SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT | SyntaxKind::KDOC) {
        out.push(tok.text_range());
    }
    for n in tok.parent_ancestors() {
        // Inside braces/parentheses first, then the whole node.
        if let (Some(open), Some(close)) = (n.first_token(), n.last_token())
            && matches!((open.kind(), close.kind()), (SyntaxKind::L_CURLY, SyntaxKind::R_CURLY) | (SyntaxKind::L_PAREN, SyntaxKind::R_PAREN) | (SyntaxKind::STRING_OPEN, SyntaxKind::STRING_CLOSE))
            && open.text_range().end() < close.text_range().start()
        {
            push(&mut out, inner_trimmed(text, open.text_range().end(), close.text_range().start()));
        }
        push(&mut out, n.text_range());
    }
    out
}

fn push(out: &mut Vec<TextRange>, r: TextRange) {
    if out.last().is_none_or(|l| *l != r && r.contains_range(*l)) {
        out.push(r);
    }
}

/// The range between `start` and `end` without surrounding whitespace.
fn inner_trimmed(text: &str, start: TextSize, end: TextSize) -> TextRange {
    let inner = &text[usize::from(start)..usize::from(end)];
    let lead = inner.len() - inner.trim_start().len();
    let trail = inner.len() - inner.trim_end().len();
    if lead + trail >= inner.len() {
        return TextRange::new(start, end);
    }
    TextRange::new(start + TextSize::from(lead as u32), end - TextSize::from(trail as u32))
}
