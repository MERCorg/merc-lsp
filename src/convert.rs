//! Conversion between the byte-offset based [`merc_syntax::Span`] used throughout `merc_syntax`
//! and the UTF-16-code-unit based [`Position`]/[`Range`] types used by the Language Server
//! Protocol.

use lsp_types::Position;
use lsp_types::Range;
use merc_syntax::Span;

/// Whether `byte` can occur inside an mCRL2 identifier. Used to find word
/// boundaries when narrowing a declaration's span down to just its identifier,
/// and to widen a zero-width parse-error location to a whole token.
pub(crate) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'\''
}

/// A single indexed line of the document, used to convert between byte offsets and UTF-16
/// based [`Position`]s in O(1) for the common (pure-ASCII) case.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// Byte offset of the first byte of this line (relative to the start of the document).
    start: usize,
    /// Whether every byte on this line is ASCII, letting the byte offset double as the UTF-16
    /// offset within the line without walking the text.
    is_ascii: bool,
}

/// Maps byte offsets within a document's source text to and from LSP [`Position`]s.
///
/// Built once per document version; re-built whenever the text changes.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// One entry per line, in order; `lines[0].start` is always `0`.
    lines: Vec<Line>,
    /// Total length of the document in bytes, used to clamp out-of-range offsets.
    len: usize,
}

impl LineIndex {
    /// Builds a [`LineIndex`] for the given document text.
    pub fn new(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut is_ascii = true;

        for (offset, byte) in text.bytes().enumerate() {
            if !byte.is_ascii() {
                is_ascii = false;
            }
            if byte == b'\n' {
                lines.push(Line { start, is_ascii });
                start = offset + 1;
                is_ascii = true;
            }
        }
        lines.push(Line { start, is_ascii });

        LineIndex { lines, len: text.len() }
    }

    /// Converts a byte offset into this document to a 0-based, UTF-16 [`Position`].
    ///
    /// `text` must be the same text this index was built from. Out-of-range offsets are clamped
    /// to the end of the document rather than panicking, since `merc_syntax` spans can
    /// legitimately point one-past-the-end (e.g. at `EOI`) or be a synthetic [`Span::default`].
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = offset.min(self.len);

