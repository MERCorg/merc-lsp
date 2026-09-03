//! Integration test driving `merc-lsp` over a real (in-memory) LSP transport: an
//! `async_lsp::MainLoop` running the actual server (`merc_lsp::serve`) and a second one running a
//! minimal test client, connected by a `tokio::io::duplex` pipe and speaking real
//! `Content-Length`-framed JSON-RPC — exactly like a real editor and `merc-lsp` would, just over
//! a pipe instead of stdio.
//!
//! `async_lsp::router::Router`'s default behavior is to *break the main loop* on any notification
//! with no registered handler (besides `$/`-prefixed ones) — including ones a real client sends
//! as a matter of course (`workspace/didChangeConfiguration`, `exit`, …) — so the test client
//! below installs a catch-all just like `backend::router` does for the server side.

use std::ops::ControlFlow;
use std::time::Duration;

use async_lsp::MainLoop;
use async_lsp::ServerSocket;
use async_lsp::router::Router;
use lsp_types::CompletionItemKind;
use lsp_types::CompletionParams;
use lsp_types::CompletionResponse;
use lsp_types::DidChangeTextDocumentParams;
use lsp_types::DidOpenTextDocumentParams;
use lsp_types::DidSaveTextDocumentParams;
use lsp_types::DocumentSymbolParams;
use lsp_types::DocumentSymbolResponse;
use lsp_types::GotoDefinitionParams;
use lsp_types::GotoDefinitionResponse;
use lsp_types::Hover;
use lsp_types::HoverContents;
use lsp_types::HoverParams;
use lsp_types::HoverProviderCapability;
use lsp_types::InitializeParams;
use lsp_types::InitializeResult;
use lsp_types::InitializedParams;
use lsp_types::OneOf;
use lsp_types::Position;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::SemanticToken;
use lsp_types::SemanticTokensParams;
use lsp_types::SemanticTokensResult;
use lsp_types::SemanticTokensServerCapabilities;
use lsp_types::TextDocumentContentChangeEvent;
use lsp_types::TextDocumentIdentifier;
use lsp_types::TextDocumentItem;
use lsp_types::TextDocumentPositionParams;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;
use lsp_types::Url;
use lsp_types::VersionedTextDocumentIdentifier;
use lsp_types::notification;
use lsp_types::request;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

const WELL_FORMED: &str = "sort D;\ninit delta;";
const MALFORMED: &str = "sort D\ninit delta;"; // missing ';'

/// Minimal test double for an LSP client: records every `textDocument/publishDiagnostics`
/// notification the server sends and ignores everything else.
struct TestClient {
    diagnostics: UnboundedSender<PublishDiagnosticsParams>,
}

fn client_router(diagnostics: UnboundedSender<PublishDiagnosticsParams>) -> Router<TestClient> {
    let mut router = Router::new(TestClient { diagnostics });
    router
        .notification::<notification::PublishDiagnostics>(|state, params| {
            let _ = state.diagnostics.send(params);
            ControlFlow::Continue(())
        })
        .notification::<notification::LogMessage>(|_, _| ControlFlow::Continue(()))
        .notification::<notification::ShowMessage>(|_, _| ControlFlow::Continue(()))
        .unhandled_notification(|_, _| ControlFlow::Continue(()));
    router
}

fn uri(name: &str) -> Url {
    Url::parse(&format!("file:///{name}")).expect("valid test URI")
}

