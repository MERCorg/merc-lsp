//! Thin binary entry point — see [`merc_lsp`] (`src/lib.rs`) for everything else.

#[tokio::main]
async fn main() {
    env_logger::init();
    merc_lsp::run_stdio().await;
}
