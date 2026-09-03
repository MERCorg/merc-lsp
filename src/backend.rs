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
use merc_typecheck::ProcessSpecification;

use crate::capabilities::server_capabilities;
use crate::completion;
use crate::completion::CompletionCategory;
use crate::completion_context;
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
            // The document's first (and, until a save, only) analysis. No `refresh_tokens` nudge
            // needed: the client's own initial `semanticTokens/full` request comes after this.
            spawn_analyze(state, doc.uri, doc.text, doc.version, false);
            ControlFlow::Continue(())
        })
        .notification::<notification::DidChangeTextDocument>(|state, params| {
            // Deliberately *not* a reparse: just records the latest buffer text/version on the
            // existing document for whenever the next `did_save` comes in — see `analyze`'s doc
            // comment for why parsing/type checking never run here. `TextDocumentSyncKind::FULL`
            // (advertised in `capabilities::server_capabilities`) means the client always sends
            // exactly one change event containing the whole new text.
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
            let pending = state.documents.get(&uri).map(|document| (document.pending_text.clone(), document.pending_version));
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
    // Falls back to `CompletionCategory::Unscoped` whenever the position
    // doesn't resolve to a byte offset at all — a client sending a position
    // outside the document is not reason enough to offer nothing.
    let offset = document.line_index.offset(&document.text, params.text_document_position.position);
    let items = match spec {
        Specification::Process(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| completion_context::process_category(spec, offset));
            completion::completions(spec, category)
        }
        Specification::Pbes(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| completion_context::pbes_category(spec, offset));
            completion::pbes_completions(spec, category)
        }
        Specification::Pres(spec) => {
            let category = offset.map_or(CompletionCategory::Unscoped, |offset| completion_context::pres_category(spec, offset));
            completion::pres_completions(spec, category)
        }
    };
    Some(CompletionResponse::Array(items))
}

/// Serves whatever `document.semantic_tokens` currently holds — deliberately *not* a fresh
/// recomputation off the live `document.parsed`/`document.text`; see that field's own doc comment
/// for why it lags a `did_change` on purpose.
fn semantic_tokens_full(documents: &DocumentStore, params: SemanticTokensParams) -> Option<SemanticTokensResult> {
    let document = documents.get(&params.text_document.uri)?;
    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: document.semantic_tokens.clone(),
    }))
}

