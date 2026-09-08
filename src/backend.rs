//! Builds the [`Router`] that dispatches requests and notifications to parsing, diagnostics, and
//! document symbols: `async-lsp`'s equivalent of a `tower-lsp` `impl LanguageServer`.
//!
//! Notification handlers run synchronously (they return `ControlFlow`, not a `Future`), so any
//! actual work — parsing/type checking is CPU-bound and diagnostics publishing is fire-and-forget
//! — happens on a `tokio::spawn`ed task instead; see [`spawn_analyze`]/[`analyze`]. That work only
//! ever runs for a `did_open` or a `did_save`, deliberately never for a `did_change` — see
//! `analyze`'s own doc comment.

use std::ops::ControlFlow;
use std::sync::Arc;

use async_lsp::ClientSocket;
use async_lsp::router::Router;
use dashmap::Entry;
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
use merc_typecheck::ModalSpecification;
use merc_typecheck::ProcessSpecification;

use crate::capabilities::server_capabilities;
use crate::completion;
use crate::completion::CompletionCategory;
use crate::completion_context;
use crate::document::CheckedOutcome;
use crate::document::Document;
use crate::document::DocumentStore;
use crate::generate;
use crate::generate::GenerateFullSpec;
use crate::goto_definition;
use crate::hover;
use crate::inlay_hints;
use crate::parse;
use crate::parse::ParseOutcome;
use crate::parse::SpecKind;
use crate::parse::Specification;
use crate::symbols;
use crate::typecheck;
use crate::virtual_document;
use crate::virtual_document::VirtualDocument;
use crate::virtual_document::VirtualDocumentStore;

