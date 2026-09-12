use std::sync::LazyLock;

use lsp_test::{LspHarness, lsp_fixture};
use lsp_types::{FileChangeType, Position, Range};
use serde_json::json;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn setup_logging() -> anyhow::Result<()> {
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(false);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("TEST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

/// Serializes the tests that mutate the process environment (`PATH`,
/// `JAVA_HOME`) to point at a shim build system: the variables are global, so
/// two such tests running concurrently would race for them.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static SETUP: LazyLock<()> = LazyLock::new(|| {
    setup_logging().expect("Failed to setup logger");
});

/// Client config for the snapshot tests.
///
/// `java_home` points at a path that cannot exist, so the server registers no
/// SDK and skips the platform (jimage) stub index: that index runs on the
/// server's task pool while holding a database snapshot, which blocks the main
/// loop's next write — and therefore every request issued before it finishes —
/// for as long as the parse takes. Tests that need platform classes pass their
/// own config (see `test_release_api_diagnostic`). The path must be absolute:
/// `AbsPathBuf::assert_utf8` panics otherwise.
fn default_client_config() -> serde_json::Value {
    json!({ "java_home": std::env::temp_dir().join("caffeine-ls-test-no-jdk") })
}

fn create_lsp() -> LspHarness {
    create_lsp_with_config(default_client_config(), |_| {})
}

fn create_lsp_with_setup(setup: impl FnOnce(&std::path::Path)) -> LspHarness {
    create_lsp_with_config(default_client_config(), setup)
}

fn create_lsp_with_config(
    config: serde_json::Value,
    setup: impl FnOnce(&std::path::Path),
) -> LspHarness {
    LazyLock::force(&SETUP);
    LspHarness::start_with_setup(config, setup, |connection| {
        caffeine_ls::cli::serve::run(connection).unwrap()
    })
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

/// The `(token, kind)` pair of a `$/progress` notification.
fn progress_event(notif: &lsp_server::Notification) -> Option<(String, String)> {
    let token = match notif.params.get("token")? {
        serde_json::Value::String(token) => token.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        _ => return None,
    };
    let kind = notif
        .params
        .get("value")?
        .get("kind")?
        .as_str()?
        .to_string();
    Some((token, kind))
}

#[test]
fn test_workspace_load_reports_progress() {
    let lsp = create_lsp();

    // The temp workspace has no build system, so only the VFS scan phase
    // runs; it must surface as `$/progress` begin/end pairs (which clients
    // only deliver after the `window/workDoneProgress/create` handshake).
    let notifications = lsp.wait_for_notifications(
        "$/progress",
        std::time::Duration::from_secs(10),
        |notifications| {
            let began = notifications
                .iter()
                .filter_map(progress_event)
                .any(|(_, kind)| kind == "begin");
            let ended = notifications
                .iter()
                .filter_map(progress_event)
                .any(|(_, kind)| kind == "end");
            began && ended
        },
    );

    let mut began = std::collections::HashSet::new();
    let mut ended = std::collections::HashSet::new();
    for (token, kind) in notifications.iter().filter_map(progress_event) {
        match kind.as_str() {
            "begin" => {
                began.insert(token);
            }
            "end" => {
                ended.insert(token);
            }
            _ => {}
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
}

/// A fake `gradle` executable that replays realistic console output, so the
/// server's Gradle sync path (and its structured progress reporting) can be
/// exercised without a real JVM/Gradle install.
#[test]
fn test_build_sync_reports_structured_progress() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("gradle");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
echo "Welcome to Gradle 8.0!"
echo "> Task :lib:compileJava"
echo "Downloading https://repo.maven.apache.org/foo-1.0.jar (4.0 KiB)"
echo "Downloaded https://repo.maven.apache.org/foo-1.0.jar (4.0 KiB)"
echo "Configuring project :app"
echo "WORKSPACE_MODEL_BEGIN"
echo '{"workspace_name":"demo","projects":[{"path":":","name":"demo","project_dir":"'$PWD'","source_roots":["'$PWD'/src/main/java"],"test_roots":[],"resource_roots":[],"generated_roots":[],"compile_classpath":[],"test_classpath":[],"java_language_version":"21","java_home":"'$JAVA_HOME'"}]}'
echo "WORKSPACE_MODEL_END"
exit 0
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();

    // Pick a real directory for JAVA_HOME (get_java_home requires is_dir).
    // The temp workspace root is created by the harness later, so fall back to
    // the crate root which certainly exists.
    let java_home = std::env::var("JAVA_HOME")
        .ok()
        .filter(|p| std::path::Path::new(p).is_dir())
        .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").to_string());

    // Set PATH/JAVA_HOME so the server's `Command::new("gradle")` finds the
    // shim and `get_java_home` succeeds. Restored on drop: other tests in this
    // binary read JAVA_HOME to locate a real JDK
    // (`test_release_api_diagnostic`), and clearing it here would make them
    // skip silently.
    struct EnvGuard(Option<std::ffi::OsString>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialized by ENV_LOCK, which outlives this
            // guard (it is declared first, so it drops last).
            unsafe {
                match self.0.take() {
                    Some(previous) => std::env::set_var("JAVA_HOME", previous),
                    None => std::env::remove_var("JAVA_HOME"),
                }
            }
        }
    }
    let java_home_before = std::env::var_os("JAVA_HOME");
    // SAFETY: env mutation is serialized by ENV_LOCK.
    unsafe {
        std::env::set_var("JAVA_HOME", &java_home);
    }
    let _guard = EnvGuard(java_home_before);

    let path_var = std::env::var("PATH").unwrap_or_default();
    // SAFETY: test process is single-threaded at this point.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", shim_dir.path().display(), path_var),
        );
    }

    let lsp = create_lsp_with_setup(|root| {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java")).unwrap();
    });

    lsp.wait_until_workspace_is_loaded();

    let notifications = lsp.wait_for_notifications(
        "$/progress",
        std::time::Duration::from_secs(15),
        |notifications| {
            notifications
                .iter()
                .filter_map(progress_event)
                .any(|(token, kind)| token.starts_with("sync-") && kind == "end")
        },
    );

    let mut messages: Vec<String> = Vec::new();
    let mut saw_100 = false;
    for notif in &notifications {
        let Some((token, _)) = progress_event(notif) else {
            continue;
        };
        if !token.starts_with("sync-") {
            continue;
        }
        if let Some(msg) = notif
            .params
            .get("value")
            .and_then(|v| v.get("message"))
            .and_then(|m| m.as_str())
        {
            messages.push(msg.to_string());
        }
        if let Some(100) = notif
            .params
            .get("value")
            .and_then(|v| v.get("percentage"))
            .and_then(|p| p.as_u64())
        {
            saw_100 = true;
        }
    }

    assert!(
        saw_100,
        "sync progress never reported 100%; messages: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("Configuring project")),
        "expected a Configuring-phase message, got: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Downloading") && m.contains("KiB")),
        "expected a Downloading-phase message with a byte size, got: {messages:?}"
    );
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
    // The file is not part of any source root before the workspace loads, so
    // it reports no diagnostics (syntax included) until it is attached to one.
    let diagnostics = lsp.pull_document_diagnostics("/src/Main.java");

    insta::assert_json_snapshot!("syntax_diagnostics_before_workspace_load", diagnostics);
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

    public void many(int... xs) {}

    public static class Inner {
        private int y;
    }
}

