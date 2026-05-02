use base_db::SourceDatabase;
use ide_db::RootDatabase;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::lsp::convert::offset_to_position;

pub fn collect_diagnostics(
    db: RootDatabase,
    file_id: vfs::FileId,
) -> anyhow::Result<Vec<Diagnostic>> {
    let Some(parse_result) = db.parse_node(file_id) else {
        anyhow::bail!("Failed to parse node");
    };

    let text = db.file_text(file_id).text(&db);

    let diagnostics = parse_result
        .errors(&db)
        .into_iter()
        .map(|err| {
            let lsp_range = Range {
                start: offset_to_position(text, err.range.start()),
                end: offset_to_position(text, err.range.end()),
            };

            Diagnostic {
                range: lsp_range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("parser".to_string()),
                message: err.message.clone(),
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();

    // TODO: collect diagnostics from validator
    Ok(diagnostics)
}
