//! Parses Maven console output lines into structured [`SyncProgress`] events.
//!
//! Maven always prints `[INFO]`-prefixed lines; the download/phase markers we
//! pattern-match on are stable across Maven 3.x. Parsing is additive: unmatched
//! lines produce nothing so the consumer falls back to an indeterminate phase.

use crate::SyncProgress;

/// Runs every raw output line through the Maven parsers and forwards any
/// recognized events to `on_progress`.
pub fn parse_line(line: &str, on_progress: &mut (dyn FnMut(SyncProgress) + Send)) {
    let trimmed = line.trim();

    // "[INFO] Downloading from central: https://..." — a transfer started.
    if let Some(rest) = trimmed.strip_prefix("[INFO] Downloading from ") {
        if let Some(url) = rest.split_once(':').map(|(_, url)| url.trim()) {
            if !url.is_empty() {
                on_progress(SyncProgress::Phase(crate::SyncPhase::Downloading));
                on_progress(SyncProgress::Download {
                    dependency: url.to_string(),
                    bytes_downloaded: 0,
                    bytes_total: None,
                });
                return;
            }
        }
    }

    // "[INFO] Downloaded from central: https://..." — a transfer finished.
    if let Some(rest) = trimmed.strip_prefix("[INFO] Downloaded from ") {
        if let Some(url) = rest.split_once(':').map(|(_, url)| url.trim()) {
            if !url.is_empty() {
                on_progress(SyncProgress::Download {
                    dependency: url.to_string(),
                    bytes_downloaded: 0,
                    bytes_total: None,
                });
                return;
            }
        }
    }

    // "[INFO] Building artifactId 1.0" / "[INFO] Building artifactId 1.0 [i/n]"
    // — a reactor module started building. The reactor order is printed before
    // any module, so per-module `index/total` is derived once we see the
    // "Reactor Build Order" list.
    if let Some(rest) = trimmed.strip_prefix("[INFO] Building ") {
        let rest = rest.trim();
        let name = first_token(rest);
        if !name.is_empty() {
            let (_, index_total) = split_index_total(rest);
            let (index, total) = index_total.unwrap_or((0, 0));
            on_progress(SyncProgress::Phase(crate::SyncPhase::Compiling));
            on_progress(SyncProgress::Project {
                name: name.to_string(),
                index,
                total,
                action: "building".to_string(),
            });
            return;
        }
    }

    // "Reactor Build Order:" begins the module list; "[INFO]   module-a (i/n)"
    // lists each module in order. We record the total for later phases.
    if trimmed.starts_with("Reactor Build Order") {
        on_progress(SyncProgress::Phase(crate::SyncPhase::Resolving));
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("[INFO]   ") {
        let rest = rest.trim();
        // Reactor order entries look like "module-a (1/3)" — parenthesized,
        // unlike the "[1/3]" on "Building ..." lines.
        if let Some(open) = rest.rfind('(')
            && let Some(close) = rest.rfind(')')
            && close > open
            && let Some((index, total)) = rest[open + 1..close].split_once('/')
            && let (Ok(index), Ok(total)) =
                (index.trim().parse::<u32>(), total.trim().parse::<u32>())
        {
            let name = first_token(&rest[..open]);
            on_progress(SyncProgress::Project {
                name: name.to_string(),
                index,
                total,
                action: "queued".to_string(),
            });
            return;
        }
    }

    // A `[INFO] <plugin>:<goal>` (compile, resources, ...) line marks progress
    // within the current module.
    if let Some(rest) = trimmed.strip_prefix("[INFO] --- ") {
        let goal = rest.trim();
        if !goal.is_empty() {
            on_progress(SyncProgress::Phase(crate::SyncPhase::Compiling));
            on_progress(SyncProgress::Info(goal.to_string()));
            return;
        }
    }

    // "BUILD SUCCESS" / "BUILD FAILURE" end the run.
    if trimmed.contains("BUILD SUCCESS") {
        on_progress(SyncProgress::Phase(crate::SyncPhase::Done));
    } else if trimmed.contains("BUILD FAILURE") {
        on_progress(SyncProgress::Phase(crate::SyncPhase::Failed));
    }
}

/// The first whitespace-delimited token of `s` — the module artifact name in a
/// `Building artifactId version` line.
fn first_token(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

/// Splits a `Building foo-1.0 [1/3]` line into `(name, Some((index, total)))`,
/// or just `(name, None)` when no bracket index is present.
fn split_index_total(s: &str) -> (&str, Option<(u32, u32)>) {
    let s = s.trim();
    let Some(open) = s.rfind('[') else {
        return (s, None);
    };
    let Some(close) = s.rfind(']') else {
        return (s, None);
    };
    if close <= open {
        return (s, None);
    }
    let name = s[..open].trim();
    let inner = &s[open + 1..close];
    let Some((idx_str, total_str)) = inner.split_once('/') else {
        return (s, None);
    };
    let Ok(index) = idx_str.trim().parse::<u32>() else {
        return (s, None);
    };
    let Ok(total) = total_str.trim().parse::<u32>() else {
        return (s, None);
    };
    if index >= 1 && total >= 1 {
        (name, Some((index, total)))
    } else {
        (s, None)
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
    fn downloading_starts_phase() {
        let events = parse("[INFO] Downloading from central: https://repo/foo-1.0.jar");
        assert_eq!(
            events,
            vec![
                SyncProgress::Phase(SyncPhase::Downloading),
                SyncProgress::Download {
                    dependency: "https://repo/foo-1.0.jar".into(),
                    bytes_downloaded: 0,
                    bytes_total: None,
                },
            ]
        );
    }

    #[test]
    fn downloaded_without_size() {
        let events = parse("[INFO] Downloaded from central: https://repo/foo-1.0.jar");
        assert_eq!(
            events,
            vec![SyncProgress::Download {
                dependency: "https://repo/foo-1.0.jar".into(),
                bytes_downloaded: 0,
                bytes_total: None,
            }]
        );
    }

    #[test]
    fn building_module_with_index() {
        let events = parse("[INFO] Building module-a 1.0 [1/3]");
        assert_eq!(events[0], SyncProgress::Phase(SyncPhase::Compiling));
        assert_eq!(
            events[1],
            SyncProgress::Project {
                name: "module-a".into(),
                index: 1,
                total: 3,
                action: "building".into(),
            }
        );
    }

    #[test]
    fn building_module_without_index() {
        let events = parse("[INFO] Building module-b 2.0");
        assert_eq!(
            events[1],
            SyncProgress::Project {
                name: "module-b".into(),
                index: 0,
                total: 0,
                action: "building".into(),
            }
        );
    }

    #[test]
    fn reactor_order_entries() {
        let events = parse("[INFO]   module-a (1/3)");
        assert_eq!(
            events,
            vec![SyncProgress::Project {
                name: "module-a".into(),
                index: 1,
                total: 3,
                action: "queued".into(),
            }]
        );
    }

    #[test]
    fn build_success_and_failure() {
        assert_eq!(
            parse("[INFO] BUILD SUCCESS"),
            vec![SyncProgress::Phase(SyncPhase::Done)]
        );
        assert_eq!(
            parse("[INFO] BUILD FAILURE"),
            vec![SyncProgress::Phase(SyncPhase::Failed)]
        );
    }

    #[test]
    fn goal_banner_is_info() {
        let events = parse(
            "[INFO] --- maven-compiler-plugin:3.8.1:compile (default-compile) @ module-a ---",
        );
        assert_eq!(events[0], SyncProgress::Phase(SyncPhase::Compiling));
    }

    #[test]
    fn unrelated_lines_emit_nothing() {
        assert!(parse("[INFO] Scanning for projects...").is_empty());
        assert!(parse("[INFO] [INFO]").is_empty());
        assert!(parse("").is_empty());
    }
}
