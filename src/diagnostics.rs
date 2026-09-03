//! Converts a document's [`ParseOutcome`] and [`TypecheckOutcome`] into LSP
//! [`Diagnostic`]s.
//!
//! An undeclared-name error additionally gets a "did you mean '...'?" suffix,
//! built by searching the document's own declared names scoped to the same
//! category of name the error is about, the same categories
//! [`crate::completion_context`] classifies a cursor position into.

use merc_syntax::Rule;
use merc_syntax::Span;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedProcessSpecification;
use merc_typecheck::InferenceError;
use merc_typecheck::PbesError;
use merc_typecheck::ProcessError;
use merc_typecheck::WellTypedError;
use merc_utilities::MercError;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticSeverity;
use lsp_types::Range;
use pest::error::Error as PestError;
use pest::error::InputLocation;

use crate::convert::LineIndex;
use crate::convert::is_identifier_byte;
use crate::edit_distance;
use crate::names;
use crate::parse::ParseOutcome;
use crate::typecheck::PbesTypecheckOutcome;
use crate::typecheck::TypecheckOutcome;

const SOURCE: &str = "merc-lsp";

/// Distinct `source` for type-checking diagnostics (see [`type_diagnostics`]).
const TYPE_SOURCE: &str = "merc-lsp:types";

/// Builds the full diagnostics list for a document from its latest parse
/// outcome.
///
/// Returns an empty vector for [`ParseOutcome::Ok`] — publishing that empty
/// vector is what clears any diagnostics from a previous, failing parse.
pub fn diagnostics(text: &str, line_index: &LineIndex, outcome: &ParseOutcome) -> Vec<Diagnostic> {
    match outcome {
        ParseOutcome::Ok(_) => Vec::new(),
        ParseOutcome::ParseError(error) => vec![parse_error_diagnostic(text, line_index, error)],
        ParseOutcome::Internal(message) => vec![internal_diagnostic(message, SOURCE)],
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
    outcome: &TypecheckOutcome,
    spec: &UntypedProcessSpecification,
) -> Vec<Diagnostic> {
    match outcome {
        TypecheckOutcome::Ok(_) => Vec::new(),
        TypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_process_error(error, spec);
            vec![error_diagnostic(text, line_index, error.span(), message)]
        }
        TypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE)],
    }
}

/// As [`type_diagnostics`], for a PBES document's [`PbesTypecheckOutcome`].
pub fn pbes_type_diagnostics(text: &str, line_index: &LineIndex, outcome: &PbesTypecheckOutcome, spec: &UntypedPbes) -> Vec<Diagnostic> {
    match outcome {
        PbesTypecheckOutcome::Ok(_) => Vec::new(),
        PbesTypecheckOutcome::Error(error) => {
            let message = error.to_string() + &suggestion_for_pbes_error(error, spec);
            vec![error_diagnostic(text, line_index, error.span(), message)]
        }
        PbesTypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE)],
    }
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

/// Builds a located type-error [`Diagnostic`], shared by [`type_diagnostics`] and
/// [`pbes_type_diagnostics`].
fn error_diagnostic(text: &str, line_index: &LineIndex, span: Option<&Span>, message: String) -> Diagnostic {
    let range = span.map(|span| line_index.range(text, span)).unwrap_or_default();
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(TYPE_SOURCE.to_string()),
        message,
        ..Diagnostic::default()
    }
}

fn parse_error_diagnostic(text: &str, line_index: &LineIndex, error: &MercError) -> Diagnostic {
    match error.downcast_ref::<PestError<Rule>>() {
        Some(pest_error) => Diagnostic {
            range: range_for_location(text, line_index, &pest_error.location),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(SOURCE.to_string()),
            message: pest_error.variant.message().into_owned(),
            ..Diagnostic::default()
        },
        None => {
            // Some other error type got wrapped as a MercError.
            let full = error.to_string();

            // We only want the first line of the error message, because it
            // could contain a stack trace.
            let message = full.split('\n').next().unwrap_or(&full).to_string();
            Diagnostic {
                range: Range::default(),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some(SOURCE.to_string()),
                message,
                ..Diagnostic::default()
            }
        }
    }
}

fn internal_diagnostic(message: &str, source: &str) -> Diagnostic {
    Diagnostic {
        range: Range::default(),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(source.to_string()),
        message: message.to_string(),
        ..Diagnostic::default()
    }
}

fn range_for_location(text: &str, line_index: &LineIndex, location: &InputLocation) -> Range {
    let span = match location {
        InputLocation::Span((start, end)) => Span { start: *start, end: *end },
        InputLocation::Pos(offset) => {
            // A zero-width range renders poorly in most editors; widen it to cover the token
            // starting at `offset`, or at minimum one character.
            let end = widen_to_token_end(text, *offset);
            Span { start: *offset, end }
        }
    };
    line_index.range(text, &span)
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
    use crate::parse::parse;

    #[tokio::test]
    async fn ok_parse_yields_no_diagnostics() {
        let text = "sort D;\ninit delta;";
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        assert!(diagnostics(text, &line_index, &outcome).is_empty());
    }

    #[tokio::test]
    async fn parse_error_downcasts_to_pest_error_with_a_located_range() {
        let text = "sort D\ninit delta;"; // missing ';' after 'sort D'
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        let diags = diagnostics(text, &line_index, &outcome);

        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
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
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        assert!(type_diagnostics(text, &line_index, &outcome, &spec).is_empty());
    }

    #[tokio::test]
    async fn ill_typed_specification_produces_a_located_type_diagnostic_with_a_distinct_source() {
        let text = "map f: Bool;\neqn f = undeclared;\ninit delta;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(TYPE_SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diag.range.start.line > 0 || diag.range.start.character > 0, "should be a located diagnostic");
    }

    #[tokio::test]
    async fn ill_typed_process_produces_a_located_type_diagnostic() {
        // `a` is not declared as an action anywhere.
        let text = "init a;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(TYPE_SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[tokio::test]
    async fn undeclared_sort_gets_a_did_you_mean_suggestion() {
        let text = "sort Bool2;\nmap f: Bol;\ninit delta;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        // "Bol" is one edit away from the built-in "Bool" — closer than the declared "Bool2".
        assert!(diags[0].message.contains("did you mean 'Bool'?"), "message was: {}", diags[0].message);
    }

    #[tokio::test]
    async fn undeclared_action_gets_a_did_you_mean_suggestion() {
        let text = "act ready: Bool;\ninit redy(true);";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("did you mean 'ready'?"), "message was: {}", diags[0].message);
    }

    #[tokio::test]
    async fn no_suggestion_when_nothing_is_close_enough() {
        let text = "init xyzzy;";
        let spec = process_specification_for(text).await;
        let outcome = crate::typecheck::typecheck(spec.clone()).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        assert!(!diags[0].message.contains("did you mean"), "message was: {}", diags[0].message);
    }

    #[tokio::test]
    async fn pbes_undeclared_propositional_variable_gets_a_did_you_mean_suggestion() {
        let text = "pbes mu Ready = true;\ninit Redy;";
        let outcome = crate::typecheck::typecheck_pbes(text.to_string()).await;
        let spec = match merc_syntax::UntypedPbes::parse(text) {
            Ok(spec) => spec,
            Err(error) => panic!("fixture failed to parse: {error}"),
        };
        let line_index = LineIndex::new(text);
        let diags = pbes_type_diagnostics(text, &line_index, &outcome, &spec);

        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("did you mean 'Ready'?"), "message was: {}", diags[0].message);
    }
}