/// Wires up the real server ([`merc_lsp::serve`]) and the [`TestClient`] above over an in-memory
/// duplex pipe, performs the `initialize`/`initialized` handshake, and returns a handle for
/// driving the rest of the session, the `initialize` response (for capability assertions), and
/// the channel `textDocument/publishDiagnostics` notifications arrive on.
async fn start() -> (ServerSocket, InitializeResult, UnboundedReceiver<PublishDiagnosticsParams>) {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (server_read, server_write) = tokio::io::split(server_end);

    tokio::spawn(merc_lsp::serve(server_read.compat(), server_write.compat_write()));

    let (diagnostics_tx, diagnostics_rx) = tokio::sync::mpsc::unbounded_channel();
    let (client_mainloop, server) = MainLoop::new_client(|_server| client_router(diagnostics_tx));
    tokio::spawn(client_mainloop.run_buffered(client_read.compat(), client_write.compat_write()));

    let initialize_result = server
        .request::<request::Initialize>(InitializeParams::default())
        .await
        .expect("initialize should succeed");
    server
        .notify::<notification::Initialized>(InitializedParams {})
        .expect("initialized should be queued");

    (server, initialize_result, diagnostics_rx)
}

/// Waits (with a timeout, so a regression here fails in seconds rather than hanging CI) for the
/// next `textDocument/publishDiagnostics` notification.
async fn next_diagnostics(rx: &mut UnboundedReceiver<PublishDiagnosticsParams>) -> PublishDiagnosticsParams {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for a publishDiagnostics notification")
        .expect("drain channel closed unexpectedly")
}

/// Requests `textDocument/semanticTokens/full` for `uri` and returns its token data (panicking if
/// the server has nothing to report at all — a bare `None` response, distinct from the empty
/// `Vec` an unhighlighted-but-parsed document would yield).
async fn request_tokens(server: &ServerSocket, uri: &Url) -> Vec<SemanticToken> {
    let response = server
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("semanticTokens/full should succeed");
    let Some(SemanticTokensResult::Tokens(tokens)) = response else {
        panic!("expected a semanticTokens/full response, got {response:?}");
    };
    tokens.data
}

fn did_open(uri: Url, text: &str) -> DidOpenTextDocumentParams {
    DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri,
            language_id: "mcrl2".to_string(),
            version: 1,
            text: text.to_string(),
        },
    }
}

#[tokio::test]
async fn initialize_advertises_document_symbol_support() {
    let (_server, result, _rx) = start().await;
    assert_eq!(result.capabilities.document_symbol_provider, Some(OneOf::Left(true)));
}

/// The bare-`TextDocumentSyncKind` shorthand this used to advertise implicitly means *no*
/// `didSave` notifications at all.
#[tokio::test]
async fn initialize_advertises_save_notification_support() {
    let (_server, result, _rx) = start().await;
    let Some(TextDocumentSyncCapability::Options(options)) = result.capabilities.text_document_sync else {
        panic!("expected full TextDocumentSyncOptions, got {:?}", result.capabilities.text_document_sync);
    };
    assert_eq!(options.change, Some(TextDocumentSyncKind::FULL));
    assert!(options.save.is_some(), "didSave notifications must be requested");
}

#[tokio::test]
async fn initialize_advertises_hover_and_goto_definition_support() {
    let (_server, result, _rx) = start().await;
    assert_eq!(result.capabilities.hover_provider, Some(HoverProviderCapability::Simple(true)));
    assert_eq!(result.capabilities.definition_provider, Some(OneOf::Left(true)));
}

#[tokio::test]
async fn initialize_advertises_semantic_tokens_support() {
    let (_server, result, _rx) = start().await;
    let Some(SemanticTokensServerCapabilities::SemanticTokensOptions(options)) =
        result.capabilities.semantic_tokens_provider
    else {
        panic!("expected plain semanticTokens options, got {:?}", result.capabilities.semantic_tokens_provider);
    };
    assert!(!options.legend.token_types.is_empty());
}

#[tokio::test]
async fn initialize_advertises_completion_support() {
    let (_server, result, _rx) = start().await;
    assert!(result.capabilities.completion_provider.is_some());
}

#[tokio::test]
async fn did_open_with_well_formed_document_publishes_no_diagnostics() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(uri("well-formed.mcrl2"), WELL_FORMED))
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert!(diagnostics.diagnostics.is_empty());
}

