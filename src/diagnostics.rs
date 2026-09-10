//! Converts a document's [`ParseOutcome`] and [`TypecheckOutcome`] into LSP
//! [`Diagnostic`]s.
//!
//! An undeclared-name error additionally gets a "did you mean '...'?" suffix,
//! built by searching the document's own declared names scoped to the same
//! category of name the error is about, the same categories
//! [`crate::completion_context`] classifies a cursor position into.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use merc_syntax::ImportError;
use merc_syntax::Rule;
use merc_syntax::SourceId;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;
use merc_syntax::scan_imports;
use merc_typecheck::InferenceError;
use merc_typecheck::ModalError;
use merc_typecheck::PbesError;
use merc_typecheck::PresError;
use merc_typecheck::ProcessError;
use merc_typecheck::WellTypedError;
use merc_utilities::MercError;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticRelatedInformation;
use lsp_types::DiagnosticSeverity;
use lsp_types::Location;
use lsp_types::NumberOrString;
use lsp_types::Range;
use lsp_types::Url;
use pest::error::Error as PestError;
use pest::error::InputLocation;

use crate::ambiguity;
use crate::ambiguity::AmbiguousPrefixConflict;
use crate::convert;
use crate::convert::LineIndex;
use crate::convert::is_identifier_byte;
use crate::edit_distance;
use crate::names;
use crate::parse::ParseOutcome;
use crate::typecheck::ModalTypecheckOutcome;
use crate::typecheck::PbesTypecheckOutcome;
use crate::typecheck::PresTypecheckOutcome;
use crate::typecheck::TypecheckOutcome;

const SOURCE: &str = "merc-lsp";

/// Distinct `source` for type-checking diagnostics (see [`type_diagnostics`]).
const TYPE_SOURCE: &str = "merc-lsp:types";

/// Distinct `source` for the [`AmbiguousPrefixConflict`] lint (see [`ambiguity_diagnostics_process`]
/// and its PBES/PRES/modal-formula counterparts) — purely syntactic, so unlike [`TYPE_SOURCE`] it
/// never depends on type checking having succeeded.
const AMBIGUITY_SOURCE: &str = "merc-lsp:ambiguity";

/// `Diagnostic::code` for the [`AmbiguousPrefixConflict`] lint, shared with [`crate::code_action`],
/// which matches on it to offer the parenthesize quick fix.
pub const AMBIGUOUS_PREFIX_CONFLICT_CODE: &str = "ambiguous-prefix-conflict";

/// Builds the full diagnostics list for a document from its latest parse
/// outcome, each paired with the URI of the file it actually belongs to (see
/// [`locate`]'s doc comment for why that isn't always `root_uri`).
///
/// Returns an empty vector for [`ParseOutcome::Ok`] — publishing that empty
/// vector (for `root_uri` at least) is what clears any diagnostics from a
/// previous, failing parse.
pub fn diagnostics(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], outcome: &ParseOutcome, root_uri: &Url) -> Vec<(Url, Diagnostic)> {
    match outcome {
        ParseOutcome::Ok(_) => Vec::new(),
        ParseOutcome::ParseError(error) => vec![parse_error_diagnostic(text, line_index, sources, line_indexes, error, root_uri)],
        ParseOutcome::Internal(message) => vec![internal_diagnostic(message, SOURCE, root_uri)],
    }
}

/// Builds the type-checking diagnostics list for a document's process specification, from the
/// result of typechecking it. Only performed after a successful parse — `spec` is that same
/// successful parse, used only to build an undeclared-name suggestion (see the module docs), not
/// to re-derive anything `outcome` already carries.
///
/// Returns an empty vector for [`TypecheckOutcome::Ok`], for the same reason [`diagnostics`]
/// does for [`ParseOutcome::Ok`].
pub fn type_diagnostics(
    text: &str,
    line_index: &LineIndex,
    sources: &SourceMap,
    line_indexes: &[LineIndex],
    outcome: &TypecheckOutcome,
    spec: &UntypedProcessSpecification,
    root_uri: &Url,
) -> Vec<(Url, Diagnostic)> {
    match outcome {
        TypecheckOutcome::Ok(_) => Vec::new(),
        TypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_process_error(error, spec);
            vec![error_diagnostic(text, line_index, sources, line_indexes, error.span(), message, root_uri)]
        }
        TypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE, root_uri)],
    }
}

