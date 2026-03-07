use tower_lsp::{LspService, Server};

mod logging;

#[tokio::main]
async fn main() {
    logging::init_logger();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "java-analyzer starting"
    );

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(java_analyzer::lsp::Backend::new).finish();

    tracing::info!("LSP server listening on stdio");
    Server::new(stdin, stdout, socket).serve(service).await;
}
