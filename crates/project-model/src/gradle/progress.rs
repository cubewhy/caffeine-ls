//! Parses Gradle console output lines into structured [`SyncProgress`] events.
//!
//! Gradle is run with `--console=plain` so output is line-oriented and stable
//! enough to pattern-match. Parsing is deliberately additive: lines that match
//! nothing produce no event, so the consumer falls back to an indeterminate
//! phase rather than guessing.

use crate::SyncProgress;

/// Runs every raw output line through the Gradle parsers and forwards any
/// recognized events to `on_progress`.
pub fn parse_line(line: &str, on_progress: &mut (dyn FnMut(SyncProgress) + Send)) {
    let trimmed = line.trim();

    // "Downloading https://repo/... (some mib)" — no size on older Gradle.
    if let Some(url) = trimmed.strip_prefix("Downloading ") {
        if let Some((url, size)) = split_download_size(url) {
            on_progress(SyncProgress::Phase(crate::SyncPhase::Downloading));
            on_progress(SyncProgress::Download {
                dependency: url.to_string(),
                bytes_downloaded: 0,
                bytes_total: Some(size),
            });
            return;
        }
    }

    // "Downloaded https://repo/... (some mib)" — a transfer completed.
    if let Some(rest) = trimmed.strip_prefix("Downloaded ") {
        if let Some((url, size)) = split_download_size(rest) {
            on_progress(SyncProgress::Download {
                dependency: url.to_string(),
                bytes_downloaded: size,
                bytes_total: Some(size),
            });
            return;
        }
    }

    // "> Task :app:compileJava" — a task started running.
    if let Some(rest) = trimmed.strip_prefix("> Task ") {
        let task = rest.trim();
        if !task.is_empty() {
            on_progress(SyncProgress::Phase(crate::SyncPhase::Compiling));
            let (project, task_name) = split_task(task);
            if let Some(project) = project {
                on_progress(SyncProgress::Project {
                    name: project.to_string(),
                    index: 0,
                    total: 0,
                    action: task_name.to_string(),
                });
            } else {
                on_progress(SyncProgress::Info(task.to_string()));
            }
            return;
        }
    }

    // "> Task :app" (no name) is emitted when a project is being configured.
    if let Some(rest) = trimmed.strip_prefix("> ") {
        let project = rest.trim();
        if project.starts_with(':') && !project.is_empty() {
            on_progress(SyncProgress::Phase(crate::SyncPhase::Configuring));
            on_progress(SyncProgress::Project {
                name: project.to_string(),
                index: 0,
                total: 0,
                action: "configuring".to_string(),
            });
            return;
        }
    }

    // "Configuring project :app"
    if let Some(rest) = trimmed.strip_prefix("Configuring project ") {
        let project = rest.trim();
        if project.starts_with(':') && !project.is_empty() {
            on_progress(SyncProgress::Phase(crate::SyncPhase::Configuring));
            on_progress(SyncProgress::Project {
                name: project.to_string(),
                index: 0,
                total: 0,
                action: "configuring".to_string(),
            });
        }
    }
}

/// Splits `url (123.4 MiB)` into the URL and its byte size, or returns
/// `(url, None)` when the size portion is missing/unparseable.
fn split_download_size(s: &str) -> Option<(&str, u64)> {
    let idx = s.rfind('(')?;
    let url = s[..idx].trim();
    let size_str = s[idx + 1..].trim_end();
    let size_str = size_str.trim_end_matches(')');
    let bytes = parse_size(size_str)?;
    Some((url, bytes))
}

/// Parses a Gradle-style size suffix: `MiB`, `KiB`, `GB`, `MB`, `KB`, or bare
/// bytes. Returns `None` for unrecognized formats.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num_str, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .map(|i| (&s[..i], &s[i..]))
        .unwrap_or((s, ""));

    let value: f64 = num_str.replace(',', ".").parse().ok()?;
    let bytes = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => value,
        "kb" => value * 1024.0,
        "kib" => value * 1024.0,
        "mb" => value * 1024.0 * 1024.0,
        "mib" => value * 1024.0 * 1024.0,
        "gb" => value * 1024.0 * 1024.0 * 1024.0,
        "gib" => value * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(bytes.round() as u64)
}