record Point(int x, int y) {}
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
}

#[test]
fn test_workspace_symbols() {
    let lsp = create_lsp();
    lsp.write_file(
        "/src/com/example/Foo.java",
        r#"package com.example;

public class Foo {
    public int x;
    public void bar(int... a) {}
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
    // Only Foo is opened: the empty query must limit to opened files.
    lsp.open_document("/src/com/example/Foo.java");

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
    let normalized = normalize_uris(response.clone(), &workspace_root);
    // The new snapshot must show only Foo's symbols (no Bar.java rows).
    insta::assert_json_snapshot!("workspace_symbols", normalized);

    // Resolve round-trip: the `Foo.bar` row carries data; resolve adds the
    // real location and preserves everything else.
    let bar_row = response
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "Foo.bar")
        .unwrap()
        .clone();
    let resolved = lsp.request("workspaceSymbol/resolve", bar_row.clone());
    assert_eq!(resolved["name"], "Foo.bar");
    assert_eq!(resolved["data"], bar_row["data"]);
    assert_eq!(resolved["location"]["uri"], bar_row["location"]["uri"]);
    assert_eq!(resolved["location"]["range"]["start"]["line"], 4);
    assert_eq!(resolved["location"]["range"]["start"]["character"], 4);
    assert_eq!(resolved["location"]["range"]["end"]["character"], 32);

    // Non-empty queries still search the whole workspace: Bar.java is NOT
    // open, but a typed query finds it.
    let typed = lsp.request("workspace/symbol", json!({ "query": "Bar" }));
    assert_eq!(
        typed
            .as_array()
            .unwrap()
            .iter()
            .filter(|symbol| symbol["name"] == "Bar")
            .count(),
        1
    );

    // Dotted query: `Class.member` resolves via the canonical name, and the
    // row's `name` carries the enclosing type (`Foo.bar`) with the container
    // reduced to the package, so the client's local filter keeps it.
    let dotted = lsp.request("workspace/symbol", json!({ "query": "Foo.bar" }));
    assert_eq!(
        dotted
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["name"] == "Foo.bar")
            .count(),
        1
    );
    assert_eq!(
        dotted.as_array().unwrap()[0]["containerName"],
        "com.example"
    );

    // FQN query: partial package-qualified name still finds the type.
    let fqn = lsp.request("workspace/symbol", json!({ "query": "org.other" }));
    assert_eq!(
        fqn.as_array()
            .unwrap()
            .iter()
            .filter(|s| s["name"] == "Bar")
            .count(),
        1
    );

    // Resolve without data is a request error.
    let error = lsp
        .request_raw(
            "workspaceSymbol/resolve",
            json!({ "name": "Foo", "kind": 5, "location": { "uri": "file:///x" } }),
        )
        .expect_err("resolve without data must be a request error");
    assert_eq!(error.code, lsp_server::ErrorCode::InternalError as i32);
    assert!(error.message.contains("missing data"), "{error:?}");
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

/// The IDEA experience, tested through the *pull* channel: typing a missing
/// method into `A.java` resolves `B`'s undefined `go()` error. `B` is open, so
/// its diagnostics are recomputed by the cross-file refresh pass and surface
/// when pulled.
#[test]
fn cross_file_typing_resolves_dependent_error() {
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

    // Seed a comment to trigger the initial refresh pass.
    lsp.change_at_mark(a, "// seed\n    <|>");

    // B's undefined `go()` error must surface when pulled.
    let before = wait_until_pull(&lsp, b, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| !items.is_empty())
    });
    insta::assert_json_snapshot!(
        "cross_file_typing_dependent_b_error",
        normalize_and_sort(before, &lsp)
    );

    // "Typing" the method into A immediately clears B's error, no save needed.
    lsp.change_at_mark(a, "public void go() {}\n    <|>");
    let after = wait_until_pull(&lsp, b, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| items.is_empty())
    });
    insta::assert_json_snapshot!("cross_file_typing_fixed_b", normalize_and_sort(after, &lsp));
}

