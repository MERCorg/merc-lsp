//! `textDocument/hover`: offset → [`TypedNode`] → hover text, built from a document's whole
//! [`TypingInfo`] ([`crate::document::Document::typing_info`]).
//!
//! Covers every checked expression node — `eqn` blocks *and* process bodies (action arguments,
//! process-instantiation arguments, conditions, time bounds, `dist` weights) — since
//! `ProcessSpecification::typing_info` merges both. A sort declaration, an action/process
//! declaration's own parameter list, and anything outside a checked expression still has no
//! `TypedNode` to look up, so hovering one of those yields `None` rather than degraded hover. See
//! [`crate::document::Document::checked_process_specification`] for the further caveat on *when* a
//! checked specification is available at all.

use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::Position;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypedNode;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Builds hover content for `position`, or `None` when it isn't over a typed expression node, or
/// `position` doesn't resolve to an offset in `text` at all.
pub fn hover(text: &str, line_index: &LineIndex, typing_info: &TypingInfo, position: Position) -> Option<Hover> {
    let offset = line_index.offset(text, position)?;
    let node = typed_node_at(typing_info, offset)?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: hover_markdown(node),
        }),
        range: Some(line_index.range(text, &node.span)),
    })
}

/// The most specific [`TypedNode`] at `offset`, if any — the shared first step of both
/// [`hover`] and [`crate::goto_definition::definition_range`].
pub(crate) fn typed_node_at(typing_info: &TypingInfo, offset: usize) -> Option<&TypedNode> {
    typing_info.at_offset(offset)
}

/// Renders `node` as Markdown: an mCRL2-highlighted `name: Sort` code block (just `Sort` for a
/// node with no resolved name — a literal or an operator's overall application, say; just `name`
/// for an action/process reference, which has no data-expression sort at all), followed by what
/// kind of name it resolved to, when known.
fn hover_markdown(node: &TypedNode) -> String {
    let named = match &node.name {
        Some(ResolvedName::Variable { name, .. }) => Some((name.as_str(), "variable")),
        Some(ResolvedName::Constructor { name, .. }) => Some((name.as_str(), "constructor")),
        Some(ResolvedName::Mapping { name, .. }) => Some((name.as_str(), "mapping")),
        Some(ResolvedName::SystemDefined { name }) => Some((name.as_str(), "system-defined")),
        Some(ResolvedName::Builtin { name }) => Some((name.as_str(), "built-in operator")),
        Some(ResolvedName::Action { name, .. }) => Some((name.as_str(), "action")),
        Some(ResolvedName::Process { name, .. }) => Some((name.as_str(), "process")),
        // `#[non_exhaustive]`: fall back to an unlabelled sort for any future variant.
        Some(_) | None => None,
    };
    match (named, &node.sort) {
        (Some((name, kind)), Some(sort)) => format!("```mcrl2\n{name}: {sort}\n```\n{kind}"),
        // An action/process reference: no data-expression sort to show at all.
        (Some((name, kind)), None) => format!("```mcrl2\n{name}\n```\n{kind}"),
        (None, Some(sort)) => format!("```mcrl2\n{sort}\n```"),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;
    use crate::typecheck::TypecheckOutcome;
    use crate::typecheck::typecheck;

    async fn typing_info_for(text: &str) -> TypingInfo {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        match typecheck(spec).await {
            TypecheckOutcome::Ok(mut checked) => checked.typing_info(),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn hovers_a_mapping_use_with_its_sort() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        // `SortExpression`'s `Display` parenthesizes a function sort's arrow.
        assert!(content.value.contains("f: (D -> D)"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("mapping"));
    }

    #[tokio::test]
    async fn no_hover_outside_any_typed_node() {
        let text = "sort D;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let position = line_index.position(text, 0);
        assert!(hover(text, &line_index, &typing_info, position).is_none());
    }

    #[tokio::test]
    async fn hovers_an_action_argument_with_its_declared_sort() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("n);").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("n: Nat"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("variable"));
    }
}