/// Splits a `:app:compileJava` task path into `(Some(":app"), "compileJava")`,
/// handling the root project (`:compileJava`) and bare tasks (`compileJava`).
fn split_task(task: &str) -> (Option<&str>, &str) {
    match task.rfind(':') {
        Some(idx) if idx > 0 => {
            let project = &task[..idx];
            let name = &task[idx + 1..];
            (Some(project), name)
        }
        // ":compileJava" — the root project's path is ":".
        Some(0) => (Some(":"), &task[1..]),
        _ => (None, task),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SyncPhase, SyncProgress};

    fn parse(line: &str) -> Vec<SyncProgress> {
        let mut events = Vec::new();
        parse_line(line, &mut |e| events.push(e));
        events
    }

    #[test]
    fn downloading_with_size() {
        let events = parse("Downloading https://repo.maven.apache.org/foo-1.0.jar (12.3 MiB)");
        assert_eq!(
            events,
            vec![
                SyncProgress::Phase(SyncPhase::Downloading),
                SyncProgress::Download {
                    dependency: "https://repo.maven.apache.org/foo-1.0.jar".into(),
                    bytes_downloaded: 0,
                    bytes_total: Some(12_897_485),
                },
            ]
        );
    }

    #[test]
    fn downloading_without_size() {
        // Older Gradle prints no size; we must not crash and emit nothing.
        assert!(parse("Downloading https://repo.maven.apache.org/foo-1.0.jar").is_empty());
    }

    #[test]
    fn downloaded_completes_transfer() {
        let events = parse("Downloaded https://repo.maven.apache.org/foo-1.0.jar (4 KiB)");
        assert_eq!(
            events,
            vec![SyncProgress::Download {
                dependency: "https://repo.maven.apache.org/foo-1.0.jar".into(),
                bytes_downloaded: 4096,
                bytes_total: Some(4096),
            }]
        );
    }

    #[test]
    fn task_banner_emits_compiling() {
        let events = parse("> Task :app:compileJava");
        assert_eq!(events[0], SyncProgress::Phase(SyncPhase::Compiling));
        assert_eq!(
            events[1],
            SyncProgress::Project {
                name: ":app".into(),
                index: 0,
                total: 0,
                action: "compileJava".into(),
            }
        );
    }

    #[test]
    fn root_task_banner() {
        let events = parse("> Task :compileJava");
        assert_eq!(
            events,
            vec![
                SyncProgress::Phase(SyncPhase::Compiling),
                SyncProgress::Project {
                    name: ":".into(),
                    index: 0,
                    total: 0,
                    action: "compileJava".into(),
                },
            ]
        );
    }

    #[test]
    fn configuring_project() {
        let events = parse("Configuring project :app");
        assert_eq!(
            events,
            vec![
                SyncProgress::Phase(SyncPhase::Configuring),
                SyncProgress::Project {
                    name: ":app".into(),
                    index: 0,
                    total: 0,
                    action: "configuring".into(),
                },
            ]
        );
    }

    #[test]
    fn unrelated_lines_emit_nothing() {
        assert!(parse("Welcome to Gradle 8.0!").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn size_units() {
        assert_eq!(parse_size("1 B"), Some(1));
        assert_eq!(parse_size("1 KiB"), Some(1024));
        assert_eq!(parse_size("1.5 MiB"), Some(1_572_864));
        assert_eq!(parse_size("2 MB"), Some(2_097_152));
        assert_eq!(parse_size("nope"), None);
        // 12.3 MiB = 12.3 * 1024 * 1024 = 12,897,484.8 → rounds to 12,897,485.
        assert_eq!(parse_size("12.3 MiB"), Some(12_897_485));
    }
}