/// Per-connection server state backing the [`Router`] built by [`router`].
///
/// `documents` and `virtual_documents` are both `Arc`-wrapped because notification handlers can't
/// `.await`, so parsing happens on a spawned task that outlives the handler call and needs its own
/// shared handle to each store.
pub struct Backend {
    client: ClientSocket,
    documents: Arc<DocumentStore>,
    /// See [`crate::virtual_document`]'s module doc comment.
    virtual_documents: Arc<VirtualDocumentStore>,
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
        virtual_documents: Arc::new(VirtualDocumentStore::default()),
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
        // ordinary request.
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
        .request::<VirtualDocument, _>(|state, params| {
            let virtual_documents = state.virtual_documents.clone();
            async move {
                Ok(virtual_document::virtual_document_request(
                    &virtual_documents,
                    params,
                ))
            }
        })
        .request::<GenerateFullSpec, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(generate::generate_full_spec_request(&documents, params)) }
        })
        .notification::<notification::Initialized>(|state, _| {
            if let Err(error) = state
                .client
                .notify::<notification::LogMessage>(LogMessageParams {
                    typ: MessageType::INFO,
                    message: "merc-lsp initialized".to_string(),
                })
            {
                log::warn!("failed to send initialized log message: {error}");
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidOpenTextDocument>(|state, params| {
            let doc = params.text_document;
            // The document's first (and, until a save, only) analysis. No `refresh_tokens` nudge
            // needed: the client's own initial `semanticTokens/full` request comes after this.
            spawn_analyze(state, doc.uri, doc.text, doc.version, false);
            ControlFlow::Continue(())
        })
        .notification::<notification::DidChangeTextDocument>(|state, params| {
            // Deliberately *not* a reparse: just records the latest buffer text/version on the
            // existing document for whenever the next `did_save` comes in.
            let uri = params.text_document.uri;
            if let Some(change) = params.content_changes.into_iter().next()
                && let Some(mut document) = state.documents.get_mut(&uri)
            {
                document.pending_text = change.text;
                document.pending_version = params.text_document.version;
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidSaveTextDocument>(|state, params| {
            // The buffer text at save time is whatever `did_change` last recorded as pending —
            // `analyze` does the actual (re)parsing/type checking and publishes the result, then
            // asks the client to re-pull semantic tokens, which (unlike a `did_change`) it has no
            // other reason to do on its own for a save with no further edit after it.
            let uri = params.text_document.uri;
            let pending = state
                .documents
                .get(&uri)
                .map(|document| (document.pending_text.clone(), document.pending_version));
            if let Some((text, version)) = pending {
                spawn_analyze(state, uri, text, version, true);
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidCloseTextDocument>(|state, params| {
            state.documents.remove(&params.text_document.uri);
            ControlFlow::Continue(())
        })
        // Ignore anything we don't handle instead of taking the server down.
        .unhandled_notification(|_, _| ControlFlow::Continue(()));

    router
}

fn document_symbol(
    documents: &DocumentStore,
    params: DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let document = documents.get(&params.text_document.uri)?;
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };
    let symbols = match spec {
        Specification::Process(spec) => {
            symbols::document_symbols(&document.text, &document.line_index, spec)
        }
        Specification::Pbes(spec) => {
            symbols::pbes_symbols(&document.text, &document.line_index, spec)
        }
        Specification::Pres(spec) => {
            symbols::pres_symbols(&document.text, &document.line_index, spec)
        }
        Specification::Modal(spec) => {
            symbols::modal_symbols(&document.text, &document.line_index, spec)
        }
    };
    Some(DocumentSymbolResponse::Nested(symbols))
}

fn completion_request(
    documents: &DocumentStore,
    params: CompletionParams,
) -> Option<CompletionResponse> {
    let document = documents.get(&params.text_document_position.text_document.uri)?;
    // Same "no parse, nothing to offer" rule as `document_symbol`/`semantic_tokens_full`.
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };

    // Falls back to `CompletionCategory::Unscoped` whenever the position
    // doesn't resolve to a byte offset at all.
    let offset = document
        .line_index
        .offset(&document.text, params.text_document_position.position);
    let items = match spec {
        Specification::Process(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| {
                completion_context::process_category(spec, offset)
            });
            completion::completions(spec, category)
        }
        Specification::Pbes(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| {
                completion_context::pbes_category(spec, offset)
            });
            completion::pbes_completions(spec, category)
        }
        Specification::Pres(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| {
                completion_context::pres_category(spec, offset)
            });
            completion::pres_completions(spec, category)
        }
        Specification::Modal(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| {
                completion_context::modal_category(spec, offset)
            });
            completion::modal_completions(spec, category)
        }
    };
    Some(CompletionResponse::Array(items))
}

/// Serves whatever `document.semantic_tokens` currently holds.
fn semantic_tokens_full(
    documents: &DocumentStore,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let document = documents.get(&params.text_document.uri)?;
    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: document.semantic_tokens.clone(),
    }))
}

/// `typing_info()` memoizes internally but still needs `&mut Document` to call
/// — every handler below reaches its document through `get_mut`, not `get`, for
/// exactly that reason, even though only this one line needs the mutable
/// borrow.
fn hover_request(documents: &DocumentStore, params: HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;

    // Process and modal specifications are the only kinds with `act` declarations to show, and a
    // document is checked as at most one kind at a time (see `document::CheckedOutcome`).
    let actions = document
        .checked_process_specification()
        .map(ProcessSpecification::action_declarations)
        .or_else(|| {
            document
                .checked_modal_specification()
                .map(ModalSpecification::action_declarations)
        })
        .unwrap_or(&[]);
    let processes = document
        .checked_process_specification()
        .map_or(&[][..], ProcessSpecification::process_declarations);
    let spec = match &document.parsed {
        ParseOutcome::Ok(spec) => Some(spec),
        _ => None,
    };
    let ctx = hover::HoverContext {
        text: &document.text,
        line_index: &document.line_index,
        typing_info: &typing_info,
        actions,
        processes,
        spec,
        doc_uri: Some(uri),
    };
    hover::hover(&ctx, params.text_document_position_params.position)
}

