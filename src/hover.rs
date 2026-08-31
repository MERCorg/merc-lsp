//! `textDocument/hover`: offset → [`TypedNode`] → hover text, built from a document's checked
//! data specification typing info ([`DataSpecification::typing_info`]).
//!
//! Scoped by what [`DataSpecification::typing_info`] itself covers: only expression nodes inside
//! `eqn` blocks get a [`TypedNode`] — a sort declaration, an action argument, or a process
//! parameter has no typing info to look up (no `TypingInfo` route exists upstream yet for
//! anything outside the data specification's own equations), so hovering one of those yields
//! `None` rather than degraded hover. See [`Document::checked_data_specification`] for the
//! further caveat on *when* a checked specification is available at all.
//!
//! [`Document::checked_data_specification`]: crate::document::Document::checked_data_specification

use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::Position;
use merc_typecheck::DataSpecification;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypedNode;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Builds hover content for `position`, or `None` when it isn't over a typed expression node, or
/// `position` doesn't resolve to an offset in `text` at all.
pub fn hover(text: &str, line_index: &LineIndex, data_specification: &DataSpecification, position: Position) -> Option<Hover> {
    let offset = line_index.offset(text, position)?;
    let typing_info = data_specification.typing_info();
    let node = typed_node_at(&typing_info, offset)?;

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
/// node with no resolved name — a literal or an operator's overall application, say), followed by
/// what kind of name it resolved to, when known.
fn hover_markdown(node: &TypedNode) -> String {
    let sort = &node.sort;
    let named = match &node.name {
        Some(ResolvedName::Variable { name }) => Some((name.as_str(), "equation variable")),
        Some(ResolvedName::Constructor { name, .. }) => Some((name.as_str(), "constructor")),
        Some(ResolvedName::Mapping { name, .. }) => Some((name.as_str(), "mapping")),
        Some(ResolvedName::SystemDefined { name }) => Some((name.as_str(), "system-defined")),
        Some(ResolvedName::Builtin { name }) => Some((name.as_str(), "built-in operator")),
        // `#[non_exhaustive]`: fall back to an unlabelled sort for any future variant.
        Some(_) | None => None,
    };
    match named {
        Some((name, kind)) => format!("```mcrl2\n{name}: {sort}\n```\n{kind}"),
        None => format!("```mcrl2\n{sort}\n```"),
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

    async fn checked_for(text: &str) -> DataSpecification {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        match typecheck(spec).await {
            TypecheckOutcome::Ok(checked) => checked.into_data_specification(),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn hovers_a_mapping_use_with_its_sort() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let data_specification = checked_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &data_specification, position).expect("expected hover content");

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
        let data_specification = checked_for(text).await;
        let line_index = LineIndex::new(text);

        let position = line_index.position(text, 0);
        assert!(hover(text, &line_index, &data_specification, position).is_none());
    }
}
