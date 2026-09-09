//! Per-document state kept by the server: the latest text, its parse result, and the
//! [`LineIndex`] built from it.

use std::collections::HashMap;
use std::time::SystemTime;

use dashmap::DashMap;
use lsp_types::Diagnostic;
use lsp_types::SemanticToken;
use lsp_types::Url;
use merc_syntax::SourceId;
use merc_syntax::SourceMap;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;
use merc_typecheck::ModalSpecification;
use merc_typecheck::PbesSpecification;
use merc_typecheck::PresSpecification;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;
use crate::diagnostics;
use crate::parse::ParseOutcome;
use crate::parse::Specification;
use crate::semantic_tokens;
use crate::typecheck::ModalTypecheckOutcome;
use crate::typecheck::PbesTypecheckOutcome;
use crate::typecheck::PresTypecheckOutcome;
use crate::typecheck::TypecheckOutcome;

/// A single open (or otherwise tracked) document.
///
/// `text`, `line_index`, `parsed`, `checked`, and `semantic_tokens` are the last *analyzed*
/// snapshot — always mutually consistent, all five updated together, only by `backend::analyze`
/// (on `did_open` or `did_save`) — and every completion/hover/goto-definition/inlay-hint/
/// semantic-tokens/document-symbol request reads exactly this snapshot, stale or not. `checked` is
/// `None` only when `parsed` isn't [`ParseOutcome::Ok`] — type checking only makes sense once
/// parsing has already succeeded — since every parsed kind now has a type checker (see
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
    /// Every file `parsed`'s spans are global offsets into.
    pub sources: SourceMap,
    pub line_indexes: Vec<LineIndex>,
    /// Snapshot, as of this analysis, of every non-virtual file in `sources`' on-disk
    /// modification time.
    import_mtimes: HashMap<String, SystemTime>,
    /// Every URI besides this document's own that `backend::analyze` published diagnostics
    /// against as of the last analysis — a diagnostic whose real span lands in something this
    /// document `%import`s (see [`Self::diagnostics`]) is published against *that* file's URI, so
    /// `backend::analyze` needs to remember which foreign URIs it touched to clear them (publish
    /// an empty list) once a later analysis no longer produces anything for them; starts empty and
    /// is only ever written by `backend::analyze` itself.
    pub published_foreign_uris: Vec<Url>,
}

/// The result of type checking a document, tagged by which kind of specification it checked —
/// mirrors [`crate::parse::Specification`] one level down, one variant per document kind.
pub enum CheckedOutcome {
    Process(TypecheckOutcome),
    Pbes(PbesTypecheckOutcome),
    Pres(PresTypecheckOutcome),
    Modal(ModalTypecheckOutcome),
}

