//! Builds the [`Router`] that dispatches requests and notifications to parsing, diagnostics, and
//! document symbols: `async-lsp`'s equivalent of a `tower-lsp` `impl LanguageServer`.
//!
//! Notification handlers run synchronously (they return `ControlFlow`, not a `Future`), so any
//! actual work — parsing/type checking is CPU-bound and diagnostics publishing is fire-and-forget
//! — happens on a `tokio::spawn`ed task instead; see [`spawn_analyze`]/[`analyze`]. That work only
//! ever runs for a `did_open`, a `did_save`, or a stale `merc/didFocusTextDocument` (see
//! [`crate::focus`]), deliberately never for a `did_change` — see `analyze`'s own doc comment.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

use async_lsp::ClientSocket;
use async_lsp::router::Router;
use dashmap::DashMap;
use dashmap::Entry;
use lsp_types::CompletionList;
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
use crate::code_action;
use crate::completion;
use crate::completion::CompletionCategory;
use crate::completion_context;
use crate::convert;
use crate::convert::LineIndex;
use crate::document::CheckedOutcome;
use crate::document::Document;
use crate::document::DocumentStore;
use crate::focus::DidFocusTextDocument;
use crate::focus::DidFocusTextDocumentParams;
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

/// Diagnostics published against a URI on some document's behalf because a span in its analysis
/// actually landed there (see [`Document::diagnostics`]'s doc comment) — e.g. a type error inside
/// an `%import`ed file — keyed first by that target URI, then by which importing document
/// currently contributes to it. More than one open document can `%import` the same file, so a
/// target's published diagnostics are the union across every owner still contributing to it;
/// tracking owners separately is what lets [`analyze`] and `did_close` each update or clear just
/// their own contribution without clobbering another importer's still-valid one for the same file.
type ForeignDiagnostics = DashMap<Url, HashMap<Url, Vec<Diagnostic>>>;

/// Per-connection server state backing the [`Router`] built by [`router`].
///
/// `documents`, `virtual_documents`, and `foreign_diagnostics` are all `Arc`-wrapped because
/// notification handlers can't `.await`, so parsing happens on a spawned task that outlives the
/// handler call and needs its own shared handle to each store; `Backend` itself is `Clone` (cheap —
/// every field is a handle) so a spawned task can take one bundled clone of everything it needs
/// (see [`spawn_analyze`]/[`analyze`]) instead of each of those handles as its own parameter.
#[derive(Clone)]
pub struct Backend {
    client: ClientSocket,
    documents: Arc<DocumentStore>,
    /// See [`crate::virtual_document`]'s module doc comment.
    virtual_documents: Arc<VirtualDocumentStore>,
    foreign_diagnostics: Arc<ForeignDiagnostics>,
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
        foreign_diagnostics: Arc::new(ForeignDiagnostics::default()),
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
        .request::<request::CodeActionRequest, _>(|state, params| {
            let documents = state.documents.clone();
            async move { Ok(code_action::code_actions(&documents, params)) }
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
            // The document's first (and, until a save, only) analysis. No `refresh_views` nudge
            // needed: the client's own initial `semanticTokens/full`/`inlayHint` requests come
            // after this.
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
            // A closed document may have been the sole reason a broken `%import`ed file (or
            // another foreign URI) had diagnostics published against it at all — release just this
            // document's own contribution (see `ForeignDiagnostics`'s doc comment) and republish
            // whatever, if anything, other still-open importers still legitimately contribute.
            if let Some((_, document)) = state.documents.remove(&params.text_document.uri) {
                for target in document.published_foreign_uris {
                    let remaining = release_foreign_diagnostics(&state.foreign_diagnostics, &target, &params.text_document.uri);
                    publish_diagnostics(&state.client, target, remaining, None);
                }
            }
            ControlFlow::Continue(())
        })
        .notification::<DidFocusTextDocument>(|state, params: DidFocusTextDocumentParams| {
            // See `crate::focus`'s module doc comment for why this exists at all. Only reanalyzes
            // when `Document::is_stale` actually finds a changed import — the common case (nothing
            // changed since this document was last analyzed) does no work beyond that check.
            let stale = state
                .documents
                .get(&params.uri)
                .filter(|document| document.is_stale())
                .map(|document| (document.text.clone(), document.version));
            if let Some((text, version)) = stale {
                spawn_analyze(state, params.uri, text, version, true);
            }
            ControlFlow::Continue(())
        })
        .notification::<notification::DidChangeWatchedFiles>(|state, params| {
            // The client watches every `.mcrl2`/`.pbes`/`.pres`/`.mcf` file in the workspace (see
            // `vscode-client/src/extension.ts`'s `synchronize.fileEvents`) and forwards every
            // create/change/delete here. Deciding which open documents that actually affects means
            // a `fs::metadata` call per file each document's own last analysis pulled in (see
            // `Document::is_stale`) plus, for `has_newly_available_import`, a directory join per
            // `%import` directive in every open document — real I/O and (with many open documents)
            // real work, so it's done on a spawned task rather than blocking this notification
            // handler, which can't `.await` anyway.
            let backend = state.clone();
            tokio::spawn(async move {
                let changed_paths: Vec<std::path::PathBuf> = params.changes.iter().filter_map(|change| change.uri.to_file_path().ok()).collect();
                let stale: Vec<(Url, String, i32)> = backend
                    .documents
                    .iter()
                    .filter(|entry| {
                        let document = entry.value();
                        document.is_stale() || document.has_newly_available_import(parse::path_of(entry.key()).as_deref(), &changed_paths)
                    })
                    .map(|entry| (entry.key().clone(), entry.value().text.clone(), entry.value().version))
                    .collect();
                for (uri, text, version) in stale {
                    tokio::spawn(analyze(backend.clone(), uri, text, version, true));
                }
            });
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
            symbols::document_symbols(&document.text, &document.line_index, &document.sources, spec)
        }
        Specification::Pbes(spec) => {
            symbols::pbes_symbols(&document.text, &document.line_index, &document.sources, spec)
        }
        Specification::Pres(spec) => {
            symbols::pres_symbols(&document.text, &document.line_index, &document.sources, spec)
        }
        Specification::Modal(spec) => {
            symbols::modal_symbols(&document.text, &document.line_index, &document.sources, spec)
        }
    };
    Some(DocumentSymbolResponse::Nested(symbols))
}

