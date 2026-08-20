use std::{path::PathBuf, sync::LazyLock};

use caffeine_ls::{
    config::{Config, ConfigChange, ConfigErrors},
    from_json,
};
use camino::Utf8PathBuf;
use lsp_test::{LspHarness, lsp_fixture};
use lsp_types::{
    Notification, ShowMessageNotification, WorkspaceFolders, WorkspaceFoldersInitializeParams,
};
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
    create_lsp_with_setup(|_| {})
}

fn create_lsp_with_setup(setup: impl FnOnce(&std::path::Path)) -> LspHarness {
    LazyLock::force(&SETUP);
    let client_config = json!({});

    LspHarness::start_with_setup(client_config, setup, run_server)
}

fn create_lsp_with_progress() -> LspHarness {
    LazyLock::force(&SETUP);
    let client_config = json!({});

    LspHarness::start_with_setup_progress(client_config, |_| {}, run_server)
}

fn run_server(connection: lsp_server::Connection) {
    let (initialize_id, initialize_params) = connection.initialize_start().unwrap();

    tracing::info!("InitializeParams: {}", initialize_params);
    #[allow(deprecated)]
    let lsp_types::InitializeParams {
        root_uri,
        capabilities,
        workspace_folders_initialize_params: WorkspaceFoldersInitializeParams { workspace_folders },
        initialization_options,
        client_info,
        ..
    } = from_json::<lsp_types::InitializeParams>("InitializeParams", &initialize_params).unwrap();

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

    let workspace_folders = match workspace_folders {
        Some(WorkspaceFolders::WorkspaceFolderList(folders)) => Some(folders),
        _ => None,
    };

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
            use lsp_types::{MessageType, ShowMessageParams};
            let not = lsp_server::Notification::new(
                ShowMessageNotification::METHOD.to_string(),
                ShowMessageParams {
                    kind: MessageType::Warning,
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
    };

    let initialize_result = serde_json::to_value(initialize_result).unwrap();

    connection
        .initialize_finish(initialize_id, initialize_result)
        .expect("Failed to finish initialization");

    caffeine_ls::main_loop(config, connection).unwrap();

    tracing::info!("server did shut down");
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
    test_type_diagnostics,
    r#"
    //- /src/Main.java
    public class Main {
        void test(Main m) {
            m.noSuchMethod();
            unknown;
        }
    }
    "#,
    |lsp| {
        lsp.open_document("/src/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

        insta::assert_json_snapshot!("type_diagnostics", diagnostics);
    }
);

lsp_test!(
    test_kotlin_syntax_diagnostics,
    r#"
    //- /src/Main.kt
    fun main() {
        val s = "unterminated
    }
    "#,
    |lsp| {
        lsp.open_document("/src/Main.kt");
        let diagnostics = lsp.pull_document_diagnostics("/src/Main.kt");

        insta::assert_json_snapshot!("kotlin_syntax_diagnostics", diagnostics);
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

#[test]
fn test_workspace_load_reports_progress() {
    let lsp = create_lsp_with_progress();

    // The temp workspace has no build system, so only the VFS scan phase
    // runs; it must surface as `$/progress` begin/end pairs (which clients
    // only deliver after the `window/workDoneProgress/create` handshake).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut began = std::collections::HashSet::new();
    let mut ended = std::collections::HashSet::new();

    while std::time::Instant::now() < deadline {
        match lsp
            .notification_receiver
            .recv_timeout(std::time::Duration::from_millis(100))
        {
            Ok(notif) if notif.method == "$/progress" => {
                let kind = notif
                    .params
                    .get("value")
                    .and_then(|v| v.get("kind"))
                    .and_then(|k| k.as_str());
                let token = notif
                    .params
                    .get("token")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                match kind {
                    Some("begin") => {
                        began.insert(token);
                    }
                    Some("end") => {
                        ended.insert(token);
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        if !began.is_empty() && !ended.is_empty() {
            break;
        }
    }

    assert!(
        !began.is_empty(),
        "server never reported a $/progress begin for workspace loading"
    );
    assert!(
        !ended.is_empty(),
        "server never reported a $/progress end for workspace loading"
    );

    lsp.shutdown();
}

#[test]
fn test_syntax_diagnostics_before_workspace_load() {
    let lsp = create_lsp_with_setup(|root| {
        // Two build systems make the probe ambiguous, so the workspace is
        // never fully loaded.
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::write(root.join("pom.xml"), "<project></project>").unwrap();
    });

    lsp.write_file(
        "src/Main.java",
        "public class Main {\n    public void m() {\n        int a = 1\n    }\n}",
    );
    lsp.open_document("/src/Main.java");
    let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

    insta::assert_json_snapshot!("syntax_diagnostics_before_workspace_load", diagnostics);

    lsp.shutdown();
}

#[test]
fn test_document_symbols() {
    let lsp = create_lsp();
    let path = "/src/com/example/Foo.java";
    lsp.write_file(
        path,
        r#"package com.example;

public class Foo {
    public int x;
    private String s;

    public Foo() {}

    public void bar(int a) {}

    public static class Inner {
        private int y;
    }
}
"#,
    );
    lsp.open_document(path);

    let response = request_until(
        &lsp,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": lsp.uri(path) } }),
        |response| !response.is_null(),
    );
    insta::assert_json_snapshot!("document_symbols", response);

    lsp.shutdown();
}

#[test]
fn test_workspace_symbols() {
    let lsp = create_lsp();
    lsp.write_file(
        "/src/com/example/Foo.java",
        r#"package com.example;

public class Foo {
    public int x;
    public void bar(int a) {}
}
"#,
    );
    lsp.write_file(
        "/src/org/other/Bar.java",
        r#"package org.other;

public interface Bar {
    void baz();
}
"#,
    );

    // `workspace/symbol` needs the workspace to be loaded, so retry until the
    // plain source root graph has been applied.
    let response = request_until(
        &lsp,
        "workspace/symbol",
        json!({ "query": "" }),
        |response| {
            response
                .as_array()
                .map(|symbols| !symbols.is_empty())
                .unwrap_or(false)
        },
    );

    // The file URIs embed the temp workspace path, which varies between runs.
    let workspace_root = lsp.workspace_root.path().to_string_lossy().to_string();
    let normalized = normalize_uris(response, &workspace_root);
    insta::assert_json_snapshot!("workspace_symbols", normalized);

    lsp.shutdown();
}

/// Sends `method`/`params`, retrying while `accept` fails. The server cancels
/// an in-flight request when the database is modified mid-query (salsa raises
/// `Cancelled`); a cancelled request surfaces as a `null` result here.
fn request_until(
    lsp: &LspHarness,
    method: &str,
    params: serde_json::Value,
    accept: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let response = lsp.request(method, params.clone());
        if accept(&response) {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server never produced an acceptable {method} response"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Rewrites every `file://` URI containing `workspace_root` to a stable
/// placeholder, so snapshots don't capture the temp dir path.
fn normalize_uris(value: serde_json::Value, workspace_root: &str) -> serde_json::Value {
    let mut value = value;
    walk_json(&mut value, workspace_root);
    value
}

fn walk_json(value: &mut serde_json::Value, workspace_root: &str) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(pos) = s.find(workspace_root) {
                s.replace_range(pos..pos + workspace_root.len(), "<WORKSPACE_ROOT>");
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                walk_json(item, workspace_root);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                walk_json(value, workspace_root);
            }
        }
        _ => {}
    }
}