/// The inverse direction: deleting the method from `A` puts the error back into
/// the open `B`.
#[test]
fn cross_file_reverts_when_method_removed() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    let a_text = "package p;\npublic class A {\n    public void go() {}\n    <|>\n}\n";
    lsp.write_fixture_file(a, a_text);
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.open_document(b);

    // Trigger the build; A and B are both clean.
    lsp.change_at_mark(a, "// seed\n    <|>");
    let clean = wait_until_pull(&lsp, b, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| items.is_empty())
    });
    insta::assert_json_snapshot!("cross_file_revert_clean", normalize_and_sort(clean, &lsp));

    // Delete the method with an incremental edit (no save).
    let without_mark = a_text.replace("<|>", "");
    let range = lsp_range_of(&without_mark, "    public void go() {}\n");
    lsp.change_document_incremental(a, range, "");

    let broken = wait_until_pull(&lsp, b, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| !items.is_empty())
    });
    insta::assert_json_snapshot!(
        "cross_file_revert_broken_b",
        normalize_and_sort(broken, &lsp)
    );
}

/// Deleting a source file on disk (reported via the client's
/// `didChangeWatchedFiles` watcher) must drop it from the VFS source roots and
/// the workspace diagnostics — not leave a stale entry behind.
#[test]
fn cross_file_deletes_when_source_file_removed_on_disk() {
    let lsp = create_lsp();
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
}

/// After a source file is deleted, pulling diagnostics for its URI (e.g. the
/// open-tab pull the client issues as the file disappears) must return a clean
/// empty report — not an internal error that surfaces to the user.
#[test]
fn cross_file_deleted_file_diagnostic_is_empty_not_error() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.change_at_mark(a, "// seed\n    <|>");
    request_workspace_until(&lsp, json!([]), |r| {
        r["items"].as_array().is_some_and(|items| items.len() == 2)
    });

    lsp.remove_file(b);
    lsp.did_change_watched_files(b, FileChangeType::Deleted);

    // Wait until B is gone from the workspace file set.
    request_workspace_until(&lsp, json!([]), |r| {
        r["items"].as_array().is_some_and(|items| items.len() == 1)
    });

    // Pulling the deleted file's diagnostics must yield an empty full report.
    let uri = lsp.uri(b);
    let response = lsp
        .request_raw(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        )
        .expect("deleted file pull must not be an internal error");
    assert_eq!(
        response["kind"].as_str(),
        Some("full"),
        "deleted file pull must be a full report: {response}"
    );
    assert_eq!(
        response["items"].as_array().map(Vec::len),
        Some(0),
        "deleted file pull must carry no items: {response}"
    );
}

