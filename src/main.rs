#[tokio::main]
async fn main() {
    env_logger::init();
    merc_lsp::run_stdio().await;
}
