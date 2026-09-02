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
use merc_syntax::ActDecl;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypedNode;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Builds hover content for `position`, or `None` when it isn't over a typed expression node, or
/// `position` doesn't resolve to an offset in `text` at all.
///
/// `actions` is the document's `act` declarations (empty for a PBES, which has none) — an action
/// reference has no data-expression sort of its own to show (see [`hover_markdown`]), so its
/// declared argument sorts are looked up here instead, by matching `ResolvedName::Action`'s
/// `declaration` span against each `ActDecl`'s own span.
pub fn hover(
    text: &str,
    line_index: &LineIndex,
    typing_info: &TypingInfo,
    actions: &[ActDecl],
    position: Position,
) -> Option<Hover> {
    let offset = line_index.offset(text, position)?;
    let node = typed_node_at(typing_info, offset)?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: hover_markdown(node, actions),
        }),
        range: Some(line_index.range(text, &node.span)),
    })
}

/// The most specific [`TypedNode`] at `offset`, if any — the shared first step of both
/// [`hover`] and [`crate::goto_definition::definition_range`].
///
/// [`TypingInfo::at_offset`] treats a span's end as exclusive, so a cursor sitting right after an
/// identifier's last character (offset == that identifier's `span.end`) misses the identifier
/// itself.
pub(crate) fn typed_node_at(typing_info: &TypingInfo, offset: usize) -> Option<&TypedNode> {
    let mut best: Option<&TypedNode> = None;
    for node in typing_info.nodes() {
        if node.span.start > offset || offset > node.span.end {
            continue;
        }
        let width = node.span.end - node.span.start;
        let narrower_or_tied = best.is_none_or(|current| width <= current.span.end - current.span.start);
        if narrower_or_tied {
            best = Some(node);
        }
    }
    best
}

/// Renders `node` as Markdown: an mCRL2-highlighted `name: Sort` code block (just `Sort` for a
/// node with no resolved name — a literal or an operator's overall application, say; just `name`
/// for a process reference, which has no sort at all), followed by what kind of name it resolved
/// to, when known.
///
/// An action reference is the one case with no `node.sort` (it isn't a data expression) that
/// still has a sort worth showing: its declared argument sorts, found in `actions` by matching
/// the resolved name's own `declaration` span — the winning overload, not just any declaration
/// sharing its name (see [`ResolvedName::Action`]'s docs).
fn hover_markdown(node: &TypedNode, actions: &[ActDecl]) -> String {
    if let Some(ResolvedName::Action { name, declaration }) = &node.name {
        let decl = declaration.as_ref().and_then(|span| actions.iter().find(|decl| &decl.span == span));
        return match decl.filter(|decl| !decl.args.is_empty()) {
            Some(decl) => {
                let sorts = decl.args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # ");
                format!("```mcrl2\n{name}: {sorts}\n```\naction")
            }
            None => format!("```mcrl2\n{name}\n```\naction"),
        };
    }

    let named = match &node.name {
        Some(ResolvedName::Variable { name, .. }) => Some((name.as_str(), "variable")),
        Some(ResolvedName::Constructor { name, .. }) => Some((name.as_str(), "constructor")),
        Some(ResolvedName::Mapping { name, .. }) => Some((name.as_str(), "mapping")),
        Some(ResolvedName::SystemDefined { name }) => Some((name.as_str(), "system-defined")),
        Some(ResolvedName::Builtin { name }) => Some((name.as_str(), "built-in operator")),
        Some(ResolvedName::Process { name, .. }) => Some((name.as_str(), "process")),
        // `#[non_exhaustive]`: fall back to an unlabelled sort for any future variant.
        Some(_) | None => None,
    };
    match (named, &node.sort) {
        (Some((name, kind)), Some(sort)) => format!("```mcrl2\n{name}: {sort}\n```\n{kind}"),
        // A process reference: no data-expression sort to show at all.
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

    async fn typing_info_for(text: &str) -> (TypingInfo, Vec<ActDecl>) {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        match typecheck(spec).await {
            TypecheckOutcome::Ok(mut checked) => (checked.typing_info(), checked.action_declarations().to_vec()),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn hovers_a_mapping_use_with_its_sort() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let (typing_info, actions) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, &actions, position).expect("expected hover content");

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
        let (typing_info, actions) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let position = line_index.position(text, 0);
        assert!(hover(text, &line_index, &typing_info, &actions, position).is_none());
    }

    #[tokio::test]
    async fn hovers_an_action_argument_with_its_declared_sort() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (typing_info, actions) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("n);").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, &actions, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("n: Nat"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("variable"));
    }

    #[tokio::test]
    async fn hovers_an_action_reference_with_its_declared_sorts() {
        let text = "act a: Nat # Bool;\nproc P(n: Nat, b: Bool) = a(n, b);\ninit P(1, true);";
        let (typing_info, actions) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("a(n, b)").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, &actions, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("a: Nat # Bool"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("action"));
    }

    #[tokio::test]
    async fn hovers_a_niladic_action_reference_with_just_its_name() {
        let text = "act a;\ninit a;";
        let (typing_info, actions) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let offset = text.find("init a;").unwrap() + "init ".len();
        let position = line_index.position(text, offset);
        let hover = hover(text, &line_index, &typing_info, &actions, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("```mcrl2\na\n```"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("action"));
    }
}
