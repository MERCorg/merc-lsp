//! `merc-lsp`: a Language Server for mCRL2 specifications, built on `merc_syntax`, `tower-lsp`,
//! and `tokio`.
//!
//! Split into a library and a thin [`main.rs`](../src/main.rs) binary (rather than putting
//! everything in `main.rs`) so `tests/protocol.rs` can drive [`Backend`] through [`LspService`]
//! directly, without going through the stdio transport.
//!
//! Nothing in this crate may ever write to stdout: stdout is the JSON-RPC transport when running
//! as the binary, and a single stray print corrupts the stream and gets the connection dropped
//! by the client. All logging goes to stderr via `env_logger`.
#![deny(clippy::print_stdout)]

mod backend;
mod capabilities;
mod convert;
mod diagnostics;
mod document;
mod parse;
mod symbols;

pub use backend::Backend;
pub use tower_lsp::ClientSocket;
pub use tower_lsp::LspService;

/// Runs the server over stdio until the client disconnects. This is what `main.rs` calls; it's
/// not itself under test (there's no stdio to drive in-process), but everything it wires
/// together is.
pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    tower_lsp::Server::new(stdin, stdout, socket).serve(service).await;
}
