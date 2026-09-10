//! Conversion between the byte-offset based [`merc_syntax::Span`] used throughout `merc_syntax`
//! and the UTF-16-code-unit based [`Position`]/[`Range`] types used by the Language Server
//! Protocol.

use line_index::LineCol;
use line_index::TextSize;
use line_index::WideEncoding;
use line_index::WideLineCol;
use lsp_types::Location;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::Url;
use merc_syntax::SourceId;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use percent_encoding::AsciiSet;
use percent_encoding::NON_ALPHANUMERIC;
use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;

/// Used to find word boundaries when narrowing a declaration's span down to
/// just its identifier, and to widen a zero-width parse-error location to a
/// whole token.
pub(crate) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'\''
}

/// Maps byte offsets within a document's source text to and from LSP [`Position`]s.
///
/// A thin adapter over the [`line_index`] crate (the same one rust-analyzer maintains and uses for
/// this exact job) onto this crate's own [`Position`]/[`Span`] types — every method still takes
/// `text` even though [`line_index::LineIndex`] itself doesn't need it, both so `text` must be the
/// same text this index was built from stays load-bearing documentation at every call site, and
/// because [`Self::offset`]'s own clamping (see its doc comment) needs the line's actual text to
/// measure.
///
/// Built once per document version; re-built whenever the text changes.
#[derive(Debug, Clone)]
pub struct LineIndex {
    inner: line_index::LineIndex,
    /// Total length of the document in bytes, used to clamp out-of-range offsets.
    len: usize,
}

impl LineIndex {
    /// Builds a [`LineIndex`] for the given document text.
    pub fn new(text: &str) -> Self {
        LineIndex {
            inner: line_index::LineIndex::new(text),
            len: text.len(),
        }
    }

    /// Converts a byte offset into this document to a 0-based, UTF-16
    /// [`Position`].
    ///
    /// `text` must be the same text this index was built from. Out-of-range
    /// offsets are clamped to the end of the document rather than panicking,
    /// since `merc_syntax` spans can be a synthetic [`Span::default`].
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let mut offset = offset.min(self.len);
        // A span boundary should always already sit on one, but clamping above can turn a valid
        // offset into `self.len`, and nothing guarantees every caller's raw offset does either —
        // walk back to the nearest one rather than let `try_line_col` reject it. `self.len` and `0`
        // are always boundaries, so this is guaranteed to terminate.
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }

        let line_col = self.inner.try_line_col(TextSize::from(offset as u32)).unwrap_or(LineCol { line: 0, col: 0 });
        let wide = self.inner.to_wide(WideEncoding::Utf16, line_col).unwrap_or(WideLineCol { line: line_col.line, col: line_col.col });
        Position { line: wide.line, character: wide.col }
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
    /// past the end of an existing line clamps to the line's end (i.e. its own trailing newline,
    /// if any, included — so this lands at the *next* line's start) instead of failing, matching
    /// how most LSP clients send positions that are momentarily out of sync with the server.
    ///
    /// Used by `backend::completion_request` to find the byte offset a completion request's
    /// cursor position names, for [`crate::completion_context`] to classify.
    pub fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line_range = self.inner.line(position.line)?;
        let line_text = &text[usize::from(line_range.start())..usize::from(line_range.end()).min(text.len())];
        let line_wide_len = WideEncoding::Utf16.measure(line_text) as u32;

        let wide = WideLineCol { line: position.line, col: position.character.min(line_wide_len) };
        let line_col = self.inner.to_utf8(WideEncoding::Utf16, wide)?;
        self.inner.offset(line_col).map(usize::from)
    }
}

/// The URI scheme a [`location`] builds for a span into *virtual* content.
pub(crate) const VIRTUAL_DOCUMENT_SCHEME: &str = "merc-builtin";

/// Characters [`virtual_uri`] leaves unescaped — the URI-path "unreserved" set (RFC 3986):
/// alphanumerics plus `-_.~`. Everything else, [`NON_ALPHANUMERIC`] would also escape.
const VIRTUAL_NAME_UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// Encodes `name` — a virtual [`merc_syntax::SourceMap`] entry's own registered name, e.g.
/// `<builtin>/nat.mcrl2` or `<generated>/struct/c1.mcrl2` — as a [`VIRTUAL_DOCUMENT_SCHEME`] URI.
pub(crate) fn virtual_uri(name: &str) -> Url {
    let encoded = utf8_percent_encode(name, VIRTUAL_NAME_UNRESERVED);
    Url::parse(&format!("{VIRTUAL_DOCUMENT_SCHEME}:///{encoded}")).expect("a percent-encoded name is always a valid URI path")
}

