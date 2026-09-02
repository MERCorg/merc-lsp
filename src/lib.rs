//! `merc-lsp`: a Language Server for mCRL2 specifications, built on `merc_syntax`, `async-lsp`,
//! and `tokio`.
//!
//! Split into a library and a thin [`main.rs`](../src/main.rs) binary (rather than putting
//! everything in `main.rs`) so `tests/protocol.rs` can drive the real service through [`serve`]
//! over an in-memory pipe, without going through the stdio transport.
//!
//! Nothing in this crate may ever write to stdout: stdout is the JSON-RPC transport when running
//! as the binary, and a single stray print corrupts the stream and gets the connection dropped
//! by the client. All logging goes to stderr via `env_logger`.
#![deny(clippy::print_stdout)]

mod backend;
mod capabilities;
mod completion;
mod convert;
mod diagnostics;
mod document;
mod goto_definition;
mod hover;
mod inlay_hints;
mod parse;
mod semantic_tokens;
mod sort_ref;
mod symbols;
mod typecheck;

use async_lsp::client_monitor::ClientProcessMonitorLayer;
use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::server::LifecycleLayer;
use futures::AsyncRead;
use futures::AsyncWrite;
use tower::ServiceBuilder;

/// Runs the server over `input`/`output` until the client disconnects (`exit` notification) or a
/// transport error occurs.
///
/// Generic over the transport (rather than hardcoding stdio) so `tests/protocol.rs` can drive
/// this exact service construction — [`backend::router`] plus the full middleware stack — over
/// an in-memory duplex pipe instead of real stdio. [`run_stdio`] is the only other caller.
pub async fn serve(input: impl AsyncRead, output: impl AsyncWrite) {
    let (mainloop, _client) = async_lsp::MainLoop::new_server(|client| {
        let router = backend::router(client.clone());
        ServiceBuilder::new()
            .layer(LifecycleLayer::default())
            .layer(CatchUnwindLayer::default())
            .layer(ConcurrencyLayer::default())
            .layer(ClientProcessMonitorLayer::new(client))
            .service(router)
    });

    if let Err(error) = mainloop.run_buffered(input, output).await {
        log::error!("main loop exited with error: {error}");
    }
}

/// Runs the server over stdio until the client disconnects. This is what `main.rs` calls.
pub async fn run_stdio() {
    // `async-lsp`'s main loop speaks the runtime-agnostic `futures` IO traits, not tokio's; the
    // two platform branches below each produce a `futures::{AsyncRead,AsyncWrite}` pair, just via
    // different routes.
    //
    // Prefer truly asynchronous piped stdin/stdout without blocking tasks.
    #[cfg(unix)]
    let (stdin, stdout) = (
        async_lsp::stdio::PipeStdin::lock_tokio().expect("stdin is not lockable as an async pipe"),
        async_lsp::stdio::PipeStdout::lock_tokio().expect("stdout is not lockable as an async pipe"),
    );
    // Fallback to spawn-blocking read/write otherwise, bridged from tokio's IO traits to
    // `futures`'s via `tokio-util`'s compatibility layer.
    #[cfg(not(unix))]
    let (stdin, stdout) = (
        tokio_util::compat::TokioAsyncReadCompatExt::compat(tokio::io::stdin()),
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(tokio::io::stdout()),
    );

    serve(stdin, stdout).await;
}