/// As [`type_diagnostics`], for a PBES document's [`PbesTypecheckOutcome`].
pub fn pbes_type_diagnostics(
    text: &str,
    line_index: &LineIndex,
    sources: &SourceMap,
    line_indexes: &[LineIndex],
    outcome: &PbesTypecheckOutcome,
    spec: &UntypedPbes,
    root_uri: &Url,
) -> Vec<(Url, Diagnostic)> {
    match outcome {
        PbesTypecheckOutcome::Ok(_) => Vec::new(),
        PbesTypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_pbes_error(error, spec);
            vec![error_diagnostic(text, line_index, sources, line_indexes, error.span(), message, root_uri)]
        }
        PbesTypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE, root_uri)],
    }
}

/// As [`type_diagnostics`], for a PRES document's [`PresTypecheckOutcome`].
pub fn pres_type_diagnostics(
    text: &str,
    line_index: &LineIndex,
    sources: &SourceMap,
    line_indexes: &[LineIndex],
    outcome: &PresTypecheckOutcome,
    spec: &UntypedPres,
    root_uri: &Url,
) -> Vec<(Url, Diagnostic)> {
    match outcome {
        PresTypecheckOutcome::Ok(_) => Vec::new(),
        PresTypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_pres_error(error, spec);
            vec![error_diagnostic(text, line_index, sources, line_indexes, error.span(), message, root_uri)]
        }
        PresTypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE, root_uri)],
    }
}

/// As [`type_diagnostics`], for a modal-formula document's [`ModalTypecheckOutcome`].
pub fn modal_type_diagnostics(
    text: &str,
    line_index: &LineIndex,
    sources: &SourceMap,
    line_indexes: &[LineIndex],
    outcome: &ModalTypecheckOutcome,
    spec: &UntypedStateFrmSpec,
    root_uri: &Url,
) -> Vec<(Url, Diagnostic)> {
    match outcome {
        ModalTypecheckOutcome::Ok(_) => Vec::new(),
        ModalTypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_modal_error(error, spec);
            vec![error_diagnostic(text, line_index, sources, line_indexes, error.span(), message, root_uri)]
        }
        ModalTypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE, root_uri)],
    }
}

/// Warnings for every [`AmbiguousPrefixConflict`] in a process specification (see
/// `crate::ambiguity`'s module doc comment) — purely syntactic, so (unlike [`type_diagnostics`])
/// this runs on any successful parse, whether or not type checking also succeeded.
pub fn ambiguity_diagnostics_process(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], spec: &UntypedProcessSpecification, root_uri: &Url) -> Vec<(Url, Diagnostic)> {
    ambiguity::find_in_process_specification(spec, text, sources)
        .iter()
        .map(|hit| ambiguity_diagnostic(text, line_index, sources, line_indexes, hit, root_uri))
        .collect()
}

/// As [`ambiguity_diagnostics_process`], for a PBES.
pub fn ambiguity_diagnostics_pbes(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], spec: &UntypedPbes, root_uri: &Url) -> Vec<(Url, Diagnostic)> {
    ambiguity::find_in_pbes_specification(spec, text, sources)
        .iter()
        .map(|hit| ambiguity_diagnostic(text, line_index, sources, line_indexes, hit, root_uri))
        .collect()
}

/// As [`ambiguity_diagnostics_process`], for a PRES.
pub fn ambiguity_diagnostics_pres(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], spec: &UntypedPres, root_uri: &Url) -> Vec<(Url, Diagnostic)> {
    ambiguity::find_in_pres_specification(spec, text, sources)
        .iter()
        .map(|hit| ambiguity_diagnostic(text, line_index, sources, line_indexes, hit, root_uri))
        .collect()
}

