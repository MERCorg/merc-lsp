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
use lsp_types::DidChangeTextDocumentParams;
use lsp_types::DidOpenTextDocumentParams;
use lsp_types::DocumentSymbolParams;
use lsp_types::DocumentSymbolResponse;
use lsp_types::InitializeParams;
use lsp_types::InitializeResult;
use lsp_types::InitializedParams;
use lsp_types::OneOf;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::SemanticTokensParams;
use lsp_types::SemanticTokensResult;
use lsp_types::SemanticTokensServerCapabilities;
use lsp_types::TextDocumentContentChangeEvent;
use lsp_types::TextDocumentIdentifier;
use lsp_types::TextDocumentItem;
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
    // Phase 1 deliberately does not advertise capabilities it doesn't implement.
    assert!(result.capabilities.definition_provider.is_none());
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
            text_document: VersionedTextDocumentIdentifier { uri: document_uri, version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: WELL_FORMED.to_string(),
            }],
        })
        .expect("didChange should be queued");
    let second = next_diagnostics(&mut rx).await;
    assert!(second.diagnostics.is_empty(), "diagnostics must be cleared once the document is fixed");
    assert_eq!(second.version, Some(2));
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