fn completion_request(
    documents: &DocumentStore,
    params: CompletionParams,
) -> Option<CompletionResponse> {
    let uri = &params.text_document_position.text_document.uri;
    let document = documents.get(uri)?;
    let position = params.text_document_position.position;

    // Checked ahead of (and independently from) the AST-driven categories below, the same way
    // `goto_definition_request` checks `import_directive_target` first — an `%import` directive
    // isn't part of any of the four grammars this crate parses (see `parse.rs`'s module docs), so
    // this needs no successful parse at all, unlike everything below it. Deliberately read off
    // `pending_text` (the live buffer `did_change` last recorded), not `text` (the snapshot from
    // whenever this document was last opened/saved/focused) — unlike everything below, which needs
    // a fresh parse and so is stuck on that snapshot until the next `analyze` run, this needs
    // nothing but the raw text, so there's no reason to make the user save just to see the
    // directory listing for a path they're still typing.
    let pending_line_index = LineIndex::new(&document.pending_text);
    if let Some(items) = completion::import_path_completions(&document.pending_text, &pending_line_index, parse::path_of(uri).as_deref(), position) {
        // `is_incomplete: true` (not a plain `Array`, which implies `false`) — an item's `label`
        // is a bare file/directory name (`"common.mcrl2"`, `"sub/"`), never prefixed with
        // whatever path segment the user already typed, so it only ever literally prefix-matches
        // client-side filtering by sheer coincidence (typing into an empty path, or the exact
        // start of a name). The moment the user types a `.` (`./`, `../`) or anything else that
        // isn't itself a name prefix, a client that filters this same list locally instead of
        // asking again finds nothing and the suggestions silently vanish; flagging the list
        // incomplete tells it to always re-request instead of ever reusing a stale one.
        return Some(CompletionResponse::List(CompletionList { is_incomplete: true, items }));
    }

    // Same "no parse, nothing to offer" rule as `document_symbol`/`semantic_tokens_full`.
    let ParseOutcome::Ok(spec) = &document.parsed else {
        return None;
    };

    // Falls back to `CompletionCategory::Unscoped` whenever the position
    // doesn't resolve to a byte offset at all.
    let offset = document.line_index.offset(&document.text, position);
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
        sources: &document.sources,
        line_indexes: &document.line_indexes,
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

    // Read off `pending_text` (the live buffer), not `text` (the last-analyzed snapshot) — same
    // reasoning as `completion_request`'s own import-path handling just above: an `%import`
    // directive isn't part of any of the four grammars this crate parses, so resolving one needs
    // nothing but the raw text, and shouldn't be stuck on a stale offset an unsaved edit has since
    // shifted.
    let pending_line_index = LineIndex::new(&document.pending_text);
    if let Some(link) = goto_definition::import_directive_target(
        &document.pending_text,
        &pending_line_index,
        parse::path_of(&uri).as_deref(),
        position,
    ) {
        return Some(GotoDefinitionResponse::Link(vec![link]));
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
            &document.sources,
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
            &document.sources,
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
            &document.sources,
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
        &document.sources,
        params.range,
    ))
}

