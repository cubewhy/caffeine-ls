use ide::Analysis;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::from_proto::offset_to_position;

pub fn collect_diagnostics(
    analysis: Analysis,
    file_id: vfs::FileId,
    text: String,
) -> anyhow::Result<Vec<Diagnostic>> {
    let errors = if let Some(parsed) = analysis.parsed_file(file_id) {
        parsed.syntax_errors.clone()
    } else {
        syntax::parse_file(
            syntax::LanguageId::Java,
            &text,
            &lasso::ThreadedRodeo::new(),
        )
        .errors
    };

    Ok(errors
        .into_iter()
        .map(|err| Diagnostic {
            range: Range {
                start: offset_to_position(&text, err.range.start()),
                end: offset_to_position(&text, err.range.end()),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some(crate::NAME.to_string()),
            message: err.message,
            ..Default::default()
        })
        .collect())
}
