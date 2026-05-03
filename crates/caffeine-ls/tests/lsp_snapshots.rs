use std::sync::LazyLock;

use lsp_test::{LspHarness, lsp_fixture};
use serde_json::json;
use tokio::sync::mpsc;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use triomphe::Arc;

fn setup_logging() -> anyhow::Result<()> {
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(false);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("TEST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

static TRACING: LazyLock<()> = LazyLock::new(|| setup_logging().expect("Failed to setup logger"));

async fn create_lsp() -> LspHarness {
    LazyLock::force(&TRACING);
    let config = json!({});

    LspHarness::start(config, |client| {
        let state = Arc::new(caffeine_ls::GlobalState::default());
        let (worker_tx, worker_rx) = mpsc::channel(500);
        let worker = caffeine_ls::Worker::new(client.clone(), state.clone(), worker_rx);
        worker.spawn_in_background();

        caffeine_ls::Backend::new(client, state, worker_tx)
    })
    .await
}

#[macro_export]
macro_rules! lsp_test {
    ($name:ident, $fixture:expr, |$lsp:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let $lsp = $crate::create_lsp().await;

            $crate::lsp_fixture!($lsp, $fixture);

            {
                $body
            };

            $lsp.shutdown().await;
        }
    };
}

lsp_test!(
    test_parser_recovery_missing_semicolon,
    r#"
    //- /src/Main.java
    public class Main {
        public void test() {
            int a = 1
            int b = 2
        }
    }
    "#,
    |lsp| {
        lsp.open_document("/src/Main.java").await;
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java").await;

        insta::assert_json_snapshot!("parser_recovery_missing_semicolon", diagnostics);
    }
);

lsp_test!(
    test_lexer_errors,
    r#"
    //- /src/Main.java
    public class Main {
        int x = `invalid_backtick`; 
        char c = 'ab';
    }
    "#,
    |lsp| {
        lsp.open_document("/src/Main.java").await;
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java").await;

        insta::assert_json_snapshot!("lexer_errors", diagnostics);
    }
);

lsp_test!(
    test_unclosed_block,
    r#"
    //- /src/Main.java
    public class Main {
        public void unfinished( {
            if (true) {
    "#,
    |lsp| {
        lsp.open_document("/src/Main.java").await;
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java").await;

        insta::assert_json_snapshot!("unclosed_block", diagnostics);
    }
);

lsp_test!(
    test_empty_and_garbage,
    r#"
    //- /src/Empty.java

    //- /src/Garbage.java
    #$@%^&*()
    "#,
    |lsp| {
        lsp.open_document("/src/Empty.java").await;
        let diag_empty = lsp.pull_document_diagnostics("/src/Empty.java").await;

        lsp.open_document("/src/Garbage.java").await;
        let diag_garbage = lsp.pull_document_diagnostics("/src/Garbage.java").await;

        insta::assert_json_snapshot!("sanity_checks", (diag_empty, diag_garbage));
    }
);

lsp_test!(
    test_incremental_break_and_fix,
    r#"
    //- /src/Main.java
    public class Main {
        public void m() {<|>}
    }
    "#,
    |lsp| {
        let path = "/src/Main.java";
        lsp.open_document(path).await;

        lsp.change_at_mark(path, "\n        if (true) <|>").await;

        let diag_broken = lsp.pull_document_diagnostics(path).await;

        lsp.change_at_mark(path, "{ }").await;

        let diag_fixed = lsp.pull_document_diagnostics(path).await;

        insta::assert_json_snapshot!("incremental_sync", (diag_broken, diag_fixed));
    }
);
