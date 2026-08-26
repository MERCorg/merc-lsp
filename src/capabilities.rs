//! Builds the server's advertised [`ServerCapabilities`] in one place.

use lsp_types::OneOf;
use lsp_types::ServerCapabilities;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;

/// Capabilities advertised by this LSP.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // We don't support increment parsing.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}
