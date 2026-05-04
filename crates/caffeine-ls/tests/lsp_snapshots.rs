use std::{path::PathBuf, sync::LazyLock};

use caffeine_ls::{
    config::{Config, ConfigChange, ConfigErrors},
    from_json,
};
use camino::Utf8PathBuf;
use lsp_test::{LspHarness, lsp_fixture};
use serde_json::json;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use vfs::AbsPathBuf;

fn setup_logging() -> anyhow::Result<()> {
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(false);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("TEST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

static SETUP: LazyLock<()> = LazyLock::new(|| {
    setup_logging().expect("Failed to setup logger");

    rayon::ThreadPoolBuilder::new()
        .thread_name(|ix| format!("RayonWorker{}", ix))
        .build_global()
        .unwrap();
});

fn create_lsp() -> LspHarness {
    LazyLock::force(&SETUP);
    let client_config = json!({});

    LspHarness::start(client_config, |connection| {
        let (initialize_id, initialize_params) = connection.initialize_start().unwrap();

        tracing::info!("InitializeParams: {}", initialize_params);
        #[allow(deprecated)]
        let lsp_types::InitializeParams {
            root_uri,
            mut capabilities,
            workspace_folders,
            initialization_options,
            client_info,
            ..
        } = from_json::<lsp_types::InitializeParams>("InitializeParams", &initialize_params)
            .unwrap();

        // lsp-types has a typo in the `/capabilities/workspace/diagnostics` field, its typoed as `diagnostic`
        if let Some(val) = initialize_params.pointer("/capabilities/workspace/diagnostics")
            && let Ok(diag_caps) = from_json::<lsp_types::DiagnosticWorkspaceClientCapabilities>(
                "DiagnosticWorkspaceClientCapabilities",
                val,
            )
        {
            tracing::info!("Patching lsp-types workspace diagnostics capabilities: {diag_caps:#?}");
            capabilities
                .workspace
                .get_or_insert_default()
                .diagnostic
                .get_or_insert(diag_caps);
        }

        let root_path = root_uri
            .and_then(|it| it.to_file_path().ok())
            .map(patch_path_prefix)
            .and_then(|it| Utf8PathBuf::from_path_buf(it).ok())
            .and_then(|it| AbsPathBuf::try_from(it).ok())
            .unwrap();

        if let Some(client_info) = &client_info {
            tracing::info!(
                "Client '{}' {}",
                client_info.name,
                client_info.version.as_deref().unwrap_or_default()
            );
        }

        let workspace_roots = workspace_folders
            .map(|workspaces| {
                workspaces
                    .into_iter()
                    .filter_map(|it| it.uri.to_file_path().ok())
                    .map(patch_path_prefix)
                    .filter_map(|it| Utf8PathBuf::from_path_buf(it).ok())
                    .filter_map(|it| AbsPathBuf::try_from(it).ok())
                    .collect::<Vec<_>>()
            })
            .filter(|workspaces| !workspaces.is_empty())
            .unwrap_or_else(|| vec![root_path.clone()]);
        let mut config = Config::new(capabilities, workspace_roots, client_info, None);
        if let Some(json) = initialization_options {
            let mut change = ConfigChange::default();
            change.change_client_config(json);

            let error_sink: ConfigErrors;
            (config, error_sink, _) = config.apply_change(change);

            if !error_sink.is_empty() {
                use lsp_types::{
                    MessageType, ShowMessageParams,
                    notification::{Notification, ShowMessage},
                };
                let not = lsp_server::Notification::new(
                    ShowMessage::METHOD.to_owned(),
                    ShowMessageParams {
                        typ: MessageType::WARNING,
                        message: error_sink.to_string(),
                    },
                );
                connection
                    .sender
                    .send(lsp_server::Message::Notification(not))
                    .unwrap();
            }
        }

        let server_capabilities = caffeine_ls::server_capabilities(&config);

        let initialize_result = lsp_types::InitializeResult {
            capabilities: server_capabilities,
            server_info: Some(lsp_types::ServerInfo {
                name: caffeine_ls::NAME.to_string(),
                version: Some(caffeine_ls::VERSION.to_string()),
            }),
            offset_encoding: None,
        };

        let initialize_result = serde_json::to_value(initialize_result).unwrap();

        connection
            .initialize_finish(initialize_id, initialize_result)
            .expect("Failed to finish initialization");

        caffeine_ls::main_loop(config, connection).unwrap();

        tracing::info!("server did shut down");
    })
}

fn patch_path_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    if cfg!(windows) {
        // VSCode might report paths with the file drive in lowercase, but this can mess
        // with env vars set by tools and build scripts executed by r-a such that it invalidates
        // cargo's compilations unnecessarily. https://github.com/rust-lang/rust-analyzer/issues/14683
        // So we just uppercase the drive letter here unconditionally.
        // (doing it conditionally is a pain because std::path::Prefix always reports uppercase letters on windows)
        let mut comps = path.components();
        match comps.next() {
            Some(Component::Prefix(prefix)) => {
                let prefix = match prefix.kind() {
                    Prefix::Disk(d) => {
                        format!("{}:", d.to_ascii_uppercase() as char)
                    }
                    Prefix::VerbatimDisk(d) => {
                        format!(r"\\?\{}:", d.to_ascii_uppercase() as char)
                    }
                    _ => return path,
                };
                let mut path = PathBuf::new();
                path.push(prefix);
                path.extend(comps);
                path
            }
            _ => path,
        }
    } else {
        path
    }
}

#[macro_export]
macro_rules! lsp_test {
    ($name:ident, $fixture:expr, |$lsp:ident| $body:block) => {
        #[test]
        fn $name() {
            let $lsp = $crate::create_lsp();

            $crate::lsp_fixture!($lsp, $fixture);

            {
                $body
            };

            $lsp.shutdown();
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
        lsp.open_document("/src/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

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
        lsp.open_document("/src/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

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
        lsp.open_document("/src/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

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
        lsp.open_document("/src/Empty.java");
        let diag_empty = lsp.pull_document_diagnostics("/src/Empty.java");

        lsp.open_document("/src/Garbage.java");
        let diag_garbage = lsp.pull_document_diagnostics("/src/Garbage.java");

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
        lsp.open_document(path);

        lsp.change_at_mark(path, "\n        if (true) <|>");

        let diag_broken = lsp.pull_document_diagnostics(path);

        lsp.change_at_mark(path, "{ }");

        let diag_fixed = lsp.pull_document_diagnostics(path);

        insta::assert_json_snapshot!("incremental_sync", (diag_broken, diag_fixed));
    }
);
