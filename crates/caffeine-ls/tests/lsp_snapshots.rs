use std::{path::PathBuf, sync::LazyLock};

use caffeine_ls::{
    config::{Config, ConfigChange, ConfigErrors},
    from_json,
};
use camino::Utf8PathBuf;
use lsp_test::{LspHarness, lsp_fixture};
use lsp_types::{
    FileChangeType, Notification, Position, Range, ShowMessageNotification, WorkspaceFolders,
    WorkspaceFoldersInitializeParams,
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

/// The LSP position of the middle of `needle` (ASCII fixture files only).
fn position_of(text: &str, needle: &str) -> (u32, u32) {
    let idx = text.find(needle).expect("needle in text") + needle.len() / 2;
    let before = &text[..idx];
    let line = before.matches('\n').count() as u32;
    let last = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = before[last..].chars().count() as u32;
    (line, character)
}

#[test]
fn test_goto_definition() {
    let lsp = create_lsp();
    let path = "/src/com/example/Nav.java";
    let text = r#"package com.example;

class Nav {
    int count;

    int compute() {
        int local = count;
        return local;
    }

    int add(int a, int b) {
        return a + b;
    }

    void call() {
        int r = add(1, 2);
    }
}
"#;
    lsp.write_file(path, text);
    lsp.open_document(path);

    // Goto-definition on the field read `count` resolves to its declaration.
    let (line, character) = position_of(text, "= count;");
    let response = request_until(
        &lsp,
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
        |response| !response.is_null(),
    );
    let workspace_root = lsp.workspace_root.path().to_string_lossy().to_string();
    let normalized = normalize_uris(response, &workspace_root);
    insta::assert_json_snapshot!("goto_definition_field_read", normalized);

    lsp.shutdown();
}

#[test]
fn test_hover() {
    let lsp = create_lsp();
    let path = "/src/com/example/Nav.java";
    let text = r#"package com.example;

class Nav {
    int count;

    int compute() {
        int local = count;
        return local;
    }
}
"#;
    lsp.write_file(path, text);
    lsp.open_document(path);

    // Hover over the `count` read shows its type `int`; over the method
    // declaration shows its signature.
    let (line, character) = position_of(text, "= count;");
    let response = request_until(
        &lsp,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
        |response| !response.is_null(),
    );
    insta::assert_json_snapshot!("hover_expression", response);

    let (line, character) = position_of(text, "int compute(");
    let response = request_until(
        &lsp,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
        |response| !response.is_null(),
    );
    insta::assert_json_snapshot!("hover_method_declaration", response);

    lsp.shutdown();
}

fn create_lsp_with_config(client_config: serde_json::Value) -> LspHarness {
    LazyLock::force(&SETUP);
    LspHarness::start_with_setup(client_config, |_| {}, run_server)
}

/// Re-issues `workspace/diagnostic` until `pred` holds, returning the accepted
/// raw report (the server cancels queries when a write lands mid-request).
fn request_workspace_until(
    lsp: &LspHarness,
    previous_result_ids: serde_json::Value,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let response = lsp.request(
            "workspace/diagnostic",
            json!({ "previousResultIds": previous_result_ids.clone() }),
        );
        if pred(&response) {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a workspace diagnostic report"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The `(uri, resultId)` pairs of a workspace report, to echo back as
/// `previousResultIds`.
fn extract_previous_ids(report: &serde_json::Value) -> serde_json::Value {
    let items = report["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|it| {
                    json!({
                        "uri": it["uri"].clone(),
                        "value": it["resultId"].clone(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serde_json::json!(items)
}

/// Pulls diagnostics until `pred` holds, re-issuing on the same cancellation/
/// write retry the real clients do. Returns the accepted raw report.
fn wait_until_pull(
    lsp: &LspHarness,
    path: &str,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    wait_until_pull_with_previous(lsp, path, None, pred)
}

fn wait_until_pull_with_previous(
    lsp: &LspHarness,
    path: &str,
    previous_result_id: Option<String>,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let report =
            lsp.pull_document_diagnostics_raw_with_previous(path, previous_result_id.clone());
        if pred(&report) {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for diagnostic state of {path}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The `relatedDocuments` entry whose key ends with `suffix` (the URI embeds the
/// per-run temp workspace root).
fn related_entry<'a>(report: &'a serde_json::Value, suffix: &str) -> Option<&'a serde_json::Value> {
    report
        .get("relatedDocuments")
        .and_then(|r| r.as_object())
        .and_then(|map| map.iter().find(|(k, _)| k.ends_with(suffix)))
        .map(|(_, v)| v)
}

/// Normalizes temp paths out of every URI (including `relatedDocuments` map
/// keys), then sorts all JSON object keys, so snapshots are stable across runs
/// and map iteration order.
fn normalize_and_sort(value: serde_json::Value, lsp: &LspHarness) -> serde_json::Value {
    let workspace_root = lsp.workspace_root.path().to_string_lossy().to_string();
    let mut value = normalize_uris(value, &workspace_root);
    rewrite_json_keys(&mut value, &workspace_root);
    sort_json_objects(&mut value);
    value
}

/// The workspace URI currently embeds a per-run temp dir. Rewrite it inside
/// object keys (e.g. the keys of `relatedDocuments`), not just string values.
fn rewrite_json_keys(value: &mut serde_json::Value, workspace_root: &str) {
    if let serde_json::Value::Object(map) = value {
        for value in map.values_mut() {
            rewrite_json_keys(value, workspace_root);
        }
        let entries: Vec<(String, serde_json::Value)> = std::mem::take(map)
            .into_iter()
            .map(|(mut key, value)| {
                if let Some(pos) = key.find(workspace_root) {
                    key.replace_range(pos..pos + workspace_root.len(), "<WORKSPACE_ROOT>");
                }
                (key, value)
            })
            .collect();
        *map = entries.into_iter().collect();
    } else if let serde_json::Value::Array(items) = value {
        for item in items {
            rewrite_json_keys(item, workspace_root);
        }
    }
}

fn sort_json_objects(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                sort_json_objects(value);
            }
            map.sort_keys();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_objects(item);
            }
        }
        _ => {}
    }
}

/// An LSP range covering `needle` inside `text` (ASCII fixture text only).
fn lsp_range_of(text: &str, needle: &str) -> Range {
    let start = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in text: {text}"));
    let end = start + needle.len();
    fn pos(text: &str, idx: usize) -> Position {
        let before = &text[..idx.min(text.len())];
        let line = before.matches('\n').count() as u32;
        let col = before.rfind('\n').map(|i| idx - i - 1).unwrap_or(idx);
        Position {
            line,
            character: col as u32,
        }
    }
    Range {
        start: pos(text, start),
        end: pos(text, end),
    }
}

/// The IDEA experience, tested through the *pull* channel: `B.java` is never
/// opened, yet typing a missing method into `A.java` resolves `B`'s undefined
/// `go()` error in the diagnostics snapshot of `A`.
#[test]
fn cross_file_typing_resolves_unopened_error() {
    let lsp = create_lsp_with_config(json!({ "diagnostics": { "push_unopened": false } }));
    let a = "/src/p/A.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);

    // Seed a comment to trigger the initial reverse-index build pass. The
    // trailing newline keeps the next edit on a fresh, un-commented line.
    lsp.change_at_mark(a, "// seed\n    <|>");

    // B is not open, yet its `go()` error must surface in A's related docs.
    let before = wait_until_pull(&lsp, a, |report| related_entry(report, "B.java").is_some());
    assert!(
        related_entry(&before, "B.java").is_some_and(|entry| {
            entry["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        }),
        "B's undefined `go()` error must appear while B is unopened"
    );
    insta::assert_json_snapshot!(
        "cross_file_typing_unopened_b_error",
        normalize_and_sort(before, &lsp)
    );

    // "Typing" the method into A immediately clears B's error, no save needed.
    lsp.change_at_mark(a, "public void go() {}\n    <|>");
    let after = wait_until_pull(&lsp, a, |report| {
        related_entry(report, "B.java").is_some_and(|entry| {
            entry["items"]
                .as_array()
                .is_some_and(|items| items.is_empty())
        })
    });
    insta::assert_json_snapshot!(
        "cross_file_typing_fixed_b_in_related",
        normalize_and_sort(after, &lsp)
    );

    lsp.shutdown();
}

/// The inverse direction: deleting the method from `A` puts the error back into
/// `B`.
#[test]
fn cross_file_reverts_when_method_removed() {
    let lsp = create_lsp_with_config(json!({ "diagnostics": { "push_unopened": false } }));
    let a = "/src/p/A.java";
    let a_text = "package p;\npublic class A {\n    public void go() {}\n    <|>\n}\n";
    lsp.write_fixture_file(a, a_text);
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);

    // Trigger the build; A and B are both clean.
    lsp.change_at_mark(a, "// seed\n    <|>");
    let clean = wait_until_pull(&lsp, a, |report| {
        related_entry(report, "B.java").is_some_and(|entry| {
            entry["kind"].as_str() == Some("unchanged")
                || entry["items"]
                    .as_array()
                    .is_some_and(|items| items.is_empty())
        })
    });
    insta::assert_json_snapshot!("cross_file_revert_clean", normalize_and_sort(clean, &lsp));

    // Delete the method with an incremental edit (no save).
    let without_mark = a_text.replace("<|>", "");
    let range = lsp_range_of(&without_mark, "    public void go() {}\n");
    lsp.change_document_incremental(a, range, "");

    let broken = wait_until_pull(&lsp, a, |report| {
        related_entry(report, "B.java").is_some_and(|entry| {
            entry["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        })
    });
    insta::assert_json_snapshot!(
        "cross_file_revert_broken_b",
        normalize_and_sort(broken, &lsp)
    );

    lsp.shutdown();
}

/// Deleting a source file on disk (reported via the client's
/// `didChangeWatchedFiles` watcher) must drop it from the VFS source roots, the
/// reverse-dependency index, and the workspace diagnostics — not leave a stale
/// entry behind.
#[test]
fn cross_file_deletes_when_source_file_removed_on_disk() {
    let lsp = create_lsp_with_config(json!({ "diagnostics": { "push_unopened": false } }));
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);

    // Seed the build; both A and B are part of the workspace file set.
    lsp.change_at_mark(a, "// seed\n    <|>");
    request_workspace_until(&lsp, json!([]), |report| {
        report["items"].as_array().is_some_and(|items| {
            items.len() == 2
                && items
                    .iter()
                    .any(|it| it["uri"].as_str().is_some_and(|u| u.ends_with("/B.java")))
        })
    });

    // Delete B on disk and report it through the watcher.
    lsp.remove_file(b);
    lsp.did_change_watched_files(b, FileChangeType::Deleted);

    // B must be dropped from the workspace file set entirely.
    let after = request_workspace_until(&lsp, json!([]), |report| {
        report["items"].as_array().is_some_and(|items| {
            items.len() == 1
                && items
                    .iter()
                    .all(|it| !it["uri"].as_str().is_some_and(|u| u.ends_with("/B.java")))
        })
    });
    insta::assert_json_snapshot!(
        "cross_file_deleted_after_delete",
        normalize_and_sort(after, &lsp)
    );

    lsp.shutdown();
}

/// Editing an unrelated file must not touch the diagnostics of A or B at all.
#[test]
fn cross_file_unrelated_edit_is_isolated() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let c = "/src/p/C.java";
    lsp.write_fixture_file(
        a,
        "package p;\npublic class A {\n    public void go() {}\n}\n",
    );
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.write_fixture_file(
        c,
        "package p;\npublic class C {\n    void n() {\n        <|>\n    }\n}\n",
    );
    lsp.open_document(c);

    // The seed edit triggers the build without changing anything A/B depends on.
    lsp.change_at_mark(c, "int local = 1;<|>");

    // No diagnostics may be pushed for A or B.
    let pushes = lsp.wait_notifications(
        "textDocument/publishDiagnostics",
        1,
        std::time::Duration::from_millis(700),
    );
    assert!(
        pushes.is_empty(),
        "unexpected cross-file pushes: {pushes:#?}"
    );

    // A's pull must not relate to C.
    let report = wait_until_pull(&lsp, a, |r| related_entry(r, "B.java").is_some());
    assert!(
        related_entry(&report, "C.java").is_none(),
        "A must not be related to the unrelated C.java"
    );
    lsp.shutdown();
}

/// `result_id` round-trip: pulling again with the previous id yields a tiny
/// `Unchanged` report instead of re-serializing items.
#[test]
fn cross_file_result_id_roundtrip() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.change_at_mark(a, "// seed\n    <|>");

    let first = wait_until_pull(&lsp, a, |r| related_entry(r, "B.java").is_some());
    let result_id = first["resultId"].as_str().expect("resultId").to_owned();

    let second = wait_until_pull_with_previous(&lsp, a, Some(result_id.clone()), |r| {
        r["kind"].as_str() == Some("unchanged")
    });
    assert_eq!(
        second["resultId"].as_str().unwrap(),
        result_id,
        "Unchanged report must echo the previous result_id"
    );
    assert!(
        second.get("items").is_none(),
        "Unchanged report must not re-serialize items"
    );
    lsp.shutdown();
}

/// `workspace/diagnostic`: the whole-workspace pull returns one full report per
/// source file; echoing the received `(uri, resultId)` pairs back yields all
/// `Unchanged` entries; and fixing A turns B's previously-returned report into
/// a fresh (empty) one.
#[test]
fn cross_file_workspace_pull() {
    let lsp = create_lsp_with_config(json!({ "diagnostics": { "push_unopened": false } }));
    let a = "/src/p/A.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);

    // First pull: a full report per workspace file (A clean, B with the
    // undefined `go()` error) — no prior edit required.
    let first = request_workspace_until(&lsp, json!([]), |report| {
        report["items"]
            .as_array()
            .is_some_and(|items| items.len() == 2)
    });
    insta::assert_json_snapshot!(
        "cross_file_workspace_pull_full",
        normalize_and_sort(first.clone(), &lsp)
    );

    // Re-pull with the received result ids: every document is `unchanged`.
    let previous_ids = extract_previous_ids(&first);
    request_workspace_until(&lsp, previous_ids.clone(), |report| {
        report["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .all(|it| it["kind"].as_str() == Some("unchanged"))
        })
    });

    // Fix A (no save needed): B's stale report must come back full and empty.
    lsp.change_at_mark(a, "public void go() {}\n    <|>");
    let after_fix = request_workspace_until(&lsp, previous_ids.clone(), |report| {
        report["items"].as_array().is_some_and(|items| {
            items.iter().any(|it| {
                it["uri"].as_str().is_some_and(|u| u.ends_with("/B.java"))
                    && it["kind"].as_str() == Some("full")
            })
        })
    });
    insta::assert_json_snapshot!(
        "cross_file_workspace_pull_after_fix",
        normalize_and_sort(after_fix, &lsp)
    );

    lsp.shutdown();
}

/// Closing `B` and then editing `A` still updates `B`'s stored diagnostics via
/// the push channel — no focus change required.
#[test]
fn cross_file_did_close_updates_unopened() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.open_document(b);

    // Seed edit triggers the build. B is open, so nothing is pushed for it.
    lsp.change_at_mark(a, "// seed\n    <|>");
    std::thread::sleep(std::time::Duration::from_millis(400));

    lsp.close_document(b);
    lsp.change_at_mark(a, "public void go() {}\n");

    let pushes = lsp.wait_notifications(
        "textDocument/publishDiagnostics",
        1,
        std::time::Duration::from_secs(5),
    );
    assert_eq!(
        pushes.len(),
        1,
        "expected exactly one push for the closed B.java"
    );
    assert_eq!(pushes[0].method, "textDocument/publishDiagnostics");
    assert!(
        pushes[0].params["uri"]
            .as_str()
            .unwrap()
            .ends_with("/B.java"),
        "push must target B.java: {:#?}",
        pushes[0].params
    );
    assert!(
        pushes[0].params["diagnostics"]
            .as_array()
            .is_some_and(|d| d.is_empty()),
        "B's diagnostics must be cleared after A gains go(): {:#?}",
        pushes[0].params
    );

    lsp.shutdown();
}

/// A burst of body-only edits must neither re-push an unchanged file nor come
/// back as a full related report: steady-state typing stays payload-cheap.
#[test]
fn cross_file_burst_no_duplicate_and_small_payloads() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);

    // Seed edit triggers the build; the initial (broken) B is pushed exactly once.
    lsp.change_at_mark(a, "// seed\n    <|>");
    let pushes = lsp.wait_notifications(
        "textDocument/publishDiagnostics",
        1,
        std::time::Duration::from_secs(5),
    );
    assert_eq!(
        pushes.len(),
        1,
        "initial B report must be pushed exactly once"
    );

    // A burst of five body-only edits (no exported-name change, no dependency
    // change) must not push B again.
    for i in 0..5 {
        lsp.change_at_mark(a, &format!("// burst {i}\n    <|>"));
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    let extra = lsp.wait_notifications(
        "textDocument/publishDiagnostics",
        1,
        std::time::Duration::from_millis(600),
    );
    assert!(extra.is_empty(), "no duplicate push expected: {extra:#?}");

    let report = wait_until_pull(&lsp, a, |r| related_entry(r, "B.java").is_some());
    let normalized = normalize_and_sort(report, &lsp);
    insta::assert_json_snapshot!("cross_file_burst_related", normalized);

    lsp.shutdown();
}

/// Perf-guard: a body-only (idle-scope) edit must resolve B as an *unchanged*
/// related document — no full re-emission, no growing payloads.
#[test]
fn cross_file_idle_edit_emits_unchanged_only() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    lsp.write_fixture_file(
        a,
        "package p;\npublic class A {\n    public void go() {}\n    <|>\n}\n",
    );
    lsp.write_fixture_file(
        "/src/p/B.java",
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.change_at_mark(a, "// seed\n    <|>");
    let _ = wait_until_pull(&lsp, a, |r| related_entry(r, "B.java").is_some());

    lsp.change_at_mark(a, "// idle<|>");
    std::thread::sleep(std::time::Duration::from_millis(600));
    let report = lsp.pull_document_diagnostics_raw_with_previous(a, None);
    if let Some(related) = report.get("relatedDocuments").and_then(|r| r.as_object()) {
        for (uri, value) in related {
            assert_eq!(
                value["kind"].as_str(),
                Some("unchanged"),
                "idle edits must yield only tiny unchanged related reports for {uri}: {value}"
            );
        }
    }
    lsp.shutdown();
}