/// Deleting a file must refresh the diagnostics of its watched dependents: a
/// watched `B` that referenced the deleted `A` now reports the unresolved
/// symbol, not a stale report.
#[test]
fn cross_file_delete_refreshes_dependent_diagnostics() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    lsp.write_fixture_file(a, "package p;\npublic class A {\n    <|>\n}\n");
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m() { A a = null; a.go(); }\n}\n",
    );
    lsp.open_document(a);
    lsp.open_document(b);

    // Seed the build. B is watched and currently has the undefined-`go()` error.
    lsp.change_at_mark(a, "// seed\n    <|>");

    // Close A and delete it on disk, reporting the change to the server; only a
    // closed file can be re-read as missing by the loader.
    lsp.close_document(a);
    lsp.remove_file(a);
    lsp.did_change_watched_files(a, FileChangeType::Deleted);

    // The watched B must now report the unresolved `A` when pulled.
    let report = wait_until_pull(&lsp, b, |r| {
        r["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|it| it["message"].as_str().is_some_and(|m| m.contains("A")))
        })
    });
    assert!(
        report["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|it| it["message"].as_str().is_some_and(|m| m.contains("A")))
        }),
        "B must report the unresolved A: {report}"
    );
}

/// Adding a file must refresh the diagnostics of the watched files that
/// referenced it: a watched `B` whose reference to a missing `A` was an error
/// is reported clean once `A` appears on disk.
#[test]
fn cross_file_add_refreshes_dependent_diagnostics() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let b = "/src/p/B.java";
    let c = "/src/p/C.java";
    // B references A before A exists; C is the other watched seed file.
    lsp.write_fixture_file(
        b,
        "package p;\npublic class B {\n    void m() { A a = null; a.go(); }\n}\n",
    );
    lsp.write_fixture_file(c, "package p;\npublic class C {\n    <|>\n}\n");
    lsp.open_document(b);
    lsp.open_document(c);

    // Seed the build; B is watched and currently has an unresolved-A error.
    lsp.change_at_mark(c, "// seed\n    <|>");
    let before = wait_until_pull(&lsp, b, |r| {
        r["items"].as_array().is_some_and(|items| !items.is_empty())
    });
    assert!(
        before["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "B must report the unresolved A before A exists: {before}"
    );

    // Add A on disk and report it through the watcher.
    lsp.write_fixture_file(
        a,
        "package p;\npublic class A {\n    public void go() {}\n}\n",
    );
    lsp.did_change_watched_files(a, FileChangeType::Created);

    // B must be reported clean once A appears.
    let after = wait_until_pull(&lsp, b, |r| {
        r["items"].as_array().is_some_and(|items| items.is_empty())
    });
    assert!(
        after["items"]
            .as_array()
            .is_some_and(|items| items.is_empty()),
        "B's diagnostics must clear after A appears: {after}"
    );
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
    lsp.open_document("/src/p/B.java");
    lsp.change_at_mark(a, "// seed\n    <|>");

    let first = wait_until_pull(&lsp, a, |r| r["kind"].as_str() == Some("full"));
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
}

/// `workspace/diagnostic`: the whole-workspace pull returns one full report per
/// source file; echoing the received `(uri, resultId)` pairs back yields all
/// `Unchanged` entries; and fixing A turns B's previously-returned report into
/// a fresh (empty) one.
#[test]
fn cross_file_workspace_pull() {
    let lsp = create_lsp();
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
}

/// A burst of body-only edits must not move a file's result id: the
/// deterministic fingerprint keeps steady-state typing payload-cheap.
#[test]
fn cross_file_burst_keeps_stable_result_ids() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let fixture = "\
package p;
public class A {
    public void go() {
        this.undefinedMethod();
        <|>
    }
}
";
    lsp.write_fixture_file(a, fixture);
    lsp.open_document(a);

    // Baseline: A's broken report and its result id.
    let first = wait_until_pull(&lsp, a, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| !items.is_empty())
    });
    let first_id = first["resultId"].as_str().expect("resultId").to_owned();

    // A burst of body-only edits (no exported-name change, no dependency change)
    // must not move the result id: the fingerprint is a pure function of the
    // diagnostics, so re-pulling echoes `Unchanged`.
    for i in 0..5 {
        lsp.change_at_mark(a, &format!("// burst {i}\n    <|>"));
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    let after = lsp.pull_document_diagnostics_raw_with_previous(a, Some(first_id.clone()));
    assert_eq!(
        after["kind"].as_str(),
        Some("unchanged"),
        "body-only edits must not change the report: {after}"
    );
    assert_eq!(after["resultId"].as_str(), Some(first_id.as_str()));
}

