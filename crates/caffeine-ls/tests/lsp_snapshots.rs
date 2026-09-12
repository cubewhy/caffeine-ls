use std::path::PathBuf;
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
    test_constructor_and_static_import_diagnostics,
    r#"
    //- /src/com/example/Example.java
    package com.example;

    public class Example {
        public Example(int x) {}
    }

    //- /src/com/example/Main.java
    package com.example;

    import static org.objectweb.asm.ClassWriter;

    public class Main extends Example {
        ClassWriter writer;

        public Main(int a) {
            super(1);
            this(2, 3);
        }

        public Main(int a, int b) {
        }
    }
    "#,
    |lsp| {
        // Both files must be in the source-set graph before the subclass's
        // superclass and the static import are resolved; a pull issued first
        // would report the unloaded-workspace view instead.
        lsp.wait_until_workspace_is_loaded();
        lsp.open_document("/src/com/example/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/com/example/Main.java");

        insta::assert_json_snapshot!("constructor_and_static_import_diagnostics", diagnostics);
    }
);

lsp_test!(
    test_superinterface_kind_diagnostics,
    r#"
    //- /src/com/example/Example.java
    package com.example;

    public class Example {
    }

    //- /src/com/example/Main.java
    package com.example;

    public class Main implements Example {
    }
    "#,
    |lsp| {
        // Both files must be in the source-set graph before the type is
        // resolved; a pull issued first would report the unloaded-workspace
        // view instead.
        lsp.wait_until_workspace_is_loaded();
        lsp.open_document("/src/com/example/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/com/example/Main.java");

        insta::assert_json_snapshot!("superinterface_kind_diagnostics", diagnostics);
    }
);

