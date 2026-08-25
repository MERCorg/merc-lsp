//! The [`LanguageServer`] implementation: wires document lifecycle notifications to parsing,
//! diagnostics, and document symbols.

use tower_lsp::Client;
use tower_lsp::LanguageServer;
use tower_lsp::async_trait;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::DidChangeTextDocumentParams;
use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tower_lsp::lsp_types::DidSaveTextDocumentParams;
use tower_lsp::lsp_types::DocumentSymbolParams;
use tower_lsp::lsp_types::DocumentSymbolResponse;
use tower_lsp::lsp_types::InitializeParams;
use tower_lsp::lsp_types::InitializeResult;
use tower_lsp::lsp_types::InitializedParams;
use tower_lsp::lsp_types::MessageType;
use tower_lsp::lsp_types::ServerInfo;
use tower_lsp::lsp_types::Url;

use crate::capabilities::server_capabilities;
use crate::diagnostics;
use crate::document::Document;
use crate::document::DocumentStore;
use crate::parse;
use crate::parse::ParseOutcome;
use crate::symbols;

pub struct Backend {
    client: Client,
    documents: DocumentStore,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            documents: DocumentStore::default(),
        }
    }

    /// Re-parses `text` at `version` for `uri`, stores the result, and publishes diagnostics for
    /// it. Used by `did_open` and `did_change`.
    async fn on_change(&self, uri: Url, text: String, version: i32) {
        let outcome = parse::parse(text.clone()).await;

        // `did_change` notifications for the same document can in principle be
        // handled out of order. Never let a result for an older version clobber
        // one for a newer version that already landed; discarding here is a
        // cheap debounce against a burst of per-keystroke notifications.
        if let Some(existing) = self.documents.get(&uri)
            && existing.version > version
        {
            log::debug!("discarding stale parse of {uri} (version {version}, have {})", existing.version);
            return;
        }

        let document = Document::new(text, version, outcome);
        let diags = diagnostics::diagnostics(&document.text, &document.line_index, &document.parsed);
        self.documents.insert(uri.clone(), document);
        // Publishing is mandatory even when `diags` is empty: an empty vector is what clears any
        // diagnostics left over from a previous, failing parse.
        self.client.publish_diagnostics(uri, diags, Some(version)).await;
    }
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "merc-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        self.on_change(doc.uri, doc.text, doc.version).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // `TextDocumentSyncKind::FULL` (advertised in `capabilities::server_capabilities`) means
        // the client always sends exactly one change event containing the whole new text.
        let Some(change) = params.content_changes.into_iter().next() else {
            return;
        };
        self.on_change(params.text_document.uri, change.text, params.text_document.version)
            .await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // With FULL sync, `did_change` already re-parsed and published on every edit; re-publish
        // the already-computed diagnostics for clients that only reliably fire on save.
        let uri = params.text_document.uri;
        if let Some(document) = self.documents.get(&uri) {
            let diags = diagnostics::diagnostics(&document.text, &document.line_index, &document.parsed);
            let version = document.version;
            drop(document);
            self.client.publish_diagnostics(uri, diags, Some(version)).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn document_symbol(&self, params: DocumentSymbolParams) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let Some(document) = self.documents.get(&uri) else {
            return Ok(None);
        };
        let ParseOutcome::Ok(spec) = &document.parsed else {
            return Ok(None);
        };
        let symbols = symbols::document_symbols(&document.text, &document.line_index, spec);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }
}