/// Almost every reference resolves to exactly one declaration, reported as
/// [`GotoDefinitionResponse::Scalar`] — a plain, single-location jump, which is what every client
/// handles best. Only a bare action name inside a `hide`/`block`/`allow`/`comm`/`rename` action set
/// can resolve to more than one `act` declaration sharing that name (see
/// [`goto_definition::definition_locations`]'s doc comment), reported as
/// [`GotoDefinitionResponse::Array`] instead so the client offers a picker rather than silently
/// jumping to just one of them.
///
/// Checked ahead of (and independently from) every other case: whether `position` sits on an
/// `%import "relative/path"` directive's own quoted path — see
/// [`goto_definition::import_directive_target`]'s doc comment for why this needs no `TypingInfo`
/// (and so no successfully checked specification) at all.
fn goto_definition_request(
    documents: &DocumentStore,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .clone();
    let position = params.text_document_position_params.position;
    let mut document = documents.get_mut(&uri)?;

    if let Some(location) = goto_definition::import_directive_target(
        &document.text,
        &document.line_index,
        parse::path_of(&uri).as_deref(),
        position,
    ) {
        return Some(GotoDefinitionResponse::Scalar(location));
    }

    let typing_info = document.typing_info()?;
    let locations = goto_definition::definition_locations(
        &document.text,
        &document.line_index,
        &document.sources,
        &document.line_indexes,
        &typing_info,
        position,
    );
    match locations.as_slice() {
        [] => None,
        [location] => Some(GotoDefinitionResponse::Scalar(location.clone())),
        _ => Some(GotoDefinitionResponse::Array(locations)),
    }
}

fn inlay_hint_request(
    documents: &DocumentStore,
    params: InlayHintParams,
) -> Option<Vec<InlayHint>> {
    let uri = &params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;

    if let Some(spec) = document.checked_process_specification() {
        let sort_declarations = &document
            .parsed_process_specification()?
            .data_specification
            .sort_declarations;
        return Some(inlay_hints::inlay_hints(
            &document.text,
            &document.line_index,
            spec,
            sort_declarations,
            &typing_info,
            params.range,
        ));
    }

    if let Some(spec) = document.checked_pbes_specification() {
        let sort_declarations = &document
            .parsed_pbes_specification()?
            .data_specification
            .sort_declarations;
        return Some(inlay_hints::pbes_inlay_hints(
            &document.text,
            &document.line_index,
            spec,
            sort_declarations,
            &typing_info,
            params.range,
        ));
    }

    if let Some(spec) = document.checked_pres_specification() {
        let sort_declarations = &document
            .parsed_pres_specification()?
            .data_specification
            .sort_declarations;
        return Some(inlay_hints::pres_inlay_hints(
            &document.text,
            &document.line_index,
            spec,
            sort_declarations,
            &typing_info,
            params.range,
        ));
    }

    let spec = document.checked_modal_specification()?;
    let sort_declarations = &document
        .parsed_modal_specification()?
        .data_specification
        .sort_declarations;
    Some(inlay_hints::modal_inlay_hints(
        &document.text,
        &document.line_index,
        spec,
        sort_declarations,
        &typing_info,
        params.range,
    ))
}

/// Clones out of `state` whatever [`analyze`] needs and spawns it, so parsing/type checking can
/// `.await` past this (synchronous) notification handler's borrow of `state`. `refresh_tokens` is
/// threaded straight through to [`analyze`] — see there for what it controls.
fn spawn_analyze(state: &mut Backend, uri: Url, text: String, version: i32, refresh_tokens: bool) {
    let client = state.client.clone();
    let documents = state.documents.clone();
    let virtual_documents = state.virtual_documents.clone();
    tokio::spawn(analyze(
        client,
        documents,
        virtual_documents,
        uri,
        text,
        version,
        refresh_tokens,
    ));
}

