//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use dashmap::DashMap;
use lsp_types::Diagnostic;
use lsp_types::Url;

use crate::convert::LineIndex;
use crate::diagnostics;
use crate::parse::ParseOutcome;
use crate::typecheck::TypecheckOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, and `parsed` are always kept consistent with each other. `typechecked`
/// is `None` whenever `parsed` isn't [`ParseOutcome::Ok`] — type checking the data specification
/// only makes sense once parsing has already succeeded.
pub struct Document {
    pub text: String,
    pub version: i32,
    pub line_index: LineIndex,
    pub parsed: ParseOutcome,
    pub typechecked: Option<TypecheckOutcome>,
}

impl Document {
    /// Builds a new document snapshot from `text` at `version`, together with the outcomes of
    /// having parsed and (if parsing succeeded) type checked that exact `text`.
    pub fn new(text: String, version: i32, parsed: ParseOutcome, typechecked: Option<TypecheckOutcome>) -> Self {
        let line_index = LineIndex::new(&text);
        Document {
            text,
            version,
            line_index,
            parsed,
            typechecked,
        }
    }

    /// All diagnostics for this document: parse errors (if any), plus — once parsing has
    /// succeeded — any data-specification type errors (tagged with a distinct `source`; see
    /// [`crate::diagnostics::type_diagnostics`]).
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diags = diagnostics::diagnostics(&self.text, &self.line_index, &self.parsed);
        if let Some(typechecked) = &self.typechecked {
            diags.extend(diagnostics::type_diagnostics(&self.text, &self.line_index, typechecked));
        }
        diags
    }
}

/// The set of documents currently tracked by the server, keyed by URI.
pub type DocumentStore = DashMap<Url, Document>;
