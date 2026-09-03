//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use dashmap::DashMap;
use lsp_types::Diagnostic;
use lsp_types::SemanticToken;
use lsp_types::Url;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedProcessSpecification;
use merc_typecheck::PbesSpecification;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;
use crate::diagnostics;
use crate::parse::ParseOutcome;
use crate::parse::Specification;
use crate::semantic_tokens;
use crate::typecheck::PbesTypecheckOutcome;
use crate::typecheck::TypecheckOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, `parsed`, `checked`, and `semantic_tokens` are the last *analyzed*
/// snapshot — always mutually consistent, all five updated together, only by `backend::analyze`
/// (on `did_open` or `did_save`) — and every completion/hover/goto-definition/inlay-hint/
/// semantic-tokens/document-symbol request reads exactly this snapshot, stale or not. `checked` is
/// `None` whenever `parsed` isn't [`ParseOutcome::Ok`] (type checking only makes sense once
/// parsing has already succeeded) or the parsed kind has no type checker at all yet
/// ([`ParseOutcome::Ok(Specification::Pres(_))`](crate::parse::Specification::Pres) — see
/// `backend::analyze`).
///
/// `pending_text`/`pending_version` are the separate, *unanalyzed* half: the latest buffer
/// contents `did_change` has recorded (see `backend::router`), updated on every keystroke — cheap
/// bookkeeping only, never parsed or type checked until the next `did_save` hands them to
/// `backend::analyze`, which is deliberately the only place mCRL2 parsing/type checking happens.
/// That's expensive enough that re-running it on every edit would make typing sluggish for no
/// benefit, since none of the analyzed fields above are shown to the client before a save anyway —
/// see `backend::analyze`'s doc comment.
pub struct Document {
    pub text: String,
    pub version: i32,
    pub line_index: LineIndex,
    pub parsed: ParseOutcome,
    pub checked: Option<CheckedOutcome>,
    /// The `textDocument/semanticTokens/full` payload for `text`/`parsed` above.
    pub semantic_tokens: Vec<SemanticToken>,
    pub pending_text: String,
    pub pending_version: i32,
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
            // A freshly analyzed document has nothing pending beyond what it was just analyzed
            // from — `backend::analyze` may still overwrite this immediately after construction if
            // `did_change` recorded a newer edit while the analysis it just finished was in
            // flight; see its doc comment.
            pending_text: text.clone(),
            pending_version: version,
            text,
            version,
            line_index,
            parsed,
            checked,
            // Left empty here; callers fill this in via `compute_semantic_tokens` once the rest of
            // the snapshot above is in place (it reads `text`/`line_index`/`parsed`).
            semantic_tokens: Vec::new(),
        }
    }

    /// All diagnostics for this document: parse errors (if any), plus — once parsing has
    /// succeeded — any type errors (tagged with a distinct `source`; see
    /// [`crate::diagnostics::type_diagnostics`]/[`crate::diagnostics::pbes_type_diagnostics`]).
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diags = diagnostics::diagnostics(&self.text, &self.line_index, &self.parsed);
        match &self.checked {
            // `checked` is only ever `Some(CheckedOutcome::Process(_))`/`Some(CheckedOutcome::Pbes(_))`
            // when `parsed` is the matching `ParseOutcome::Ok(Specification::Process(_)/Pbes(_))` —
            // see this struct's own doc comment and `backend::analyze` — so the raw parse is
            // always available here to build an undeclared-name suggestion from (see
            // `diagnostics.rs`'s module docs).
            Some(CheckedOutcome::Process(outcome)) => {
                let spec = self.parsed_process_specification().expect("checked implies a parsed process specification");
                diags.extend(diagnostics::type_diagnostics(&self.text, &self.line_index, outcome, spec));
            }
            Some(CheckedOutcome::Pbes(outcome)) => {
                let spec = self.parsed_pbes_specification().expect("checked implies a parsed PBES");
                diags.extend(diagnostics::pbes_type_diagnostics(&self.text, &self.line_index, outcome, spec));
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
    /// it, so PBES hover/goto-def works without it. Used by [`crate::inlay_hints::pbes_inlay_hints`]
    /// (which does need the checked spec itself, for its equations' parameter names), the PBES
    /// counterpart of [`Self::checked_process_specification`].
    pub fn checked_pbes_specification(&self) -> Option<&PbesSpecification> {
        match &self.checked {
            Some(CheckedOutcome::Pbes(PbesTypecheckOutcome::Ok(spec))) => Some(spec),
            _ => None,
        }
    }

    /// The *raw*, un-type-checked process specification backing [`crate::inlay_hints::inlay_hints`]'s
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

    /// As [`Self::parsed_process_specification`], for a PBES document — backs
    /// [`crate::inlay_hints::pbes_inlay_hints`]'s struct-field-name lookup the same way.
    pub fn parsed_pbes_specification(&self) -> Option<&UntypedPbes> {
        match &self.parsed {
            ParseOutcome::Ok(spec) => spec.as_pbes(),
            _ => None,
        }
    }

    /// Computes a fresh `textDocument/semanticTokens/full` payload from `self.parsed`/`self.text`
    /// as they stand right now — empty if `parsed` isn't [`ParseOutcome::Ok`], same "no parse,
    /// nothing to offer" rule every other AST-driven accessor here follows. Callers decide when
    /// this is worth calling and assign the result to `self.semantic_tokens`; see that field's own
    /// doc comment for why it isn't simply recomputed inline on every access.
    pub fn compute_semantic_tokens(&self) -> Vec<SemanticToken> {
        let ParseOutcome::Ok(spec) = &self.parsed else {
            return Vec::new();
        };
        match spec {
            Specification::Process(spec) => semantic_tokens::semantic_tokens(&self.text, &self.line_index, spec),
            Specification::Pbes(spec) => semantic_tokens::pbes_semantic_tokens(&self.text, &self.line_index, spec),
            Specification::Pres(spec) => semantic_tokens::pres_semantic_tokens(&self.text, &self.line_index, spec),
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
