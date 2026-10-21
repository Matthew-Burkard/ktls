//! Formatting: indentation and trailing whitespace only (spacing inside lines is kept).
//!
//! A line is indented one unit deeper than the line that opened the innermost bracket still
//! open. A line that continues an expression (`.call()`, `?: x`, `&& y`, after `=`, an
//! operator or a `when` arrow), the brace-less body of `if`/`else`/`for`/`while`, a
//! property accessor and a supertype list on its own line get one unit more than the line
//! where that expression or declaration starts. Lines starting with a closing bracket align
//! with their opener's line; a block opened after closing a bracket (`if (a &&\n b) {`)
//! is indented from the line that opened that bracket. The inside of multi-line strings is
//! never touched; the lines of a block comment move with its first line.

use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize};

/// Edits (whitespace only) formatting `text`; only lines starting in `range` if given.
pub fn format(root: &SyntaxNode, text: &str, unit: &str, range: Option<TextRange>) -> Vec<(TextRange, String)> {
    let mut edits = Vec::new();
    let mut line_starts = vec![0usize];
    line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let line_of = |offset: usize| line_starts.partition_point(|&s| s <= offset) - 1;
    let tokens: Vec<SyntaxToken> = root.descendants_with_tokens().filter_map(|t| t.into_token()).collect();
    // Levels of the lines (or block bases) that opened the brackets still open.
    let mut stack: Vec<usize> = Vec::new();
    let mut levels: Vec<usize> = Vec::new();
    let mut block_base = 0usize;
    let mut next = 0;
    let mut prev_code: Option<SyntaxToken> = None;
    for (li, &start) in line_starts.iter().enumerate() {
        let end = line_starts.get(li + 1).map_or(text.len(), |n| n - 1);
        let line = text[start..end].strip_suffix('\r').unwrap_or(&text[start..end]);
        let content_end = start + line.len();
        // Feed the tokens of the previous lines into the bracket stack.
        while next < tokens.len() && usize::from(tokens[next].text_range().start()) < start {
            let t = &tokens[next];
            match t.kind() {
                SyntaxKind::L_CURLY | SyntaxKind::L_PAREN | SyntaxKind::L_BRACK => stack.push(block_base),
                SyntaxKind::R_CURLY | SyntaxKind::R_PAREN | SyntaxKind::R_BRACK => {
                    if let Some(v) = stack.pop() {
                        block_base = block_base.min(v);
                    }
                }
                _ => {}
            }
            if !t.kind().is_trivia() {
                prev_code = Some(t.clone());
            }
            next += 1;
        }
        let wanted = range.is_none_or(|r| r.start() <= TextSize::from(start as u32) && TextSize::from(start as u32) <= r.end());
        let previous_level = levels.last().copied().unwrap_or(0);
        // A line inside a token that started on an earlier line: a string or a comment.
        let at = TextSize::from(start as u32);
        let containing = (start > 0).then(|| root.token_at_offset(at).find(|t| t.text_range().start() < at && t.text_range().end() > at)).flatten();
        if let Some(t) = containing {
            levels.push(previous_level);
            if wanted && matches!(t.kind(), SyntaxKind::KDOC | SyntaxKind::BLOCK_COMMENT) {
                // The comment moves with its first line, keeping each line's offset.
                let opener = usize::from(t.text_range().start());
                let ol = line_of(opener);
                let opener_ws = text[line_starts[ol]..].len() - text[line_starts[ol]..].trim_start().len();
                let starts_line = text[line_starts[ol]..opener].trim().is_empty();
                let trimmed = line.trim_start();
                let ws = line.len() - trimmed.len();
                if starts_line && !trimmed.is_empty() && !line[..ws].contains('\t') && !unit.contains('\t') {
                    let new_opener = unit.len() * levels[ol];
                    let width = (ws + new_opener).checked_sub(opener_ws);
                    if let Some(width) = width
                        && width != ws
                    {
                        edits.push((TextRange::new(at, TextSize::from((start + ws) as u32)), " ".repeat(width)));
                    }
                }
                trailing(line, start, &mut edits);
            }
            continue;
        }
        let trimmed = line.trim_start();
        let ws = line.len() - trimmed.len();
        if trimmed.is_empty() {
            levels.push(previous_level);
            if wanted && !line.is_empty() {
                edits.push((TextRange::new(at, TextSize::from(content_end as u32)), String::new()));
            }
            continue;
        }
        let Some(first) = root.token_at_offset(TextSize::from((start + ws) as u32)).right_biased() else {
            levels.push(previous_level);
            continue;
        };
        let (level, base) = level_of(&first, prev_code.as_ref(), &stack, |o| {
            let l = line_of(usize::from(o));
            (l < li).then(|| levels[l])
        });
        levels.push(level);
        block_base = base;
        if !wanted {
            continue;
        }
        let indent = unit.repeat(level);
        if line[..ws] != indent {
            edits.push((TextRange::new(at, TextSize::from((start + ws) as u32)), indent));
        }
        // Trailing whitespace, unless the line ends inside a string.
        let end_at = TextSize::from(content_end as u32);
        let in_string = root.token_at_offset(end_at).any(|t| is_string(&t) && t.text_range().end() > end_at);
        if !in_string {
            trailing(line, start, &mut edits);
        }
    }
    if range.is_none() && !text.is_empty() && !text.ends_with('\n') {
        edits.push((TextRange::empty(TextSize::from(text.len() as u32)), "\n".into()));
    }
    edits
}

fn trailing(line: &str, start: usize, edits: &mut Vec<(TextRange, String)>) {
    let kept = line.trim_end_matches([' ', '\t']).len();
    if kept < line.len() && kept > 0 {
        edits.push((TextRange::new(TextSize::from((start + kept) as u32), TextSize::from((start + line.len()) as u32)), String::new()));
    }
}

