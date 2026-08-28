use std::collections::HashMap;

use crate::{
    diagnostics,
    global_state::GlobalStateSnapshot,
    lsp::{symbols, to_proto},
};

use ide::LanguageKind;
use lsp_types::*;
use rustc_hash::FxHashMap;

pub fn on_diagnostic(
    state: GlobalStateSnapshot,
    params: DocumentDiagnosticParams,
) -> anyhow::Result<DocumentDiagnosticReport> {
    tracing::info!(uri = ?params.text_document.uri, "request diagnostics");

    // The file may have been deleted (e.g. an open tab whose file is removed on
    // disk); report no problems rather than failing the request.
    let Some(file_id) = state.url_to_file_id(&params.text_document.uri)? else {
        return Ok(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: Vec::new(),
            },
        }
        .into());
    };
    // Before the workspace is loaded, files are not part of any source
    // root, so fall back to the language kind inferred from the path.
    let fallback_language_kind = LanguageKind::from_path(params.text_document.uri.path());
    // Track this file across edits and compute (or read back) its report.
    state
        .diagnostics
        .ensure_subscribed(&state.analysis, file_id)?;
    let (generation, report) =
        state
            .diagnostics
            .file_report(&state.analysis, file_id, fallback_language_kind)?;

    // Related documents: watched files whose diagnostics an edit to this file
    // can move, sealed with their generation.
    let related_documents: Option<HashMap<Uri, RelatedDocument>> =
        diagnostics::related_for(&state, file_id)?.map(|map| map.into_iter().collect());

    let id = generation.to_string();
    if params.previous_result_id.as_deref() == Some(id.as_str()) {
        return Ok(RelatedUnchangedDocumentDiagnosticReport {
            related_documents,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                result_id: id,
            },
        }
        .into());
    }

    let items = diagnostics::convert_items(&state, file_id, &report)?;
    Ok(RelatedFullDocumentDiagnosticReport {
        related_documents,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: Some(id),
            items,
        },
    }
    .into())
}

/// The workspace-wide diagnostic report ([§Diagnostic]): one full or unchanged
/// entry per source file, sealed with the file's generation. Documents whose
/// [`previousResultId`][WorkspaceDiagnosticParams#previous_result_ids] still
/// matches are echoed as `Unchanged`; the rest come back in full, so the
/// client can refresh its whole in-memory diagnostic store in a single round
/// trip.
pub fn on_workspace_diagnostic(
    state: GlobalStateSnapshot,
    params: WorkspaceDiagnosticParams,
) -> anyhow::Result<WorkspaceDiagnosticReport> {
    tracing::info!(
        previous = params.previous_result_ids.len(),
        "request workspace diagnostics"
    );

    let previous_ids: FxHashMap<Uri, String> = params
        .previous_result_ids
        .into_iter()
        .map(|previous| (previous.uri, previous.value))
        .collect();

    let items = diagnostics::workspace_diagnostic_reports(&state, &previous_ids)?;
    Ok(WorkspaceDiagnosticReport::new(items))
}

pub fn on_document_symbol(
    state: GlobalStateSnapshot,
    params: DocumentSymbolParams,
) -> anyhow::Result<Option<DocumentSymbolResponse>> {
    tracing::info!(uri = ?params.text_document.uri, "request document symbols");

    let Some(file_id) = state.url_to_file_id(&params.text_document.uri)? else {
        return Ok(None);
    };
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

    let Some(file_id) = state.url_to_file_id(&pos.text_document.uri)? else {
        return Ok(None);
    };
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

    let Some(file_id) = state.url_to_file_id(&pos.text_document.uri)? else {
        return Ok(None);
    };
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
