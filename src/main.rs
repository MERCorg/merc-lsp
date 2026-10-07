#[tokio::main]
async fn main() {
    merc_lsp::init_logging();
    merc_lsp::run_stdio().await;
}
