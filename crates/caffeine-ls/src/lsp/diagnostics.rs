use crate::lsp;

pub(crate) fn convert_diagnostic(
    line_index: &crate::line_index::LineIndex,
    d: ide::Diagnostic,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp::to_proto::range(line_index, d.range.range),
        severity: Some(lsp::to_proto::diagnostic_severity(d.severity)),
        code: d
            .code
            .map(|code| lsp_types::Code::String(code_javac_or_custom(code))),
        code_description: None,
        source: Some(crate::NAME.to_owned()),
        message: lsp_types::Message::String(d.message),
        related_information: None,
        tags: d
            .unused
            .then(|| vec![lsp_types::DiagnosticTag::Unnecessary]),
        data: None,
    }
}

/// The wire code of a diagnostic: the javac `compiler.*` code when the
/// underlying construct has a 1:1 twin, else the custom stable code.
fn code_javac_or_custom(code: ide::DiagnosticCode) -> String {
    use ide::DiagnosticCode;
    match code {
        DiagnosticCode::Java(code) => code
            .javac_code()
            .unwrap_or_else(|| code.as_str())
            .to_owned(),
        DiagnosticCode::Kotlin(_) => code.as_str().to_owned(),
    }
}
