//! Fetches the decompiler jars bundled with the VS Code extension.
//!
//! The decompilers are third-party releases served from Maven Central, so they are not
//! checked into the repository: `editors/code/resources/decompilers` is gitignored. Anything
//! that packages the extension therefore has to download them first, and this module is the
//! single place that knows which releases the extension ships and where they come from.
//! Re-running is cheap because an already downloaded jar is left alone unless `--force` is
//! passed.
//!
//! The download is done by `ureq` rather than by shelling out to a tool: a packaging step
//! that every CI runner and every contributor runs should not depend on a second program
//! being installed, and a half-written file must never look like a completed download.

use std::{fs, io, path::Path, time::Duration};

use anyhow::Context;

use crate::args::PrepareTarget;

/// A decompiler release to place in the extension's resources directory.
struct Jar {
    /// Backend id; the file is named `<id>.jar` and the server is told about it by id.
    id: &'static str,
    url: &'static str,
}

/// The pinned releases. Versions are fixed so a packaged VSIX always ships the same tool.
const JARS: &[Jar] = &[
    Jar {
        id: "cfr",
        url: "https://repo1.maven.org/maven2/org/benf/cfr/0.152/cfr-0.152.jar",
    },
    Jar {
        id: "vineflower",
        url: "https://repo1.maven.org/maven2/org/vineflower/vineflower/1.12.0/vineflower-1.12.0.jar",
    },
];

/// How many times a download is attempted before the run fails. A CDN drops a connection
/// now and then, and a transient failure should not fail a release build.
const ATTEMPTS: usize = 3;

/// The pause between two attempts: long enough for a reset connection to be worth retrying,
/// short enough not to stall a packaging job.
const RETRY_PAUSE: Duration = Duration::from_secs(2);

/// The whole request — connection, headers and body — must finish within this. `ureq` sets
/// no timeout of its own, and a hung connection would otherwise hang a CI job forever.
const TIMEOUT: Duration = Duration::from_secs(300);

pub fn prepare(target: Option<PrepareTarget>, force: bool) -> anyhow::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask always lives in a directory below the workspace root");
    let dest_dir = root
        .join("editors")
        .join("code")
        .join("resources")
        .join("decompilers");

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    for jar in JARS.iter().filter(|jar| is_wanted(target, jar)) {
        let dest = dest_dir.join(format!("{}.jar", jar.id));
        if !force && is_present(&dest) {
            println!("Skipping {}.jar (already present)", jar.id);
            continue;
        }

        println!("Downloading {}.jar from {}", jar.id, jar.url);
        download(&agent, jar.url, &dest, RETRY_PAUSE)
            .with_context(|| format!("failed to fetch {}", jar.id))?;
    }

    Ok(())
}

/// Downloads `url` into `dest`, retrying while the failure looks transient and pausing
/// `retry_pause` between two attempts.
///
/// A failed attempt never leaves a file behind: the destination only exists as a finished
/// download, so an interrupted run is retried rather than mistaken for a usable jar.
fn download(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    retry_pause: Duration,
) -> anyhow::Result<()> {
    for attempt in 1..=ATTEMPTS {
        match fetch(agent, url, dest) {
            Ok(()) => return Ok(()),
            Err(err) => {
                let _ = fs::remove_file(dest);
                if !err.retryable || attempt == ATTEMPTS {
                    return Err(err.error);
                }
                // The outermost message only: the full chain is printed when the download
                // finally fails.
                println!("  attempt {attempt}/{ATTEMPTS} failed: {}", err.error);
                std::thread::sleep(retry_pause);
            }
        }
    }
    unreachable!("the last attempt either returns or fails the download")
}

/// One attempt, split by whether another one could help.
struct AttemptError {
    error: anyhow::Error,
    /// Whether the failure looks like a dropped connection or a server-side error rather
    /// than the CDN saying the release is not there.
    retryable: bool,
}