#[tokio::test]
async fn did_open_with_malformed_document_publishes_a_located_diagnostic() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(uri("malformed.mcrl2"), MALFORMED))
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].source.as_deref(), Some("merc-lsp"));
}

/// A document that parses cleanly but whose data specification doesn't type check should still
/// publish a diagnostic — tagged with the distinct `"merc-lsp:types"` source (see
/// `crate::typecheck`'s module docs for why it's kept distinct from plain parse errors).
#[tokio::test]
async fn did_open_with_ill_typed_document_publishes_a_type_diagnostic() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(
            uri("ill-typed.mcrl2"),
            "map f: Bool;\neqn f = undeclared;",
        ))
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].source.as_deref(), Some("merc-lsp:types"));
}

/// Regression test for the single most common LSP bug: forgetting to publish the *empty*
/// diagnostics vector on a bad -> good transition, leaving stale squiggles in the editor forever.
///
/// Diagnostics are only surfaced on `didOpen`/`didSave` (see the next two tests for that in
/// isolation), so this drives a `didChange` followed by a `didSave` to observe the clear.
#[tokio::test]
async fn fixing_a_malformed_document_clears_its_diagnostics() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("fix-me.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), MALFORMED))
        .expect("didOpen should be queued");
    let first = next_diagnostics(&mut rx).await;
    assert_eq!(first.diagnostics.len(), 1);

    server
        .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: document_uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: WELL_FORMED.to_string(),
            }],
        })
        .expect("didChange should be queued");
    // `didChange` only records WELL_FORMED as pending text — parsing/type checking happen on the
    // spawned `didSave` analysis below, awaited via `next_diagnostics`, not here.
    server
        .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            text: None,
        })
        .expect("didSave should be queued");
    let second = next_diagnostics(&mut rx).await;
    assert!(second.diagnostics.is_empty(), "diagnostics must be cleared once the document is fixed");
    assert_eq!(second.version, Some(2));
}

/// The core of "check on save": editing a document without saving must not surface a diagnostic
/// for it.
#[tokio::test]
async fn editing_without_saving_does_not_publish_diagnostics() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("edit-only.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WELL_FORMED))
        .expect("didOpen should be queued");
    let opened = next_diagnostics(&mut rx).await;
    assert!(opened.diagnostics.is_empty());

    server
        .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: document_uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: MALFORMED.to_string(),
            }],
        })
        .expect("didChange should be queued");

    // The change above introduced a parse error, but with no `didSave` yet, no notification
    // should follow it.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), next_diagnostics(&mut rx)).await.is_err(),
        "didChange must not publish diagnostics on its own"
    );

    server
        .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            text: None,
        })
        .expect("didSave should be queued");
    let saved = next_diagnostics(&mut rx).await;
    assert_eq!(saved.diagnostics.len(), 1, "didSave should publish the diagnostics for the unsaved edit");
}

#[tokio::test]
async fn document_symbol_returns_the_outline() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("outline.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), "sort D;\nact a;\ninit a;"))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("documentSymbol should succeed");

    let Some(DocumentSymbolResponse::Nested(symbols)) = response else {
        panic!("expected a nested documentSymbol response, got {response:?}");
    };
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"D"), "expected the 'D' sort in the outline, got {names:?}");
    assert!(names.contains(&"a"), "expected the 'a' action in the outline, got {names:?}");
    assert!(names.contains(&"init"), "expected the 'init' entry in the outline, got {names:?}");
}

#[tokio::test]
async fn semantic_tokens_full_returns_tokens_for_a_parsed_document() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("tokens.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), "sort D;\nact a;\ninit a;"))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("semanticTokens/full should succeed");

    let Some(SemanticTokensResult::Tokens(tokens)) = response else {
        panic!("expected a semanticTokens/full response, got {response:?}");
    };
    // `D` (a sort declaration) and `a` (an action instantiation in `init a;`) should each yield a
    // token; the exact classification is `semantic_tokens.rs`'s own unit tests' job.
    assert!(tokens.data.len() >= 2, "expected at least a sort and an action token, got {:?}", tokens.data);
}