lsp_test!(
    test_extends_interface_diagnostics,
    r#"
    //- /src/com/example/Example.java
    package com.example;

    public interface Example {
    }

    //- /src/com/example/Main.java
    package com.example;

    public class Main extends Example {
    }
    "#,
    |lsp| {
        // Both files must be in the source-set graph before the superclass is
        // resolved; a pull issued first would report the unloaded-workspace
        // view instead.
        lsp.wait_until_workspace_is_loaded();
        lsp.open_document("/src/com/example/Main.java");
        let diagnostics = lsp.pull_document_diagnostics("/src/com/example/Main.java");

        insta::assert_json_snapshot!("extends_interface_diagnostics", diagnostics);
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

/// The LSP position of byte `offset` in `text` (ASCII fixture files only).
fn position_at(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let last = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = before[last..].chars().count() as u32;
    (line, character)
}

/// The LSP position of the middle of `needle` (ASCII fixture files only).
fn position_of(text: &str, needle: &str) -> (u32, u32) {
    position_at(
        text,
        text.find(needle).expect("needle in text") + needle.len() / 2,
    )
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

/// The workspace of [`definition_matrix_over_a_workspace`], written below
/// `src/com/example/`: a base class with two overloads and a static field, a
/// subclass, a static-import helper, an annotation type and an enum.
const DEFINITION_MATRIX_FILES: &[(&str, &str)] = &[
    (
        "/src/com/example/Base.java",
        r#"package com.example;

public class Base {
    public static int STATIC = 1;

    public int count;

    public Base self;

    public Base() {}

    public Base(int n) {}

    public void method(int n) {}

    public void method(long n) {}
}
"#,
    ),
    (
        "/src/com/example/Sub.java",
        r#"package com.example;

public class Sub extends Base {
    public Sub(int n) {}
}
"#,
    ),
    (
        "/src/com/example/Helper.java",
        r#"package com.example;

public class Helper {
    public static Base of() {
        return null;
    }

    public static void sum(int... xs) {}
}
"#,
    ),
    (
        "/src/com/example/Marker.java",
        r#"package com.example;

public @interface Marker {}
"#,
    ),
    (
        "/src/com/example/E.java",
        r#"package com.example;

public enum E {
    FIRST,
    SECOND
}
"#,
    ),
    (
        "/src/com/example/Use.java",
        r#"package com.example;

import com.example.Base;
import static com.example.Helper.of;
import static com.example.Helper.sum;

class Use<R> extends Base {
    Use() {
        this(0);
    }

    Use(int n) {
        super(n);
    }

    @Marker
    int marked;

    R value;

    <T> T id(T v) {
        T copy = v;
        return copy;
    }

    void run(Base b, Base other, java.util.List<Base> list) {
        count = b.count;
        method(1);
        super.method(1);
        this.count = 1;
        b.method(1L);
        int s = Base.STATIC;
        E e = E.FIRST;
        switch (e) {
            case SECOND:
                break;
        }
        of();
        sum(1, 2);
        boolean is = other instanceof Base;
        Class<?> lit = Base.class;
        Factory f = (Base elem) -> elem.count;
        Base cast = (Base) other;
        int local = 0;
        int missing = nope + 1;
        new Sub();
        new Sub(1);
    }

    interface Factory {
        int size(Base b);
    }
}
"#,
    ),
];

/// One row of [`definition_matrix_over_a_workspace`]: the file whose reference
/// is requested, the needle locating it (the `occurrence`-th occurrence, at its
/// first character), the file the response must name, a needle inside that
/// target's declaration, and the declaration's own name — the exact identifier
/// the response range must be.
type DefinitionRow = (
    &'static str,
    (&'static str, usize),
    &'static str,
    &'static str,
    &'static str,
);

/// The reference → declaration matrix. Every row drives the real server over
/// stdio and asserts the one location it answers with.
const DEFINITION_MATRIX: &[DefinitionRow] = &[
    // §7.5.1: a single-type import names the class it imports.
    (
        "/src/com/example/Use.java",
        ("Base;", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    // §8.1.4: an `extends` clause names the superclass.
    (
        "/src/com/example/Sub.java",
        ("Base {", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    // §8.8.9/§15.9: a class creation with no applicable constructor names the
    // class itself; with one, the constructor declaration.
    (
        "/src/com/example/Use.java",
        ("new Sub()", 0),
        "/src/com/example/Sub.java",
        "class Sub",
        "Sub",
    ),
    (
        "/src/com/example/Use.java",
        ("new Sub(1)", 0),
        "/src/com/example/Sub.java",
        "Sub(int",
        "Sub",
    ),
    // §8.8.7.1: an explicit constructor invocation `this(...)`/`super(...)`
    // names the constructor it delegates to, selected by its parameter list
    // exactly as a class instance creation's is.
    (
        "/src/com/example/Use.java",
        ("this(0)", 0),
        "/src/com/example/Use.java",
        "Use(int n)",
        "Use",
    ),
    (
        "/src/com/example/Use.java",
        ("super(n)", 0),
        "/src/com/example/Base.java",
        "Base(int n)",
        "Base",
    ),
    // §15.8.3/§15.8.4: a `this` keyword names the enclosing class and a
    // `super` keyword the direct superclass — never the field or method the
    // access reads.
    (
        "/src/com/example/Use.java",
        ("this.count", 0),
        "/src/com/example/Use.java",
        "class Use",
        "Use",
    ),
    (
        "/src/com/example/Use.java",
        ("super.method", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    // §6.5.6.1: a field of the implicit `this`, and of an explicit receiver.
    (
        "/src/com/example/Use.java",
        ("count = b", 0),
        "/src/com/example/Base.java",
        "count",
        "count",
    ),
    (
        "/src/com/example/Use.java",
        ("count;", 0),
        "/src/com/example/Base.java",
        "count",
        "count",
    ),
    // §15.12.2: overload selection — the invoked declaration, not the first
    // same-named one, and `super`'s declaration rather than an override.
    (
        "/src/com/example/Use.java",
        ("method(1);", 0),
        "/src/com/example/Base.java",
        "void method(int",
        "method",
    ),
    (
        "/src/com/example/Use.java",
        ("method(1)", 1),
        "/src/com/example/Base.java",
        "void method(int",
        "method",
    ),
    (
        "/src/com/example/Use.java",
        ("method(1L)", 0),
        "/src/com/example/Base.java",
        "void method(long",
        "method",
    ),
    (
        "/src/com/example/Use.java",
        ("STATIC", 0),
        "/src/com/example/Base.java",
        "STATIC",
        "STATIC",
    ),
    // §7.5.4: a static import names the member its last segment writes, and the
    // call site names the same declaration.
    (
        "/src/com/example/Use.java",
        ("of;", 0),
        "/src/com/example/Helper.java",
        "static Base of(",
        "of",
    ),
    (
        "/src/com/example/Use.java",
        ("of();", 0),
        "/src/com/example/Helper.java",
        "static Base of(",
        "of",
    ),
    // §15.12.2.4: a variable-arity declaration.
    (
        "/src/com/example/Use.java",
        ("sum(1, 2)", 0),
        "/src/com/example/Helper.java",
        "void sum(int...",
        "sum",
    ),
    // §8.9.2/§14.11.1: an enum constant through its type and through a `case`
    // label.
    (
        "/src/com/example/Use.java",
        ("FIRST", 0),
        "/src/com/example/E.java",
        "FIRST",
        "FIRST",
    ),
    (
        "/src/com/example/Use.java",
        ("SECOND:", 0),
        "/src/com/example/E.java",
        "SECOND",
        "SECOND",
    ),
    // §9.7: an annotation name.
    (
        "/src/com/example/Use.java",
        ("Marker", 0),
        "/src/com/example/Marker.java",
        "@interface Marker",
        "Marker",
    ),
    // §6.5.5.1: declaration-side and body type references.
    (
        "/src/com/example/Use.java",
        ("Base;", 1),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    (
        "/src/com/example/Use.java",
        ("Base.class", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    (
        "/src/com/example/Use.java",
        ("Base> list", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    (
        "/src/com/example/Use.java",
        ("Base) other", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    // §15.27.2: a lambda parameter names its own declarator.
    (
        "/src/com/example/Use.java",
        ("elem.count", 0),
        "/src/com/example/Use.java",
        "elem",
        "elem",
    ),
    // §4.4/§6.4.1: a written type variable names the parameter that declares it
    // — the class's own, and a method's own.
    (
        "/src/com/example/Use.java",
        ("R value", 0),
        "/src/com/example/Use.java",
        "<R>",
        "R",
    ),
    (
        "/src/com/example/Use.java",
        ("T copy", 0),
        "/src/com/example/Use.java",
        "<T>",
        "T",
    ),
    // §6.5.5.1: a class naming itself — the declared type of a field of its own
    // class names the class, not the field's own declaration.
    (
        "/src/com/example/Base.java",
        ("Base self", 0),
        "/src/com/example/Base.java",
        "class Base",
        "Base",
    ),
    // A declaration's own name (§6.3: a declaration is not a reference to
    // itself) still has a definition: goto-definition answers with the
    // declaration it names — the class, the field, the method, the local, the
    // lambda parameter and the type parameter.
    (
        "/src/com/example/Use.java",
        ("Use<R> extends", 0),
        "/src/com/example/Use.java",
        "class Use",
        "Use",
    ),
    (
        "/src/com/example/Base.java",
        ("count;", 0),
        "/src/com/example/Base.java",
        "count",
        "count",
    ),
    (
        "/src/com/example/Base.java",
        ("method(int n)", 0),
        "/src/com/example/Base.java",
        "void method(int n",
        "method",
    ),
    (
        "/src/com/example/Use.java",
        ("local = 0", 0),
        "/src/com/example/Use.java",
        "local",
        "local",
    ),
    (
        "/src/com/example/Use.java",
        ("elem) ->", 0),
        "/src/com/example/Use.java",
        "elem",
        "elem",
    ),
    (
        "/src/com/example/Use.java",
        ("R> extends", 0),
        "/src/com/example/Use.java",
        "<R>",
        "R",
    ),
    (
        "/src/com/example/Use.java",
        ("T> T id", 0),
        "/src/com/example/Use.java",
        "<T>",
        "T",
    ),
];

/// The references of the matrix that name no declaration: a name nothing
/// declares.
const DEFINITION_MATRIX_NONE: &[(&str, (&str, usize))] =
    &[("/src/com/example/Use.java", ("nope + 1", 0))];

/// End to end over stdio: every reference of [`DEFINITION_MATRIX`] answers with
/// the one declaration it denotes — file and range — and the unresolvable ones
/// answer `null`. The workspace has no build system and no JDK
/// (`default_client_config` points `java_home` at a path that does not exist),
/// so every target is one of the fixture's own files.
#[test]
fn definition_matrix_over_a_workspace() {
    let lsp = create_lsp_with_config(default_client_config(), |root| {
        for (path, text) in DEFINITION_MATRIX_FILES {
            let path = root.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    });
    for (path, _) in DEFINITION_MATRIX_FILES {
        lsp.open_document(path);
    }
    lsp.wait_until_workspace_is_loaded();

    for &(from, (needle, occurrence), to, declaration, name) in DEFINITION_MATRIX {
        let position = start_of(&source_of(from), needle, occurrence);
        let response = lsp.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": lsp.uri(from) },
                "position": position,
            }),
        );
        let locations = response
            .as_array()
            .unwrap_or_else(|| panic!("{from}: {needle:?}#{occurrence} answered {response:?}"));
        assert_eq!(
            locations.len(),
            1,
            "{from}: {needle:?}#{occurrence} answered {locations:?}"
        );
        let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
        let path = uri.to_file_path().expect("a file URI");
        assert!(
            path.ends_with(to.trim_start_matches('/')),
            "{from}: {needle:?}#{occurrence} answered {path:?}, expected {to:?}"
        );
        assert_definition_name(&locations[0]["range"], &source_of(to), declaration, name);
    }

    for &(from, (needle, occurrence)) in DEFINITION_MATRIX_NONE {
        let position = start_of(&source_of(from), needle, occurrence);
        let response = lsp.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": lsp.uri(from) },
                "position": position,
            }),
        );
        assert!(
            response.is_null(),
            "{from}: {needle:?}#{occurrence} answered {response:?}"
        );
    }
}

/// The fixture source of `path`.
fn source_of(path: &str) -> String {
    DEFINITION_MATRIX_FILES
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, text)| (*text).to_owned())
        .unwrap_or_else(|| panic!("{path} is not a fixture file"))
}

/// The LSP position of the first character of the `occurrence`-th (0-based)
/// occurrence of `needle` — anchored on the reference itself rather than on a
/// preceding token.
fn start_of(text: &str, needle: &str, occurrence: usize) -> Position {
    let mut from = 0;
    for _ in 0..occurrence {
        let found = text[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
        from += found + needle.len();
    }
    let index = from
        + text[from..]
            .find(needle)
            .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"));
    let before = &text[..index];
    let line = before.matches('\n').count() as u32;
    let last = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = before[last..].chars().count() as u32;
    Position { line, character }
}

/// A Kotlin file has no HIR yet: the definition request answers `null` from the
/// documented placeholder rather than walking the empty item tree the Kotlin
/// lowering leaves behind, and the server keeps serving the file.
#[test]
fn kotlin_definition_answers_null() {
    let lsp = create_lsp();
    let path = "/src/Main.kt";
    let text = "fun main() {\n    println(\"hi\")\n}\n";
    lsp.write_file(path, text);
    lsp.open_document(path);

    let (line, character) = position_of(text, "println");
    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
    );
    assert!(
        response.is_null(),
        "a Kotlin definition request answers null: {response:?}"
    );

    // The request neither materialized a library source nor left the file in a
    // state the next request trips over.
    let pending = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": 0, "character": 0 },
        }),
    );
    assert!(pending.is_null(), "got: {pending:?}");

    let symbols = lsp.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": lsp.uri(path) } }),
    );
    assert!(symbols.is_array(), "got: {symbols:?}");
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
/// on a member the jar declares materializes the source files it needs out of
/// the archive, loads them into the database, and answers with the real source
/// location — a classfile stub alone has no file and no range. A class instance
/// creation does the same for the constructor it selects ([§15.9]), whose
/// declaration the classfile names `<init>`, and so do an explicit `super(...)`
/// delegation ([§8.8.7.1]) and a bare `super` keyword ([§15.8.4]) against a
/// superclass only the jar declares.
///
/// The fixture's `Foo extends Base`, both declared by the jar, so the first
/// member request walks two owners that are not loaded yet and reads both files
/// in a single round, then resolves into `Foo.java`.
///
/// The model JSON reports the jar as a `flat-file` origin, so the sources are
/// found by the sibling probe (`<stem>-sources.jar`) and not by the build
/// system.
#[test]
fn library_source_definition_materializes_and_navigates() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let foo_source = "package com.example;\n\npublic class Foo extends Base {\n    public void greet(int count) {}\n}\n";
    let base_source =
        "package com.example;\n\npublic class Base {\n    public void hello(int n) {}\n}\n";
    let widget_source = "package com.example;\n\npublic class Widget {\n    public int size;\n\n    public Widget(int size) {}\n}\n";
    let app_source = "package app;\n\nclass App {\n    Object make() {\n        return new com.example.Foo();\n    }\n\n    Object widget() {\n        return new com.example.Widget(1);\n    }\n\n    void call(com.example.Foo f) {\n        f.greet(1);\n    }\n}\n\nclass Sub extends com.example.Widget {\n    Sub() {\n        super(1);\n    }\n\n    int read() {\n        return super.size;\n    }\n}\n";
    let app_path = "/src/main/java/app/App.java";

    // The shim build system and a decompiler whose JVM records every run: a
    // library that ships sources must never reach it.
    let decompiler = Decompiler::new(foo_source);

    let lsp = create_lsp_with_config(decompiler.config(), |root| {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java/app")).unwrap();
        std::fs::write(root.join("src/main/java/app/App.java"), app_source).unwrap();
        lsp_test::classfile::build_jar(
            &root.join("lib/foo.jar"),
            &[
                (
                    "com/example/Foo.class",
                    lsp_test::classfile::class_bytes(
                        "com/example/Foo",
                        "com/example/Base",
                        &[],
                        &[("greet", 1)],
                    ),
                ),
                (
                    "com/example/Base.class",
                    lsp_test::classfile::class_bytes(
                        "com/example/Base",
                        "java/lang/Object",
                        &[],
                        &[("hello", 1)],
                    ),
                ),
                // The classfile's `<init>(int)` is the constructor the source
                // declares under `Widget`; the implicit `<init>()V` every
                // classfile carries has no declaration to point at.
                (
                    "com/example/Widget.class",
                    lsp_test::classfile::class_bytes(
                        "com/example/Widget",
                        "java/lang/Object",
                        &["size"],
                        &[("<init>", 1)],
                    ),
                ),
            ],
        )
        .unwrap();
        lsp_test::classfile::build_jar(
            &root.join("lib/foo-sources.jar"),
            &[
                ("com/example/Foo.java", foo_source.as_bytes().to_vec()),
                ("com/example/Base.java", base_source.as_bytes().to_vec()),
                ("com/example/Widget.java", widget_source.as_bytes().to_vec()),
            ],
        )
        .unwrap();
    });

    lsp.open_document(app_path);
    lsp.wait_until_workspace_is_loaded();

    // -- hover first: the merged signature needs the declaring source, so the
    // request defers, the walk collects `Foo` and its supertype `Base` in one
    // round, both files are materialized, and the retried hover renders the
    // signature with the source's parameter name — the hand-built classfile
    // carries no `MethodParameters` attribute, so `count` can only come from
    // the source.
    let (line, character) = position_of(app_source, "f.greet(1)");
    let hover = lsp.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": lsp.uri(app_path) },
            "position": { "line": line, "character": character },
        }),
    );
    let hover_value = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("expected hover contents, got: {hover:?}"));
    assert!(
        hover_value.contains("void greet(int count)"),
        "the merged signature must carry the source parameter name: {hover_value:?}"
    );

    // -- the member call resolves the same way now that both files are loaded.
    let (line, character) = position_of(app_source, "f.greet(1)");
    let params = json!({
        "textDocument": { "uri": lsp.uri(app_path) },
        "position": { "line": line, "character": character },
    });
    let response = lsp.request("textDocument/definition", params.clone());
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");

    let member_uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let member_path = member_uri.to_file_path().expect("a file URI");
    let cache_sources = lsp.cache_dir().join("sources").join("v1");
    assert!(
        member_path.starts_with(&cache_sources),
        "expected a path under {}, got {}",
        cache_sources.display(),
        member_path.display()
    );
    assert!(
        member_path.ends_with("com/example/Foo.java"),
        "the member is declared by `Foo`: {}",
        member_path.display()
    );
    assert!(member_path.is_file(), "{}", member_path.display());
    assert_definition_name(
        &locations[0]["range"],
        foo_source,
        "public void greet(int count)",
        "greet",
    );

    let materialized = java_file_names(&cache_sources);
    assert_eq!(
        materialized,
        vec!["Base.java".to_string(), "Foo.java".to_string()],
        "one round materialized exactly the owners it walked"
    );

    // -- the type reference resolves the same way, and the source is already
    // loaded: a second request answers in a single pass.
    let (line, character) = position_of(app_source, "com.example.Foo()");
    let type_params = json!({
        "textDocument": { "uri": lsp.uri(app_path) },
        "position": { "line": line, "character": character },
    });
    let type_response = lsp.request("textDocument/definition", type_params.clone());
    let type_locations = type_response.as_array().expect("definition locations");
    assert_eq!(type_locations.len(), 1, "got: {type_response:?}");
    let type_uri: lsp_types::Uri =
        serde_json::from_value(type_locations[0]["uri"].clone()).unwrap();
    assert_eq!(
        type_uri.to_file_path().unwrap(),
        member_path,
        "the type reference resolves into the same materialized file"
    );
    assert_definition_name(&type_locations[0]["range"], foo_source, "class Foo", "Foo");

    let second = lsp.request("textDocument/definition", type_params);
    assert_eq!(
        second, type_response,
        "the deferred path runs once per source file, not once per request"
    );
    assert_eq!(
        java_file_names(&cache_sources),
        materialized,
        "re-asking materializes nothing new"
    );

    // -- §15.9: a class instance creation names the constructor it selected.
    // The classfile declares it `<init>(int)` ([JVMS §4.6]); the source
    // declares the same constructor under the class's own name, so the answer
    // is that declaration — not the class, and not the implicit `<init>()V`.
    let (line, character) = position_of(app_source, "com.example.Widget(1)");
    let constructor_response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(app_path) },
            "position": { "line": line, "character": character },
        }),
    );
    let constructor_locations = constructor_response
        .as_array()
        .expect("definition locations");
    assert_eq!(
        constructor_locations.len(),
        1,
        "got: {constructor_response:?}"
    );
    let constructor_uri: lsp_types::Uri =
        serde_json::from_value(constructor_locations[0]["uri"].clone()).unwrap();
    assert!(
        constructor_uri
            .to_file_path()
            .unwrap()
            .ends_with("com/example/Widget.java"),
        "the constructor is declared by `Widget`: {constructor_uri:?}"
    );
    assert_definition_name(
        &constructor_locations[0]["range"],
        widget_source,
        "public Widget(int size)",
        "Widget",
    );
    assert_eq!(
        java_file_names(&cache_sources),
        vec![
            "Base.java".to_string(),
            "Foo.java".to_string(),
            "Widget.java".to_string()
        ],
        "the creation materialized exactly its constructor's declaring file"
    );

    // -- §8.8.7.1: an explicit `super(...)` delegation names the superclass
    // constructor it invokes. The superclass ships only classfiles here, so the
    // classfile's `<init>(int)` is read back as the constructor the source
    // declares under `Widget`'s own name — and that declaration is quoted out
    // of the archive, not the class.
    let (line, character) = position_of(app_source, "super(1)");
    let delegation_response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(app_path) },
            "position": { "line": line, "character": character },
        }),
    );
    let delegation_locations = delegation_response
        .as_array()
        .expect("definition locations");
    assert_eq!(
        delegation_locations.len(),
        1,
        "got: {delegation_response:?}"
    );
    let delegation_uri: lsp_types::Uri =
        serde_json::from_value(delegation_locations[0]["uri"].clone()).unwrap();
    assert!(
        delegation_uri
            .to_file_path()
            .unwrap()
            .ends_with("com/example/Widget.java"),
        "the delegated constructor is declared by `Widget`: {delegation_uri:?}"
    );
    assert_definition_name(
        &delegation_locations[0]["range"],
        widget_source,
        "public Widget(int size)",
        "Widget",
    );
    assert_eq!(
        java_file_names(&cache_sources),
        vec![
            "Base.java".to_string(),
            "Foo.java".to_string(),
            "Widget.java".to_string()
        ],
        "the delegation materializes the same declaring file the creation did"
    );

    // -- §15.8.4: a bare `super` keyword names the direct superclass, even when
    // only its classfile is on the classpath: the field the enclosing
    // `super.size` reads is a different declaration, and the class it belongs
    // to is quoted out of the source archive.
    let super_position = start_of(app_source, "super.size", 0);
    let super_response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(app_path) },
            "position": super_position,
        }),
    );
    let super_locations = super_response.as_array().expect("definition locations");
    assert_eq!(super_locations.len(), 1, "got: {super_response:?}");
    let super_uri: lsp_types::Uri =
        serde_json::from_value(super_locations[0]["uri"].clone()).unwrap();
    assert!(
        super_uri
            .to_file_path()
            .unwrap()
            .ends_with("com/example/Widget.java"),
        "the superclass is declared by `Widget`: {super_uri:?}"
    );
    assert_definition_name(
        &super_locations[0]["range"],
        widget_source,
        "class Widget",
        "Widget",
    );

    // A library source file is read-only third-party code: its report is empty.
    let report = lsp.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": type_locations[0]["uri"].clone() } }),
    );
    assert_eq!(
        report["items"].as_array().map(Vec::len),
        Some(0),
        "library sources report no diagnostics: {report:?}"
    );

    // Every navigation above resolved into an attached source archive, so the
    // decompiler was never consulted: its JVM never started.
    assert_eq!(
        decompiler.jvm_runs(),
        0,
        "a library with sources must never be decompiled"
    );
}