/// A `textDocument/diagnostic` pull issued immediately after `didChange` must
/// reflect the new snapshot, never the pre-edit state: the handler derives from
/// the current analysis, so it must not echo a stale `Unchanged` `resultId`.
/// Both directions are checked: fixing an error must clear it, and
/// reintroducing it must surface it.
#[test]
fn document_pull_after_did_change_is_full_not_unchanged() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let fixture = "\
package p;
public class A {
    public void go() {
        this.undefinedMethod();
    }
}
";
    lsp.write_fixture_file(a, fixture);
    lsp.open_document(a);

    // Baseline: the unresolved call is a full report with a result id.
    let first = wait_until_pull(&lsp, a, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| !items.is_empty())
    });
    let first_id = first["resultId"].as_str().expect("resultId").to_owned();

    // Fix the call, then pull with the stale previous result id immediately —
    // no wait for the background pass. The stale `unchanged` would retain the
    // error, so this must come back full and empty.
    let range = lsp_range_of(fixture, "this.undefinedMethod();");
    lsp.change_document_incremental(a, range, "// fixed");
    let after_fix = lsp.pull_document_diagnostics_raw_with_previous(a, Some(first_id.clone()));
    assert_eq!(
        after_fix["kind"].as_str(),
        Some("full"),
        "fix edit must re-derive a full report, not echo the stale unchanged id: {after_fix}"
    );
    assert_ne!(
        after_fix["resultId"].as_str(),
        Some(first_id.as_str()),
        "resultId must advance after the edit"
    );
    assert_eq!(
        after_fix["items"].as_array().map(Vec::len),
        Some(0),
        "the fixed report must no longer carry the unresolved-call error: {after_fix}"
    );

    // Reintroduce the error and pull with the just-delivered (clean) result id:
    // the new syntax/type error must surface immediately.
    let fixed_id = after_fix["resultId"].as_str().expect("resultId").to_owned();
    let range = lsp_range_of(
        &fixture.replace("this.undefinedMethod();", "// fixed"),
        "// fixed",
    );
    lsp.change_document_incremental(a, range, "this.undefinedMethod();");
    let after_break = lsp.pull_document_diagnostics_raw_with_previous(a, Some(fixed_id));
    assert_eq!(
        after_break["kind"].as_str(),
        Some("full"),
        "reintroduced error must come back full, not unchanged: {after_break}"
    );
    assert!(
        after_break["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "reintroduced error must be reported immediately: {after_break}"
    );
}

/// `workspace/diagnostic` must behave like the single-document pull: a pull
/// issued immediately after `didChange` must re-derive the edited (open) file
/// from the current snapshot instead of serving its pre-edit cached generation
/// as `Unchanged`. Previously the subscribed-file cache was trusted until the
/// debounced pass ran, so the workspace channel retained stale errors the
/// single-document channel had already cleared.
#[test]
fn workspace_pull_after_did_change_is_full_not_unchanged() {
    let lsp = create_lsp();
    let a = "/src/p/A.java";
    let fixture = "\
package p;
public class A {
    public void go() {
        this.undefinedMethod();
    }
}
";
    lsp.write_fixture_file(a, fixture);
    lsp.open_document(a);

    // Subscribe A (open + first document pull) and settle on the error state.
    let _ = wait_until_pull(&lsp, a, |r| {
        r["kind"].as_str() == Some("full")
            && r["items"].as_array().is_some_and(|items| !items.is_empty())
    });

    // Baseline workspace pull: capture the per-file result id of the error state.
    let baseline = request_workspace_until(&lsp, json!([]), |report| {
        report["items"].as_array().is_some_and(|items| {
            items.iter().any(|it| {
                it["uri"].as_str().is_some_and(|u| u.ends_with("/A.java"))
                    && it["kind"].as_str() == Some("full")
            })
        })
    });
    let previous_ids = extract_previous_ids(&baseline);
    let baseline_id = baseline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["uri"].as_str().is_some_and(|u| u.ends_with("/A.java")))
        .and_then(|it| it["resultId"].as_str())
        .expect("A baseline resultId")
        .to_owned();

    // Fix the call and pull the workspace with the stale ids immediately.
    let range = lsp_range_of(fixture, "this.undefinedMethod();");
    lsp.change_document_incremental(a, range, "// fixed");
    let after_fix = lsp.request(
        "workspace/diagnostic",
        json!({ "previousResultIds": previous_ids }),
    );
    let a_item = after_fix["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["uri"].as_str().is_some_and(|u| u.ends_with("/A.java")))
        .expect("A present in workspace report")
        .clone();
    assert_eq!(
        a_item["kind"].as_str(),
        Some("full"),
        "A must be re-derived full after the edit, not stale-unchanged: {after_fix}"
    );
    assert_ne!(
        a_item["resultId"].as_str(),
        Some(baseline_id.as_str()),
        "workspace resultId must match the single-document resultId lifecycle"
    );
    assert_eq!(
        a_item["items"].as_array().map(Vec::len),
        Some(0),
        "the fixed workspace report must be clean: {after_fix}"
    );
}