/// Parses `text` at `version` for `uri` (as whichever [`SpecKind`] its
/// extension selects), type checks it if parsing succeeded (every kind has a
/// type checker now — see [`crate::typecheck`]), and commits the result — text,
/// parse, type check, and semantic tokens alike — as the document's new
/// analyzed snapshot, then publishes its diagnostics.
///
/// We don't type check on every keystroke; only when this function is called,
/// which happens on save.
async fn analyze(
    client: ClientSocket,
    documents: Arc<DocumentStore>,
    virtual_documents: Arc<VirtualDocumentStore>,
    uri: Url,
    text: String,
    version: i32,
    refresh_tokens: bool,
) {
    // `path` is `None` for an untitled/unsaved buffer — `parse` falls back to a plain,
    // single-file parse for those.
    let path = parse::path_of(&uri);
    let (outcome, sources) = parse::parse(SpecKind::from_uri(&uri), text.clone(), path).await;

    // Only meaningful once parsing succeeded.
    let (checked, sources) = match &outcome {
        ParseOutcome::Ok(Specification::Process(spec)) => {
            let (result, sources) = typecheck::typecheck((**spec).clone(), sources).await;
            (Some(CheckedOutcome::Process(result)), sources)
        }
        ParseOutcome::Ok(Specification::Pbes(spec)) => (
            Some(CheckedOutcome::Pbes(
                typecheck::typecheck_pbes((**spec).clone()).await,
            )),
            sources,
        ),
        ParseOutcome::Ok(Specification::Pres(spec)) => (
            Some(CheckedOutcome::Pres(
                typecheck::typecheck_pres((**spec).clone()).await,
            )),
            sources,
        ),
        ParseOutcome::Ok(Specification::Modal(spec)) => {
            let (result, sources) = typecheck::typecheck_modal((**spec).clone(), sources).await;
            (Some(CheckedOutcome::Modal(result)), sources)
        }
        ParseOutcome::ParseError(_) | ParseOutcome::Internal(_) => (None, sources),
    };

    // Registers this analysis's virtual (Appendix-B) content for `merc/virtualDocument` to serve
    // later..
    virtual_document::register(&virtual_documents, &sources);

    let mut document = Document::new(text, version, outcome, checked, sources);
    document.semantic_tokens = document.compute_semantic_tokens();
    // Computed now, off `document` as just built, before it's handed to the map below.
    let diags = document.diagnostics();

    // Discard this analysis if it's for an older version than what's already committed.
    match documents.entry(uri.clone()) {
        Entry::Occupied(mut occupied) => {
            let existing = occupied.get();
            if existing.version > version {
                log::debug!(
                    "discarding stale analysis of {uri} (version {version}, have {})",
                    existing.version
                );
                return;
            }

            if existing.pending_version > version {
                document.pending_text = existing.pending_text.clone();
                document.pending_version = existing.pending_version;
            }

            *occupied.get_mut() = document;
        }
        Entry::Vacant(vacant) => {
            vacant.insert(document);
        }
    }

    publish_diagnostics(&client, uri, diags, version);

    if refresh_tokens {
        request_semantic_tokens_refresh(&client);
    }
}

fn publish_diagnostics(
    client: &ClientSocket,
    uri: Url,
    diagnostics: Vec<Diagnostic>,
    version: i32,
) {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: Some(version),
    };
    if let Err(error) = client.notify::<notification::PublishDiagnostics>(params) {
        log::warn!("failed to publish diagnostics: {error}");
    }
}

/// Nudges the client to re-pull semantic tokens for its open editors, via the standalone
/// `workspace/semanticTokens/refresh` request.
fn request_semantic_tokens_refresh(client: &ClientSocket) {
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(error) = client.request::<request::SemanticTokensRefresh>(()).await {
            log::debug!(
                "semanticTokens/refresh request failed (client may not support it): {error}"
            );
        }
    });
}
