//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

use crate::convert::LineIndex;
use crate::parse::ParseOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, and `parsed` are always kept consistent with each other.
pub struct Document {
    pub text: String,
    pub version: i32,
    pub line_index: LineIndex,
    pub parsed: ParseOutcome,
}

impl Document {
    /// Builds a new document snapshot from `text` at `version`, together with the outcome of
    /// having parsed that exact `text`.
    pub fn new(text: String, version: i32, parsed: ParseOutcome) -> Self {
        let line_index = LineIndex::new(&text);
        Document {
            text,
            version,
            line_index,
            parsed,
        }
    }
}

/// The set of documents currently tracked by the server, keyed by URI.
pub type DocumentStore = DashMap<Url, Document>;
