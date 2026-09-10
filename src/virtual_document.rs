//! Produces a virtual document for every `SourceMap`'s virtual file.

use dashmap::DashMap;
use lsp_types::Url;
use lsp_types::request::Request;
use merc_syntax::SourceId;
use merc_syntax::SourceMap;
use serde::Deserialize;
use serde::Serialize;

use crate::convert;

/// The server-wide registry [`register`] populates and [`VirtualDocument`]'s handler reads —
/// registered name (e.g. `<builtin>/nat.mcrl2`) -> its text.
pub type VirtualDocumentStore = DashMap<String, String>;

/// Registers every virtual (see [`merc_syntax::SourceMap::is_virtual`]) file in `sources` into
/// `store`, keyed by its own registered name. Called once per [`crate::backend::analyze`] run,
/// against the fresh `SourceMap` that run just produced.
pub fn register(store: &VirtualDocumentStore, sources: &SourceMap) {
    for index in 0..sources.file_count() {
        let id = SourceId::new(index);
        if sources.is_virtual(id) {
            store.insert(sources.path(id).to_string(), sources.text(id).to_string());
        }
    }
}

/// Request parameters: the `merc-builtin:` URI the client wants the content of.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualDocumentParams {
    pub uri: Url,
}

/// Resolves a `merc-builtin:` URI to the text it stands for, or `null` (`None`)
/// if the server has no entry under that name.
pub enum VirtualDocument {}

impl Request for VirtualDocument {
    type Params = VirtualDocumentParams;
    type Result = Option<String>;
    const METHOD: &'static str = "merc/virtualDocument";
}

/// The actual request handler: decodes `params.uri` back to its registered name (see
/// [`convert::decode_virtual_uri`]) and looks it up in `store`.
pub fn virtual_document_request(store: &VirtualDocumentStore, params: VirtualDocumentParams) -> Option<String> {
    let name = convert::decode_virtual_uri(&params.uri)?;
    store.get(&name).map(|entry| entry.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_populates_only_virtual_entries() {
        let mut sources = SourceMap::new();
        sources.add_text("real.mcrl2", "sort D;");
        sources.add_virtual("<builtin>/nat.mcrl2", "sort Nat;");

        let store = VirtualDocumentStore::new();
        register(&store, &sources);

        assert_eq!(store.len(), 1);
        assert_eq!(store.get("<builtin>/nat.mcrl2").map(|entry| entry.clone()), Some("sort Nat;".to_string()));
        assert!(store.get("real.mcrl2").is_none());
    }

    #[test]
    fn virtual_document_request_round_trips_through_the_encoded_uri() {
        let mut sources = SourceMap::new();
        sources.add_virtual("<builtin>/nat.mcrl2", "sort Nat;");
        let store = VirtualDocumentStore::new();
        register(&store, &sources);

        let uri = convert::virtual_uri("<builtin>/nat.mcrl2");
        let content = virtual_document_request(&store, VirtualDocumentParams { uri });
        assert_eq!(content, Some("sort Nat;".to_string()));
    }

    #[test]
    fn virtual_document_request_is_none_for_an_unregistered_name() {
        let store = VirtualDocumentStore::new();
        let uri = convert::virtual_uri("<builtin>/nowhere.mcrl2");
        assert_eq!(virtual_document_request(&store, VirtualDocumentParams { uri }), None);
    }
}