/// Fetches `url` into `dest` once. Any error status (4xx and 5xx alike) is an error, exactly
/// like a `curl --fail`: an error page is not a jar.
fn fetch(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<(), AttemptError> {
    let retryable = |error: ureq::Error| AttemptError {
        // A 4xx is the CDN telling us the pinned release is not there; retrying only delays
        // the report. Everything else — no connection, a timeout, a 5xx — may be transient.
        retryable: !matches!(error, ureq::Error::StatusCode(code) if code < 500),
        error: error.into(),
    };

    let mut response = agent.get(url).call().map_err(retryable)?;
    let mut file = fs::File::create(dest).map_err(|error| AttemptError {
        error: anyhow::Error::new(error).context(format!("failed to create {}", dest.display())),
        retryable: true,
    })?;
    io::copy(&mut response.body_mut().as_reader(), &mut file).map_err(|error| AttemptError {
        error: anyhow::Error::new(error).context(format!("failed to write {}", dest.display())),
        retryable: true,
    })?;
    Ok(())
}

/// Whether the requested target, if any, selects this jar.
fn is_wanted(target: Option<PrepareTarget>, jar: &Jar) -> bool {
    match target {
        None => true,
        Some(PrepareTarget::Cfr) => jar.id == "cfr",
        Some(PrepareTarget::Vineflower) => jar.id == "vineflower",
    }
}

/// Whether `dest` already holds a download. An empty file is treated as absent so a
/// previous failed run is retried instead of being mistaken for a usable jar.
fn is_present(dest: &Path) -> bool {
    fs::metadata(dest).is_ok_and(|metadata| metadata.len() > 0)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;

    /// A throwaway HTTP server answering the first bytes of every request with `response`
    /// and closing the connection, plus the number of connections it accepted.
    fn serve(response: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let url = format!("http://{}/jar", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        (url, connections)
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into()
    }

    /// Downloads without pausing between attempts: the retry itself is what is under test,
    /// not the pause.
    fn fetch_into(url: &str, dest: &Path) -> anyhow::Result<()> {
        download(&agent(), url, dest, Duration::ZERO)
    }

    #[test]
    fn the_body_is_written_to_the_destination() {
        let (url, connections) = serve("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\njar");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cfr.jar");

        fetch_into(&url, &dest).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"jar");
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_release_fails_once_and_leaves_nothing_behind() {
        let (url, connections) = serve("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cfr.jar");

        let error = fetch_into(&url, &dest).unwrap_err();

        // The error names the status the CDN answered with, so a moved release is a
        // one-line diagnosis rather than three identical failures.
        assert!(format!("{error:#}").contains("404"), "{error:#}");
        assert!(!dest.exists(), "a failed download leaves no file");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "a 4xx is not worth retrying"
        );
    }

    #[test]
    fn a_dropped_connection_is_retried_and_reported() {
        // The server accepts and hangs up without answering: exactly the transient failure
        // a packaging job must survive.
        let (url, connections) = serve("");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cfr.jar");

        assert!(fetch_into(&url, &dest).is_err());

        assert_eq!(
            connections.load(Ordering::SeqCst),
            ATTEMPTS,
            "a connection that never answered is retried"
        );
        assert!(!dest.exists(), "no attempt leaves a file behind");
    }

    #[test]
    fn an_empty_file_is_not_a_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cfr.jar");

        assert!(!is_present(&dest), "nothing was downloaded yet");
        fs::write(&dest, b"").unwrap();
        assert!(
            !is_present(&dest),
            "an empty file is a previous failure, not a jar"
        );
        fs::write(&dest, b"jar").unwrap();
        assert!(is_present(&dest));
    }

    #[test]
    fn a_target_selects_its_own_jar() {
        let cfr = JARS.iter().find(|jar| jar.id == "cfr").unwrap();
        let vineflower = JARS.iter().find(|jar| jar.id == "vineflower").unwrap();

        assert!(is_wanted(None, cfr) && is_wanted(None, vineflower));
        assert!(is_wanted(Some(PrepareTarget::Cfr), cfr));
        assert!(!is_wanted(Some(PrepareTarget::Cfr), vineflower));
        assert!(is_wanted(Some(PrepareTarget::Vineflower), vineflower));
        assert!(!is_wanted(Some(PrepareTarget::Vineflower), cfr));
    }
}