/// The semantic-tokens counterpart of `editing_without_saving_does_not_publish_diagnostics`: an
/// unsaved `didChange` — whether it breaks parsing outright or just introduces a new declaration —
/// must leave the previously reported semantic tokens untouched; only a `didSave` may refresh them.
#[tokio::test]
async fn semantic_tokens_do_not_blank_out_on_an_unsaved_edit_and_only_refresh_on_save() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("tokens-stable.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), "sort D;\nact a;\ninit a;"))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let initial = request_tokens(&server, &document_uri).await;
    assert!(!initial.is_empty(), "expected tokens for the initial well-formed document");

    // An unsaved edit that breaks parsing outright: `didChange` only ever records it as pending
    // text (see `Document::pending_text`), never reparses, so this must not blank out the tokens
    // already reported. No need to wait for anything to "land" — `didChange` does no async work
    // at all any more.
    server
        .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: document_uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: MALFORMED.to_string() }],
        })
        .expect("didChange should be queued");
    assert_eq!(
        request_tokens(&server, &document_uri).await,
        initial,
        "semantic tokens must not blank out on an unsaved edit that breaks parsing"
    );

    // An unsaved edit back to something well-formed, but different — still just `didChange`, no
    // `didSave` yet, so the tokens reported must still be the stale, pre-edit ones.
    let grown = "sort D;\nact a;\nact b;\ninit a;";
    server
        .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: document_uri.clone(), version: 3 },
            content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text: grown.to_string() }],
        })
        .expect("didChange should be queued");
    assert_eq!(
        request_tokens(&server, &document_uri).await,
        initial,
        "an unsaved edit must not refresh semantic tokens even once it parses again"
    );

    // Only now, on `didSave`, should the new declaration's tokens show up.
    server
        .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: document_uri.clone() },
            text: None,
        })
        .expect("didSave should be queued");
    let _ = next_diagnostics(&mut rx).await;
    assert_ne!(
        request_tokens(&server, &document_uri).await,
        initial,
        "saving should finally refresh semantic tokens to reflect the new declaration"
    );
}

/// mCRL2 text used by the hover/goto-definition tests below: a mapping `f` used once in its own
/// defining equation, so a position inside `f(x)` on the `eqn` line resolves through
/// `DataSpecification::typing_info` to `f`'s declaration on the `map` line.
const WITH_A_MAPPING: &str = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";

fn position_of(text: &str, needle: &str) -> Position {
    let offset = text.find(needle).expect("needle should occur in the fixture text");
    let prefix = &text[..offset];
    let line = prefix.matches('\n').count() as u32;
    let character = prefix.rsplit('\n').next().expect("split always yields at least one piece").len() as u32;
    Position { line, character }
}

#[tokio::test]
async fn hover_reports_a_mapping_uses_sort() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("hover.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri },
                position: position_of(WITH_A_MAPPING, "f(x) = x"),
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .expect("hover should succeed");

    let Some(Hover { contents: HoverContents::Markup(content), .. }) = response else {
        panic!("expected markup hover content, got {response:?}");
    };
    assert!(content.value.contains('f'), "expected hover text to mention 'f', got {}", content.value);
}

#[tokio::test]
async fn hover_reports_a_sort_references_declaration() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("hover-sort.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    // The domain `D` in `map f: D -> D;`: never part of any checked `DataExpr`, unlike the
    // mapping use above, so this only resolves through `merc_lsp::sort_ref`'s fallback.
    let response = server
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri },
                position: position_of(WITH_A_MAPPING, "D -> D"),
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .expect("hover should succeed");

    let Some(Hover { contents: HoverContents::Markup(content), .. }) = response else {
        panic!("expected markup hover content, got {response:?}");
    };
    assert!(content.value.contains("sort D;"), "expected hover text to show the sort declaration, got {}", content.value);
}

