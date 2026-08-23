use crate::{global_state::GlobalStateSnapshot, lsp::diagnostics, lsp::symbols, lsp::to_proto};

use ide::LanguageKind;
use lsp_types::*;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReport> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    if let Ok(Some(file_id)) = state.url_to_file_id(&params.text_document.uri) {
        let line_index = state.file_line_index(file_id)?;
        // Before the workspace is loaded, files are not part of any source
        // root, so fall back to the language kind inferred from the path.
        let fallback_language_kind = LanguageKind::from_path(params.text_document.uri.path());
        let diagnostics = state
            .analysis
            .syntax_diagnostics(file_id, fallback_language_kind)?
            .into_iter()
            .chain(state.analysis.type_diagnostics(file_id)?)
            .map(|diagnostic| diagnostics::convert_diagnostic(&line_index, diagnostic))
            .collect();

        tracing::debug!(?diagnostics, "finished collecting diagnostics");

        Ok(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: diagnostics,
            },
        }
        .into())
    } else {
        anyhow::bail!("failed to get vfs path from uri")
    }
}

pub fn on_document_symbol(
    state: GlobalStateSnapshot,
    params: DocumentSymbolParams,
) -> anyhow::Result<Option<DocumentSymbolResponse>> {
    tracing::info!(uri = ?params.text_document.uri, "request document symbols");

    let file_id = state
        .url_to_file_id(&params.text_document.uri)?
        .ok_or_else(|| anyhow::format_err!("failed to get vfs path from uri"))?;
    let line_index = state.file_line_index(file_id)?;
    let document_symbols = state.analysis.document_symbols(file_id)?;
    let nested = symbols::nest_document_symbols(&line_index, &document_symbols);

    Ok(Some(nested.into()))
}

pub fn on_workspace_symbol(
    state: GlobalStateSnapshot,
    params: WorkspaceSymbolParams,
) -> anyhow::Result<Option<WorkspaceSymbolResponse>> {
    tracing::info!(query = ?params.query, "request workspace symbols");

    let workspace_symbols = state.analysis.workspace_symbols(&params.query)?;
    let mut out = Vec::with_capacity(workspace_symbols.len());
    for symbol in workspace_symbols {
        let uri = state.file_id_to_url(symbol.file)?;
        let line_index = state.file_line_index(symbol.file)?;
        let location = symbols::location(&line_index, uri, &symbol);
        out.push(symbols::workspace_symbol(location, &symbol));
    }

    Ok(Some(out.into()))
}

/// The declaration(s) a reference at a position resolves to, as LSP
/// locations ([JLS §6.5]).
pub fn on_goto_definition(
    state: GlobalStateSnapshot,
    params: DefinitionParams,
) -> anyhow::Result<Option<DefinitionResponse>> {
    let pos = params.text_document_position_params;
    tracing::info!(uri = ?pos.text_document.uri, "request goto definition");

    let file_id = state
        .url_to_file_id(&pos.text_document.uri)?
        .ok_or_else(|| anyhow::format_err!("failed to get vfs path from uri"))?;
    let line_index = state.file_line_index(file_id)?;
    let offset = crate::lsp::from_proto::offset(&line_index, pos.position)?;
    let targets = state.analysis.goto_definition(file_id, offset)?;
    if targets.is_empty() {
        return Ok(None);
    }

    let mut locations = Vec::with_capacity(targets.len());
    for target in targets {
        let uri = state.file_id_to_url(target.file)?;
        let line_index = state.file_line_index(target.file)?;
        locations.push(Location {
            uri,
            range: to_proto::range(&line_index, target.range),
        });
    }
    Ok(Some(DefinitionResponse::Definition(locations.into())))
}

/// The hover at a position: the type of the expression or the signature of
/// the declaration there.
pub fn on_hover(state: GlobalStateSnapshot, params: HoverParams) -> anyhow::Result<Option<Hover>> {
    let pos = params.text_document_position_params;
    tracing::info!(uri = ?pos.text_document.uri, "request hover");

    let file_id = state
        .url_to_file_id(&pos.text_document.uri)?
        .ok_or_else(|| anyhow::format_err!("failed to get vfs path from uri"))?;
    let line_index = state.file_line_index(file_id)?;
    let offset = crate::lsp::from_proto::offset(&line_index, pos.position)?;
    let info = state.analysis.hover(file_id, offset)?;
    Ok(info.map(|info| Hover {
        contents: Contents::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```java\n{}\n```", info.value),
        }),
        range: None,
    }))
}
