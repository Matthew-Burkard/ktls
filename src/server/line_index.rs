//! Conversion between byte offsets and LSP positions (UTF-8 or UTF-16 columns).

use lsp_types::{Position, Range};

use crate::syntax::{TextRange, TextSize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

#[derive(Clone, Debug)]
pub struct LineIndex {
    /// Byte offset of the start of every line.
    starts: Vec<u32>,
    len: u32,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut starts = vec![0];
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => starts.push(i as u32 + 1),
                b'\r' if bytes.get(i + 1) != Some(&b'\n') => starts.push(i as u32 + 1),
                _ => {}
            }
            i += 1;
        }
        LineIndex { starts, len: text.len() as u32 }
    }

    pub fn position(&self, text: &str, offset: TextSize, enc: Encoding) -> Position {
        let offset = u32::from(offset).min(self.len);
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        let start = self.starts[line] as usize;
        let mut end = offset as usize;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let col = match enc {
            Encoding::Utf8 => end - start,
            Encoding::Utf16 => text[start..end].encode_utf16().count(),
        };
        Position { line: line as u32, character: col as u32 }
    }

    pub fn range(&self, text: &str, range: TextRange, enc: Encoding) -> Range {
        Range { start: self.position(text, range.start(), enc), end: self.position(text, range.end(), enc) }
    }

    /// Byte offset of an LSP position (clamped to the line / document).
    pub fn offset(&self, text: &str, pos: Position, enc: Encoding) -> TextSize {
        let Some(&line_start) = self.starts.get(pos.line as usize) else { return TextSize::from(self.len) };
        let line_end = self.starts.get(pos.line as usize + 1).map_or(self.len, |&e| e) as usize;
        // Columns past the end of the line clamp to the line's length (terminator excluded).
        let line = text[line_start as usize..line_end].trim_end_matches(['\n', '\r']);
        let col = match enc {
            Encoding::Utf8 => {
                let mut c = (pos.character as usize).min(line.len());
                while !line.is_char_boundary(c) {
                    c -= 1;
                }
                c
            }
            Encoding::Utf16 => {
                let mut units = 0usize;
                let mut bytes = 0usize;
                for ch in line.chars() {
                    if units >= pos.character as usize {
                        break;
                    }
                    units += ch.len_utf16();
                    bytes += ch.len_utf8();
                }
                bytes
            }
        };
        TextSize::from(line_start + col as u32)
    }
}

/// Apply LSP content changes to `text`.
pub fn apply_changes(text: &mut String, changes: Vec<lsp_types::TextDocumentContentChangeEvent>, enc: Encoding) {
    for change in changes {
        match change.range {
            None => *text = change.text,
            Some(range) => {
                let idx = LineIndex::new(text);
                let start = usize::from(idx.offset(text, range.start, enc));
                let end = usize::from(idx.offset(text, range.end, enc)).max(start);
                text.replace_range(start..end, &change.text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_positions() {
        let text = "a😀b\nxyz\r\nlast";
        let idx = LineIndex::new(text);
        // 'b' is at byte 5, UTF-16 column 3
        assert_eq!(idx.position(text, TextSize::from(5), Encoding::Utf16), Position::new(0, 3));
        assert_eq!(idx.position(text, TextSize::from(5), Encoding::Utf8), Position::new(0, 5));
        assert_eq!(idx.offset(text, Position::new(0, 3), Encoding::Utf16), TextSize::from(5));
        assert_eq!(idx.offset(text, Position::new(2, 2), Encoding::Utf16), TextSize::from(14));
        assert_eq!(idx.offset(text, Position::new(1, 99), Encoding::Utf16), TextSize::from(10));
        assert_eq!(idx.offset(text, Position::new(9, 0), Encoding::Utf16), TextSize::from(16));
    }

    #[test]
    fn incremental_edits() {
        let mut text = "fun main() {\n    println(1)\n}\n".to_string();
        apply_changes(
            &mut text,
            vec![lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 12), Position::new(1, 13))),
                range_length: None,
                text: "\"hi\"".into(),
            }],
            Encoding::Utf16,
        );
        assert_eq!(text, "fun main() {\n    println(\"hi\")\n}\n");
    }
}
