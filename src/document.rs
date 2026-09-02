//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use dashmap::DashMap;
use lsp_types::Diagnostic;
use lsp_types::Url;
use merc_syntax::UntypedProcessSpecification;
use merc_typecheck::PbesSpecification;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;
use crate::diagnostics;
use crate::parse::ParseOutcome;
use crate::typecheck::PbesTypecheckOutcome;
use crate::typecheck::TypecheckOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, and `parsed` are always kept consistent with each other. `checked` is
/// `None` whenever `parsed` isn't [`ParseOutcome::Ok`] (type checking only makes sense once
/// parsing has already succeeded) or the parsed kind has no type checker at all yet
/// ([`ParseOutcome::Ok(Specification::Pres(_))`](crate::parse::Specification::Pres) — see
/// `backend::on_change`).
pub struct Document {
    pub text: String,
    pub version: i32,
    pub line_index: LineIndex,
    pub parsed: ParseOutcome,
    pub checked: Option<CheckedOutcome>,
}

/// The result of type checking a document, tagged by which kind of specification it checked —
/// mirrors [`crate::parse::Specification`] one level down, for whichever half of that enum has a
/// type checker upstream at all (mCRL2 process specifications and PBES; not PRES yet).
pub enum CheckedOutcome {
    Process(TypecheckOutcome),
    Pbes(PbesTypecheckOutcome),
}

impl Document {
    /// Builds a new document snapshot from `text` at `version`, together with the outcomes of
    /// having parsed and (if parsing succeeded, and a type checker exists for the kind) type
    /// checked that exact `text`.
    pub fn new(text: String, version: i32, parsed: ParseOutcome, checked: Option<CheckedOutcome>) -> Self {
        let line_index = LineIndex::new(&text);
        Document {
            text,
            version,
            line_index,
            parsed,
            checked,
        }
    }

