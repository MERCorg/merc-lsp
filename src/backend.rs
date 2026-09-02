//! Builds the [`Router`] that dispatches requests and notifications to parsing, diagnostics, and
//! document symbols: `async-lsp`'s equivalent of a `tower-lsp` `impl LanguageServer`.
//!
//! Notification handlers run synchronously (they return `ControlFlow`, not a `Future`), so any
//! actual work — parsing is CPU-bound and diagnostics publishing is fire-and-forget — happens on
//! a `tokio::spawn`ed task instead; see [`spawn_on_change`]/[`on_change`].

use std::ops::ControlFlow;
use std::sync::Arc;

use async_lsp::ClientSocket;
use async_lsp::router::Router;
use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::Diagnostic;
use lsp_types::DocumentSymbolParams;
use lsp_types::DocumentSymbolResponse;
use lsp_types::GotoDefinitionParams;
use lsp_types::GotoDefinitionResponse;
use lsp_types::Hover;
use lsp_types::HoverParams;
use lsp_types::InitializeResult;
use lsp_types::InlayHint;
use lsp_types::InlayHintParams;
use lsp_types::Location;
use lsp_types::LogMessageParams;
use lsp_types::MessageType;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::SemanticTokens;
use lsp_types::SemanticTokensParams;
use lsp_types::SemanticTokensResult;
use lsp_types::ServerInfo;
use lsp_types::Url;
use lsp_types::notification;
use lsp_types::request;

use crate::capabilities::server_capabilities;
use crate::completion;
use crate::document::CheckedOutcome;
use crate::document::Document;
use crate::document::DocumentStore;
use crate::goto_definition;
use crate::hover;
use crate::inlay_hints;
use crate::parse;
use crate::parse::ParseOutcome;
use crate::parse::SpecKind;
use crate::parse::Specification;
use crate::semantic_tokens;
use crate::symbols;
use crate::typecheck;

/// Per-connection server state backing the [`Router`] built by [`router`].
///
/// `documents` is `Arc`-wrapped (rather than owned directly, as it was in the `tower-lsp`
/// version's `Backend`) because notification handlers can't `.await`, so parsing happens on a
/// spawned task that outlives the handler call and needs its own shared handle to the store.
pub struct Backend {
    client: ClientSocket,
    documents: Arc<DocumentStore>,
}

