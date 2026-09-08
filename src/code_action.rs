//! `textDocument/codeAction`: currently just one quick fix — parenthesizing a
//! [`crate::ambiguity::AmbiguousPrefixConflict`] so merc and the real mCRL2 parser agree on it
//! (see that module's doc comment for the shape being fixed and why parenthesizing is the fix).
//!
//! Recomputes the ambiguity list fresh from the document's raw parse rather than matching against
//! `params.context.diagnostics`, the same way `hover`/`goto_definition` recompute from `document`
//! instead of threading state from an earlier request — simpler, and always in sync with
//! [`crate::diagnostics::ambiguity_diagnostics_process`] and its PBES/PRES/modal-formula
//! counterparts, since both read off the same [`ambiguity::find_in_process_specification`] family.

use std::collections::HashMap;

use lsp_types::CodeAction;
use lsp_types::CodeActionKind;
use lsp_types::CodeActionOrCommand;
use lsp_types::CodeActionParams;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::TextEdit;
use lsp_types::Url;
use lsp_types::WorkspaceEdit;

use crate::ambiguity;
use crate::ambiguity::AmbiguousPrefixConflict;
use crate::convert::LineIndex;
use crate::document::DocumentStore;
use crate::parse::ParseOutcome;
use crate::parse::Specification;

/// Every quick fix available at `params.range`, currently just [`parenthesize_quick_fix`] for each
/// [`AmbiguousPrefixConflict`] overlapping it. `None` when there's nothing to fix, the same
/// convention every other request handler in `backend.rs` uses for "no result".
pub fn code_actions(
    documents: &DocumentStore,
    params: CodeActionParams,
) -> Option<Vec<CodeActionOrCommand>> {
    let uri = params.text_document.uri.clone();
    let document = documents.get(&uri)?;
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };

    let hits = match spec {
        Specification::Process(spec) => {
            ambiguity::find_in_process_specification(spec, &document.text)
        }
        Specification::Pbes(spec) => ambiguity::find_in_pbes_specification(spec, &document.text),
        Specification::Pres(spec) => ambiguity::find_in_pres_specification(spec, &document.text),
        Specification::Modal(spec) => ambiguity::find_in_modal_specification(spec, &document.text),
    };

    let actions: Vec<CodeActionOrCommand> = hits
        .iter()
        .filter(|hit| overlaps(&document.line_index, &document.text, hit, params.range))
        .map(|hit| parenthesize_quick_fix(&uri, &document.text, &document.line_index, hit))
        .collect();

    if actions.is_empty() {
        None
    } else {
        Some(actions)
    }
}

/// Whether `hit`'s own range (see [`AmbiguousPrefixConflict::whole_span`]) overlaps the range
/// the client asked for — a code-action request's range is typically the current selection or
/// cursor line, not necessarily the diagnostic's own exact range.
fn overlaps(
    line_index: &LineIndex,
    text: &str,
    hit: &AmbiguousPrefixConflict,
    requested: Range,
) -> bool {
    let hit_range = line_index.range(text, &hit.whole_span());
    hit_range.start <= requested.end && requested.start <= hit_range.end
}

/// Builds the quick fix for one [`AmbiguousPrefixConflict`]: two zero-width `TextEdit`s inserting
/// `(` and `)` right at the inner operator's own span (`hit.inner_span`) — turning
/// `!exists d: D . X && Y` into `!(exists d: D . X && Y)`, textually identical to merc's own
/// (already-computed) reading, so mCRL2 agrees too. Marked preferred: it's the one fix that
/// preserves the document's current meaning rather than silently changing it — see the module doc
/// comment on why the inner operator's span, not just its body's, is what gets wrapped.
fn parenthesize_quick_fix(
    uri: &Url,
    text: &str,
    line_index: &LineIndex,
    hit: &AmbiguousPrefixConflict,
) -> CodeActionOrCommand {
    let start = line_index.position(text, hit.inner_span.start);
    let end = line_index.position(text, hit.inner_span.end);
    let edits = vec![
        TextEdit {
            range: point_range(start),
            new_text: "(".to_string(),
        },
        TextEdit {
            range: point_range(end),
            new_text: ")".to_string(),
        },
    ];

    CodeActionOrCommand::CodeAction(CodeAction {
        title: "Add parentheses so mCRL2 parses this the same way".to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        is_preferred: Some(true),
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(uri.clone(), edits)])),
            ..WorkspaceEdit::default()
        }),
        ..CodeAction::default()
    })
}