/// The release-view check end to end through the build-system path: a Gradle
/// shim reports `--release 8` while `JAVA_HOME` points at a real JDK 9+, so the
/// server resolves `java.util.SequencedCollection` and `List.of` against the
/// runtime JDK and must report both as `api-not-supported-in-release`
/// ([JEP 247](https://openjdk.org/jeps/247)).
#[test]
fn test_release_api_diagnostic() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let java_home = std::env::var("JAVA_HOME")
        .ok()
        .filter(|p| std::path::Path::new(p).join("lib/ct.sym").is_file());
    let Some(java_home) = java_home else {
        eprintln!("skipping: JAVA_HOME is unset or ships no lib/ct.sym");
        return;
    };

    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("gradle");
    std::fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
echo "WORKSPACE_MODEL_BEGIN"
echo '{{"workspace_name":"demo","projects":[{{"path":":","name":"demo","project_dir":"'$PWD'","source_roots":["'$PWD'/src/main/java"],"test_roots":[],"resource_roots":[],"generated_roots":[],"compile_classpath":[],"test_classpath":[],"java_release":8,"java_language_version":"8","java_home":"{java_home}"}}]}}'
echo "WORKSPACE_MODEL_END"
exit 0
"#
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();

    // SAFETY: env mutation is serialized by ENV_LOCK, which outlives this guard
    // (it is declared first, so it drops last).
    let java_home_before = std::env::var_os("JAVA_HOME");
    unsafe {
        std::env::set_var("JAVA_HOME", &java_home);
    }
    struct EnvGuard(Option<std::ffi::OsString>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                match self.0.take() {
                    Some(previous) => std::env::set_var("JAVA_HOME", previous),
                    None => std::env::remove_var("JAVA_HOME"),
                }
            }
        }
    }
    let _guard = EnvGuard(java_home_before);

    let path_var = std::env::var("PATH").unwrap_or_default();
    // SAFETY: single-threaded, as above.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", shim_dir.path().display(), path_var),
        );
    }

    let lsp = create_lsp_with_config(json!({ "java_home": java_home }), |root| {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java/demo")).unwrap();
        std::fs::write(
            root.join("src/main/java/demo/Main.java"),
            "package demo;\n\nclass Main {\n    void f() {\n        java.util.SequencedCollection<String> c = null;\n        java.util.List<String> l = java.util.List.of(\"a\");\n    }\n}\n",
        )
        .unwrap();
    });

    let path = "/src/main/java/demo/Main.java";
    lsp.open_document(path);

    // The Gradle sync, the workspace load it drives, and the platform stub index
    // all run on the server's own threads; the gate returns only once every
    // progress token has ended, so the pull below sees the loaded workspace.
    lsp.wait_until_workspace_is_loaded();

    let diagnostics = lsp.pull_document_diagnostics(path);

    let lsp_types::DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) =
        &diagnostics
    else {
        panic!("expected a full diagnostic report, got: {diagnostics:?}");
    };
    let codes: Vec<String> = report
        .full_document_diagnostic_report
        .items
        .iter()
        .map(|diag| match diag.code.as_ref() {
            Some(lsp_types::Code::String(code)) => code.clone(),
            other => format!("{other:?}"),
        })
        .collect();

    let messages: Vec<&str> = report
        .full_document_diagnostic_report
        .items
        .iter()
        .map(|diag| match &diag.message {
            lsp_types::Message::String(message) => message.as_str(),
            other => panic!("expected a string diagnostic message, got: {other:?}"),
        })
        .collect();

    assert!(
        codes
            .iter()
            .all(|code| code.contains("api-not-supported-in-release")),
        "expected only release reports, got: {codes:?}"
    );
    // `SequencedCollection` arrived in release 21 and `List.of` in release 9, so a
    // `--release 8` compile must flag both: the single-report expectation this
    // used to carry was recorded before the platform stub index was awaited, and
    // could not be reproduced with a real JDK (the pre-gate pull saw only the
    // class report).
    assert_eq!(
        messages.len(),
        2,
        "expected the release reports of `SequencedCollection` and `List.of`, got: {diagnostics:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("SequencedCollection")),
        "expected the `SequencedCollection` report, got: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("java.util.List") && message.contains("of(")),
        "expected the `List.of` report, got: {messages:?}"
    );

    insta::assert_json_snapshot!("release_api_diagnostic", diagnostics);
}

