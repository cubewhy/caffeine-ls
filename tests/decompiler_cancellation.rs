#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use java_analyzer::decompiler::Decompiler;
use java_analyzer::decompiler::backend::cfr::CfrDecompiler;
use java_analyzer::lsp::request_cancellation::{CancellationToken, Cancelled};

fn write_executable_script(path: &PathBuf, started: &Path, finished: &Path) {
    let script = format!(
        "#!/bin/sh\nset -eu\ntouch '{}'\nsleep 1\nprintf 'class Output {{}}\\n'\ntouch '{}'\n",
        started.display(),
        finished.display(),
    );
    std::fs::write(path, script).expect("write fake java script");
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("set script executable");
    }
}

async fn wait_for_path(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("path should appear before timeout");
}

#[tokio::test]
#[cfg(unix)]
async fn cancelling_cfr_decompile_kills_subprocess() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fake_java = temp.path().join("fake-java.sh");
    let fake_jar = temp.path().join("fake-decompiler.jar");
    let output = temp.path().join("Output.java");
    let started = temp.path().join("started");
    let finished = temp.path().join("finished");

    write_executable_script(&fake_java, &started, &finished);
    std::fs::write(&fake_jar, b"fake").expect("write fake jar");

    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let fake_java = fake_java.clone();
        let fake_jar = fake_jar.clone();
        let output = output.clone();
        let cancel = cancel.clone();
        async move {
            CfrDecompiler
                .decompile(&fake_java, &fake_jar, b"not-a-class", &output, &cancel)
                .await
        }
    });

    wait_for_path(&started).await;
    assert!(
        cancel.cancel(Cancelled::Client),
        "cancellation should be observed once"
    );

    let error = task
        .await
        .expect("decompile join")
        .expect_err("decompile should stop on cancellation");
    assert!(
        error.to_string().contains("request cancelled"),
        "unexpected decompile error: {error:#}"
    );

    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !finished.exists(),
        "fake decompiler should have been killed before completion"
    );
    assert!(
        !output.exists(),
        "cancelled decompile should not leave cached output behind"
    );
}
