//! `textDocument/hover`: offset → [`TypedNode`] → hover text, built from a document's whole
//! [`TypingInfo`] ([`crate::document::Document::typing_info`]).

use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::Position;
use lsp_types::Url;
use merc_syntax::ActDecl;
use merc_syntax::ProcDecl;
use merc_syntax::SortDecl;
use merc_syntax::Span;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypedNode;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;
use crate::parse::Specification;

/// Everything [`hover`] needs that doesn't vary per request is added.
/// `position` stays a separate argument to [`hover`] since it's the one input
/// that actually differs across a burst of requests against the same document.
pub struct HoverContext<'a> {
    pub text: &'a str,
    pub line_index: &'a LineIndex,
    pub typing_info: &'a TypingInfo,
    pub actions: &'a [ActDecl],
    pub processes: &'a [ProcDecl],
    pub spec: Option<&'a Specification>,
    pub doc_uri: Option<&'a Url>,
}

/// Builds hover content for `position`, or `None` when it isn't over a typed
/// node at all, or `position` doesn't resolve to an offset into `ctx.text` at
/// all.
///
/// When `ctx.doc_uri` is provided, hover text includes a "Go to definition"
/// link pointing at the declaration.
pub fn hover(ctx: &HoverContext, position: Position) -> Option<Hover> {
    let &HoverContext {
        text,
        line_index,
        typing_info,
        actions,
        processes,
        spec,
        doc_uri,
    } = ctx;
    let offset = line_index.offset(text, position)?;
    let node = typing_info.at_offset(offset)?;

    // A sort-name reference resolves to a name and a declaration span, not a declaration itself.
    if let Some(ResolvedName::Sort { name, .. }) = &node.name {
        let decl = spec.and_then(|spec| find_sort_declaration(spec, name))?;
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: sort_hover_markdown(decl, doc_uri, line_index, text),
            }),
            range: Some(line_index.range(text, &node.span)),
        });
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: hover_markdown(node, actions, processes, doc_uri, line_index, text),
        }),
        range: Some(line_index.range(text, &node.span)),
    })
}

/// The `sort` block declaring `name`, if the specification's data specification has one — `None`
/// only when `spec` doesn't have one, which shouldn't arise for a name a [`ResolvedName::Sort`]
/// node reported.
fn find_sort_declaration<'a>(spec: &'a Specification, name: &str) -> Option<&'a SortDecl> {
    let declarations = match spec {
        Specification::Process(spec) => &spec.data_specification.sort_declarations,
        Specification::Pbes(spec) => &spec.data_specification.sort_declarations,
        Specification::Pres(spec) => &spec.data_specification.sort_declarations,
    };
    declarations.iter().find(|decl| decl.identifier == name)
}

/// Renders the sort declaration as Markdown.
fn sort_hover_markdown(decl: &SortDecl, doc_uri: Option<&Url>, line_index: &LineIndex, text: &str) -> String {
    let SortDecl { identifier, expr, span, .. } = decl;
    let body = match expr {
        Some(expr) => format!("sort {identifier} = {expr};"),
        None => format!("sort {identifier};"),
    };
    let link = goto_def_link(span, doc_uri, line_index, text);
    format!("```mcrl2\n{body}\n```\nsort{link}")
}