/// The deprecation warnings of
/// [JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)
/// end to end: `Thread.stop()` is `@Deprecated(forRemoval = true)` and
/// `Thread.getId()` plainly `@Deprecated` in a modern JDK, and both are
/// reported — a client's configuration has no lint switch, so the terminal
/// and the ordinary warning arrive together. The messages are javac's own:
/// `stop() in Thread has been deprecated and marked for removal`,
/// `getId() in Thread has been deprecated`.
///
/// Requires a real JDK (`JAVA_HOME`), like the release-view test: the platform
/// stub index is what makes `java.lang.Thread` resolvable here.
#[test]
fn test_deprecation_diagnostics() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let java_home = std::env::var("JAVA_HOME")
        .ok()
        .filter(|p| std::path::Path::new(p).join("lib/modules").is_file());
    let Some(java_home) = java_home else {
        eprintln!("skipping: JAVA_HOME is unset or ships no lib/modules");
        return;
    };

    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("gradle");
    std::fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
echo "WORKSPACE_MODEL_BEGIN"
echo '{{"workspace_name":"demo","projects":[{{"path":":","name":"demo","project_dir":"'$PWD'","source_roots":["'$PWD'/src/main/java"],"test_roots":[],"resource_roots":[],"generated_roots":[],"compile_classpath":[],"test_classpath":[],"java_release":21,"java_language_version":"21","java_home":"{java_home}"}}]}}'
echo "WORKSPACE_MODEL_END"
exit 0
"#
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();

    // SAFETY: env mutation is serialized by ENV_LOCK, which outlives this guard
    // (it is declared first, so it drops last).
    let java_home_before = std::env::var_os("JAVA_HOME");
    let path_var = std::env::var("PATH").unwrap_or_default();
    unsafe {
        std::env::set_var("JAVA_HOME", &java_home);
        std::env::set_var(
            "PATH",
            format!("{}:{}", shim_dir.path().display(), path_var),
        );
    }
    struct EnvGuard(Option<std::ffi::OsString>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: as above.
            unsafe {
                match self.0.take() {
                    Some(previous) => std::env::set_var("JAVA_HOME", previous),
                    None => std::env::remove_var("JAVA_HOME"),
                }
            }
        }
    }
    let _guard = EnvGuard(java_home_before);

    let lsp = create_lsp_with_config(json!({ "java_home": java_home }), |root| {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java/demo")).unwrap();
        std::fs::write(
            root.join("src/main/java/demo/Main.java"),
            "package demo;\n\nclass Main {\n    void f(Thread t) {\n        t.stop();\n        t.getId();\n    }\n}\n",
        )
        .unwrap();
    });

    let path = "/src/main/java/demo/Main.java";
    lsp.open_document(path);
    lsp.wait_until_workspace_is_loaded();

    let report = lsp.pull_document_diagnostics(path);
    let lsp_types::DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(report) = report
    else {
        panic!("expected a full diagnostic report, got: {report:?}");
    };
    let items: Vec<serde_json::Value> = report
        .full_document_diagnostic_report
        .items
        .iter()
        .map(|item| serde_json::to_value(item).unwrap())
        .collect();

    for (code, message) in [
        (
            "compiler.warn.has.been.deprecated.for.removal",
            "stop() in Thread has been deprecated and marked for removal",
        ),
        (
            "compiler.warn.has.been.deprecated",
            "getId() in Thread has been deprecated",
        ),
    ] {
        assert!(
            items.iter().any(|item| {
                item["severity"] == 2
                    && item["code"] == code
                    && item["message"].as_str().is_some_and(|m| m == message)
            }),
            "expected {code} ({message}), got: {items:?}"
        );
    }
    assert_eq!(
        items.len(),
        2,
        "exactly the two deprecations of `Thread` are expected: {items:?}"
    );

    insta::assert_json_snapshot!("deprecation_diagnostics", items);
}