impl Document {
    /// Builds a new document snapshot from `text` at `version`, together with
    /// the outcomes of having parsed and (if parsing succeeded) type checked
    /// that exact `text`.
    pub fn new(text: String, version: i32, parsed: ParseOutcome, checked: Option<CheckedOutcome>, sources: SourceMap) -> Self {
        let line_index = LineIndex::new(&text);
        let line_indexes = (0..sources.file_count()).map(|id| LineIndex::new(sources.text(SourceId::new(id)))).collect();
        let import_mtimes = (0..sources.file_count())
            .map(SourceId::new)
            .filter(|&id| !sources.is_virtual(id))
            .filter_map(|id| {
                let path = sources.path(id);
                let mtime = std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok()?;
                Some((path.to_string(), mtime))
            })
            .collect();
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
            sources,
            line_indexes,
            import_mtimes,
            published_foreign_uris: Vec::new(),
        }
    }

    /// Whether any file this document's own analysis pulled in (its `%import`s, or itself, since
    /// both are tracked in [`Self::import_mtimes`] the same way) now has a different on-disk
    /// modification time than it did when this snapshot was analyzed.
    pub fn is_stale(&self) -> bool {
        self.import_mtimes.iter().any(|(path, &snapshot)| {
            let current = std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok();
            current != Some(snapshot)
        })
    }

    /// All diagnostics for this document: parse errors (if any), plus — once parsing has
    /// succeeded — any type errors (tagged with a distinct `source`; see
    /// [`crate::diagnostics::type_diagnostics`] and its PBES/PRES/modal-formula counterparts).
    ///
    /// Each diagnostic is paired with the URI of the file it actually belongs to — `uri` (this
    /// document's own) for anything local to `self.text`, but the URI of whichever `%import`ed
    /// file a diagnostic's span actually falls into otherwise (see
    /// [`crate::diagnostics::locate`]'s doc comment), so a diagnostic about content that came from
    /// somewhere else lands its red squiggle in that file rather than at a meaningless zero-width
    /// range superimposed on `uri`'s own text. `backend::analyze` is responsible for actually
    /// publishing each group against its own URI.
    pub fn diagnostics(&self, uri: &Url) -> Vec<(Url, Diagnostic)> {
        let mut diags = diagnostics::diagnostics(&self.text, &self.line_index, &self.sources, &self.line_indexes, &self.parsed, uri);
        // Purely syntactic (see `crate::ambiguity`'s module doc comment), so — unlike the
        // `checked` match below — this runs on any successful parse regardless of whether type
        // checking also succeeded.
        if let ParseOutcome::Ok(spec) = &self.parsed {
            match spec {
                Specification::Process(spec) => diags.extend(diagnostics::ambiguity_diagnostics_process(&self.text, &self.line_index, &self.sources, &self.line_indexes, spec, uri)),
                Specification::Pbes(spec) => diags.extend(diagnostics::ambiguity_diagnostics_pbes(&self.text, &self.line_index, &self.sources, &self.line_indexes, spec, uri)),
                Specification::Pres(spec) => diags.extend(diagnostics::ambiguity_diagnostics_pres(&self.text, &self.line_index, &self.sources, &self.line_indexes, spec, uri)),
                Specification::Modal(spec) => diags.extend(diagnostics::ambiguity_diagnostics_modal(&self.text, &self.line_index, &self.sources, &self.line_indexes, spec, uri)),
            }
        }
        match &self.checked {
            // `checked`'s variant always matches `parsed`'s (see this struct's own doc comment
            // and `backend::analyze`), so the raw parse is always available here to build an
            // undeclared-name suggestion from (see `diagnostics.rs`'s module docs).
            Some(CheckedOutcome::Process(outcome)) => {
                let spec = self.parsed_process_specification().expect("checked implies a parsed process specification");
                diags.extend(diagnostics::type_diagnostics(&self.text, &self.line_index, &self.sources, &self.line_indexes, outcome, spec, uri));
            }
            Some(CheckedOutcome::Pbes(outcome)) => {
                let spec = self.parsed_pbes_specification().expect("checked implies a parsed PBES");
                diags.extend(diagnostics::pbes_type_diagnostics(&self.text, &self.line_index, &self.sources, &self.line_indexes, outcome, spec, uri));
            }
            Some(CheckedOutcome::Pres(outcome)) => {
                let spec = self.parsed_pres_specification().expect("checked implies a parsed PRES");
                diags.extend(diagnostics::pres_type_diagnostics(&self.text, &self.line_index, &self.sources, &self.line_indexes, outcome, spec, uri));
            }
            Some(CheckedOutcome::Modal(outcome)) => {
                let spec = self.parsed_modal_specification().expect("checked implies a parsed modal formula");
                diags.extend(diagnostics::modal_type_diagnostics(&self.text, &self.line_index, &self.sources, &self.line_indexes, outcome, spec, uri));
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
    /// again. `None` for a PBES/PRES/modal-formula document too — see
    /// [`Self::checked_pbes_specification`]/[`Self::checked_pres_specification`]/
    /// [`Self::checked_modal_specification`] for those.
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

    /// As [`Self::checked_pbes_specification`], for a PRES document — backs
    /// [`crate::inlay_hints::pres_inlay_hints`]'s equation-parameter-name lookup.
    pub fn checked_pres_specification(&self) -> Option<&PresSpecification> {
        match &self.checked {
            Some(CheckedOutcome::Pres(PresTypecheckOutcome::Ok(spec))) => Some(spec),
            _ => None,
        }
    }

    /// As [`Self::checked_pbes_specification`], for a modal-formula document — backs
    /// [`crate::hover`]'s action-declaration lookup and
    /// [`crate::inlay_hints::modal_inlay_hints`]'s fixpoint-variable-parameter-name lookup.
    pub fn checked_modal_specification(&self) -> Option<&ModalSpecification> {
        match &self.checked {
            Some(CheckedOutcome::Modal(ModalTypecheckOutcome::Ok(spec))) => Some(spec),
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

    /// As [`Self::parsed_pbes_specification`], for a PRES document.
    pub fn parsed_pres_specification(&self) -> Option<&UntypedPres> {
        match &self.parsed {
            ParseOutcome::Ok(spec) => spec.as_pres(),
            _ => None,
        }
    }

    /// As [`Self::parsed_pbes_specification`], for a modal-formula document.
    pub fn parsed_modal_specification(&self) -> Option<&UntypedStateFrmSpec> {
        match &self.parsed {
            ParseOutcome::Ok(spec) => spec.as_modal(),
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
            Specification::Modal(spec) => semantic_tokens::modal_semantic_tokens(&self.text, &self.line_index, spec),
        }
    }

    /// Every checked expression's typing across the whole document (see
    /// `ProcessSpecification::typing_info` and its PBES/PRES/modal-formula counterparts), if a
    /// checked specification of any kind is available.
    ///
    /// Computed lazily, on demand — not cached eagerly at typecheck time. `ProcessSpecification`/
    /// `DataSpecification` already memoize the expensive half internally (an `Arc`-cached
    /// singleton in each's own context — the other three kinds' own `typing_info` isn't memoized
    /// the same way upstream yet, but is still cheap: just an already-computed clone), so a first
    /// call per edit does the real work and every later call in the same request burst (hover,
    /// then goto-def, then inlay hints, all against the same unedited document) is cheap. Takes
    /// `&mut self` because `ProcessSpecification`'s memoization requires it; callers reach this
    /// through `documents.get_mut`, not `get`.
    pub fn typing_info(&mut self) -> Option<TypingInfo> {
        match &mut self.checked {
            Some(CheckedOutcome::Process(TypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            Some(CheckedOutcome::Pbes(PbesTypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            Some(CheckedOutcome::Pres(PresTypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            Some(CheckedOutcome::Modal(ModalTypecheckOutcome::Ok(spec))) => Some(spec.typing_info()),
            _ => None,
        }
    }
}

/// The set of documents currently tracked by the server, keyed by URI.
pub type DocumentStore = DashMap<Url, Document>;

#[cfg(test)]
mod tests {
    use lsp_types::Range;

    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse_ignoring_sources as parse;
    use crate::typecheck::typecheck_modal_ignoring_sources as typecheck_modal;
    use crate::typecheck::typecheck_pbes;
    use crate::typecheck::typecheck_pres;

    async fn pbes_document_for(text: &str) -> Document {
        let outcome = parse(SpecKind::Pbes, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Pbes(spec)) = &outcome else {
            panic!("fixture failed to parse as a PBES");
        };
        let checked = Some(CheckedOutcome::Pbes(typecheck_pbes((**spec).clone()).await));
        Document::new(text.to_string(), 0, outcome, checked, SourceMap::new())
    }

    async fn pres_document_for(text: &str) -> Document {
        let outcome = parse(SpecKind::Pres, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Pres(spec)) = &outcome else {
            panic!("fixture failed to parse as a PRES");
        };
        let checked = Some(CheckedOutcome::Pres(typecheck_pres((**spec).clone()).await));
        Document::new(text.to_string(), 0, outcome, checked, SourceMap::new())
    }

    async fn modal_document_for(text: &str) -> Document {
        let outcome = parse(SpecKind::Modal, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Modal(spec)) = &outcome else {
            panic!("fixture failed to parse as a modal formula");
        };
        let checked = Some(CheckedOutcome::Modal(typecheck_modal((**spec).clone()).await));
        Document::new(text.to_string(), 0, outcome, checked, SourceMap::new())
    }

    #[tokio::test]
    async fn checked_pbes_specification_is_available_once_a_pbes_document_type_checks() {
        let document = pbes_document_for("pbes mu X = true;\ninit X;").await;
        assert!(document.checked_pbes_specification().is_some());
        // Not a process specification — the two accessors are mutually exclusive.
        assert!(document.checked_process_specification().is_none());
    }

    fn test_uri() -> Url {
        Url::parse("file:///test.mcrl2").expect("valid URL")
    }

    #[tokio::test]
    async fn checked_pbes_specification_is_none_for_an_ill_typed_pbes_document() {
        let document = pbes_document_for("pbes mu X = Y;\ninit X;").await;
        assert!(document.checked_pbes_specification().is_none());
        assert!(!document.diagnostics(&test_uri()).is_empty());
    }

    #[tokio::test]
    async fn checked_pres_specification_is_available_once_a_pres_document_type_checks() {
        let document = pres_document_for("pres mu X = true;\ninit X;").await;
        assert!(document.checked_pres_specification().is_some());
        assert!(document.checked_process_specification().is_none());
    }

    #[tokio::test]
    async fn checked_pres_specification_is_none_for_an_ill_typed_pres_document() {
        let document = pres_document_for("pres mu X = Y;\ninit X;").await;
        assert!(document.checked_pres_specification().is_none());
        assert!(!document.diagnostics(&test_uri()).is_empty());
    }

    #[tokio::test]
    async fn checked_modal_specification_is_available_once_a_modal_document_type_checks() {
        let document = modal_document_for("act a: Nat;\nform nu X . [a(0)]X;").await;
        assert!(document.checked_modal_specification().is_some());
        assert!(document.checked_process_specification().is_none());
    }

    #[tokio::test]
    async fn checked_modal_specification_is_none_for_an_ill_typed_modal_document() {
        let document = modal_document_for("act a: Nat;\nform nu X . [b(0)]X;").await;
        assert!(document.checked_modal_specification().is_none());
        assert!(!document.diagnostics(&test_uri()).is_empty());
    }

    /// Writes `files` (relative-path -> contents) into a fresh temp directory and returns it —
    /// mirrors `goto_definition.rs`'s own test helper of the same name.
    fn temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("should create a temp directory");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("should create parent directories");
            }
            std::fs::write(path, contents).expect("should write the fixture file");
        }
        dir
    }

    #[tokio::test]
    async fn diagnostics_stay_correctly_located_once_a_document_imports_another_file() {
        // `Document::diagnostics()` now threads `self.sources`/`self.line_indexes` through to
        // `diagnostics::type_diagnostics` so a type error whose span lands in an `%import`ed
        // file's own content doesn't get silently clamped to the end of the root document (see
        // `diagnostics::error_diagnostic`'s doc comment). This checks the far more common case
        // stays correct once that machinery is in play at all: an error whose span is still in
        // the root document (`main.mcrl2` itself, referencing an undeclared action) must resolve
        // to its own real position, not fall into the "different file" branch meant only for a
        // span that's genuinely elsewhere.
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit undeclared;\n"),
            ("common.mcrl2", "act a: Nat;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let main_uri = Url::from_file_path(&main_path).expect("valid file path");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let expected_offset = text.find("undeclared").expect("fixture contains 'undeclared'");
        let expected = LineIndex::new(&text).position(&text, expected_offset);
        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = &outcome else {
            panic!("fixture failed to parse");
        };
        let (checked, sources) = crate::typecheck::typecheck((**spec).clone(), sources).await;
        let document = Document::new(text, 0, outcome, Some(CheckedOutcome::Process(checked)), sources);

        let diags = document.diagnostics(&main_uri);
        assert!(!diags.is_empty(), "expected the undeclared action to be reported");
        assert_eq!(diags[0].0, main_uri, "should be published against main.mcrl2 itself: {:?}", diags[0]);
        assert_eq!(
            diags[0].1.range.start, expected,
            "diagnostic should be located at 'undeclared' in main.mcrl2, not clamped elsewhere: {:?}",
            diags[0]
        );
    }

    #[tokio::test]
    async fn ambiguity_warning_is_published_against_the_importing_file() {
        // As `diagnostics_stay_correctly_located_once_a_document_imports_another_file`, but for the
        // `AmbiguousPrefixConflict` lint: `ambiguity::find_in_process_specification` walks the
        // *merged* data specification, so a flagged expression can come from something the root
        // document `%import`s rather than from the root document's own text.
        //
        // A `Diagnostic::range` only ever means something relative to the one URI it's published
        // under, so a hit whose real position is in `common.mcrl2` can't be reported as a `range`
        // against `main.mcrl2`'s URI at all — `diagnostics::locate` instead resolves the (URI,
        // range) pair to publish against, which `backend::analyze` then actually publishes,
        // landing the diagnostic's red squiggle in `common.mcrl2` itself, at its own real
        // position — not silently mislocated in `main.mcrl2`.
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"),
            (
                "common.mcrl2",
                "sort D;\nmap q, f: Bool;\neqn f = !exists e: D . e == e && q;\n",
            ),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let common_path = dir.path().join("common.mcrl2");
        let main_uri = Url::from_file_path(&main_path).expect("valid file path");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let common_text = std::fs::read_to_string(&common_path).unwrap();
        // The warning's span starts at the outer `!`'s own span (`AmbiguousPrefixConflict::whole_span`),
        // not at `exists`.
        let expected_offset = common_text.find("!exists").expect("fixture contains '!exists'");
        let expected = LineIndex::new(&common_text).position(&common_text, expected_offset);
        let expected_uri = Url::from_file_path(&common_path).expect("valid file path");

        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = &outcome else {
            panic!("fixture failed to parse");
        };
        let (checked, sources) = crate::typecheck::typecheck((**spec).clone(), sources).await;
        let document = Document::new(text, 0, outcome, Some(CheckedOutcome::Process(checked)), sources);

        let diags = document.diagnostics(&main_uri);
        let (uri, ambiguity_diag) = diags
            .iter()
            .find(|(_, diag)| diag.source.as_deref() == Some("merc-lsp:ambiguity"))
            .expect("expected the ambiguous prefix conflict to be reported");
        assert_eq!(uri, &expected_uri, "should be published against common.mcrl2, not main.mcrl2: {ambiguity_diag:?}");
        assert_eq!(ambiguity_diag.range.start, expected);
    }

    #[tokio::test]
    async fn type_error_is_published_against_the_importing_file() {
        // As `ambiguity_warning_is_published_against_the_importing_file`, but for a genuine type
        // error (an undeclared name) whose span lands in an `%import`ed file's own content rather
        // than in the root document.
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"),
            ("common.mcrl2", "map f: Bool;\neqn f = undeclared;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let common_path = dir.path().join("common.mcrl2");
        let main_uri = Url::from_file_path(&main_path).expect("valid file path");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let common_text = std::fs::read_to_string(&common_path).unwrap();
        let expected_offset = common_text.find("undeclared").expect("fixture contains 'undeclared'");
        let expected = LineIndex::new(&common_text).position(&common_text, expected_offset);
        let expected_uri = Url::from_file_path(&common_path).expect("valid file path");

        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = &outcome else {
            panic!("fixture failed to parse");
        };
        let (checked, sources) = crate::typecheck::typecheck((**spec).clone(), sources).await;
        let document = Document::new(text, 0, outcome, Some(CheckedOutcome::Process(checked)), sources);

        let diags = document.diagnostics(&main_uri);
        let (uri, type_diag) = diags
            .iter()
            .find(|(_, diag)| diag.source.as_deref() == Some("merc-lsp:types"))
            .expect("expected the undeclared name to be reported");
        assert_eq!(uri, &expected_uri, "should be published against common.mcrl2, not main.mcrl2: {type_diag:?}");
        assert_eq!(type_diag.range.start, expected);
    }

    #[tokio::test]
    async fn parse_error_in_an_imported_file_keeps_its_full_message() {
        // A syntax error anywhere in the import graph of a real (`%import`-capable) document never
        // reaches `diagnostics::parse_error_diagnostic` as a downcastable `PestError<Rule>` at all —
        // `merc_syntax::imports::Resolver::load_with_text` stringifies it first (`format!("in
        // {path}:\n{error}")`), discarding the structured location before it ever crosses into
        // `merc-lsp`. That's a real gap, but it lives upstream in `merc_syntax`, not here: the only
        // thing `merc-lsp` can still get right without that structured location is to not throw
        // away what text it *does* get — this pins that the diagnostic keeps pest's actual message
        // (which file, what was expected) rather than truncating to just "in <path>:" (its own
        // first line) the way it used to, and stays published against `main.mcrl2` (the only URI
        // it can meaningfully resolve to without a structured location).
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"),
            ("common.mcrl2", "act a\n"), // missing ';'
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let main_uri = Url::from_file_path(&main_path).expect("valid file path");
        let text = std::fs::read_to_string(&main_path).unwrap();

        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::ParseError(_) = &outcome else {
            panic!("fixture should fail to parse");
        };
        let document = Document::new(text, 0, outcome, None, sources);

        let diags = document.diagnostics(&main_uri);
        assert_eq!(diags.len(), 1);
        let (uri, diag) = &diags[0];
        assert_eq!(uri, &main_uri);
        assert_eq!(diag.source.as_deref(), Some("merc-lsp"));
        assert_eq!(diag.range, Range::default(), "no structured location survives the stringified upstream error: {diag:?}");
        assert!(
            diag.message.contains("common.mcrl2") && diag.message.lines().count() > 1,
            "message should keep pest's own detail, not just its first line: {diag:?}"
        );
    }

    #[tokio::test]
    async fn is_stale_is_false_immediately_after_analysis() {
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"),
            ("common.mcrl2", "act a: Nat;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let document = Document::new(text, 0, outcome, None, sources);

        assert!(!document.is_stale());
    }

    #[tokio::test]
    async fn is_stale_becomes_true_once_an_imported_file_changes_on_disk() {
        // Mirrors what `merc/didFocusTextDocument`'s handler (`backend::router`) checks: a
        // document imports `common.mcrl2`, which changes (and is saved) after this document's own
        // last analysis — `is_stale` must notice, even though nothing about the document's own
        // text changed.
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"),
            ("common.mcrl2", "act a: Nat;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let common_path = dir.path().join("common.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let document = Document::new(text, 0, outcome, None, sources);
        assert!(!document.is_stale());

        // Rewritten with a modification time set explicitly (rather than relying on the wall
        // clock having moved on since `Document::new` took its snapshot) so this doesn't flake on
        // a filesystem with coarse mtime resolution.
        std::fs::write(&common_path, "act a, b: Nat;\n").unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::open(&common_path).unwrap().set_modified(future).unwrap();

        assert!(document.is_stale());
    }
}