/// The source of the class the shared decompiler fixture's jar declares and
/// the fake decompiler reproduces.
const FOO_SOURCE: &str =
    "package com.example;\n\npublic class Foo {\n    public void greet(int count) {}\n}\n";

/// The workspace file that references it.
const APP_SOURCE: &str = "package app;\n\nclass App {\n    Object make() {\n        return new com.example.Foo();\n    }\n}\n";

/// The workspace-relative path of that file.
const APP_PATH: &str = "/src/main/java/app/App.java";

/// The fixture the three decompiler tests share: a shim build system, a
/// dependency jar **with no attached sources**, and a fake JDK whose `bin/java`
/// writes the Java a real decompiler would have produced — and records every
/// run, so a test can prove a JVM was started exactly as often as it should
/// have been.
///
/// The jar file itself is never read (the server only checks that it exists),
/// and the fake `bin/java` finds its output directory the way the real tools are
/// handed one: as `--outputdir <dir>`, else as the last argument.
///
/// The temp dirs must outlive the harness the fixture configures.
struct Decompiler {
    /// The fake JDK: `bin/java` is the stand-in for a decompiler, and the run
    /// log the proof of whether it was started.
    jdk: tempfile::TempDir,
    /// The jar the configuration points the backend at. Never read, but the
    /// server requires it to exist.
    _jars: tempfile::TempDir,
    /// The shim build system `PATH` points at.
    _shim: tempfile::TempDir,
    cfr: PathBuf,
    runs: PathBuf,
}

