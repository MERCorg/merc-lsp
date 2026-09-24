//! One submodule per LSP request/capability the server implements, each consuming
//! [`crate::analysis`] output and producing that request's LSP response type.

pub(crate) mod code_action;
pub(crate) mod completion;
pub(crate) mod completion_context;
pub(crate) mod diagnostics;
#[cfg(feature = "lsp-extensions")]
pub(crate) mod generate;
pub(crate) mod goto_definition;
pub(crate) mod hover;
pub(crate) mod inlay_hints;
pub(crate) mod semantic_tokens;
pub(crate) mod symbols;
#[cfg(feature = "lsp-extensions")]
pub(crate) mod virtual_document;
