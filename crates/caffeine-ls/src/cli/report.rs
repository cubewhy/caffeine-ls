//! Aggregated diagnostics report: data model, text/JSON rendering and
//! severity filtering.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use lsp_types::{Diagnostic, DiagnosticSeverity};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticEntry {
    /// Path relative to the analyzed workspace root, with `/` separators.
    pub file: String,
    /// 1-based line of the diagnostic start.
    pub line: u32,
    /// 1-based column (in characters) of the diagnostic start.
    pub column: u32,
    /// `error`, `warning`, `information` or `hint`.
    pub severity: &'static str,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DiagnosticReport {
    /// Number of files a diagnostics pull succeeded for.
    pub files_analyzed: usize,
    /// Number of candidate files the server does not know (outside any
    /// source root or filtered by gitignore), which were skipped.
    pub files_skipped: usize,
    /// Diagnostics that passed the severity filter.
    pub diagnostics: Vec<DiagnosticEntry>,
}

/// Converts the raw LSP diagnostics of one file into report entries,
/// keeping only those within the severity threshold.
///
/// The caller is responsible for counting skipped/analyzed files.
pub(crate) fn collect_entries(
    file_display: &str,
    file_path: &Path,
    diagnostics: &[Diagnostic],
    max_rank: u32,
) -> Vec<DiagnosticEntry> {
    let mut entries = Vec::new();
    for diagnostic in diagnostics {
        let Some(severity) = diagnostic.severity else {
            continue;
        };
        let rank = severity_rank(severity);
        if rank > max_rank {
            continue;
        }

        let (line, column) = one_based_position(file_path, diagnostic.range.start);
        entries.push(DiagnosticEntry {
            file: file_display.to_string(),
            line,
            column,
            severity: severity_name(severity),
            code: diagnostic.code.as_ref().map(code_to_string),
            message: message_text(&diagnostic.message),
        });
    }
    entries
}

fn code_to_string(code: &lsp_types::Code) -> String {
    match code {
        lsp_types::Code::String(s) => s.clone(),
        lsp_types::Code::Int(n) => n.to_string(),
    }
}

fn message_text(message: &lsp_types::Message) -> String {
    match message {
        lsp_types::Message::String(s) => s.clone(),
        lsp_types::Message::MarkupContent(markup) => markup.value.clone(),
    }
}

pub(crate) fn severity_rank(severity: DiagnosticSeverity) -> u32 {
    match severity {
        DiagnosticSeverity::Error => crate::flags::SeverityRank::ERROR,
        DiagnosticSeverity::Warning => crate::flags::SeverityRank::WARNING,
        DiagnosticSeverity::Information | DiagnosticSeverity::Custom(_) => {
            crate::flags::SeverityRank::INFORMATION
        }
        DiagnosticSeverity::Hint => crate::flags::SeverityRank::HINT,
    }
}

pub(crate) fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Information | DiagnosticSeverity::Custom(_) => "information",
        DiagnosticSeverity::Hint => "hint",
    }
}

/// Converts an LSP position (0-based line, 0-based UTF-8 character offset —
/// the negotiated encoding is UTF-8) into a 1-based human-readable line and
/// column. Falls back to the raw offsets when the file cannot be read.
fn one_based_position(path: &Path, position: lsp_types::Position) -> (u32, u32) {
    let fallback = || {
        (
            position.line.saturating_add(1),
            position.character.saturating_add(1),
        )
    };

    let Ok(text) = std::fs::read_to_string(path) else {
        return fallback();
    };

    let Some(line_text) = text.split('\n').nth(position.line as usize) else {
        return fallback();
    };

    // Positions use UTF-8 code units here, so slice by bytes and count chars.
    let mut byte_offset = (position.character as usize).min(line_text.len());
    while byte_offset > 0 && !line_text.is_char_boundary(byte_offset) {
        byte_offset -= 1;
    }
    let column = line_text[..byte_offset].chars().count() as u32 + 1;
    (position.line + 1, column)
}

/// Renders the report in a compiler-style human-readable format:
/// `path/to/File.java:3:9: error[code]: message`.
pub fn render_text(report: &DiagnosticReport) -> String {
    let mut out = String::new();
    for entry in &report.diagnostics {
        out.push_str(&entry.file);
        out.push(':');
        out.push_str(&entry.line.to_string());
        out.push(':');
        out.push_str(&entry.column.to_string());
        out.push_str(": ");
        out.push_str(entry.severity);
        if let Some(code) = &entry.code {
            out.push('[');
            out.push_str(code);
            out.push(']');
        }
        out.push_str(": ");
        out.push_str(&entry.message);
        out.push('\n');
    }

    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for entry in &report.diagnostics {
        *counts.entry(entry.severity).or_default() += 1;
    }

    let order = ["error", "warning", "information", "hint"];
    let parts: Vec<String> = order
        .into_iter()
        .filter_map(|name| counts.get(name).map(|n| format!("{n} {name}(s)")))
        .collect();

    let summary = if parts.is_empty() {
        "no diagnostics".to_string()
    } else {
        format!(
            "{} found across {} analyzed file(s), {} skipped",
            parts.join(", "),
            report.files_analyzed,
            report.files_skipped
        )
    };

    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("summary: ");
    out.push_str(&summary);
    out.push('\n');
    out
}

/// Renders the report as pretty-printed JSON.
pub fn render_json(report: &DiagnosticReport) -> anyhow::Result<String> {
    serde_json::to_string_pretty(report)
        .map_err(|e| anyhow::format_err!("failed to serialize report: {e}"))
}

/// Builds a display path relative to the workspace root with forward slashes.
pub(crate) fn display_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Discovers candidate source files under `root`, mirroring the VFS loader's
/// configuration: gitignore-aware walk (`require_git(false)` like the
/// loader's ignore matchers) restricted to `.java`, `.kt` and `.kts`.
pub(crate) fn discover_files(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .build();

    let mut files: Vec<PathBuf> = walker
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter(|entry| {
            matches!(
                entry.path().extension().and_then(|ext| ext.to_str()),
                Some("java") | Some("kt") | Some("kts")
            )
        })
        .map(|entry| entry.into_path())
        .collect();

    files.sort();
    files.dedup();
    files
}
