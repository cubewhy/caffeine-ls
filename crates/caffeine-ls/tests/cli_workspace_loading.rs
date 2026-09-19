#![cfg(target_os = "linux")]

use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::json;
use tempfile::{TempDir, tempdir};

struct Fixture {
    workspace: TempDir,
    java_home: TempDir,
    cache: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            workspace: tempdir().unwrap(),
            java_home: tempdir().unwrap(),
            cache: tempdir().unwrap(),
        };
        let root = fixture.workspace.path();
        fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();
        fs::create_dir_all(root.join("src/main/java")).unwrap();
        fs::write(root.join("src/main/java/Main.java"), "class Main {}\n").unwrap();
        fixture
    }

    fn diagnostics(&self, script: &str) -> (ExitStatus, String) {
        let root = self.workspace.path();
        let wrapper = root.join("gradlew");
        fs::write(&wrapper, script).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let stdout = root.join("stdout");
        let stderr = root.join("stderr");
        let mut child = Command::new(env!("CARGO_BIN_EXE_caffeine-ls"))
            .arg("diagnostics")
            .arg(root)
            .arg("--no-progress")
            .env("JAVA_HOME", self.java_home.path())
            .env("XDG_CACHE_HOME", self.cache.path())
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "diagnostics timed out after 10 seconds\nstdout:\n{}\nstderr:\n{}",
                    fs::read_to_string(&stdout).unwrap(),
                    fs::read_to_string(&stderr).unwrap(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        (status, fs::read_to_string(stderr).unwrap())
    }

    fn build_log(&self) -> String {
        let logs = self.cache.path().join("caffeine_ls/logs");
        let log = fs::read_dir(logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("build-tool-gradle-")
            })
            .expect("failed import must retain its build log");
        let contents = fs::read_to_string(log).unwrap();
        assert!(
            !contents.is_empty(),
            "retained build log must contain output"
        );
        contents
    }
}

fn missing_jar_model(root: &Path) -> serde_json::Value {
    json!({
        "workspace_name": "fixture",
        "projects": [{
            "path": ":",
            "name": "fixture",
            "project_dir": root,
            "source_roots": [root.join("src/main/java")],
            "test_roots": [],
            "resource_roots": [],
            "generated_roots": [],
            "compile_classpath": [{
                "type": "jar",
                "path": root.join("missing.jar"),
                "origin": "flat-file",
                "sources": null
            }],
            "test_classpath": [],
            "java_language_version": null,
            "java_release": null,
            "java_language_preview": null,
            "java_home": null
        }]
    })
}

#[test]
fn missing_gradle_jar_reports_failure() {
    let fixture = Fixture::new();
    let root = fixture.workspace.path();
    assert!(!root.join("missing.jar").exists());
    let script = format!(
        "#!/bin/sh\ncat <<'MODEL'\nWORKSPACE_MODEL_BEGIN\n{}\nWORKSPACE_MODEL_END\nMODEL\n",
        missing_jar_model(root)
    );
    let (status, stderr) = fixture.diagnostics(&script);
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("missing.jar"), "{stderr}");
    assert!(stderr.contains("project ':'"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    fixture.build_log();
}

#[test]
fn gradle_process_failure_reports_immediately() {
    let fixture = Fixture::new();
    let cause = "fixture artifact generation failed";
    let (status, stderr) = fixture.diagnostics(&format!(
        "#!/bin/sh\nprintf '%s\\n' '{cause}' >&2\nexit 7\n"
    ));
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains(cause), "{stderr}");
    assert!(fixture.build_log().contains(cause));
}