/// As [`ambiguity_diagnostics_process`], for a modal (mu-calculus) formula.
pub fn ambiguity_diagnostics_modal(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], spec: &UntypedStateFrmSpec, root_uri: &Url) -> Vec<(Url, Diagnostic)> {
    ambiguity::find_in_modal_specification(spec, text, sources)
        .iter()
        .map(|hit| ambiguity_diagnostic(text, line_index, sources, line_indexes, hit, root_uri))
        .collect()
}

/// Builds one [`AmbiguousPrefixConflict`] warning, spanning the whole ambiguous expression so it's
/// visible at a glance, not just on the outer or inner operator alone. Names both operators by
/// slicing their own leading token straight out of the source text rather than hard-coding operator
/// names — the shape this lint finds is general across five different grammars (see the module doc
/// comment), so there's no one fixed pair of names ("`!`"/"`exists`") to spell out here.
///
/// `hit`'s spans are global offsets into `sources`' shared byte-offset space — they land outside
/// `text` (the root document's own text) whenever the flagged expression actually came from
/// something the root `%import`s, since `find_in_process_specification` and its counterparts walk
/// the *merged* data specification. So, exactly like [`error_diagnostic`], this resolves the span
/// against whichever file it actually falls into rather than assuming it's always local to `text`.
fn ambiguity_diagnostic(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], hit: &AmbiguousPrefixConflict, root_uri: &Url) -> (Url, Diagnostic) {
    let span = hit.whole_span();
    let (local_text, local_outer) = convert::local_text_and_span(text, sources, &hit.outer_span);
    let (_, local_inner) = convert::local_text_and_span(text, sources, &hit.inner_span);
    let outer_token = local_text[local_outer.start..local_inner.start].trim();
    let inner_token = local_text[local_inner.start..local_inner.end].split_whitespace().next().unwrap_or_default();
    let message = format!(
        "merc and the real mCRL2 parser can disagree on how far `{outer_token}` reaches here. Add parentheses around `{inner_token}`'s highlighted \
         span so both parsers agree."
    );
    let (uri, range) = locate(text, line_index, sources, line_indexes, &span, root_uri);
    (
        uri,
        Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some(AMBIGUITY_SOURCE.to_string()),
            code: Some(NumberOrString::String(AMBIGUOUS_PREFIX_CONFLICT_CODE.to_string())),
            message,
            ..Diagnostic::default()
        },
    )
}

/// A `" — did you mean '...'?"` suffix for `error`, if it names an undeclared identifier and a
/// close-enough candidate exists among `spec`'s own declarations — an empty string otherwise
/// (including for every error variant that isn't "undeclared" shaped at all, like a duplicate
/// declaration or an arity mismatch, which no typo fix would address).
///
/// One arm per undeclared-name-shaped variant, matched through the `WellTyped`/`Inference`
/// wrapper variants a data-specification-level error arrives through as well as the two
/// [`ProcessError`] raises directly — see `merc_typecheck`'s `ProcessError`/`WellTypedError`/
/// `InferenceError` doc comments for why the nesting looks like this.
fn suggestion_for_process_error(error: &ProcessError, spec: &UntypedProcessSpecification) -> String {
    match error {
        ProcessError::WellTyped(WellTypedError::UndefinedSort { sort, .. }) => {
            edit_distance::suggestion(sort, names::process_sort_names(spec).chain(names::SYSTEM_SORTS.iter().copied()))
        }
        ProcessError::WellTyped(WellTypedError::Inference(InferenceError::UndeclaredName { name, .. }))
        | ProcessError::Inference(InferenceError::UndeclaredName { name, .. }) => {
            edit_distance::suggestion(name, names::process_data_value_names(spec))
        }
        ProcessError::UndeclaredActionOrProcess { name, .. } => {
            edit_distance::suggestion(name, names::process_action_or_process_names(spec))
        }
        ProcessError::UndeclaredAction { name, .. } => edit_distance::suggestion(name, names::process_action_names(spec)),
        ProcessError::UnknownProcessParameter { process, name, .. } => {
            edit_distance::suggestion(name, names::process_parameter_names(spec, process))
        }
        _ => String::new(),
    }
}