impl Decompiler {
    fn new(foo_source: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let jdk = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(jdk.path().join("bin")).unwrap();
        let produced = jdk.path().join("Foo.java");
        std::fs::write(&produced, foo_source).unwrap();
        let runs = jdk.path().join("runs.log");
        let java = jdk.path().join("bin/java");
        std::fs::write(
            &java,
            format!(
                r#"#!/bin/sh
echo run >> {runs}
out=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--outputdir" ]; then out="$arg"; fi
  prev="$arg"
  last="$arg"
done
if [ -z "$out" ]; then out="$last"; fi
mkdir -p "$out/com/example"
cp {produced} "$out/com/example/Foo.java"
"#,
                runs = runs.display(),
                produced = produced.display(),
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&java).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&java, perms).unwrap();

        let jars = tempfile::tempdir().unwrap();
        let cfr = jars.path().join("cfr.jar");
        std::fs::write(&cfr, b"not a jar; the fake java never reads it").unwrap();

        let shim = tempfile::tempdir().unwrap();
        let shim_file = shim.path().join("gradle");
        std::fs::write(
            &shim_file,
            r#"#!/bin/sh
echo "WORKSPACE_MODEL_BEGIN"
echo '{"workspace_name":"demo","projects":[{"path":":","name":"demo","project_dir":"'$PWD'","source_roots":["'$PWD'/src/main/java"],"test_roots":[],"resource_roots":[],"generated_roots":[],"compile_classpath":[{"type":"jar","path":"'$PWD'/lib/foo.jar","origin":"flat-file"}],"test_classpath":[],"java_language_version":"21","java_home":"'$JAVA_HOME'"}]}'
echo "WORKSPACE_MODEL_END"
exit 0
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&shim_file).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim_file, perms).unwrap();

