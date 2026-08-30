use crate::line_index::LineIndex;
use ide::Severity;
use rowan::{TextRange, TextSize};
use std::path::{Path, PathBuf};
use vfs::AbsPath;

use crate::line_index::PositionEncoding;

/// Builds the URI for an absolute path. The Windows drive letter is
/// upper-cased (mirroring [`crate::lsp::from_proto::abs_path`]) so a path
/// round-trips to a URI that compares equal to the one the client sent.
pub(crate) fn url(path: &AbsPath) -> lsp_types::Uri {
    let path: PathBuf = PathBuf::from(Path::new(path.as_str()));
    let path = crate::lsp::from_proto::normalize_windows_path(path);
    lsp_types::Uri::from_file_path(path).expect("failed to convert path to URI")
}

pub(crate) fn position(line_index: &LineIndex, offset: TextSize) -> lsp_types::Position {
    let line_col = line_index.index.line_col(offset);
    match line_index.encoding {
        PositionEncoding::Utf8 => lsp_types::Position::new(line_col.line, line_col.col),
        PositionEncoding::Wide(enc) => {
            let line_col = line_index.index.to_wide(enc, line_col).unwrap();
            lsp_types::Position::new(line_col.line, line_col.col)
        }
    }
}

pub(crate) fn range(line_index: &LineIndex, range: TextRange) -> lsp_types::Range {
    let start = position(line_index, range.start());
    let end = position(line_index, range.end());
    lsp_types::Range::new(start, end)
}

pub(crate) fn diagnostic_severity(severity: Severity) -> lsp_types::DiagnosticSeverity {
    match severity {
        Severity::Error => lsp_types::DiagnosticSeverity::Error,
        Severity::Warning => lsp_types::DiagnosticSeverity::Warning,
        Severity::WeakWarning => lsp_types::DiagnosticSeverity::Hint,
        // unreachable
        Severity::Allow => lsp_types::DiagnosticSeverity::Information,
    }
}