/// As [`suggestion_for_process_error`], for a [`PbesError`].
fn suggestion_for_pbes_error(error: &PbesError, spec: &UntypedPbes) -> String {
    match error {
        PbesError::WellTyped(WellTypedError::UndefinedSort { sort, .. }) => {
            edit_distance::suggestion(sort, names::pbes_sort_names(spec).chain(names::SYSTEM_SORTS.iter().copied()))
        }
        PbesError::WellTyped(WellTypedError::Inference(InferenceError::UndeclaredName { name, .. }))
        | PbesError::Inference(InferenceError::UndeclaredName { name, .. }) => {
            edit_distance::suggestion(name, names::pbes_data_value_names(spec))
        }
        PbesError::UndeclaredPropositionalVariable { name, .. } => {
            edit_distance::suggestion(name, names::pbes_propositional_variable_names(spec))
        }
        _ => String::new(),
    }
}

/// As [`suggestion_for_pbes_error`], for a [`PresError`] — [`PresError`] mirrors [`PbesError`]'s
/// shape one level down (see `typecheck.rs`'s module docs), so this is the identical match, just
/// against a [`UntypedPres`] for its candidate names.
fn suggestion_for_pres_error(error: &PresError, spec: &UntypedPres) -> String {
    match error {
        PresError::WellTyped(WellTypedError::UndefinedSort { sort, .. }) => {
            edit_distance::suggestion(sort, names::pres_sort_names(spec).chain(names::SYSTEM_SORTS.iter().copied()))
        }
        PresError::WellTyped(WellTypedError::Inference(InferenceError::UndeclaredName { name, .. }))
        | PresError::Inference(InferenceError::UndeclaredName { name, .. }) => {
            edit_distance::suggestion(name, names::pres_data_value_names(spec))
        }
        PresError::UndeclaredPropositionalVariable { name, .. } => {
            edit_distance::suggestion(name, names::pres_propositional_variable_names(spec))
        }
        _ => String::new(),
    }
}

/// As [`suggestion_for_process_error`], for a [`ModalError`] — a modal formula's undeclared-name
/// shapes are a sort (as everywhere else), a data value, an action (`ModalError::UndeclaredAction`,
/// the modal counterpart of [`ProcessError::UndeclaredAction`]), or a fixpoint variable
/// (`ModalError::UndeclaredStateVariable`).
fn suggestion_for_modal_error(error: &ModalError, spec: &UntypedStateFrmSpec) -> String {
    match error {
        ModalError::WellTyped(WellTypedError::UndefinedSort { sort, .. }) => {
            edit_distance::suggestion(sort, names::modal_sort_names(spec).chain(names::SYSTEM_SORTS.iter().copied()))
        }
        ModalError::WellTyped(WellTypedError::Inference(InferenceError::UndeclaredName { name, .. }))
        | ModalError::Inference(InferenceError::UndeclaredName { name, .. }) => {
            edit_distance::suggestion(name, names::modal_data_value_names(spec))
        }
        ModalError::UndeclaredAction { name, .. } => edit_distance::suggestion(name, names::modal_action_names(spec)),
        ModalError::UndeclaredStateVariable { name, .. } => edit_distance::suggestion(name, names::modal_state_variable_names(spec)),
        _ => String::new(),
    }
}

/// Resolves `span` to the URI and [`Range`] a [`Diagnostic`] about it should actually be published
/// against — shared by [`error_diagnostic`], [`ambiguity_diagnostic`], and [`parse_error_diagnostic`].
///
/// A `Diagnostic::range` is only ever meaningful relative to the one URI its containing
/// `PublishDiagnosticsParams` is published under — so when `span` falls inside `text` itself
/// (`root_uri`'s own document) this returns an accurate range against `root_uri`. Otherwise, this
/// publishes directly against whichever file `span` actually falls into (e.g. something `text`
/// `%import`s), so the diagnostic lands on the real error location rather than on the `%import`
/// directive that brought that file in.
fn locate(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], span: &Span, root_uri: &Url) -> (Url, Range) {
    if convert::is_local_span(sources, span) {
        return (root_uri.clone(), line_index.range(text, span));
    }

    match convert::location(sources, line_indexes, span) {
        Some(location) => (location.uri, location.range),
        None => (root_uri.clone(), Range::default()),
    }
}

