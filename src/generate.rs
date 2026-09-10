//! Serves the `merc/generateFullSpec` request: renders a process specification's already-merged
//! (see `parse.rs`'s "`%import` and the returned `SourceMap`" section — every `%import`, however
//! deeply nested, is resolved and flattened before this ever runs) [`UntypedProcessSpecification`]
//! back to mCRL2 source text via its `Display` impl.
//!
//! That `Display` impl (in `merc_syntax::syntax_tree_display`) unconditionally parenthesizes every
//! binary/unary/quantifier/lambda/conditional, so the text this produces is always unambiguous —
//! deliberately more heavily parenthesized than the real mCRL2 toolset's own pretty printer, which
//! only adds parens precedence actually requires. That's the point, not a shortcoming: it's exactly
//! what sidesteps the divergence [`crate::ambiguity`] documents between merc's Pratt parser and
//! mCRL2's real dparser-based one on "deep priority conflicts" — a parenthesized group is read the
//! same way by both grammars, so output from here is safe to feed into the real mCRL2 tools even
//! where the two parsers would otherwise disagree.
//!
//! Scoped to process specifications only: PBES/PRES don't resolve `%import` yet (see `parse.rs`),
//! and a modal formula is consumed by mCRL2's tools alongside the process specification it's
//! checked against, not as a standalone spec of its own, so there is nothing useful to inline for
//! one.
//!
//! Writing the result to disk (so `mcrl22lps`/`lpsxsim`/... can actually consume it) is the
//! client's job, same as every other filesystem concern in this server — see
//! `vscode-client/src/extension.ts`'s `merc-lsp.generateFullSpec` command.

use lsp_types::Url;
use lsp_types::request::Request;
use serde::Deserialize;
use serde::Serialize;

use crate::document::DocumentStore;

/// Request parameters: the document whose merged specification should be rendered.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateFullSpecParams {
    pub uri: Url,
}

/// Renders `uri`'s merged process specification to mCRL2 source text, or `null` (`None`) if `uri`
/// isn't a currently-open, successfully-parsed process specification.
pub enum GenerateFullSpec {}

impl Request for GenerateFullSpec {
    type Params = GenerateFullSpecParams;
    type Result = Option<String>;
    const METHOD: &'static str = "merc/generateFullSpec";
}

/// The actual request handler: looks `params.uri` up in `documents` and, if it's a process
/// specification that both parsed *and* type checked, renders it via [`std::fmt::Display`].
pub fn generate_full_spec_request(
    documents: &DocumentStore,
    params: GenerateFullSpecParams,
) -> Option<String> {
    let document = documents.get(&params.uri)?;
    document.checked_process_specification()?;
    let spec = document.parsed_process_specification()?;
    Some(spec.to_string())
}

#[cfg(test)]
mod tests {
    use merc_syntax::SourceMap;

    use super::*;
    use crate::document::CheckedOutcome;
    use crate::document::Document;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse_ignoring_sources;

    #[tokio::test]
    async fn renders_a_well_typed_process_specification() {
        let text = "act a;\ninit a;".to_string();
        let parsed = parse_ignoring_sources(SpecKind::Process, text.clone()).await;
        let spec = match &parsed {
            ParseOutcome::Ok(Specification::Process(spec)) => (**spec).clone(),
            _ => panic!("fixture failed to parse"),
        };
        let checked = crate::typecheck::typecheck_ignoring_sources(spec).await;
        let document = Document::new(text, 0, parsed, Some(CheckedOutcome::Process(checked)), SourceMap::new());

        let documents = DocumentStore::default();
        let uri: Url = "file:///a.mcrl2".parse().unwrap();
        documents.insert(uri.clone(), document);

        let rendered = generate_full_spec_request(&documents, GenerateFullSpecParams { uri })
            .expect("should render");
        assert!(rendered.contains("act"));
        assert!(rendered.contains("init a;"));
    }

    #[tokio::test]
    async fn is_none_for_an_ill_typed_process_specification() {
        // `undeclared` isn't declared anywhere, so this parses but fails to type check.
        let text = "map f: Bool;\neqn f = undeclared;\ninit delta;".to_string();
        let parsed = parse_ignoring_sources(SpecKind::Process, text.clone()).await;
        let spec = match &parsed {
            ParseOutcome::Ok(Specification::Process(spec)) => (**spec).clone(),
            _ => panic!("fixture failed to parse"),
        };
        let checked = crate::typecheck::typecheck_ignoring_sources(spec).await;
        assert!(matches!(checked, crate::typecheck::TypecheckOutcome::Error(_)));
        let document = Document::new(text, 0, parsed, Some(CheckedOutcome::Process(checked)), SourceMap::new());

        let documents = DocumentStore::default();
        let uri: Url = "file:///ill-typed.mcrl2".parse().unwrap();
        documents.insert(uri.clone(), document);

        assert!(generate_full_spec_request(&documents, GenerateFullSpecParams { uri }).is_none());
    }

    #[tokio::test]
    async fn is_none_for_a_document_that_failed_to_parse() {
        let text = "sort D".to_string(); // missing terminating ';'
        let parsed = parse_ignoring_sources(SpecKind::Process, text.clone()).await;
        assert!(matches!(parsed, ParseOutcome::ParseError(_)));
        let document = Document::new(text, 0, parsed, None, SourceMap::new());

        let documents = DocumentStore::default();
        let uri: Url = "file:///broken.mcrl2".parse().unwrap();
        documents.insert(uri.clone(), document);

        assert!(generate_full_spec_request(&documents, GenerateFullSpecParams { uri }).is_none());
    }

    #[tokio::test]
    async fn is_none_for_a_pbes_document() {
        let text = "pbes mu X = true;\ninit X;".to_string();
        let parsed = parse_ignoring_sources(SpecKind::Pbes, text.clone()).await;
        assert!(matches!(&parsed, ParseOutcome::Ok(Specification::Pbes(_))));
        let document = Document::new(text, 0, parsed, None, SourceMap::new());

        let documents = DocumentStore::default();
        let uri: Url = "file:///a.pbes".parse().unwrap();
        documents.insert(uri.clone(), document);

        assert!(generate_full_spec_request(&documents, GenerateFullSpecParams { uri }).is_none());
    }

    #[test]
    fn is_none_for_an_unopened_document() {
        let documents = DocumentStore::default();
        let uri: Url = "file:///nowhere.mcrl2".parse().unwrap();
        assert!(generate_full_spec_request(&documents, GenerateFullSpecParams { uri }).is_none());
    }
}