        // Binary search for the last line whose start is <= offset.
        let line_idx = match self.lines.binary_search_by_key(&offset, |line| line.start) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };
        let line = self.lines[line_idx];

        let character = if line.is_ascii {
            // Fast path: byte offset within the line is already the UTF-16 offset.
            (offset - line.start) as u32
        } else {
            // Slow path: walk the line counting UTF-16 code units up to `offset`.
            let line_end = self.lines.get(line_idx + 1).map_or(self.len, |next| next.start);
            let line_text = &text[line.start..line_end.min(text.len())];
            let mut units = 0u32;
            for (byte_offset, ch) in line_text.char_indices() {
                if line.start + byte_offset >= offset {
                    break;
                }
                units += ch.len_utf16() as u32;
            }
            units
        };

        Position {
            line: line_idx as u32,
            character,
        }
    }

    /// Converts a [`Span`] into this document to an LSP [`Range`].
    pub fn range(&self, text: &str, span: &Span) -> Range {
        Range {
            start: self.position(text, span.start),
            end: self.position(text, span.end),
        }
    }

    /// Converts a 0-based, UTF-16 [`Position`] back to a byte offset into this document.
    ///
    /// Returns `None` if `position` names a line beyond the end of the document; a `character`
    /// past the end of an existing line clamps to the line's end instead of failing, matching
    /// how most LSP clients send positions that are momentarily out of sync with the server.
    ///
    /// Not called from production code yet — reserved for the offset-lookup side of hover
    /// (PLAN.md §3.7), a stretch goal not implemented in phase 1. Exercised by the round-trip
    /// test below in the meantime.
    #[allow(dead_code)]
    pub fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line = *self.lines.get(position.line as usize)?;
        let line_end = self
            .lines
            .get(position.line as usize + 1)
            .map_or(self.len, |next| next.start);
        let line_text = &text[line.start..line_end.min(text.len())];

        if line.is_ascii {
            let offset = line.start + position.character as usize;
            return Some(offset.min(line_end));
        }

        let mut units = 0u32;
        for (byte_offset, ch) in line_text.char_indices() {
            if units >= position.character {
                return Some(line.start + byte_offset);
            }
            units += ch.len_utf16() as u32;
        }
        Some(line_end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_of_document() {
        let text = "eqn f = x;";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, 0), Position { line: 0, character: 0 });
    }

    #[test]
    fn counts_newlines() {
        let text = "sort D;\nmap f: D;\neqn f = undeclared;";
        let start = text.rfind("undeclared").unwrap();
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, start), Position { line: 2, character: 8 });
    }

    #[test]
    fn last_line_without_trailing_newline() {
        let text = "sort D;\nmap f: D;";
        let index = LineIndex::new(text);
        let offset = text.len();
        assert_eq!(index.position(text, offset), Position { line: 1, character: 9 });
    }

    #[test]
    fn crlf_line_endings() {
        // The '\r' is just another byte on the line as far as this index is concerned; it counts
        // toward the column like any other ASCII byte, matching how editors that use CRLF report
        // positions back to the server.
        let text = "sort D;\r\nmap f: D;\r\n";
        let index = LineIndex::new(text);
        let offset = text.find("map").unwrap();
        assert_eq!(index.position(text, offset), Position { line: 1, character: 0 });
    }

    #[test]
    fn multibyte_comment_shifts_utf16_column_not_byte_or_char_column() {
        // "é" is 2 bytes / 1 char / 1 UTF-16 unit. Byte-counting (`syntax_tree_display::line_column`)
        // would put `x` one column too far right; this asserts the UTF-16 count instead.
        let text = "% naïve\nx";
        let index = LineIndex::new(text);
        let offset = text.rfind('x').unwrap();
        assert_eq!(index.position(text, offset), Position { line: 1, character: 0 });

        let text = "eqn é = x;";
        let index = LineIndex::new(text);
        let offset = text.rfind('x').unwrap();
        // char-counting (`Span::start_line_col`) gives column 9 (1-based) == character 8 (0-based).
        // Byte-counting gives character 9, since 'é' is 2 bytes but only 1 UTF-16 unit.
        assert_eq!(index.position(text, offset), Position { line: 0, character: 8 });
    }

    #[test]
    fn astral_plane_character_counts_as_two_utf16_units() {
        // An emoji outside the BMP is 1 char but 2 UTF-16 code units (a surrogate pair) — this is
        // the case that distinguishes UTF-16 counting from char counting, which the BMP-only
        // multibyte test above cannot.
        let text = "% 🎉 party\nx";
        let index = LineIndex::new(text);
        let offset = text.rfind('x').unwrap();
        assert_eq!(index.position(text, offset), Position { line: 1, character: 0 });

        let text = "eqn 🎉 = x;";
        let index = LineIndex::new(text);
        let offset = text.rfind('x').unwrap();
        // "eqn " (4) + "🎉" (2 UTF-16 units) + " = " (3) = 9.
        assert_eq!(index.position(text, offset), Position { line: 0, character: 9 });
    }

    #[test]
    fn empty_document() {
        let text = "";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text, 0), Position { line: 0, character: 0 });
    }

    #[test]
    fn offset_past_eof_clamps() {
        let text = "eqn f = x;";
        let index = LineIndex::new(text);
        let past_end = index.position(text, text.len() + 1000);
        assert_eq!(past_end, index.position(text, text.len()));
    }

    #[test]
    fn default_span_points_at_document_start() {
        let text = "eqn f = 1;";
        let index = LineIndex::new(text);
        let span = Span::default();
        assert_eq!(index.range(text, &span).start, Position { line: 0, character: 0 });
    }

    #[test]
    fn round_trip_offset_position_on_char_boundaries() {
        let text = "sort D;\nmap f: D;\neqn f = é + 🎉;\n";
        let index = LineIndex::new(text);
        for (offset, _) in text.char_indices() {
            let position = index.position(text, offset);
            let back = index.offset(text, position).expect("position should resolve back");
            assert_eq!(back, offset, "offset {offset} did not round-trip via {position:?}");
        }
    }

    #[test]
    fn range_covers_span() {
        let text = "eqn f = undeclared;";
        let start = text.find("undeclared").unwrap();
        let span = Span {
            start,
            end: start + "undeclared".len(),
        };
        let index = LineIndex::new(text);
        assert_eq!(
            index.range(text, &span),
            Range {
                start: Position { line: 0, character: 8 },
                end: Position { line: 0, character: 18 },
            }
        );
    }
}