/// The directory `root_uri`'s own `%import` directives resolve relative paths against.
pub(crate) fn root_import_directory(sources: &SourceMap) -> Option<std::path::PathBuf> {
    if sources.file_count() == 0 {
        return None;
    }
    let root_path = Path::new(sources.path(SourceId::new(0)));
    Some(root_path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf())
}

/// The `%import` directive, within `text` (a document living in `dir`), that (directly or
/// transitively) pulls in the file `target` belongs to — `None` if no import reachable from
/// `text` leads there.
pub(crate) fn owning_import_directive(text: &str, sources: &SourceMap, dir: &Path, target: SourceId) -> Option<merc_utilities::Spanned<merc_syntax::ImportDirective>> {
    // Built once per call rather than re-resolved (each with its own linear scan and, on a miss,
    // a `canonicalize()` syscall) at every node of the DFS below.
    let index = path_index(sources);
    let mut visited = HashSet::new();
    scan_imports(text).into_iter().find_map(|directive| {
        let import_path = dir.join(&directive.node.path);
        let child_id = resolve_indexed(&index, &import_path)?;
        // A fresh search from each top-level directive: a file reachable from one directive but
        // not another must still be explored for the other.
        visited.clear();
        contains_source(sources, &index, &import_path, child_id, target, &mut visited).then_some(directive)
    })
}