/// Renders `node` as Markdown. An optional "Go to definition" link is appended
/// when `doc_uri` is available and the resolved name has a declaration span.
///
/// Uses actions to look up the argument sorts for action references.
/// [`ResolvedName::Action`]'s docs).
fn hover_markdown(
    node: &TypedNode,
    actions: &[ActDecl],
    processes: &[ProcDecl],
    doc_uri: Option<&Url>,
    line_index: &LineIndex,
    text: &str,
) -> String {
    if let Some(ResolvedName::Action { name, declaration }) = &node.name {
        let decl = declaration.as_ref().and_then(|span| actions.iter().find(|decl| &decl.identifier.span == span));
        let link = declaration.as_ref().map_or(String::new(), |span| goto_def_link(span, doc_uri, line_index, text));
        return match decl.filter(|decl| !decl.args.is_empty()) {
            Some(decl) => {
                let sorts = decl.args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # ");
                format!("```mcrl2\n{name}: {sorts}\n```\naction{link}")
            }
            None => format!("```mcrl2\n{name}\n```\naction{link}"),
        };
    }

    if let Some(ResolvedName::Process { name, declaration }) = &node.name {
        let decl = declaration.as_ref().and_then(|span| processes.iter().find(|decl| &decl.identifier.span == span));
        let link = declaration.as_ref().map_or(String::new(), |span| goto_def_link(span, doc_uri, line_index, text));
        let kind = "process";
        return match decl.filter(|decl| !decl.params.is_empty()) {
            Some(decl) => {
                let params = decl.params.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                format!("```mcrl2\n{name}({params})\n```\n{kind}{link}")
            }
            None => format!("```mcrl2\n{name}\n```\n{kind}{link}"),
        };
    }

    let named = match &node.name {
        Some(ResolvedName::Variable { name, declaration }) => Some((name.as_str(), "variable", declaration.clone())),
        Some(ResolvedName::Constructor { name, declaration, .. }) => Some((name.as_str(), "constructor", declaration.clone())),
        Some(ResolvedName::Mapping { name, declaration, .. }) => Some((name.as_str(), "mapping", declaration.clone())),
        Some(ResolvedName::SystemDefined { name }) => Some((name.as_str(), "system-defined", None)),
        Some(ResolvedName::Builtin { name }) => Some((name.as_str(), "built-in operator", None)),
        // `#[non_exhaustive]`: fall back to an unlabelled sort for any future variant.
        Some(_) | None => None,
    };
    match (named, &node.sort) {
        (Some((name, kind, decl)), Some(sort)) => {
            let link = decl.as_ref().map_or(String::new(), |span| goto_def_link(span, doc_uri, line_index, text));
            format!("```mcrl2\n{name}: {sort}\n```\n{kind}{link}")
        }
        (Some((name, kind, decl)), None) => {
            let link = decl.as_ref().map_or(String::new(), |span| goto_def_link(span, doc_uri, line_index, text));
            format!("```mcrl2\n{name}\n```\n{kind}{link}")
        }
        (None, Some(sort)) => format!("```mcrl2\n{sort}\n```"),
        (None, None) => String::new(),
    }
}

