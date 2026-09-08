//! Builds the server's advertised [`ServerCapabilities`] in one place.

use lsp_types::CompletionOptions;
use lsp_types::HoverProviderCapability;
use lsp_types::OneOf;
use lsp_types::SemanticTokensFullOptions;
use lsp_types::SemanticTokensOptions;
use lsp_types::SemanticTokensServerCapabilities;
use lsp_types::ServerCapabilities;
use lsp_types::TextDocumentSyncCapability;
use lsp_types::TextDocumentSyncKind;
use lsp_types::TextDocumentSyncOptions;
use lsp_types::TextDocumentSyncSaveOptions;

use crate::semantic_tokens;

/// Capabilities advertised by this LSP.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        // We don't support incremental parsing, so we use full document sync.
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::FULL),
            save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            ..TextDocumentSyncOptions::default()
        })),
        document_symbol_provider: Some(OneOf::Left(true)),
        // AST-driven coloring
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..SemanticTokensOptions::default()
            },
        )),
        // All three built on the checked specification's whole-document `TypingInfo`.
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        // Unscoped, so no `resolve` step has anything extra to add and no
        // `triggerCharacters` beyond identifier characters (which never need
        // listing) makes sense.
        completion_provider: Some(CompletionOptions::default()),
        ..ServerCapabilities::default()
    }
}