fn is_string(t: &SyntaxToken) -> bool {
    matches!(t.kind(), SyntaxKind::STRING_TEXT | SyntaxKind::STRING_OPEN | SyntaxKind::STRING_CLOSE | SyntaxKind::STRING_ESCAPE)
}

fn is_operator(k: SyntaxKind) -> bool {
    use SyntaxKind::*;
    matches!(k, PLUS | MINUS | STAR | SLASH | PERCENT | AMP2 | PIPE2 | ELVIS | EQ2 | NOT_EQ | EQ3 | NOT_EQ2 | LT | GT | LT_EQ | GT_EQ | RANGE | RANGE_UNTIL | IDENT | IN_KW | NOT_IN_KW | IS_KW | NOT_IS_KW | AS_KW | AS_SAFE_KW)
}

/// The level of a line starting with `first`, and the base for blocks it opens.
/// `level_at(offset)`: the level of an earlier line containing `offset`.
fn level_of(first: &SyntaxToken, prev: Option<&SyntaxToken>, stack: &[usize], level_at: impl Fn(TextSize) -> Option<usize>) -> (usize, usize) {
    use SyntaxKind::*;
    if matches!(first.kind(), R_CURLY | R_PAREN | R_BRACK) {
        let l = stack.last().copied().unwrap_or(0);
        return (l, l);
    }
    let base = stack.last().map_or(0, |l| l + 1);
    let parent = |t: &SyntaxToken| t.parent();
    let parent_kind = |t: &SyntaxToken| t.parent().map(|p| p.kind());
    // `else` of an `if` used as a value continues it; of an `if` starting its line, aligns.
    if first.kind() == ELSE_KW
        && let Some(if_expr) = parent(first).filter(|p| p.kind() == IF_EXPR)
        && let Some(if_level) = level_at(if_expr.text_range().start())
    {
        let starts_line = if_expr.first_token().and_then(|t| t.prev_token()).is_none_or(|p| p.kind() == NEWLINE || p.kind() == WHITESPACE && p.prev_token().is_none_or(|q| q.kind() == NEWLINE));
        let level = if starts_line { if_level } else { if_level + 1 };
        let level = level.max(stack.last().map_or(0, |l| l + 1));
        return (level, level);
    }
    // A supertype list's `:` belongs to the class.
    let header_start = |t: &SyntaxToken| match parent_kind(t) {
        Some(SUPER_TYPE_LIST) => parent(t).and_then(|l| l.parent()).map(|c| c.text_range().start()),
        Some(CLASS | FUN | OBJECT_DECLARATION | SECONDARY_CONSTRUCTOR) => parent(t).map(|c| c.text_range().start()),
        _ => None,
    };
    // Where the continued construct starts, and whether it is a declaration header.
    let mut header = false;
    let anchor: Option<TextSize> = match first.kind() {
        DOT | SAFE_DOT | COLON2 => parent(first).map(|p| p.text_range().start()),
        // `?.` is lexed as `?` `.`.
        QUESTION if parent_kind(first) == Some(SAFE_QUALIFIED_EXPR) => parent(first).map(|p| p.text_range().start()),
        k if is_operator(k) && parent_kind(first) == Some(OPERATION_REFERENCE) => parent(first).and_then(|o| o.parent()).map(|b| b.text_range().start()),
        COLON if header_start(first).is_some() => {
            header = true;
            header_start(first)
        }
        _ => None,
    }
    .or_else(|| {
        let p = prev?;
        match p.kind() {
            _ if first.kind() == L_CURLY => None,
            EQ if matches!(parent_kind(p), Some(PROPERTY | FUN | BINARY_EXPR | VALUE_PARAMETER | TYPEALIAS | PROPERTY_ACCESSOR)) => parent(p).map(|n| n.text_range().start()),
            ARROW if parent_kind(p) == Some(WHEN_ENTRY) => parent(p).map(|n| n.text_range().start()),
            DOT | SAFE_DOT => parent(p).map(|n| n.text_range().start()),
            COLON if header_start(p).is_some() => {
                header = true;
                header_start(p)
            }
            k if is_operator(k) && parent_kind(p) == Some(OPERATION_REFERENCE) => parent(p).and_then(|o| o.parent()).map(|b| b.text_range().start()),
            _ => None,
        }
    })
    .or_else(|| {
        // The first token of a brace-less branch or loop body, or of a property accessor.
        first.parent_ancestors().take_while(|a| a.text_range().start() == first.text_range().start()).find_map(|a| match a.kind() {
            THEN_BRANCH if a.first_child().is_some_and(|c| c.kind() != BLOCK) => a.parent().map(|i| i.text_range().start()),
            ELSE_BRANCH if a.first_child().is_some_and(|c| c.kind() != BLOCK) => a.prev_sibling_or_token().and_then(|_| a.parent()).and_then(|i| i.children_with_tokens().filter_map(|t| t.into_token()).find(|t| t.kind() == ELSE_KW)).map(|t| t.text_range().start()),
            BODY if a.parent().is_some_and(|p| matches!(p.kind(), FOR_EXPR | WHILE_EXPR | DO_WHILE_EXPR)) && a.first_child().is_some_and(|c| c.kind() != BLOCK) => a.parent().map(|p| p.text_range().start()),
            PROPERTY_ACCESSOR => a.parent().map(|p| p.text_range().start()),
            _ => None,
        })
    });
    let level = match anchor.and_then(&level_at) {
        Some(anchor_level) => base.max(anchor_level + 1),
        None => base,
    };
    (level, if header { level.saturating_sub(1) } else { level })
}