async fn completion_at(server: &ServerSocket, document_uri: Url, position: Position) -> Vec<lsp_types::CompletionItem> {
    let response = server
        .request::<request::Completion>(CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        })
        .await
        .expect("completion should succeed");

    let Some(CompletionResponse::Array(items)) = response else {
        panic!("expected a completion item array, got {response:?}");
    };
    items
}

/// The cursor sits on the left-hand side of an equation — a data-expression position, so
/// completion should offer the declared mapping and a data keyword, but neither a section
/// keyword like `proc` (irrelevant mid-expression) nor a sort name (`D` is not a valid value).
#[tokio::test]
async fn completion_in_a_data_expression_is_scoped_to_data_values() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("completion.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let items = completion_at(&server, document_uri, position_of(WITH_A_MAPPING, "f(x) = x")).await;

    let mapping = items.iter().find(|item| item.label == "f").expect("expected 'f' among the completions");
    assert_eq!(mapping.kind, Some(CompletionItemKind::FUNCTION));
    assert!(items.iter().any(|item| item.label == "true"), "expected a data keyword like 'true'");
    assert!(!items.iter().any(|item| item.label == "proc"), "a section keyword does not belong in a data expression");
    assert!(!items.iter().any(|item| item.label == "D"), "a sort name is not a valid data value");
}

/// The cursor sits in the sort position of a `map` signature — completion should offer the
/// declared sort and the built-in sorts, but neither the mapping itself nor a data keyword.
#[tokio::test]
async fn completion_in_a_sort_expression_is_scoped_to_sorts() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("sort-completion.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let items = completion_at(&server, document_uri, position_of(WITH_A_MAPPING, "-> D;")).await;

    assert!(items.iter().any(|item| item.label == "D"), "expected the declared sort 'D'");
    assert!(items.iter().any(|item| item.label == "Nat"), "expected a built-in sort");
    assert!(!items.iter().any(|item| item.label == "f"), "a mapping is not a valid sort");
}

/// A `.pbes` document is routed to `UntypedPbes::parse` (via `SpecKind::from_uri`), not the plain
/// mCRL2 process-specification grammar — a well-formed PBES should parse cleanly and publish no
/// diagnostics.
#[tokio::test]
async fn did_open_with_well_formed_pbes_document_publishes_no_diagnostics() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(uri("well-formed.pbes"), "pbes mu X = true;\ninit X;"))
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert!(diagnostics.diagnostics.is_empty());
}

/// A `.pres` document routes to `UntypedPres::parse` the same way; a syntax error in it should
/// still produce a located `merc-lsp` diagnostic, exactly like a malformed `.mcrl2` document does.
#[tokio::test]
async fn did_open_with_malformed_pres_document_publishes_a_located_diagnostic() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(uri("malformed.pres"), "pres mu X = 0\ninit X;")) // missing ';'
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].source.as_deref(), Some("merc-lsp"));
}

/// A `.pbes` document that parses cleanly but doesn't type check (an undeclared propositional
/// variable) should publish a type diagnostic too — `PbesSpecification::from_untyped` via
/// `typecheck::typecheck_pbes`, same distinct `"merc-lsp:types"` source as a process
/// specification's own type errors.
#[tokio::test]
async fn did_open_with_ill_typed_pbes_document_publishes_a_type_diagnostic() {
    let (server, _result, mut rx) = start().await;

    server
        .notify::<notification::DidOpenTextDocument>(did_open(uri("ill-typed.pbes"), "pbes mu X = Y;\ninit X;"))
        .expect("didOpen should be queued");

    let diagnostics = next_diagnostics(&mut rx).await;
    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].source.as_deref(), Some("merc-lsp:types"));
}