/// Whether `target` is `id`'s own file, or something `id`'s file (transitively) `%import`s. `path`
/// is `id`'s own path, exactly as resolved to reach it — needed to resolve *its* `%import`s in
/// turn, relative to its own directory. `visited` guards against a cycle recursing forever; a
/// resolvable import graph can't actually contain one today (`merc_syntax::imports::Resolver`
/// itself rejects a cycle before this ever runs — see its own `stack` field), but nothing ties
/// that invariant to this function, so it's guarded directly rather than assumed.
fn contains_source(sources: &SourceMap, index: &HashMap<PathBuf, SourceId>, path: &Path, id: SourceId, target: SourceId, visited: &mut HashSet<SourceId>) -> bool {
    if id == target {
        return true;
    }
    if !visited.insert(id) {
        return false;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    scan_imports(sources.text(id)).into_iter().any(|directive| {
        let import_path = dir.join(&directive.node.path);
        resolve_indexed(index, &import_path).is_some_and(|child_id| contains_source(sources, index, &import_path, child_id, target, visited))
    })
}

/// Finds the [`SourceId`] already loaded into `sources` for `path`.
fn resolve_source(sources: &SourceMap, path: &Path) -> Option<SourceId> {
    resolve_indexed(&path_index(sources), path)
}

/// Every file in `sources`, indexed by both its own (non-canonical) path and, where it resolves,
/// its canonical one — the same two forms [`resolve_indexed`] tries in turn, just computed once
/// up front instead of on every lookup.
fn path_index(sources: &SourceMap) -> HashMap<PathBuf, SourceId> {
    let mut index: HashMap<PathBuf, SourceId> = HashMap::new();
    for id in (0..sources.file_count()).map(SourceId::new) {
        let path = PathBuf::from(sources.path(id));
        if let Ok(canonical) = path.canonicalize() {
            index.entry(canonical).or_insert(id);
        }
        index.entry(path).or_insert(id);
    }
    index
}

/// Looks `path` up in `index`, trying it as given first and, on a miss, canonicalized — matching
/// how the file it names was itself indexed by [`path_index`].
fn resolve_indexed(index: &HashMap<PathBuf, SourceId>, path: &Path) -> Option<SourceId> {
    index.get(path).copied().or_else(|| index.get(&path.canonicalize().ok()?).copied())
}

/// Companion diagnostics for `uri`'s own `%import` directives, so a broken import is visible right
/// there instead of only inside whatever file it actually pulls in.
pub fn import_error_diagnostics(text: &str, line_index: &LineIndex, sources: &SourceMap, uri: &Url, diags: &[(Url, Diagnostic)]) -> Vec<(Url, Diagnostic)> {
    let Some(dir) = root_import_directory(sources) else {
        return Vec::new();
    };

    let mut related: HashMap<Span, Vec<DiagnosticRelatedInformation>> = HashMap::new();
    for (target_uri, diagnostic) in diags {
        if target_uri == uri || diagnostic.severity != Some(DiagnosticSeverity::ERROR) {
            continue;
        }
        let Ok(path) = target_uri.to_file_path() else { continue };
        let Some(id) = resolve_source(sources, &path) else { continue };
        let Some(directive) = owning_import_directive(text, sources, &dir, id) else { continue };
        related.entry(directive.span).or_default().push(DiagnosticRelatedInformation {
            location: Location { uri: target_uri.clone(), range: diagnostic.range },
            message: diagnostic.message.clone(),
        });
    }

    related
        .into_iter()
        .map(|(span, related_information)| {
            let message = if related_information.len() == 1 {
                "the imported file has an error".to_string()
            } else {
                format!("the imported file has {} errors", related_information.len())
            };
            (
                uri.clone(),
                Diagnostic {
                    range: line_index.range(text, &span),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some(SOURCE.to_string()),
                    message,
                    related_information: Some(related_information),
                    ..Diagnostic::default()
                },
            )
        })
        .collect()
}

/// The path of the file whose own parse actually failed, for an [`ImportError`] ultimately caused
/// by a parse failure — the same file [`ImportError::pest_error`] recovers the structured pest
/// error from, however many [`ImportError::Unresolved`] layers deep it is. `None` for
/// [`ImportError::Cycle`], which isn't anchored to one file's parse.
fn import_parse_error_path(error: &ImportError) -> Option<&Path> {
    match error {
        ImportError::Parse { path, .. } => Some(path.as_path()),
        ImportError::Unresolved { cause, .. } => cause.downcast_ref::<ImportError>().and_then(import_parse_error_path),
        ImportError::Cycle { .. } => None,
    }
}

/// Builds a located type-error [`Diagnostic`] for `span`, shared by [`type_diagnostics`] and its
/// PBES/PRES/modal-formula counterparts.
fn error_diagnostic(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], span: Option<&Span>, message: String, root_uri: &Url) -> (Url, Diagnostic) {
    let Some(span) = span else {
        return (
            root_uri.clone(),
            Diagnostic {
                range: Range::default(),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some(TYPE_SOURCE.to_string()),
                message,
                ..Diagnostic::default()
            },
        );
    };
    let (uri, range) = locate(text, line_index, sources, line_indexes, span, root_uri);
    (
        uri,
        Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(TYPE_SOURCE.to_string()),
            message,
            ..Diagnostic::default()
        },
    )
}

fn parse_error_diagnostic(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], error: &MercError, root_uri: &Url) -> (Url, Diagnostic) {
    // A document parsed via `merc_syntax::imports` (any `%import`-capable document backed by a
    // real path — see `parse.rs`'s module docs) never carries a directly downcastable
    // `PestError<Rule>` at the top level.
    let import_error = error.downcast_ref::<ImportError>();
    let pest_error = error.downcast_ref::<PestError<Rule>>().or_else(|| import_error.and_then(ImportError::pest_error));
    match pest_error {
        Some(pest_error) => {
            let message = pest_error.variant.message().into_owned();
            // The file whose own parse actually failed, known directly from `ImportError::Parse`'s
            // own `path` — not re-derived from a global offset. An offset exactly on a file
            // boundary (e.g. a syntax error at the end of a file immediately followed by an
            // imported one) can't be resolved back to the right file by value alone, since it's
            // simultaneously "end of this file" and "start of the next" in the shared offset space.
            let id = import_error.and_then(import_parse_error_path).and_then(|path| resolve_source(sources, path));
            let (local_text, id) = match id {
                Some(id) => (sources.text(id), id),
                None => (text, SourceId::new(0)),
            };
            let local_span = pest_location_to_local_span(&pest_error.location, local_text);
            let (uri, range) = locate_local(text, line_index, sources, line_indexes, id, &local_span, root_uri);
            (
                uri,
                Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some(SOURCE.to_string()),
                    message,
                    ..Diagnostic::default()
                },
            )
        }
        None => {
            // Neither a bare pest error nor an `ImportError` wrapping one with a recoverable pest
            // error.
            let message = error.to_string();
            let span = import_error.and_then(ImportError::span);
            let (uri, range) = match span {
                Some(span) => locate(text, line_index, sources, line_indexes, span, root_uri),
                None => (root_uri.clone(), Range::default()),
            };
            (
                uri,
                Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some(SOURCE.to_string()),
                    message,
                    ..Diagnostic::default()
                },
            )
        }
    }
}