        let path_var = std::env::var("PATH").unwrap_or_default();
        // SAFETY: env mutation is serialized by ENV_LOCK, which every caller
        // holds for the duration of the test.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", shim.path().display(), path_var));
        }

        Self {
            jdk,
            _jars: jars,
            _shim: shim,
            cfr,
            runs,
        }
    }

    /// The client configuration that selects the fake decompiler and the fake
    /// JDK it runs on.
    fn config(&self) -> serde_json::Value {
        let mut config = default_client_config();
        config["java_home"] = json!(self.jdk.path());
        config["decompiler"] = json!("cfr");
        config["decompiler_jars"] = json!({ "cfr": &self.cfr });
        config
    }

    /// The same configuration, but with the fake JDK configured as the
    /// *bootstrap* JDK and no project JDK at all: the decompiler must run on the
    /// bootstrap one, and nothing else may be looked for.
    fn bootstrap_config(&self) -> serde_json::Value {
        let mut config = default_client_config();
        config["java_home"] = json!(std::env::temp_dir().join("caffeine-ls-test-no-jdk"));
        config["bootstrap_java_home"] = json!(self.jdk.path());
        config["decompiler"] = json!("cfr");
        config["decompiler_jars"] = json!({ "cfr": &self.cfr });
        config
    }

    /// Writes the workspace of a test: an app file and the jar beside it. No
    /// `lib/foo-sources.jar` is written — that is what makes the class
    /// decompilable in the first place.
    fn setup_workspace(&self, root: &std::path::Path, app_source: &str) {
        std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();
        std::fs::create_dir_all(root.join("src/main/java/app")).unwrap();
        std::fs::write(root.join("src/main/java/app/App.java"), app_source).unwrap();
        lsp_test::classfile::build_jar(
            &root.join("lib/foo.jar"),
            &[(
                "com/example/Foo.class",
                lsp_test::classfile::class_bytes(
                    "com/example/Foo",
                    "java/lang/Object",
                    &[],
                    &[("greet", 1)],
                ),
            )],
        )
        .unwrap();
    }

    /// How many times the fake decompiler's JVM has been started.
    fn jvm_runs(&self) -> usize {
        std::fs::read_to_string(&self.runs)
            .unwrap_or_default()
            .lines()
            .count()
    }
}

