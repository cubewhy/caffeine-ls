use ide::Analysis;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::from_proto::offset_to_position;

pub fn collect_diagnostics(
    analysis: Analysis,
    file_id: vfs::FileId,
    text: String,
) -> anyhow::Result<Vec<Diagnostic>> {
    // let Some(parse_result) = analysis.parse_cache.get_tree(file_id) else {
    //     anyhow::bail!("file is not parsed yet");
    // };
    //
    // let diagnostics = parse_result
    //     .syntax_errors
    //     .iter()
    //     .map(|err| {
    //         let lsp_range = Range {
    //             start: offset_to_position(&text, err.range.start()),
    //             end: offset_to_position(&text, err.range.end()),
    //         };
    //
    //         Diagnostic {
    //             range: lsp_range,
    //             severity: Some(DiagnosticSeverity::ERROR),
    //             source: Some(crate::NAME.to_string()),
    //             message: err.message.clone(),
    //             ..Default::default()
    //         }
    //     })
    //     .collect::<Vec<_>>();
    //
    // // TODO: collect diagnostics from validator
    // Ok(diagnostics)

    Ok(vec![])
}