fn internal_diagnostic(message: &str, source: &str, root_uri: &Url) -> (Url, Diagnostic) {
    (
        root_uri.clone(),
        Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(source.to_string()),
            message: message.to_string(),
            ..Diagnostic::default()
        },
    )
}

/// Builds the [`Span`] a pest [`InputLocation`] names, local to `local_text` — the same file the
/// `InputLocation` itself is already local to (pest never sees any other file's text).
fn pest_location_to_local_span(location: &InputLocation, local_text: &str) -> Span {
    match location {
        InputLocation::Span((start, end)) => Span { start: *start, end: *end },
        // A zero-width range renders poorly in most editors; widen it to cover the token starting
        // at `offset`, or at minimum one character.
        InputLocation::Pos(offset) => Span { start: *offset, end: widen_to_token_end(local_text, *offset) },
    }
}

/// As [`locate`], but for a span already resolved to a known file `id` (and local to it) rather
/// than a global offset — see [`convert::location_for`] for why that distinction matters at a file
/// boundary.
fn locate_local(text: &str, line_index: &LineIndex, sources: &SourceMap, line_indexes: &[LineIndex], id: SourceId, local_span: &Span, root_uri: &Url) -> (Url, Range) {
    if id.value() == 0 {
        return (root_uri.clone(), line_index.range(text, local_span));
    }
    match convert::location_for(sources, line_indexes, id, local_span) {
        Some(location) => (location.uri, location.range),
        None => (root_uri.clone(), Range::default()),
    }
}