/// mCRL2 PBES text used by the hover/goto-definition tests below: an equation `X` with a parameter
/// `n`, referencing itself with `n` as the argument — the same "argument resolves back to its own
/// binder" shape [`WITH_A_MAPPING`] exercises for a process specification.
const PBES_WITH_A_PARAMETER: &str = "pbes mu X(n: Bool) = val(n) || X(n);\ninit X(true);";

/// Hover and goto-definition are generic over `TypingInfo` (`Document::typing_info`) and don't
/// otherwise care which kind of specification produced it — so a PBES document gets both for
/// free, once `PbesSpecification`'s own checked spec is stored the same way a process
/// specification's is (see `document::CheckedOutcome`).
#[tokio::test]
async fn hover_reports_a_propositional_variable_arguments_sort() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("hover.pbes");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), PBES_WITH_A_PARAMETER))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri },
                position: position_of(PBES_WITH_A_PARAMETER, "n);"),
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .expect("hover should succeed");

    let Some(Hover { contents: HoverContents::Markup(content), .. }) = response else {
        panic!("expected markup hover content, got {response:?}");
    };
    assert!(content.value.contains("Bool"), "expected hover text to mention 'Bool', got {}", content.value);
}

#[tokio::test]
async fn goto_definition_jumps_from_a_propositional_variable_argument_to_its_parameter() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("goto-definition.pbes");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), PBES_WITH_A_PARAMETER))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri.clone() },
                position: position_of(PBES_WITH_A_PARAMETER, "n);"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("goto-definition should succeed");

    let Some(GotoDefinitionResponse::Scalar(location)) = response else {
        panic!("expected a scalar goto-definition response, got {response:?}");
    };
    assert_eq!(location.uri, document_uri);
    assert_eq!(location.range.start, position_of(PBES_WITH_A_PARAMETER, "n: Bool)"));
}

/// `textDocument/documentSymbol` also works for a `.pbes` document — its outline is built by
/// `symbols::pbes_symbols`, not `symbols::document_symbols` (see `backend::document_symbol`).
#[tokio::test]
async fn document_symbol_returns_the_pbes_outline() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("pbes-outline.pbes");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), "pbes mu X(n: Bool) = true;\ninit X(true);"))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::DocumentSymbolRequest>(DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("documentSymbol should succeed");

    let Some(DocumentSymbolResponse::Nested(symbols)) = response else {
        panic!("expected a nested documentSymbol response, got {response:?}");
    };
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"X"), "expected the 'X' equation in the outline, got {names:?}");
    assert!(names.contains(&"init"), "expected the 'init' entry in the outline, got {names:?}");
}

#[tokio::test]
async fn goto_definition_jumps_from_a_mapping_use_to_its_declaration() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("goto-definition.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri.clone() },
                position: position_of(WITH_A_MAPPING, "f(x) = x"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("goto-definition should succeed");

    let Some(GotoDefinitionResponse::Scalar(location)) = response else {
        panic!("expected a scalar goto-definition response, got {response:?}");
    };
    assert_eq!(location.uri, document_uri);
    assert_eq!(location.range.start, position_of(WITH_A_MAPPING, "f: D -> D"));
}

#[tokio::test]
async fn goto_definition_jumps_from_a_sort_reference_to_its_declaration() {
    let (server, _result, mut rx) = start().await;
    let document_uri = uri("goto-definition-sort.mcrl2");

    server
        .notify::<notification::DidOpenTextDocument>(did_open(document_uri.clone(), WITH_A_MAPPING))
        .expect("didOpen should be queued");
    let _ = next_diagnostics(&mut rx).await;

    let response = server
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: document_uri.clone() },
                position: position_of(WITH_A_MAPPING, "D -> D"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .expect("goto-definition should succeed");

    let Some(GotoDefinitionResponse::Scalar(location)) = response else {
        panic!("expected a scalar goto-definition response, got {response:?}");
    };
    assert_eq!(location.uri, document_uri);
    assert_eq!(location.range.start, position_of(WITH_A_MAPPING, "D;"));
}