/// Clones out of `state` whatever [`analyze`] needs and spawns it, so parsing/type checking can
/// `.await` past this (synchronous) notification handler's borrow of `state`. `refresh_views` is
/// threaded straight through to [`analyze`] — see there for what it controls.
fn spawn_analyze(state: &mut Backend, uri: Url, text: String, version: i32, refresh_views: bool) {
    // A `merc-builtin:` virtual document (see `crate::virtual_document`'s module doc comment) has
    // no `%import`s and is never itself a real, standalone specification — it's a fragment of one
    // (e.g. `Bool`'s declaration), extracted for display. Parsing and type checking it as if it
    // were a whole document would spuriously report every name it doesn't itself declare as
    // undeclared, and the VS Code client's `documentSelector` opens it as an ordinary document (see
    // `registerVirtualDocumentProvider` in `vscode-client/src/extension.ts`), so this is the one
    // place that needs to know to skip it.
    if uri.scheme() == convert::VIRTUAL_DOCUMENT_SCHEME {
        return;
    }

    tokio::spawn(analyze(state.clone(), uri, text, version, refresh_views));
}

/// Parses `text` at `version` for `uri` (as whichever [`SpecKind`] its
/// extension selects), type checks it if parsing succeeded (every kind has a
/// type checker now — see [`crate::typecheck`]), and commits the result — text,
/// parse, type check, and semantic tokens alike — as the document's new
/// analyzed snapshot, then publishes its diagnostics.
///
/// We don't type check on every keystroke; only when this function is called,
/// which happens on save.
async fn analyze(backend: Backend, uri: Url, text: String, version: i32, refresh_views: bool) {
    let Backend { client, documents, virtual_documents, foreign_diagnostics } = backend;

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
    // later.
    virtual_document::register(&virtual_documents, &sources);

    let mut document = Document::new(text, version, outcome, checked, sources);
    document.semantic_tokens = document.compute_semantic_tokens();
    // Computed now, off `document` as just built, before it's handed to the map below. Grouped by
    // the URI each diagnostic actually belongs to (see `Document::diagnostics`'s doc comment): a
    // diagnostic about content this document `%import`s is published against that file's own URI,
    // with its own real range, rather than mislocated against `uri` itself.
    let mut diags_by_uri: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for (target_uri, diagnostic) in document.diagnostics(&uri) {
        diags_by_uri.entry(target_uri).or_default().push(diagnostic);
    }
    // Always publish (even if empty) against `uri` itself — an empty list is what clears any
    // diagnostics left over from a previous, failing analysis.
    diags_by_uri.entry(uri.clone()).or_default();
    let foreign_uris: Vec<Url> = diags_by_uri.keys().filter(|&target| target != &uri).cloned().collect();
    document.published_foreign_uris = foreign_uris.clone();

    // Diagnostics from a previous analysis, published against a foreign URI that this one no
    // longer has anything to say about, must be explicitly cleared (published as an empty list) —
    // nothing else would ever tell the client to drop them, since they were never tied to `uri`'s
    // own document version in the first place.
    let mut stale_foreign_uris = Vec::new();

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

            stale_foreign_uris.extend(
                existing
                    .published_foreign_uris
                    .iter()
                    .filter(|old| !foreign_uris.contains(old))
                    .cloned(),
            );

            *occupied.get_mut() = document;
        }
        Entry::Vacant(vacant) => {
            vacant.insert(document);
        }
    }

    for (target_uri, diagnostics) in diags_by_uri {
        if target_uri == uri {
            // Only `uri` itself has a meaningful document version from the client's perspective.
            publish_diagnostics(&client, target_uri, diagnostics, Some(version));
        } else {
            // Published as the union of every document currently contributing diagnostics to this
            // foreign target (see `ForeignDiagnostics`'s doc comment) — not just this analysis's
            // own, which would clobber another still-open importer's diagnostics for the same file.
            let union = record_foreign_diagnostics(&foreign_diagnostics, &target_uri, &uri, diagnostics);
            publish_diagnostics(&client, target_uri, union, None);
        }
    }
    for stale_uri in stale_foreign_uris {
        let remaining = release_foreign_diagnostics(&foreign_diagnostics, &stale_uri, &uri);
        publish_diagnostics(&client, stale_uri, remaining, None);
    }

    if refresh_views {
        // Both are standalone `workspace/*/refresh` requests, not tied to `uri`: the client
        // decides which of its own open editors to re-pull them for. Needed because this
        // reanalysis has no accompanying `did_change` of its own to make the client re-request
        // either on its own — most obviously for the stale-import case (see `crate::focus`'s
        // module doc comment), where the document text hasn't changed at all from the client's
        // point of view, so nothing else would ever tell it the old inlay hints (still anchored
        // to spans/names from before the `%import`ed file's edit) are now stale too.
        request_semantic_tokens_refresh(&client);
        request_inlay_hint_refresh(&client);
    }
}