/// A dependency jar with **no** sources at all: `textDocument/definition` on a
/// class it declares has nothing to read, so the server decompiles the class on
/// demand — off the main loop, since a JVM start costs seconds — materializes
/// the Java under the decompiled view root, and answers with that location.
///
/// The fake JDK's `bin/java` stands in for the decompiler: it writes the Java
/// the real tool would have produced and appends a line to a run log, so the
/// second request can be proven not to start a second JVM. The jar file itself
/// is never read (the server only checks that it exists).
#[test]
fn decompiled_library_definition_materializes_and_navigates() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let decompiler = Decompiler::new(FOO_SOURCE);
    let lsp = create_lsp_with_config(decompiler.config(), |root| {
        decompiler.setup_workspace(root, APP_SOURCE);
    });
    let jvm_runs = || decompiler.jvm_runs();

    lsp.open_document(APP_PATH);
    lsp.wait_until_workspace_is_loaded();
    assert_eq!(jvm_runs(), 0, "loading the workspace decompiles nothing");

    let params = json!({
        "textDocument": { "uri": lsp.uri(APP_PATH) },
        "position": {
            "line": position_of(APP_SOURCE, "com.example.Foo()").0,
            "character": position_of(APP_SOURCE, "com.example.Foo()").1,
        },
    });
    let response = lsp.request("textDocument/definition", params.clone());
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");

    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let path = uri.to_file_path().expect("a file URI");
    let cache_decompiled = lsp.cache_dir().join("decompile").join("v1").join("cfr");
    assert!(
        path.starts_with(&cache_decompiled),
        "expected a path under {}, got {}",
        cache_decompiled.display(),
        path.display()
    );
    assert!(
        path.ends_with("com/example/Foo.java"),
        "the class is declared by `Foo`: {}",
        path.display()
    );
    assert!(path.is_file(), "{}", path.display());
    assert_definition_name(
        &locations[0]["range"],
        FOO_SOURCE,
        "public class Foo",
        "Foo",
    );
    assert_eq!(
        java_file_names(&cache_decompiled),
        vec!["Foo.java".to_string()],
        "exactly the class that was asked for was decompiled"
    );
    assert_eq!(jvm_runs(), 1, "one decompile, one JVM start");

    // The decompiled file is loaded now, so the second request answers from the
    // database: no second ref, no second JVM.
    let second = lsp.request("textDocument/definition", params);
    assert_eq!(
        second, response,
        "the deferred path runs once per class, not once per request"
    );
    assert_eq!(jvm_runs(), 1, "the materialized file is reused");

    // A decompiled library file is read-only third-party code: its report is
    // empty, exactly like a materialized source.
    let report = lsp.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": locations[0]["uri"].clone() } }),
    );
    assert_eq!(
        report["items"].as_array().map(Vec::len),
        Some(0),
        "decompiled library files report no diagnostics: {report:?}"
    );
}

/// The decompiler runs on the *bootstrap* JDK when the client configures one,
/// independently of the JDK the project compiles against: the fixture's fake JDK
/// is reachable only as `bootstrap_java_home` (the project `java_home` does not
/// exist), so the decompile can only have happened through it.
#[test]
fn the_bootstrap_jdk_runs_the_decompiler() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let decompiler = Decompiler::new(FOO_SOURCE);
    let lsp = create_lsp_with_config(decompiler.bootstrap_config(), |root| {
        decompiler.setup_workspace(root, APP_SOURCE);
    });

    lsp.open_document(APP_PATH);
    lsp.wait_until_workspace_is_loaded();

    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(APP_PATH) },
            "position": {
                "line": position_of(APP_SOURCE, "com.example.Foo()").0,
                "character": position_of(APP_SOURCE, "com.example.Foo()").1,
            },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");

    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let path = uri.to_file_path().expect("a file URI");
    assert!(
        path.ends_with("com/example/Foo.java"),
        "the class is declared by `Foo`: {}",
        path.display()
    );
    assert_definition_name(
        &locations[0]["range"],
        FOO_SOURCE,
        "public class Foo",
        "Foo",
    );
    assert_eq!(
        decompiler.jvm_runs(),
        1,
        "the bootstrap JDK's java is the one that ran"
    );
}

/// A client that serves library views itself: with `library_uri_scheme`
/// configured, a definition never answers the cache path the view was
/// materialized at but a `<scheme>://` URI built from the view layout, and
/// `caffeine_ls/libraryFileContent` hands back that view's text. The URI is
/// client input on the way back in, so one naming a backend this server does
/// not have — or a `file://` URI of the very file — is refused rather than
/// answered with an empty document.
#[test]
fn library_view_uri_scheme_is_served_to_the_client() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let decompiler = Decompiler::new(FOO_SOURCE);
    let mut config = decompiler.config();
    config["library_uri_scheme"] = json!("caffeine-ls");
    let lsp = create_lsp_with_config(config, |root| {
        decompiler.setup_workspace(root, APP_SOURCE);
    });

    lsp.open_document(APP_PATH);
    lsp.wait_until_workspace_is_loaded();

    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(APP_PATH) },
            "position": {
                "line": position_of(APP_SOURCE, "com.example.Foo()").0,
                "character": position_of(APP_SOURCE, "com.example.Foo()").1,
            },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");
    assert_definition_name(
        &locations[0]["range"],
        FOO_SOURCE,
        "public class Foo",
        "Foo",
    );

    // `caffeine-ls://<library-hex>/decompiled/cfr/com/example/Foo.java`: the view
    // is named by its library and its path inside the view, never by the cache
    // directory the server happens to keep it in.
    let uri = locations[0]["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a URI, got: {:?}", locations[0]["uri"]));
    let (library, view) = uri
        .strip_prefix("caffeine-ls://")
        .and_then(|rest| rest.split_once('/'))
        .unwrap_or_else(|| panic!("expected a caffeine-ls URI, got {uri}"));
    assert_eq!(view, "decompiled/cfr/com/example/Foo.java", "{uri}");
    assert_eq!(library.len(), 16, "a library id in hex: {uri}");
    assert!(
        library
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "a library id in lowercase hex: {uri}"
    );

    // The content provider reads the view through the request, so the client
    // needs no access to the cache directory.
    let content = lsp.request("caffeine_ls/libraryFileContent", json!({ "uri": uri }));
    assert_eq!(content["content"].as_str(), Some(FOO_SOURCE));

    // A backend this server does not serve, and a plain `file://` URI of the
    // very file the view holds, are both refused.
    for bad in [
        format!("caffeine-ls://{library}/decompiled/jd/com/example/Foo.java"),
        format!("caffeine-ls://{library}/classes/com/example/Foo.java"),
        lsp.uri("/src/main/java/app/App.java").to_string(),
    ] {
        let err = lsp
            .request_raw("caffeine_ls/libraryFileContent", json!({ "uri": bad }))
            .expect_err("a URI that names no served view is an error");
        assert!(err.message.contains("not a library view"), "{bad}: {err:?}");
    }

    // A path that tries to climb out of the cache is refused too: the URI
    // parser folds the `..` segments away, and what is left names no view.
    let climbing = format!("caffeine-ls://{library}/decompiled/cfr/../../../../etc/passwd");
    let err = lsp
        .request_raw("caffeine_ls/libraryFileContent", json!({ "uri": climbing }))
        .expect_err("a path out of the cache is an error");
    assert!(
        err.message.contains("not a library view") || err.message.contains("failed to read"),
        "{err:?}"
    );

    // The JVM ran exactly once: the definition deferred, the content request
    // served the materialized view.
    assert_eq!(decompiler.jvm_runs(), 1);
}

