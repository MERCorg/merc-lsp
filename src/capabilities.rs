//! Builds the server's advertised [`ServerCapabilities`] in one place.

use lsp_types::HoverProviderCapability;
use lsp_types::OneOf;
use lsp_types::SemanticTokensFullOptions;
use lsp_types::SemanticTokensOptions;
use lsp_types::SemanticTokensServerCapabilities;
use lsp_types::ServerCapabilities;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;

use crate::semantic_tokens;

/// Capabilities advertised by this LSP.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // We don't support increment parsing.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        // AST-driven coloring
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..SemanticTokensOptions::default()
            },
        )),
        // Both built on the checked data specification's `TypingInfo` — see `hover`/
        // `goto_definition`.
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}
