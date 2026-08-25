//! Integration test driving `merc-lsp` through `LspService` directly (no stdio framing), per
//! PLAN.md §5: `initialize` -> `initialized` -> `did_open`/`did_change` -> `documentSymbol`,
//! plus draining the `ClientSocket` for the `textDocument/publishDiagnostics` notifications that
//! diagnostics arrive as (they're server-initiated, not request responses).
//!
//! The `ClientSocket`'s channel back to the client has a capacity of 1 (`tower-lsp`'s own
//! `Client::new`), so anything that sequences "await a server->client notification handler" after
//! "await another one" without something concurrently draining the socket deadlocks the moment
//! the second send has to wait for buffer space nobody is going to free. `start()` below spawns a
//! task that drains the socket into an unbounded channel for the whole lifetime of each test, so
//! request/notification calls on `service` never block on that.

use std::time::Duration;

use futures::StreamExt;
use merc_lsp::Backend;
use merc_lsp::LspService;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use tower::Service;
use tower::ServiceExt;
use tower_lsp::jsonrpc::Request as RpcRequest;
use tower_lsp::jsonrpc::Response as RpcResponse;
use tower_lsp::lsp_types::PublishDiagnosticsParams;

const WELL_FORMED: &str = "sort D;\ninit delta;";
const MALFORMED: &str = "sort D\ninit delta;"; // missing ';'

type TestService = tower_lsp::LspService<Backend>;

/// Builds a fresh, initialized server and spawns the socket-draining task described above.
/// Returns the service (for sending requests/notifications) and the receiving half of the drain
/// channel (for observing server->client notifications like `publishDiagnostics`).
async fn start() -> (TestService, UnboundedReceiver<RpcRequest>) {
    let (mut service, socket) = LspService::new(Backend::new);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut socket = socket;
        while let Some(request) = socket.next().await {
            if tx.send(request).is_err() {
                break; // Test dropped its receiver; nothing left to observe.
            }
        }
    });

    call(&mut service, 0, "initialize", json!({ "capabilities": {} })).await;
    notify(&mut service, "initialized", json!({})).await;

    (service, rx)
}

/// Sends `method` as a request (with the given id) and returns its decoded JSON result. Panics if
/// the call comes back as a JSON-RPC error, since every call in this test suite is expected to
/// succeed.
async fn call(service: &mut TestService, id: i64, method: &'static str, params: Value) -> Value {
    let request = RpcRequest::build(method).params(params).id(id).finish();
    let response = ServiceExt::<RpcRequest>::ready(service)
        .await
        .expect("service ready")
        .call(request)
        .await
        .expect("call did not fail")
        .expect("request should produce a response");
    let (_, body) = response.into_parts();
    body.expect("response should be Ok")
}

/// Sends `method` as a notification (no id, no response expected).
async fn notify(service: &mut TestService, method: &'static str, params: Value) {
    let request = RpcRequest::build(method).params(params).finish();
    let response: Option<RpcResponse> = ServiceExt::<RpcRequest>::ready(service)
        .await
        .expect("service ready")
        .call(request)
        .await
        .expect("call did not fail");
    assert!(response.is_none(), "a notification must not produce a response");
}

/// Waits (with a timeout, so a regression here fails in seconds rather than hanging CI) for the
/// next `textDocument/publishDiagnostics` notification on the drain channel.
async fn next_diagnostics(rx: &mut UnboundedReceiver<RpcRequest>) -> PublishDiagnosticsParams {
    loop {
        let request = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for a publishDiagnostics notification")
            .expect("drain channel closed unexpectedly");
        if request.method() == "textDocument/publishDiagnostics" {
            let params = request.params().cloned().expect("publishDiagnostics must carry params");
            return serde_json::from_value(params).expect("valid PublishDiagnosticsParams");
        }
        // Anything else (e.g. window/logMessage) is drained and ignored.
    }
}

#[tokio::test]
async fn initialize_advertises_document_symbol_support() {
    let (mut service, _rx) = LspService::new(Backend::new);
    let result = call(&mut service, 0, "initialize", json!({ "capabilities": {} })).await;
    assert_eq!(result["capabilities"]["documentSymbolProvider"], json!(true));
    // Phase 1 deliberately does not advertise capabilities it doesn't implement.
    assert!(result["capabilities"]["definitionProvider"].is_null());
}

#[tokio::test]
async fn did_open_with_well_formed_document_publishes_no_diagnostics() {
    let (mut service, mut rx) = start().await;

    notify(
        &mut service,
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///well-formed.mcrl2",
                "languageId": "mcrl2",
                "version": 1,
                "text": WELL_FORMED,
            }
        }),
    )
    .await;

    let diagnostics = next_diagnostics(&mut rx).await;
    assert!(diagnostics.diagnostics.is_empty());
}

#[tokio::test]
async fn did_open_with_malformed_document_publishes_a_located_diagnostic() {
    let (mut service, mut rx) = start().await;

    notify(
        &mut service,
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///malformed.mcrl2",
                "languageId": "mcrl2",
                "version": 1,
                "text": MALFORMED,
            }
        }),
    )
    .await;

    let diagnostics = next_diagnostics(&mut rx).await;
    assert_eq!(diagnostics.diagnostics.len(), 1);
    assert_eq!(diagnostics.diagnostics[0].source.as_deref(), Some("merc-lsp"));
}

/// Regression test for the single most common LSP bug: forgetting to publish the *empty*
/// diagnostics vector on a bad -> good transition, leaving stale squiggles in the editor forever.
#[tokio::test]
async fn fixing_a_malformed_document_clears_its_diagnostics() {
    let (mut service, mut rx) = start().await;
    let uri = "file:///fix-me.mcrl2";

    notify(
        &mut service,
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "mcrl2", "version": 1, "text": MALFORMED }
        }),
    )
    .await;
    let first = next_diagnostics(&mut rx).await;
    assert_eq!(first.diagnostics.len(), 1);

    notify(
        &mut service,
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [ { "text": WELL_FORMED } ]
        }),
    )
    .await;
    let second = next_diagnostics(&mut rx).await;
    assert!(second.diagnostics.is_empty(), "diagnostics must be cleared once the document is fixed");
    assert_eq!(second.version, Some(2));
}

#[tokio::test]
async fn document_symbol_returns_the_outline() {
    let (mut service, mut rx) = start().await;
    let uri = "file:///outline.mcrl2";

    notify(
        &mut service,
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "mcrl2",
                "version": 1,
                "text": "sort D;\nact a;\ninit a;",
            }
        }),
    )
    .await;
    let _ = next_diagnostics(&mut rx).await;

    let result = call(
        &mut service,
        1,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    )
    .await;
    let symbols = result.as_array().expect("documentSymbol should return an array");
    let names: Vec<&str> = symbols.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"D"), "expected the 'D' sort in the outline, got {names:?}");
    assert!(names.contains(&"a"), "expected the 'a' action in the outline, got {names:?}");
    assert!(names.contains(&"init"), "expected the 'init' entry in the outline, got {names:?}");
}