/// The client attaches to the view scheme just like to a workspace file (the
/// extension's `documentSelector` lists it), so a decompiled tab is a document
/// the server answers *about*: symbols, navigation and hover inside it, and a
/// diagnostics pull that stays empty — a library view is third-party code
/// nobody in the editor can fix. The view is read-only for the same reason: a
/// change that reaches the server anyway never reaches the database, and
/// closing the tab does not throw the materialized file away.
#[test]
fn library_view_document_supports_ide_features() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let decompiler = Decompiler::new(FOO_SOURCE);
    let mut config = decompiler.config();
    config["library_uri_scheme"] = json!("caffeine-ls");
    let lsp = create_lsp_with_config(config, |root| {
        decompiler.setup_workspace(root, APP_SOURCE);
    });

    lsp.open_document(APP_PATH);
    lsp.wait_until_workspace_is_loaded();

    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(APP_PATH) },
            "position": {
                "line": position_of(APP_SOURCE, "com.example.Foo()").0,
                "character": position_of(APP_SOURCE, "com.example.Foo()").1,
            },
        }),
    );
    let view: lsp_types::Uri = serde_json::from_value(response[0]["uri"].clone()).unwrap();

    // The editor opens the view with exactly the text the provider served.
    let content = lsp.request("caffeine_ls/libraryFileContent", json!({ "uri": view }));
    assert_eq!(content["content"].as_str(), Some(FOO_SOURCE));
    lsp.open_uri(&view, "java", FOO_SOURCE);

    // -- the outline of the view.
    let symbols = lsp.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": view } }),
    );
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap_or_else(|| panic!("expected document symbols, got {symbols:?}"))
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    assert!(names.contains(&"Foo"), "{symbols:?}");

    // -- navigation inside the view: the declaration's own name answers with
    // the declaration, in the view itself.
    let at = FOO_SOURCE.find("public class Foo").unwrap() + "public class ".len();
    let before = &FOO_SOURCE[..at];
    let params = json!({
        "textDocument": { "uri": view },
        "position": {
            "line": before.matches('\n').count() as u32,
            "character": before.rfind('\n').map_or(0, |i| before[i + 1..].len()) as u32,
        },
    });
    let inside = lsp.request("textDocument/definition", params.clone());
    let locations = inside
        .as_array()
        .unwrap_or_else(|| panic!("expected a location inside the view, got {inside:?}"));
    assert_eq!(locations.len(), 1, "{inside:?}");
    assert_eq!(
        locations[0]["uri"], response[0]["uri"],
        "the declaration is in the view it was asked from"
    );

    // -- hover inside the view renders the declaration it falls on.
    let hover = lsp.request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": view },
            "position": params["position"].clone(),
        }),
    );
    let hover_value = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("expected hover contents, got {hover:?}"));
    assert!(hover_value.contains("class Foo"), "{hover_value:?}");

    // -- a library view reports no diagnostics, so the client's Problems stay
    // empty for third-party code.
    let report = lsp.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": view } }),
    );
    assert_eq!(
        report["items"].as_array().map(Vec::len),
        Some(0),
        "{report:?}"
    );

    // -- read-only: an edit that reaches the server anyway is ignored, so the
    // database still holds the server's text and a pull stays empty.
    lsp.change_uri(
        &view,
        1,
        "package com.example;\npublic class Foo { this is not java }\n",
    );
    let report = lsp.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": view } }),
    );
    assert_eq!(
        report["items"].as_array().map(Vec::len),
        Some(0),
        "a view never reports a diagnostic about an edit nobody can make: {report:?}"
    );
    let after_edit = lsp.request("textDocument/definition", params);
    assert_eq!(
        after_edit, inside,
        "the view's text is the server's, not the one the client sent"
    );

    // -- closing the tab keeps the materialized file: navigating back answers
    // without starting a second JVM.
    lsp.close_uri(&view);
    let reopened = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(APP_PATH) },
            "position": {
                "line": position_of(APP_SOURCE, "com.example.Foo()").0,
                "character": position_of(APP_SOURCE, "com.example.Foo()").1,
            },
        }),
    );
    assert_eq!(reopened, response);
    assert_eq!(decompiler.jvm_runs(), 1);
}

/// A document no configured source root covers — a scratch file, or a file
/// opened with no workspace folder at all — is the vfs catch-all. It becomes a
/// source root of its own (the *detached* root, mapped to a detached source
/// set), so goto-definition still resolves its names against its own
/// declarations: the file navigates to itself. Dropping the catch-all (as the
/// partition once did) lowered such a file as `Unknown`, and every request
/// answered `null`.
#[test]
fn detached_file_definition_navigates_to_itself() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());

    let text = "public class Main {\n    Main m;\n}\n";

    let shim_dir = tempfile::tempdir().unwrap();
    let shim = shim_dir.path().join("gradle");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
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
        // Under the workspace root, but outside the build's source root.
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        std::fs::write(root.join("scratch/Main.java"), text).unwrap();
    });

    let path = "/scratch/Main.java";
    lsp.open_document(path);
    lsp.wait_until_workspace_is_loaded();

    for ((needle, occurrence), declaration, name) in [
        // The declared type names the class…
        (("Main m", 0), "class Main", "Main"),
        // …and a declaration's own name names the declaration.
        (("m;", 0), "Main m", "m"),
        (("Main {", 0), "class Main", "Main"),
    ] {
        let position = start_of(text, needle, occurrence);
        let response = lsp.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": lsp.uri(path) },
                "position": position,
            }),
        );
        let locations = response
            .as_array()
            .unwrap_or_else(|| panic!("{needle:?}#{occurrence} answered {response:?}"));
        assert_eq!(locations.len(), 1, "{needle:?}#{occurrence}: {locations:?}");
        assert_definition_name(&locations[0]["range"], text, declaration, name);
    }
}