/// The inverse of [`virtual_uri`]: recovers the original registered name from a
/// [`VIRTUAL_DOCUMENT_SCHEME`] URI.
pub(crate) fn decode_virtual_uri(uri: &Url) -> Option<String> {
    if uri.scheme() != VIRTUAL_DOCUMENT_SCHEME {
        return None;
    }
    let path = uri.path().trim_start_matches('/');
    percent_decode_str(path).decode_utf8().ok().map(std::borrow::Cow::into_owned)
}

/// Resolves a global byte offset to the [`SourceId`] it falls in.
pub(crate) fn split(sources: &SourceMap, offset: usize) -> (SourceId, usize) {
    let id = sources.lookup(offset);
    (id, offset - sources.base_offset(id))
}

/// Builds an LSP [`Location`] for `span`, resolving whichever file it falls
/// into via `sources`/ `line_indexes`.
///
/// `None` only if `span`'s offset resolves to a file index past the end of
/// `line_indexes`.
pub(crate) fn location(sources: &SourceMap, line_indexes: &[LineIndex], span: &Span) -> Option<Location> {
    let (id, local_start) = split(sources, span.start);
    // A span is never produced straddling two files, so rebasing `span.end` by
    // the *same* file's base offset is always correct.
    let local_end = span.end - sources.base_offset(id);
    location_for(sources, line_indexes, id, &Span::new(local_start, local_end))
}

/// Builds an LSP [`Location`] for `local_span`, already known to belong to `id` — the other half
/// of [`location`], split out for a caller that resolved `id` some other way than looking up a
/// global offset (see [`crate::diagnostics::parse_error_diagnostic`], where a parse error's own
/// [`merc_syntax::ImportError::Parse`] names the failing file directly, which a global-offset
/// lookup can't reliably do for an offset that sits exactly on a file boundary — e.g. a syntax
/// error at the end of a file immediately followed by an imported one).
///
/// `None` only if `id` is a file index past the end of `line_indexes`.
pub(crate) fn location_for(sources: &SourceMap, line_indexes: &[LineIndex], id: SourceId, local_span: &Span) -> Option<Location> {
    let line_index = line_indexes.get(id.value())?;
    let range = line_index.range(sources.text(id), local_span);
    let uri = if sources.is_virtual(id) {
        virtual_uri(sources.path(id))
    } else {
        Url::from_file_path(sources.path(id)).ok()?
    };
    Some(Location { uri, range })
}

/// Resolves `span` to the text of whichever file it actually falls into, together with a span
/// local to that file's own text — the root document's own `text`/`span` unchanged whenever
/// `sources` has nothing loaded yet (the plain, single-file parse path — see `parse.rs`'s module
/// docs — which never offsets a span at all), otherwise rebased through `sources` the same way
/// [`location`] rebases one into a [`Range`].
pub(crate) fn local_text_and_span<'a>(text: &'a str, sources: &'a SourceMap, span: &Span) -> (&'a str, Span) {
    if sources.file_count() == 0 {
        return (text, span.clone());
    }

    let (id, local_start) = split(sources, span.start);
    let local_end = span.end - sources.base_offset(id);
    (sources.text(id), Span::new(local_start, local_end))
}

/// Whether `span` is local to the root document itself.
pub(crate) fn is_local_span(sources: &SourceMap, span: &Span) -> bool {
    sources.file_count() == 0 || sources.lookup(span.start).value() == 0
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

    #[test]
    fn virtual_uri_round_trips_a_name_with_reserved_characters() {
        let name = "<builtin>/struct/c1.mcrl2";
        let uri = virtual_uri(name);
        assert_eq!(uri.scheme(), VIRTUAL_DOCUMENT_SCHEME);
        assert_eq!(decode_virtual_uri(&uri).as_deref(), Some(name));
    }

    #[test]
    fn virtual_uri_leaves_unreserved_characters_unescaped() {
        // A byte-identical check of the actual encoded form, not just that it round-trips: `-_.~`
        // and alphanumerics stay literal, matching RFC 3986's unreserved set.
        let uri = virtual_uri("a-b_c.d~e f");
        assert_eq!(uri.path(), "/a-b_c.d~e%20f");
    }

    #[test]
    fn decode_virtual_uri_rejects_a_non_virtual_scheme() {
        let uri = Url::parse("file:///a/b.mcrl2").unwrap();
        assert_eq!(decode_virtual_uri(&uri), None);
    }

    #[test]
    fn decode_virtual_uri_rejects_malformed_percent_encoding() {
        // `Url::parse` itself doesn't validate that a `%XX` escape decodes to valid UTF-8, so this
        // reaches `decode_virtual_uri` — which must fail cleanly rather than panicking or silently
        // returning corrupted text.
        let uri = Url::parse(&format!("{VIRTUAL_DOCUMENT_SCHEME}:///%FF%FE")).unwrap();
        assert_eq!(decode_virtual_uri(&uri), None);
    }
}
