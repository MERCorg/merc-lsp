//! Converts a document's [`ParseOutcome`] and [`TypecheckOutcome`] into LSP [`Diagnostic`]s.

use merc_syntax::Rule;
use merc_syntax::Span;
use merc_typecheck::ProcessError;
use merc_utilities::MercError;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticSeverity;
use lsp_types::Range;
use pest::error::Error as PestError;
use pest::error::InputLocation;

use crate::convert::LineIndex;
use crate::convert::is_identifier_byte;
use crate::parse::ParseOutcome;
use crate::typecheck::TypecheckOutcome;

const SOURCE: &str = "merc-lsp";

/// Distinct `source` for type-checking diagnostics (see [`type_diagnostics`]) — kept separate
/// from parse diagnostics' `SOURCE` so a client can tell the two apart (e.g. to only clear one
/// kind), and because communication sort-compatibility isn't checked yet (see the
/// `merc_typecheck` crate README), so an empty set of these is not a full type-correctness
/// guarantee.
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
/// result of typechecking it (only attempted once parsing has already succeeded — see
/// [`crate::typecheck`]).
///
/// Returns an empty vector for [`TypecheckOutcome::Ok`], for the same reason [`diagnostics`]
/// does for [`ParseOutcome::Ok`].
pub fn type_diagnostics(text: &str, line_index: &LineIndex, outcome: &TypecheckOutcome) -> Vec<Diagnostic> {
    match outcome {
        TypecheckOutcome::Ok(_) => Vec::new(),
        TypecheckOutcome::Error(error) => vec![process_error_diagnostic(text, line_index, error)],
        TypecheckOutcome::Internal(message) => vec![internal_diagnostic(message, TYPE_SOURCE)],
    }
}

fn process_error_diagnostic(text: &str, line_index: &LineIndex, error: &ProcessError) -> Diagnostic {
    // Every variant but a `WellTyped(WellTypedError::Custom(..))` (an opaque wrapped error with
    // no location of its own) carries a span.
    let range = error.span().map(|span| line_index.range(text, span)).unwrap_or_default();
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(TYPE_SOURCE.to_string()),
        message: error.to_string(),
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
    use crate::parse::parse;

    #[tokio::test]
    async fn ok_parse_yields_no_diagnostics() {
        let text = "sort D;\ninit delta;";
        let outcome = parse(text.to_string()).await;
        let line_index = LineIndex::new(text);
        assert!(diagnostics(text, &line_index, &outcome).is_empty());
    }

    #[tokio::test]
    async fn parse_error_downcasts_to_pest_error_with_a_located_range() {
        let text = "sort D\ninit delta;"; // missing ';' after 'sort D'
        let outcome = parse(text.to_string()).await;
        let line_index = LineIndex::new(text);
        let diags = diagnostics(text, &line_index, &outcome);

        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        // The most fragile assumption in the design: the downcast to `pest::error::Error<Rule>`
        // must succeed, or this falls back to a 0:0 file-level diagnostic instead of a located
        // one. Assert the range actually lands past the start of the file.
        assert!(diag.range.start.line > 0 || diag.range.start.character > 0);
        assert!(!diag.message.contains("-->"), "message should not contain pest's caret block");
    }

    async fn process_specification_for(text: &str) -> merc_syntax::UntypedProcessSpecification {
        match parse(text.to_string()).await {
            ParseOutcome::Ok(spec) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn well_typed_specification_yields_no_type_diagnostics() {
        let text = "sort D;\ncons c: D;\ninit delta;";
        let outcome = crate::typecheck::typecheck(process_specification_for(text).await).await;
        let line_index = LineIndex::new(text);
        assert!(type_diagnostics(text, &line_index, &outcome).is_empty());
    }

    #[tokio::test]
    async fn ill_typed_specification_produces_a_located_type_diagnostic_with_a_distinct_source() {
        let text = "map f: Bool;\neqn f = undeclared;\ninit delta;";
        let outcome = crate::typecheck::typecheck(process_specification_for(text).await).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome);

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
        let outcome = crate::typecheck::typecheck(process_specification_for(text).await).await;
        let line_index = LineIndex::new(text);
        let diags = type_diagnostics(text, &line_index, &outcome);

        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
        assert_eq!(diag.source.as_deref(), Some(TYPE_SOURCE));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    }
}