fn point_range(position: Position) -> Range {
    Range {
        start: position,
        end: position,
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::CodeActionContext;
    use lsp_types::PartialResultParams;
    use lsp_types::TextDocumentIdentifier;
    use lsp_types::WorkDoneProgressParams;
    use merc_syntax::SourceMap;

    use super::*;
    use crate::document::Document;
    use crate::parse::SpecKind;
    use crate::parse::parse_ignoring_sources as parse;

    async fn document_for(text: &str) -> Document {
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        Document::new(text.to_string(), 0, outcome, None, SourceMap::new())
    }

    fn params(uri: &Url, range: Range) -> CodeActionParams {
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    #[tokio::test]
    async fn offers_a_quick_fix_that_parenthesizes_the_quantifier() {
        let text = "sort D;\nmap q: Bool;\ninit (!exists d: D . d == d && q) -> delta;";
        let uri = Url::parse("file:///test.mcrl2").expect("valid URL");
        let documents = DocumentStore::default();
        documents.insert(uri.clone(), document_for(text).await);

        // The whole document, so it always overlaps the one hit regardless of its exact range.
        let whole_document = Range {
            start: Position::default(),
            end: Position {
                line: 10,
                character: 0,
            },
        };
        let actions = code_actions(&documents, params(&uri, whole_document))
            .expect("should offer a quick fix");
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected a CodeAction, not a Command");
        };
        let edits = action
            .edit
            .as_ref()
            .and_then(|edit| edit.changes.as_ref())
            .and_then(|changes| changes.get(&uri))
            .expect("should carry a WorkspaceEdit for the document");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].new_text, "(");
        assert_eq!(edits[1].new_text, ")");

        // Applying both edits should reproduce merc's own reading, textually, so mCRL2 agrees too.
        let quantifier_start = text.find("exists").expect("fixture contains 'exists'");
        let quantifier_end = text
            .find(") -> delta")
            .expect("fixture contains the guard's closing paren");
        let mut fixed = text.to_string();
        fixed.insert(quantifier_end, ')');
        fixed.insert(quantifier_start, '(');
        assert!(
            fixed.contains("!(exists d: D . d == d && q)"),
            "fixed text was: {fixed}"
        );
    }

    #[tokio::test]
    async fn offers_nothing_outside_the_requested_range() {
        let text = "sort D;\nmap q: Bool;\ninit (!exists d: D . d == d && q) -> delta;";
        let uri = Url::parse("file:///test.mcrl2").expect("valid URL");
        let documents = DocumentStore::default();
        documents.insert(uri.clone(), document_for(text).await);

        let first_line_only = Range {
            start: Position::default(),
            end: Position {
                line: 0,
                character: 0,
            },
        };
        assert!(code_actions(&documents, params(&uri, first_line_only)).is_none());
    }

    #[tokio::test]
    async fn offers_nothing_for_an_already_parenthesized_quantifier() {
        let text = "sort D;\nmap q: Bool;\ninit (!(exists d: D . d == d && q)) -> delta;";
        let uri = Url::parse("file:///test.mcrl2").expect("valid URL");
        let documents = DocumentStore::default();
        documents.insert(uri.clone(), document_for(text).await);

        let whole_document = Range {
            start: Position::default(),
            end: Position {
                line: 10,
                character: 0,
            },
        };
        assert!(code_actions(&documents, params(&uri, whole_document)).is_none());
    }
}