/// The union of every owner's diagnostics, without duplicates — two documents `%import`ing the
/// same file are likely to independently compute the exact same diagnostic for it, which would
/// otherwise show up as two identical squiggles for one real error.
fn union_diagnostics<'a>(owners: impl Iterator<Item = &'a Vec<Diagnostic>>) -> Vec<Diagnostic> {
    let mut result: Vec<Diagnostic> = Vec::new();
    for diagnostic in owners.flatten() {
        if !result.contains(diagnostic) {
            result.push(diagnostic.clone());
        }
    }
    result
}

/// Records `owner`'s current contribution to `target`'s [`ForeignDiagnostics`], replacing whatever
/// it contributed at its last analysis, and returns the union across every owner to publish.
fn record_foreign_diagnostics(foreign_diagnostics: &ForeignDiagnostics, target: &Url, owner: &Url, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut owners = foreign_diagnostics.entry(target.clone()).or_default();
    owners.insert(owner.clone(), diagnostics);
    union_diagnostics(owners.values())
}

/// Removes `owner`'s own contribution to `target`'s [`ForeignDiagnostics`] — because `owner` closed
/// or no longer produces anything for `target` — and returns the union of what every other owner
/// still contributes, to republish in its place.
fn release_foreign_diagnostics(foreign_diagnostics: &ForeignDiagnostics, target: &Url, owner: &Url) -> Vec<Diagnostic> {
    let Some(mut owners) = foreign_diagnostics.get_mut(target) else {
        return Vec::new();
    };
    owners.remove(owner);
    let remaining = union_diagnostics(owners.values());
    if owners.is_empty() {
        drop(owners);
        foreign_diagnostics.remove(target);
    }
    remaining
}

fn publish_diagnostics(
    client: &ClientSocket,
    uri: Url,
    diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
) {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version,
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

/// As [`request_semantic_tokens_refresh`], via the standalone `workspace/inlayHint/refresh`
/// request, so the client re-pulls inlay hints too rather than leaving stale ones on screen.
fn request_inlay_hint_refresh(client: &ClientSocket) {
    let client = client.clone();
    tokio::spawn(async move {
        if let Err(error) = client
            .request::<request::InlayHintRefreshRequest>(())
            .await
        {
            log::debug!("inlayHint/refresh request failed (client may not support it): {error}");
        }
    });
}