    /// All diagnostics for this document: parse errors (if any), plus — once parsing has
    /// succeeded — any type errors (tagged with a distinct `source`; see
    /// [`crate::diagnostics::type_diagnostics`]/[`crate::diagnostics::pbes_type_diagnostics`]).
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diags = diagnostics::diagnostics(&self.text, &self.line_index, &self.parsed);
        match &self.checked {
            Some(CheckedOutcome::Process(outcome)) => {
                diags.extend(diagnostics::type_diagnostics(&self.text, &self.line_index, outcome));
            }
            Some(CheckedOutcome::Pbes(outcome)) => {
                diags.extend(diagnostics::pbes_type_diagnostics(&self.text, &self.line_index, outcome));
            }
            None => {}
        }
        diags
    }

    /// The checked process specification backing [`crate::hover`], [`crate::goto_definition`], and
    /// [`crate::inlay_hints`], if one is available.
    ///
    /// `None` whenever the whole process specification currently fails to type check — even if
    /// the failure is in an unrelated `act`/`proc`/`init` declaration and the data specification
    /// itself would check fine on its own. `ProcessSpecification::from_untyped` (see
    /// [`crate::typecheck`]) has no partial-success entry point that would let these features keep
    /// working on the data-specification subtree alone while the rest of the document is still
    /// broken — so, for now, they simply go quiet document-wide until the whole thing checks
    /// again. `None` for a PBES/PRES document too — see [`Self::checked_pbes_specification`] for
    /// the PBES half of that, and `PLAN.md` for PRES (no type checker upstream at all yet).
    pub fn checked_process_specification(&self) -> Option<&ProcessSpecification> {
        match &self.checked {
            Some(CheckedOutcome::Process(TypecheckOutcome::Ok(spec))) => Some(spec),
            _ => None,
        }
    }

    /// As [`Self::checked_process_specification`], for a PBES document. [`crate::hover`] and
    /// [`crate::goto_definition`] don't need this directly — both are generic over `TypingInfo`
    /// (via [`Self::typing_info`]) and don't otherwise care which kind of specification produced
    /// it, so PBES hover/goto-def works without it. Kept as the PBES counterpart of
    /// [`Self::checked_process_specification`] for whatever needs the checked spec itself, not
    /// just its `TypingInfo` — a PBES-flavored `crate::inlay_hints` (not implemented yet: today's
    /// is written directly against `ProcessSpecification`'s `act`/`proc` declaration shape) or
    /// PBES semantic tokens, say.
    #[allow(dead_code)]
    pub fn checked_pbes_specification(&self) -> Option<&PbesSpecification> {
        match &self.checked {
            Some(CheckedOutcome::Pbes(PbesTypecheckOutcome::Ok(spec))) => Some(spec),
            _ => None,
        }
    }

    /// The *raw*, un-type-checked process specification backing [`crate::inlay_hints`]'s
    /// struct-field-name lookup.
    ///
    /// Type checking desugars a `struct` sort declaration in place — [`ProcessSpecification`]'s
    /// own checked `DataSpecification::data_specification` clears a desugared `SortDecl.expr`
    /// entirely, since the checker only needs the desugared constructors/projections it produced
    /// from it, not the original field names — so a struct's field names (`struct s(n: Nat)`) can
    /// only be recovered from this, the pre-checking parse. Spans (which [`TypingInfo`]'s lookups
    /// key on) are unaffected either way: checking mutates identifiers in place but never moves or
    /// rewrites a span.
    pub fn parsed_process_specification(&self) -> Option<&UntypedProcessSpecification> {
        match &self.parsed {
            ParseOutcome::Ok(spec) => spec.as_process(),
            _ => None,
        }
    }

    /// Every checked expression's typing across the whole document (see
    /// [`ProcessSpecification::typing_info`]/[`PbesSpecification::typing_info`]), if a checked
    /// specification of either kind is available.
    ///
    /// Computed lazily, on demand — not cached eagerly at typecheck time. `ProcessSpecification`/
    /// `DataSpecification` already memoize the expensive half internally (an `Arc`-cached
    /// singleton in each's own context — `PbesSpecification`'s own `typing_info` isn't memoized
    /// the same way upstream yet, but is still cheap: just an already-computed clone), so a first
    /// call per edit does the real work and every later call in the same request burst (hover,
    /// then goto-def, then inlay hints, all against the same unedited document) is cheap. Takes
    /// `&mut self` because `ProcessSpecification`'s memoization requires it; callers reach this
    /// through `documents.get_mut`, not `get`.
    pub fn typing_info(&mut self) -> Option<TypingInfo> {
        match &mut self.checked {
            Some(CheckedOutcome::Process(TypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            Some(CheckedOutcome::Pbes(PbesTypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            _ => None,
        }
    }
}

/// The set of documents currently tracked by the server, keyed by URI.
pub type DocumentStore = DashMap<Url, Document>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;
    use crate::typecheck::typecheck_pbes;

    async fn pbes_document_for(text: &str) -> Document {
        let outcome = parse(SpecKind::Pbes, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Pbes(_)) = &outcome else {
            panic!("fixture failed to parse as a PBES");
        };
        let checked = Some(CheckedOutcome::Pbes(typecheck_pbes(text.to_string()).await));
        Document::new(text.to_string(), 0, outcome, checked)
    }

    #[tokio::test]
    async fn checked_pbes_specification_is_available_once_a_pbes_document_type_checks() {
        let document = pbes_document_for("pbes mu X = true;\ninit X;").await;
        assert!(document.checked_pbes_specification().is_some());
        // Not a process specification — the two accessors are mutually exclusive.
        assert!(document.checked_process_specification().is_none());
    }

    #[tokio::test]
    async fn checked_pbes_specification_is_none_for_an_ill_typed_pbes_document() {
        let document = pbes_document_for("pbes mu X = Y;\ninit X;").await;
        assert!(document.checked_pbes_specification().is_none());
        assert!(!document.diagnostics().is_empty());
    }
}