/// Asserts that `range` is exactly the `name` identifier written in the
/// declaration `declaration` of `text`. A definition points at the declared
/// name, not at the whole declaration: a whole-class range contains every
/// reference to it, so a client that treats "already inside the definition" as
/// a no-op would never move (`String` inside `String`).
fn assert_definition_name(range: &serde_json::Value, text: &str, declaration: &str, name: &str) {
    let declaration_at = text
        .find(declaration)
        .unwrap_or_else(|| panic!("{declaration:?} is not in the fixture"));
    let name_at = declaration_at
        + text[declaration_at..]
            .find(name)
            .unwrap_or_else(|| panic!("{name:?} is not in {declaration:?}"));
    let start = position_at(text, name_at);
    let end = position_at(text, name_at + name.len());
    assert_eq!(
        (
            range["start"]["line"].as_u64(),
            range["start"]["character"].as_u64()
        ),
        (Some(start.0 as u64), Some(start.1 as u64)),
        "the definition must start at {name:?} in {declaration:?}"
    );
    assert_eq!(
        (
            range["end"]["line"].as_u64(),
            range["end"]["character"].as_u64()
        ),
        (Some(end.0 as u64), Some(end.1 as u64)),
        "the definition must end after {name:?} in {declaration:?}"
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

/// The JDK's `src.zip` end to end: a `String` creation and a member inherited
/// from `Object` both navigate into the platform sources, the JDK 9+
/// `<module>/` prefix is stripped from the materialized path, and only the
/// files those two references touched are ever written out — the archive's
/// ~25k compilation units stay inside `src.zip`.
///
/// Requires a JDK with `lib/src.zip` (or `src.zip`); skipped otherwise.
#[test]
fn jdk_sources_are_materialized_and_navigable() {
    let java_home = std::env::var("JAVA_HOME").ok().filter(|home| {
        let home = std::path::Path::new(home);
        home.join("lib/src.zip").is_file() || home.join("src.zip").is_file()
    });
    let Some(java_home) = java_home else {
        eprintln!("skipping: JAVA_HOME is unset or ships no src.zip");
        return;
    };

    let source = "package app;\n\nimport java.util.ArrayList;\n\nclass App {\n    String created() {\n        return new String(\"abc\");\n    }\n\n    ArrayList<String> list() {\n        return new ArrayList<String>(new ArrayList<>());\n    }\n\n    Class<?> member() {\n        return \"abc\".getClass();\n    }\n}\n";
    let path = "/src/app/App.java";

    // No build system in the temp workspace: the plain path registers the
    // configured JDK as the SDK, whose `lib/modules` and `lib/src.zip` are the
    // platform library and its sources.
    let lsp = create_lsp_with_config(json!({ "java_home": java_home }), |root| {
        std::fs::create_dir_all(root.join("src/app")).unwrap();
        std::fs::write(root.join("src/app/App.java"), source).unwrap();
    });

    lsp.open_document(path);
    lsp.wait_until_workspace_is_loaded();

    let cache_sources = lsp.cache_dir().join("sources").join("v1");

    // -- §15.9: a class instance creation names the constructor it selected,
    // whose declaring file is `java/lang/String.java` — an entry the archive
    // stores as `<module>/java/lang/String.java`.
    let (line, character) = position_of(source, "new String(\"abc\")");
    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");
    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let string_path = uri.to_file_path().expect("a file URI");
    assert!(
        string_path.starts_with(&cache_sources),
        "expected a path under {}, got {}",
        cache_sources.display(),
        string_path.display()
    );
    assert!(
        string_path.ends_with("java/lang/String.java"),
        "unexpected path: {}",
        string_path.display()
    );
    assert!(
        !string_path.to_string_lossy().contains("java.base"),
        "the module prefix must be stripped: {}",
        string_path.display()
    );
    let string_source = std::fs::read_to_string(&string_path).expect("the source was written");
    assert_definition_name(
        &locations[0]["range"],
        &string_source,
        "public String(String original)",
        "String",
    );

    // -- §15.12.2.2/[§8.4.2]: two constructors of one parameter *count* are
    // told apart by their parameter types, which the classfile descriptor and
    // the source declaration agree on. `new ArrayList<>(new ArrayList<>())`
    // names `ArrayList(Collection<? extends E>)` — the constructor the
    // argument's type selects — not the `ArrayList(int initialCapacity)` that
    // shares its arity and is declared first.
    let (line, character) = position_of(source, "new ArrayList<String>");
    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");
    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let arraylist_path = uri.to_file_path().expect("a file URI");
    assert!(
        arraylist_path.ends_with("java/util/ArrayList.java"),
        "the constructor is declared by `ArrayList`: {}",
        arraylist_path.display()
    );
    let arraylist_source =
        std::fs::read_to_string(&arraylist_path).expect("the source was written");
    assert_definition_name(
        &locations[0]["range"],
        &arraylist_source,
        "public ArrayList(Collection<? extends E> c)",
        "ArrayList",
    );

    // -- a self-reference *inside* the materialized class: the parameter type
    // in `public String(String original) {` names the enclosing `String`. The
    // definition is the class's own *name* — a range covering the whole class
    // would contain the cursor, and a client that treats "already inside the
    // definition" as a no-op would not move at all.
    let self_uri = lsp_types::Uri::from_file_path(&string_path).expect("a file URI");
    let (line, character) = position_of(&string_source, "String(String original)");
    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": self_uri },
            "position": { "line": line, "character": character },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");
    assert_definition_name(
        &locations[0]["range"],
        &string_source,
        "public final class String",
        "String",
    );

    // -- a member declared only on `Object`: the walk descends `String`'s
    // hierarchy on the real jimage-backed stubs.
    let (line, character) = position_of(source, "\"abc\".getClass()");
    let response = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": lsp.uri(path) },
            "position": { "line": line, "character": character },
        }),
    );
    let locations = response.as_array().expect("definition locations");
    assert_eq!(locations.len(), 1, "got: {response:?}");
    let uri: lsp_types::Uri = serde_json::from_value(locations[0]["uri"].clone()).unwrap();
    let object_path = uri.to_file_path().expect("a file URI");
    assert!(
        object_path.ends_with("java/lang/Object.java"),
        "`getClass` is declared by `Object`: {}",
        object_path.display()
    );
    let object_source = std::fs::read_to_string(&object_path).expect("the source was written");
    // The target is the `getClass` *declaration* in `Object`, not the class.
    assert_definition_name(
        &locations[0]["range"],
        &object_source,
        "public final native Class<?> getClass()",
        "getClass",
    );

    // Only the files those two references touched are on disk: `String.java`,
    // `Object.java` and the supertypes the one-round walk collected — not the
    // archive's tens of thousands of compilation units.
    let materialized = java_file_names(&cache_sources);
    assert!(
        materialized.contains(&"String.java".to_string()),
        "{materialized:?}"
    );
    assert!(
        materialized.contains(&"Object.java".to_string()),
        "{materialized:?}"
    );
    assert!(
        materialized.len() <= 16,
        "expected only the touched files, got {}: {materialized:?}",
        materialized.len()
    );
}