/// Finds the end of the identifier-like token starting at `offset`, or `offset + 1` if `offset`
/// isn't sitting on one.
fn widen_to_token_end(text: &str, offset: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = offset;
    while end < bytes.len() && is_identifier_byte(bytes[end]) {
        end += 1;
    }
    if end == offset { (offset + 1).min(text.len()) } else { end }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::SpecKind;
    use crate::parse::parse_ignoring_sources as parse;

    fn test_uri() -> Url {
        Url::parse("file:///test.mcrl2").expect("valid URL")
    }

    #[tokio::test]
    async fn ok_parse_yields_no_diagnostics() {
        let text = "sort D;\ninit delta;";
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        assert!(diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &test_uri()).is_empty());
    }

    #[tokio::test]
    async fn ambiguous_prefix_conflict_gets_a_located_warning() {
        let text = "sort D;\nmap q: Bool;\ninit (!exists d: D . d == d && q) -> delta;";
        let spec = process_specification_for(text).await;
        let line_index = LineIndex::new(text);
        let root_uri = test_uri();
        let diags = ambiguity_diagnostics_process(text, &line_index, &SourceMap::new(), &[], &spec, &root_uri);

        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].0, root_uri);
        assert_eq!(diags[0].1.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diags[0].1.source.as_deref(), Some(AMBIGUITY_SOURCE));
        assert_eq!(diags[0].1.code, Some(NumberOrString::String(AMBIGUOUS_PREFIX_CONFLICT_CODE.to_string())));
    }

    #[tokio::test]
    async fn parse_error_downcasts_to_pest_error_with_a_located_range() {
        let text = "sort D\ninit delta;"; // missing ';' after 'sort D'
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        let diags = diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &test_uri());

        assert_eq!(diags.len(), 1);
        let (_, diag) = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        // The most fragile assumption in the design, the downcast must succeed.
        assert!(diag.range.start.line > 0 || diag.range.start.character > 0);
        assert!(!diag.message.contains("-->"), "message should not contain pest's caret block");
    }

    async fn process_specification_for(text: &str) -> merc_syntax::UntypedProcessSpecification {
        match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(crate::parse::Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn well_typed_specification_yields_no_type_diagnostics() {
        let text = "sort D;\ncons c: D;\ninit delta;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        assert!(type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri()).is_empty());
    }

    #[tokio::test]
    async fn ill_typed_specification_produces_a_located_type_diagnostic_with_a_distinct_source() {
        let text = "map f: Bool;\neqn f = undeclared;\ninit delta;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let root_uri = test_uri();
        let diags = type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &root_uri);

        assert_eq!(diags.len(), 1);
        let (uri, diag) = &diags[0];
        assert_eq!(uri, &root_uri);
        assert_eq!(diag.source.as_deref(), Some(TYPE_SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diag.range.start.line > 0 || diag.range.start.character > 0, "should be a located diagnostic");
    }

    #[tokio::test]
    async fn ill_typed_process_produces_a_located_type_diagnostic() {
        // `a` is not declared as an action anywhere.
        let text = "init a;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        let (_, diag) = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(TYPE_SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[tokio::test]
    async fn undeclared_sort_gets_a_did_you_mean_suggestion() {
        let text = "sort Bool2;\nmap f: Bol;\ninit delta;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        // "Bol" is one edit away from the built-in "Bool" — closer than the declared "Bool2".
        assert!(diags[0].1.message.contains("did you mean 'Bool'?"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn undeclared_action_gets_a_did_you_mean_suggestion() {
        let text = "act ready: Bool;\ninit redy(true);";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(diags[0].1.message.contains("did you mean 'ready'?"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn no_suggestion_when_nothing_is_close_enough() {
        let text = "init xyzzy;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(!diags[0].1.message.contains("did you mean"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn pbes_undeclared_propositional_variable_gets_a_did_you_mean_suggestion() {
        let text = "pbes mu Ready = true;\ninit Redy;";
        let spec = match merc_syntax::UntypedPbes::parse(text) {
            Ok(spec) => spec,
            Err(error) => panic!("fixture failed to parse: {error}"),
        };
        let outcome = crate::typecheck::typecheck_pbes(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = pbes_type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(diags[0].1.message.contains("did you mean 'Ready'?"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn pres_undeclared_propositional_variable_gets_a_did_you_mean_suggestion() {
        let text = "pres mu Ready = true;\ninit Redy;";
        let spec = match merc_syntax::UntypedPres::parse(text) {
            Ok(spec) => spec,
            Err(error) => panic!("fixture failed to parse: {error}"),
        };
        let outcome = crate::typecheck::typecheck_pres(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = pres_type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(diags[0].1.message.contains("did you mean 'Ready'?"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn modal_undeclared_action_gets_a_did_you_mean_suggestion() {
        let text = "act ready: Bool;\nform nu X . [redy(true)]X;";
        let spec = match merc_syntax::UntypedStateFrmSpec::parse(text) {
            Ok(spec) => spec,
            Err(error) => panic!("fixture failed to parse: {error}"),
        };
        let outcome = crate::typecheck::typecheck_modal_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = modal_type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(diags[0].1.message.contains("did you mean 'ready'?"), "message was: {}", diags[0].1.message);
    }

    #[tokio::test]
    async fn modal_undeclared_state_variable_gets_a_did_you_mean_suggestion() {
        let text = "act a: Bool;\nform nu Ready . [a(true)]Redy;";
        let spec = match merc_syntax::UntypedStateFrmSpec::parse(text) {
            Ok(spec) => spec,
            Err(error) => panic!("fixture failed to parse: {error}"),
        };
        let outcome = crate::typecheck::typecheck_modal_ignoring_sources(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = modal_type_diagnostics(text, &line_index, &SourceMap::new(), &[], &outcome, &spec, &test_uri());

        assert_eq!(diags.len(), 1);
        assert!(diags[0].1.message.contains("did you mean 'Ready'?"), "message was: {}", diags[0].1.message);
    }
}
