//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use dashmap::DashMap;
use lsp_types::Diagnostic;
use lsp_types::Url;
use merc_typecheck::DataSpecification;

use crate::convert::LineIndex;
use crate::diagnostics;
use crate::parse::ParseOutcome;
use crate::typecheck::TypecheckOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, and `parsed` are always kept consistent with each other. `typechecked`
/// is `None` whenever `parsed` isn't [`ParseOutcome::Ok`] — type checking only makes sense once
/// parsing has already succeeded.
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

    /// The checked data specification backing [`crate::hover`] and [`crate::goto_definition`], if
    /// one is available.
    ///
    /// `None` whenever the whole process specification currently fails to type check — even if
    /// the failure is in an unrelated `act`/`proc`/`init` declaration and the data specification
    /// itself would check fine on its own. `ProcessSpecification::from_untyped` (see
    /// [`crate::typecheck`]) has no partial-success entry point that would let hover/goto-def keep
    /// working on the data-specification subtree alone while the rest of the document is still
    /// broken — so, for now, both features simply go quiet document-wide until the whole thing
    /// checks again.
    pub fn checked_data_specification(&self) -> Option<&DataSpecification> {
        match &self.typechecked {
            Some(TypecheckOutcome::Ok(spec)) => Some(spec.data_specification()),
            _ => None,
        }
    }
}

/// The set of documents currently tracked by the server, keyed by URI.
pub type DocumentStore = DashMap<Url, Document>;