/// A dependency jar beside its sibling `-sources.jar`: `textDocument/definition`
/// on a type the jar declares materializes the one source file it needs out of
/// the archive, loads it into the database, and answers with the real source
/// location — a classfile stub alone has no file and no range.
///
/// The model JSON reports the jar as a `flat-file` origin, so the sources are
/// found by the sibling probe (`<stem>-sources.jar`) and not by the build
/// system.
#[test]
fn library_source_definition_materializes_and_navigates() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let foo_source =
        "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";
    let app_source = "package app;\n\nclass App {\n    Object make() {\n        return new com.example.Foo();\n    }\n}\n";
    let app_path = "/src/main/java/app/App.java";

    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("gradle");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
echo "WORKSPACE_MODEL_BEGIN"
echo '{"workspace_name":"demo","projects":[{"path":":","name":"demo","project_dir":"'$PWD'","source_roots":["'$PWD'/src/main/java"],"test_roots":[],"resource_roots":[],"generated_roots":[],"compile_classpath":[{"type":"jar","path":"'$PWD'/lib/foo.jar","origin":"flat-file"}],"test_classpath":[],"java_language_version":"21","java_home":"'$JAVA_HOME'"}]}'
echo "WORKSPACE_MODEL_END"
exit 0
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&shim).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).unwrap();

    let path_var = std::env::var("PATH").unwrap_or_default();
    // SAFETY: env mutation is serialized by ENV_LOCK, which outlives this test.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", shim_dir.path().display(), path_var),
        );
    }

    let lsp = create_lsp_with_config(default_client_config(), |root| {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java/app")).unwrap();
        std::fs::write(root.join("src/main/java/app/App.java"), app_source).unwrap();
        lsp_test::classfile::build_jar(
            &root.join("lib/foo.jar"),
            &[(
                "com/example/Foo.class",
                lsp_test::classfile::class_bytes("com/example/Foo", &[], &[("greet", 1)]),
            )],
        )
        .unwrap();
        lsp_test::classfile::build_jar(
            &root.join("lib/foo-sources.jar"),
            &[("com/example/Foo.java", foo_source.as_bytes().to_vec())],
        )
        .unwrap();
    });

    lsp.open_document(app_path);
    lsp.wait_until_workspace_is_loaded();

    let (line, character) = position_of(app_source, "com.example.Foo()");
    let params = json!({
        "textDocument": { "uri": lsp.uri(app_path) },
        "position": { "line": line, "character": character },
    });

    // The request defers, the server materializes the file and re-runs the
    // request, so this one call returns the final answer.
    let response = lsp.request("textDocument/definition", params.clone());
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");

    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let materialized_path = uri.to_file_path().expect("a file URI");
    let cache_sources = lsp.cache_dir().join("sources").join("v1");
    assert!(
        materialized_path.starts_with(&cache_sources),
        "expected a path under {}, got {}",
        cache_sources.display(),
        materialized_path.display()
    );
    assert!(
        materialized_path.ends_with("com/example/Foo.java"),
        "unexpected path: {}",
        materialized_path.display()
    );
    assert!(
        materialized_path.is_file(),
        "the materialized source must exist on disk: {}",
        materialized_path.display()
    );
    assert_range_covers(locations[0]["range"].clone(), foo_source, "class Foo");

    // Only the one file the reference needed is on disk.
    let materialized = java_file_names(&cache_sources);
    assert_eq!(
        materialized,
        vec!["Foo.java".to_string()],
        "only the navigated file is materialized"
    );

    // The second request answers from the loaded file — the deferred path runs
    // once per source file, not once per request.
    let second = lsp.request("textDocument/definition", params);
    assert_eq!(
        second, response,
        "the second request answers from the loaded file"
    );
    assert_eq!(
        java_file_names(&cache_sources),
        materialized,
        "re-asking materializes nothing new"
    );

    // A library source file is read-only third-party code: its report is empty.
    let report = lsp.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": locations[0]["uri"].clone() } }),
    );
    assert_eq!(
        report["items"].as_array().map(Vec::len),
        Some(0),
        "library sources report no diagnostics: {report:?}"
    );
}

/// Asserts that an LSP range covers `needle` inside `text`.
fn assert_range_covers(range: serde_json::Value, text: &str, needle: &str) {
    let (line, character) = position_of(text, needle);
    let start = &range["start"];
    let end = &range["end"];
    let start = (
        start["line"].as_u64().unwrap(),
        start["character"].as_u64().unwrap(),
    );
    let end = (
        end["line"].as_u64().unwrap(),
        end["character"].as_u64().unwrap(),
    );
    let needle = (line as u64, character as u64);
    assert!(
        start <= needle && needle <= end,
        "range {start:?}..{end:?} must cover `{needle:?}`"
    );
}

/// The names of every materialized `.java` file under `root`, sorted.
fn java_file_names(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "java"))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}