/// Builds the request/notification router for a single connection to `client`.
///
/// Kept separate from the `tower::ServiceBuilder` layer stack (added by the caller, in
/// [`crate::serve`]) so `tests/protocol.rs` can exercise the exact same router construction the
/// real server runs, just without duplicating the layer stack.
pub fn router(client: ClientSocket) -> Router<Backend> {
    let mut router = Router::new(Backend {
        client,
        documents: Arc::new(DocumentStore::default()),
    });

    router
        .request::<request::Initialize, _>(|_, _params| async move {
            Ok(InitializeResult {
                capabilities: server_capabilities(),
                server_info: Some(ServerInfo {
                    name: env!("CARGO_PKG_NAME").to_string(),
                    version: Some(env!("CARGO_PKG_VERSION").to_string()),
                }),
            })
        })
        // `async_lsp::server::LifecycleLayer` handles the `initialize`/`shutdown`/`exit`
        // lifecycle state machine itself, but it still forwards `shutdown` down to us as an
        // ordinary request (falling through to `unhandled_request`'s METHOD_NOT_FOUND otherwise).
        .request::<request::Shutdown, _>(|_, ()| async move { Ok(()) })
        .request::<request::DocumentSymbolRequest, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(document_symbol(&documents, params)) }
        })
        .request::<request::SemanticTokensFullRequest, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(semantic_tokens_full(&documents, params)) }
        })
        .request::<request::HoverRequest, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(hover_request(&documents, params)) }
        })
        .request::<request::GotoDefinition, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(goto_definition_request(&documents, params)) }
        })
        .request::<request::InlayHintRequest, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(inlay_hint_request(&documents, params)) }
        })
        .request::<request::Completion, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(completion_request(&documents, params)) }
        })
        .notification::<notification::Initialized>(|state, _| {
            if let Err(error) = state.client.notify::<notification::LogMessage>(LogMessageParams {
                typ: MessageType::INFO,
                message: "merc-lsp initialized".to_string(),
            }) {
                log::warn!("failed to send initialized log message: {error}");
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidOpenTextDocument>(|state, params| {
            let doc = params.text_document;
            spawn_on_change(state, doc.uri, doc.text, doc.version);
            ControlFlow::Continue(())
        })
        .notification::<notification::DidChangeTextDocument>(|state, params| {
            // `TextDocumentSyncKind::FULL` (advertised in `capabilities::server_capabilities`)
            // means the client always sends exactly one change event containing the whole new
            // text.
            if let Some(change) = params.content_changes.into_iter().next() {
                spawn_on_change(state, params.text_document.uri, change.text, params.text_document.version);
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidSaveTextDocument>(|state, params| {
            // With FULL sync, `did_change` already re-parsed and published on every edit;
            // re-publish the already-computed diagnostics for clients that only reliably fire on
            // save.
            let uri = params.text_document.uri;
            if let Some(document) = state.documents.get(&uri) {
                let diags = document.diagnostics();
                let version = document.version;
                drop(document);
                publish_diagnostics(&state.client, uri, diags, version);
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidCloseTextDocument>(|state, params| {
            state.documents.remove(&params.text_document.uri);
            ControlFlow::Continue(())
        })
        // The default catch-all breaks the main loop on any notification with no registered
        // handler (besides `$/`-prefixed ones) — including standard ones we simply don't act on,
        // like `workspace/didChangeConfiguration`, and `exit` itself, which `LifecycleLayer`
        // forwards down to us before breaking the loop on its own. Ignore anything we don't
        // handle instead of taking the server down.
        .unhandled_notification(|_, _| ControlFlow::Continue(()));

    router
}

fn document_symbol(documents: &DocumentStore, params: DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
    let document = documents.get(&params.text_document.uri)?;
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };
    let symbols = match spec {
        Specification::Process(spec) => symbols::document_symbols(&document.text, &document.line_index, spec),
        Specification::Pbes(spec) => symbols::pbes_symbols(&document.text, &document.line_index, spec),
        Specification::Pres(spec) => symbols::pres_symbols(&document.text, &document.line_index, spec),
    };
    Some(DocumentSymbolResponse::Nested(symbols))
}

fn completion_request(documents: &DocumentStore, params: CompletionParams) -> Option<CompletionResponse> {
    let document = documents.get(&params.text_document_position.text_document.uri)?;
    // Same "no parse, nothing to offer" rule as `document_symbol`/`semantic_tokens_full` — but,
    // unlike semantic tokens, PRES gets its own pass too here: completion works off the raw parse,
    // not a checked specification, so PRES having no type checker upstream doesn't block it (see
    // `completion.rs`'s module docs).
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };
    let items = match spec {
        Specification::Process(spec) => completion::completions(spec),
        Specification::Pbes(spec) => completion::pbes_completions(spec),
        Specification::Pres(spec) => completion::pres_completions(spec),
    };
    Some(CompletionResponse::Array(items))
}

fn semantic_tokens_full(documents: &DocumentStore, params: SemanticTokensParams) -> Option<SemanticTokensResult> {
    let document = documents.get(&params.text_document.uri)?;
    // No parse, no tokens, so nothing to report. Nothing yet for PRES specifically (see
    // `parse::SpecKind`'s docs) — process specifications and PBES both have their own pass below.
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };
    let data = match spec {
        Specification::Process(spec) => semantic_tokens::semantic_tokens(&document.text, &document.line_index, spec),
        Specification::Pbes(spec) => semantic_tokens::pbes_semantic_tokens(&document.text, &document.line_index, spec),
        Specification::Pres(_) => return None,
    };
    Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))
}

/// `typing_info()` memoizes internally but still needs `&mut Document` to call (see
/// [`crate::document::Document::typing_info`]) — every handler below reaches its document through
/// `get_mut`, not `get`, for exactly that reason, even though only this one line needs the
/// mutable borrow.
fn hover_request(documents: &DocumentStore, params: HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;
    hover::hover(&document.text, &document.line_index, &typing_info, params.text_document_position_params.position)
}

fn goto_definition_request(documents: &DocumentStore, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
    let uri = params.text_document_position_params.text_document.uri.clone();
    let mut document = documents.get_mut(&uri)?;
    let typing_info = document.typing_info()?;
    let position = params.text_document_position_params.position;
    let range = goto_definition::definition_range(&document.text, &document.line_index, &typing_info, position)?;
    Some(GotoDefinitionResponse::Scalar(Location { uri, range }))
}

fn inlay_hint_request(documents: &DocumentStore, params: InlayHintParams) -> Option<Vec<InlayHint>> {
    let uri = &params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;
    let spec = document.checked_process_specification()?;
    let sort_declarations = &document.parsed_process_specification()?.data_specification.sort_declarations;
    Some(inlay_hints::inlay_hints(&document.text, &document.line_index, spec, sort_declarations, &typing_info, params.range))
}

/// Clones out of `state` whatever [`on_change`] needs and spawns it, so parsing can `.await`
/// past this (synchronous) notification handler's borrow of `state`.
fn spawn_on_change(state: &mut Backend, uri: Url, text: String, version: i32) {
    let client = state.client.clone();
    let documents = state.documents.clone();
    tokio::spawn(on_change(client, documents, uri, text, version));
}

/// Re-parses `text` at `version` for `uri` (as whichever [`SpecKind`] its extension selects),
/// type checks it if parsing succeeded and a type checker exists for the kind (a process
/// specification or a PBES; PRES has none upstream yet — see [`crate::parse`]'s docs), stores the
/// result, and publishes diagnostics for it. Used by `did_open` and `did_change` (via
/// [`spawn_on_change`]).
async fn on_change(client: ClientSocket, documents: Arc<DocumentStore>, uri: Url, text: String, version: i32) {
    let outcome = parse::parse(SpecKind::from_uri(&uri), text.clone()).await;

    // Only meaningful once parsing succeeded. Cloned (rather than moved) out of `outcome`: the
    // original stays in `outcome` below, since `symbols`/`semantic_tokens` need the raw AST
    // regardless of whether type checking succeeds. A PBES re-parses `text` itself internally
    // instead of cloning an already-parsed `UntypedPbes` — see `typecheck::typecheck_pbes`'s doc
    // comment for why.
    let checked = match &outcome {
        ParseOutcome::Ok(Specification::Process(spec)) => {
            Some(CheckedOutcome::Process(typecheck::typecheck((**spec).clone()).await))
        }
        ParseOutcome::Ok(Specification::Pbes(_)) => Some(CheckedOutcome::Pbes(typecheck::typecheck_pbes(text.clone()).await)),
        ParseOutcome::Ok(Specification::Pres(_)) => None,
        ParseOutcome::ParseError(_) | ParseOutcome::Internal(_) => None,
    };

    // `did_change` notifications for the same document can in principle be handled out of
    // order. Never let a result for an older version clobber one for a newer version that
    // already landed; discarding here is a cheap debounce against a burst of per-keystroke
    // notifications. Checked once, here, right before committing — not right after parsing —
    // since a newer version could just as well race the (also asynchronous) type check above.
    if let Some(existing) = documents.get(&uri)
        && existing.version > version
    {
        log::debug!("discarding stale parse of {uri} (version {version}, have {})", existing.version);
        return;
    }

    let document = Document::new(text, version, outcome, checked);
    let diags = document.diagnostics();
    documents.insert(uri.clone(), document);
    // Publishing is mandatory even when `diags` is empty: an empty vector is what clears any
    // diagnostics left over from a previous, failing parse.
    publish_diagnostics(&client, uri, diags, version);
}

fn publish_diagnostics(client: &ClientSocket, uri: Url, diagnostics: Vec<Diagnostic>, version: i32) {
    let params = PublishDiagnosticsParams { uri, diagnostics, version: Some(version) };
    if let Err(error) = client.notify::<notification::PublishDiagnostics>(params) {
        log::warn!("failed to publish diagnostics: {error}");
    }
}