/// A `\n\n[Go to definition](...)` Markdown link pointing at `span`'s start, or `""` when
/// `doc_uri` is unavailable.
fn goto_def_link(span: &Span, doc_uri: Option<&Url>, line_index: &LineIndex, text: &str) -> String {
    match doc_uri {
        Some(uri) => {
            let pos = line_index.position(text, span.start);
            format!("\n\n[Go to definition]({}#L{}:{})", uri.as_str(), pos.line + 1, pos.character + 1)
        }
        None => String::new(),
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

    async fn typing_info_for(text: &str) -> (TypingInfo, Vec<ActDecl>, Vec<ProcDecl>, Specification) {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        let raw = Specification::Process(Box::new(spec.clone()));
        match typecheck(spec).await {
            TypecheckOutcome::Ok(mut checked) => (
                checked.typing_info(),
                checked.action_declarations().to_vec(),
                checked.process_declarations().to_vec(),
                raw,
            ),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn hovers_a_mapping_use_with_its_sort() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        // `SortExpression`'s `Display` parenthesizes a function sort's arrow.
        assert!(content.value.contains("f: (D -> D)"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("mapping"));
    }

    #[tokio::test]
    async fn no_hover_outside_any_typed_node_or_sort_reference() {
        let text = "sort D;\ninit delta;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let position = line_index.position(text, 0);
        assert!(hover(&ctx, position).is_none());
    }

    #[tokio::test]
    async fn hovers_an_action_argument_with_its_declared_sort() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("n);").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("n: Nat"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("variable"));
    }

    #[tokio::test]
    async fn hovers_an_action_reference_with_its_declared_sorts() {
        let text = "act a: Nat # Bool;\nproc P(n: Nat, b: Bool) = a(n, b);\ninit P(1, true);";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("a(n, b)").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("a: Nat # Bool"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("action"));
    }

    #[tokio::test]
    async fn hovers_a_niladic_action_reference_with_just_its_name() {
        let text = "act a;\ninit a;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("init a;").unwrap() + "init ".len();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("```mcrl2\na\n```"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("action"));
    }

    #[tokio::test]
    async fn hovers_a_process_reference_with_its_parameter_types() {
        let text = "proc P(n: Nat, b: Bool) = delta;\ninit P(1, true);";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("P(1, true)").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            content.value.contains("P(n: Nat, b: Bool)"),
            "unexpected hover text: {}",
            content.value
        );
        assert!(content.value.contains("process"));
    }

    #[tokio::test]
    async fn hovers_a_niladic_process_reference_with_just_its_name() {
        let text = "proc Q = delta;\ninit Q();";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("Q()").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(content.value.contains("```mcrl2\nQ\n```"), "unexpected hover text: {}", content.value);
        assert!(content.value.contains("process"));
    }

    #[tokio::test]
    async fn action_hover_includes_go_to_definition_link() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let uri = Url::parse("file:///test.mcrl2").unwrap();
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: Some(&uri) };

        let offset = text.find("a(n)").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            content.value.contains("[Go to definition](file:///test.mcrl2#"),
            "unexpected hover text: {}",
            content.value
        );
    }

    #[tokio::test]
    async fn process_hover_includes_go_to_definition_link() {
        let text = "proc P = delta;\ninit P();";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let uri = Url::parse("file:///test.mcrl2").unwrap();
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: Some(&uri) };

        let offset = text.find("P()").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            content.value.contains("[Go to definition](file:///test.mcrl2#"),
            "unexpected hover text: {}",
            content.value
        );
    }

    #[tokio::test]
    async fn hover_without_doc_uri_has_no_go_to_definition_link() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("a(n)").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            !content.value.contains("Go to definition"),
            "unexpected Go to definition link: {}",
            content.value
        );
    }

    #[tokio::test]
    async fn hovers_a_sort_reference_with_its_declaration() {
        let text = "sort D = struct c1(a: Bool) | c2;\nmap f: D -> D;\ninit delta;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        // The domain `D` in `map f: D -> D;`: not part of any checked `DataExpr`, so this only
        // resolves via the sort-reference fallback.
        let offset = text.find("f: D").unwrap() + "f: ".len();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            content.value.contains("sort D = struct c1(a : Bool) | c2;"),
            "unexpected hover text: {}",
            content.value
        );
        assert!(content.value.contains("\nsort"));
    }

    #[tokio::test]
    async fn sort_reference_hover_includes_go_to_definition_link() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\ninit delta;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let uri = Url::parse("file:///test.mcrl2").unwrap();
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: Some(&uri) };

        let offset = text.rfind("D;").unwrap();
        let position = line_index.position(text, offset);
        let hover = hover(&ctx, position).expect("expected hover content");

        let HoverContents::Markup(content) = hover.contents else {
            panic!("expected markup content");
        };
        assert!(
            content.value.contains("[Go to definition](file:///test.mcrl2#"),
            "unexpected hover text: {}",
            content.value
        );
    }

    #[tokio::test]
    async fn no_hover_for_a_built_in_sort_reference() {
        // `Bool` parses straight to `SortExpressionKind::Simple`, never a named `Reference` — see
        // `crate::sort_ref`'s module doc comment — so there is no declaration for this to find.
        let text = "map f: Bool;\ninit delta;";
        let (typing_info, actions, processes, spec) = typing_info_for(text).await;
        let line_index = LineIndex::new(text);
        let ctx = HoverContext { text, line_index: &line_index, typing_info: &typing_info, actions: &actions, processes: &processes, spec: Some(&spec), doc_uri: None };

        let offset = text.find("Bool").unwrap();
        let position = line_index.position(text, offset);
        assert!(hover(&ctx, position).is_none());
    }
}