/// `typing_info()` memoizes internally but still needs `&mut Document` to call (see
/// [`crate::document::Document::typing_info`]) — every handler below reaches its document through
/// `get_mut`, not `get`, for exactly that reason, even though only this one line needs the
/// mutable borrow.
fn hover_request(documents: &DocumentStore, params: HoverParams) -> Option<Hover> {
    let uri = &params.text_document_position_params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;
    let actions = document.checked_process_specification().map_or(&[][..], ProcessSpecification::action_declarations);
    let processes = document.checked_process_specification().map_or(&[][..], ProcessSpecification::process_declarations);
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
/// [`goto_definition::definition_ranges`]'s doc comment), reported as
/// [`GotoDefinitionResponse::Array`] instead so the client offers a picker rather than silently
/// jumping to just one of them.
fn goto_definition_request(documents: &DocumentStore, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
    let uri = params.text_document_position_params.text_document.uri.clone();
    let mut document = documents.get_mut(&uri)?;
    let typing_info = document.typing_info()?;
    let position = params.text_document_position_params.position;
    let ranges = goto_definition::definition_ranges(&document.text, &document.line_index, &typing_info, position);
    match ranges.as_slice() {
        [] => None,
        [range] => Some(GotoDefinitionResponse::Scalar(Location { uri, range: *range })),
        _ => Some(GotoDefinitionResponse::Array(
            ranges.into_iter().map(|range| Location { uri: uri.clone(), range }).collect(),
        )),
    }
}

fn inlay_hint_request(documents: &DocumentStore, params: InlayHintParams) -> Option<Vec<InlayHint>> {
    let uri = &params.text_document.uri;
    let mut document = documents.get_mut(uri)?;
    let typing_info = document.typing_info()?;
    if let Some(spec) = document.checked_process_specification() {
        let sort_declarations = &document.parsed_process_specification()?.data_specification.sort_declarations;
        return Some(inlay_hints::inlay_hints(&document.text, &document.line_index, spec, sort_declarations, &typing_info, params.range));
    }
    let spec = document.checked_pbes_specification()?;
    let sort_declarations = &document.parsed_pbes_specification()?.data_specification.sort_declarations;
    Some(inlay_hints::pbes_inlay_hints(&document.text, &document.line_index, spec, sort_declarations, &typing_info, params.range))
}

/// Clones out of `state` whatever [`analyze`] needs and spawns it, so parsing/type checking can
/// `.await` past this (synchronous) notification handler's borrow of `state`. `refresh_tokens` is
/// threaded straight through to [`analyze`] — see there for what it controls.
fn spawn_analyze(state: &mut Backend, uri: Url, text: String, version: i32, refresh_tokens: bool) {
    let client = state.client.clone();
    let documents = state.documents.clone();
    tokio::spawn(analyze(client, documents, uri, text, version, refresh_tokens));
}

/// Parses `text` at `version` for `uri` (as whichever [`SpecKind`] its extension selects), type
/// checks it if parsing succeeded and a type checker exists for the kind (a process specification
/// or a PBES; PRES has none upstream yet — see [`crate::parse`]'s docs), and commits the result —
/// text, parse, type check, and semantic tokens alike — as the document's new analyzed snapshot,
/// then publishes its diagnostics.
///
/// This is deliberately the *only* place any of that (expensive) work happens: `did_open` calls it
/// immediately, and `did_save` calls it on whatever `did_change` has been recording as
/// `Document::pending_text`/`pending_version` in the meantime — `did_change` itself never does,
/// see `router`'s `DidChangeTextDocument` handler. Re-parsing and type checking mCRL2 on every
/// keystroke would make editing sluggish for no benefit, since none of the analyzed snapshot is
/// shown to the client before a save anyway.
///
/// `refresh_tokens` asks the client (via [`request_semantic_tokens_refresh`]) to re-pull semantic
/// tokens once this lands — needed after a `did_save`, which unlike a `did_change` gives the
/// client no reason of its own to re-request them; `did_open`'s first analysis needs no such nudge,
/// since the client's own initial semantic-tokens request comes after this call is spawned.
async fn analyze(client: ClientSocket, documents: Arc<DocumentStore>, uri: Url, text: String, version: i32, refresh_tokens: bool) {
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

    let mut document = Document::new(text, version, outcome, checked);
    document.semantic_tokens = document.compute_semantic_tokens();
    // Computed now, off `document` as just built, before it's handed to the map below — a
    // `did_save` later reads diagnostics straight off `document.diagnostics()` too, but this
    // publish is the only mandatory one: an empty `diags` is what clears any diagnostics left
    // over from a previous, failing parse, so it always has to go out, even when there's nothing
    // to report.
    let diags = document.diagnostics();

    // Only `did_save` calls this (`did_open` runs once, against a document nothing else has
    // touched yet), and only ever with the *current* `pending_version` it just read — but by the
    // time this `.await`-heavy parse/type check finishes, a `did_change` can easily have recorded
    // a newer edit past it. Discard this result outright if it's for an outright older version
    // than what's already committed (out-of-order saves); otherwise commit it, but keep whichever
    // of the two `pending_*` pairs is newer, so a save in flight never erases an edit `did_change`
    // recorded after it started.
    match documents.entry(uri.clone()) {
        dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
            let existing = occupied.get();
            if existing.version > version {
                log::debug!("discarding stale analysis of {uri} (version {version}, have {})", existing.version);
                return;
            }
            if existing.pending_version > version {
                document.pending_text = existing.pending_text.clone();
                document.pending_version = existing.pending_version;
            }
            *occupied.get_mut() = document;
        }
        dashmap::mapref::entry::Entry::Vacant(vacant) => {
            vacant.insert(document);
        }
    }

    publish_diagnostics(&client, uri, diags, version);
    if refresh_tokens {
        request_semantic_tokens_refresh(&client);
    }
}

fn publish_diagnostics(client: &ClientSocket, uri: Url, diagnostics: Vec<Diagnostic>, version: i32) {
    let params = PublishDiagnosticsParams { uri, diagnostics, version: Some(version) };
    if let Err(error) = client.notify::<notification::PublishDiagnostics>(params) {
        log::warn!("failed to publish diagnostics: {error}");
    }
}

/// Nudges the client to re-pull semantic tokens for its open editors, via the standalone
/// `workspace/semanticTokens/refresh` request. Needed because `document.semantic_tokens` (see its
/// doc comment) only actually changes on a `did_save`, which is not itself an event a client's own
/// semantic-tokens machinery would otherwise treat as a reason to re-request — unlike a
/// `did_change`, which every editor already re-requests tokens after on its own. Fire-and-forget:
/// a client with no interest in semantic tokens at all is free to not implement this request, so a
/// failure here is unremarkable, not worth surfacing above `debug`.
fn request_semantic_tokens_refresh(client: &ClientSocket) {
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(error) = client.request::<request::SemanticTokensRefresh>(()).await {
            log::debug!("semanticTokens/refresh request failed (client may not support it): {error}");
        }
    });
}
