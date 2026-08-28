use std::collections::HashMap;

use crate::{
    cross_file,
    global_state::GlobalStateSnapshot,
    lsp::{diagnostics, symbols, to_proto},
};

use ide::LanguageKind;
use lsp_types::*;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReport> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    let file_id = state
        .url_to_file_id(&params.text_document.uri)?
        .ok_or_else(|| anyhow::format_err!("failed to get vfs path from uri"))?;
    let line_index = state.file_line_index(file_id)?;
    // Before the workspace is loaded, files are not part of any source
    // root, so fall back to the language kind inferred from the path.
    let fallback_language_kind = LanguageKind::from_path(params.text_document.uri.path());
    // javac reports `rawtypes`/`unchecked` only under an explicit
    // `-Xlint` flag ([JLS-adjacent]); the same lints gate the matching
    // warnings here, so the default stream matches plain `javac`.
    let lints = state.config.client_lints();
    let diagnostics = state
        .analysis
        .syntax_diagnostics(file_id, fallback_language_kind)?
        .into_iter()
        .chain(state.analysis.file_diagnostics(file_id)?)
        .filter(|diagnostic| cross_file::lint_allows(lints, diagnostic))
        .map(|diagnostic| diagnostics::convert_diagnostic(&line_index, diagnostic))
        .collect();

    // The seal covers every reported item (syntax + type/declaration); the
    // client echoes it back as `previous_result_id` to skip re-serialization
    // when nothing changed.
    let seal = cross_file::diagnostic_seal(&state.analysis, file_id, fallback_language_kind)?;
    // Related documents: files whose diagnostics an edit to this file can move,
    // sealed so a steady-state re-pull is a handful of `resultId` comparisons.
    let related_documents: Option<HashMap<Uri, RelatedDocument>> = state
        .cross_file
        .related_for(&state, file_id)?
        .map(|map| map.into_iter().collect());

    tracing::debug!(
        ?diagnostics,
        related = related_documents.as_ref().map(|it| it.len()),
        "finished collecting diagnostics"
    );

    if params.previous_result_id.as_deref() == Some(seal.as_str()) {
        return Ok(RelatedUnchangedDocumentDiagnosticReport {
            related_documents,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                result_id: seal,
            },
        }
        .into());
    }

    Ok(RelatedFullDocumentDiagnosticReport {
        related_documents,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: Some(seal),
            items: diagnostics,
        },
    }
    .into())
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
