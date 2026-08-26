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
use lsp_types::Diagnostic;
use lsp_types::DocumentSymbolParams;
use lsp_types::DocumentSymbolResponse;
use lsp_types::InitializeResult;
use lsp_types::LogMessageParams;
use lsp_types::MessageType;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::ServerInfo;
use lsp_types::Url;
use lsp_types::notification;
use lsp_types::request;

use crate::capabilities::server_capabilities;
use crate::document::Document;
use crate::document::DocumentStore;
use crate::parse;
use crate::parse::ParseOutcome;
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
    let symbols = symbols::document_symbols(&document.text, &document.line_index, spec);
    Some(DocumentSymbolResponse::Nested(symbols))
}

/// Clones out of `state` whatever [`on_change`] needs and spawns it, so parsing can `.await`
/// past this (synchronous) notification handler's borrow of `state`.
fn spawn_on_change(state: &mut Backend, uri: Url, text: String, version: i32) {
    let client = state.client.clone();
    let documents = state.documents.clone();
    tokio::spawn(on_change(client, documents, uri, text, version));
}

/// Re-parses `text` at `version` for `uri`, type checks its data specification if parsing
/// succeeded, stores the result, and publishes diagnostics for it. Used by `did_open` and
/// `did_change` (via [`spawn_on_change`]).
async fn on_change(client: ClientSocket, documents: Arc<DocumentStore>, uri: Url, text: String, version: i32) {
    let outcome = parse::parse(text.clone()).await;

    // Only meaningful once parsing succeeded — see `crate::typecheck`'s module docs for why this
    // is scoped to the data-specification subtree only.
    let typechecked = match &outcome {
        ParseOutcome::Ok(spec) => Some(typecheck::typecheck(spec.data_specification.clone()).await),
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

    let document = Document::new(text, version, outcome, typechecked);
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
