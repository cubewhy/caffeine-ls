use crate::lsp;

pub(crate) fn convert_diagnostic(
    line_index: &crate::line_index::LineIndex,
    d: ide::Diagnostic,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp::to_proto::range(line_index, d.range.range),
        severity: Some(lsp::to_proto::diagnostic_severity(d.severity)),
        // code: Some(lsp_types::Code::String(d.code.as_str().to_owned())),
        // code_description: Some(lsp_types::CodeDescription {
        //     href: lsp_types::Uri::parse(&d.code.url()).unwrap(),
        // }),
        code: None,
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
