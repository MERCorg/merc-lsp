//! `merc/didFocusTextDocument`: a custom notification the client sends whenever the active editor
//! switches to an already-open document (see `vscode-client/src/extension.ts`'s
//! `window.onDidChangeActiveTextEditor` listener) — plain LSP has no "editor became active" signal
//! of its own, only `didOpen`/`didChange`/`didSave`/`didClose`.
//!
//! `backend::router` uses it to catch a case none of those four cover: file `a.mcrl2` `%import`s
//! `b.mcrl2`, `b.mcrl2` gets edited and saved while `a.mcrl2` isn't the active editor, and
//! switching focus back to `a.mcrl2` should show diagnostics reflecting `b.mcrl2`'s new content —
//! without requiring another edit or save on `a.mcrl2` itself. See [`crate::document::Document::is_stale`]
//! for how staleness is actually detected.

use lsp_types::Url;
use lsp_types::notification::Notification;
use serde::Deserialize;
use serde::Serialize;

/// Notification parameters: the document the client just switched focus to.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidFocusTextDocumentParams {
    pub uri: Url,
}

/// See this module's doc comment.
pub enum DidFocusTextDocument {}

impl Notification for DidFocusTextDocument {
    type Params = DidFocusTextDocumentParams;
    const METHOD: &'static str = "merc/didFocusTextDocument";
}
