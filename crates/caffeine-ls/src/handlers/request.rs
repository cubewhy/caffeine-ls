use crate::{
    diagnostics,
    global_state::GlobalStateSnapshot,
    handlers::dispatch::DeferForLibraryFiles,
    lsp::{symbols, to_proto},
};

use ide::LibraryFileRef;
use lsp_types::*;
use rustc_hash::FxHashMap;
use vfs::FileId;

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
    // Compute the report through the memoized salsa query; the `result_id` is a
    // deterministic fingerprint of the items, so an unchanged file echoes
    // `Unchanged` across edits to unrelated files.
    let report = state.analysis.file_report(file_id)?;
    let items = diagnostics::convert_items(&state, file_id, &report)?;
    let id = diagnostics::render_id(diagnostics::result_id(&items));
    if params.previous_result_id.as_deref() == Some(id.as_str()) {
        return Ok(RelatedUnchangedDocumentDiagnosticReport {
            related_documents: None,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                result_id: id,
            },
        }
        .into());
    }

    Ok(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
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

    let previous_ids: FxHashMap<FileId, String> = params
        .previous_result_ids
        .into_iter()
        .filter_map(|previous| {
            // Index by FileId instead of comparing URI strings: the client may
            // spell the same file's URI differently (`d%3A` vs `d:` vs `D:`)
            // than the server would serialize it, which would otherwise
            // force every file back to a full report. Unknown/malformed URIs
            // are dropped and simply come back full.
            state
                .url_to_file_id(&previous.uri)
                .ok()
                .flatten()
                .map(|file_id| (file_id, previous.value))
        })
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

    // An empty query would otherwise enumerate the whole workspace index just
    // to fill a picker row list; serve only the files the client has open.
    let scope: Option<Vec<FileId>> = if params.query.trim().is_empty() {
        Some(state.opened_file_ids())
    } else {
        None
    };
    let workspace_symbols = state
        .analysis
        .workspace_symbols(&params.query, scope.as_deref())?;

    let mut out = Vec::with_capacity(workspace_symbols.len());
    for symbol in workspace_symbols {
        // Deliberately cheap: name, kind, container only — no line index, no
        // range math. The client asks for the location via
        // `workspaceSymbol/resolve` on the one row it navigates to.
        let uri = state.file_id_to_url(symbol.file)?;
        out.push(symbols::workspace_symbol(uri, &symbol));
    }
    Ok(Some(out.into()))
}

/// `workspaceSymbol/resolve`: the client navigation re-ask. Computes the one
/// thing the picker row omitted — the declaration range — for the single
/// `(file, item)` in the row's `data`.
pub fn on_workspace_symbol_resolve(
    state: GlobalStateSnapshot,
    params: WorkspaceSymbol,
) -> anyhow::Result<WorkspaceSymbol> {
    tracing::info!("request workspace symbol resolve");

    let data: symbols::WorkspaceSymbolData = params
        .data
        .clone()
        .ok_or_else(|| anyhow::anyhow!("workspace symbol resolve missing data"))
        .and_then(|data| serde_json::from_value(data).map_err(Into::into))?;

    let Some(range) = state
        .analysis
        .source_symbol_range(FileId::from_raw(data.file_id), data.item)?
    else {
        // The file was rewritten or deleted since the row was served; hand
        // the symbol back unchanged rather than fail navigation.
        return Ok(params);
    };
    let file_id = FileId::from_raw(data.file_id);
    let uri = state.file_id_to_url(file_id)?;
    let line_index = state.file_line_index(file_id)?;
    Ok(symbols::resolve_workspace_symbol(
        params,
        Location {
            uri,
            range: to_proto::range(&line_index, range),
        },
    ))
}

/// The declaration(s) a reference at a position resolves to, as LSP
/// locations ([JLS §6.5]).
///
/// A reference that resolves into a library whose declaring file is not loaded
/// yet defers: the handler returns [`DeferForLibraryFiles`], the main loop
/// materializes (and, for a library without sources, decompiles) the files and
/// re-runs the request, and the retried call — now with the files loaded —
/// returns the real location.
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
        let files = state.analysis.pending_library_files(file_id, offset)?;
        if !files.is_empty() {
            return Err(DeferForLibraryFiles(files).into());
        }
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

/// The hover at a position: the merged signature of a library member, the type
/// of the expression or the signature of the declaration there.
///
/// A library member whose declaring source is not loaded yet defers, exactly
/// like [`on_goto_definition`]: the source carries the parameter names the
/// merged signature needs, so answering before it is loaded would show the
/// bytecode-only rendering instead. A class a *decompiler* would have to
/// produce is deliberately not deferred to — see [`LibraryFileRef::Decompile`] —
/// so only source refs are handed to the main loop here.
pub fn on_hover(state: GlobalStateSnapshot, params: HoverParams) -> anyhow::Result<Option<Hover>> {
    let pos = params.text_document_position_params;
    tracing::info!(uri = ?pos.text_document.uri, "request hover");

    let Some(file_id) = state.url_to_file_id(&pos.text_document.uri)? else {
        return Ok(None);
    };
    let line_index = state.file_line_index(file_id)?;
    let offset = crate::lsp::from_proto::offset(&line_index, pos.position)?;
    let info = state.analysis.hover(file_id, offset)?;
    let Some(info) = info else {
        let files: Vec<LibraryFileRef> = state
            .analysis
            .pending_library_files(file_id, offset)?
            .into_iter()
            .filter(|file| matches!(file, LibraryFileRef::Source { .. }))
            .collect();
        if !files.is_empty() {
            return Err(DeferForLibraryFiles(files).into());
        }
        return Ok(None);
    };
    Ok(Some(Hover {
        contents: Contents::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```java\n{}\n```", info.value),
        }),
        range: None,
    }))
}

/// `caffeine_ls/libraryFileContent`: the content provider a client reads a
/// library view through, instead of reaching into the server's cache directory
/// (whose path it does not know and whose layout is the server's business).
pub struct LibraryFileContent;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryFileContentParams {
    /// A `caffeine-ls://` URI the server handed out in a definition, hover or
    /// symbol result.
    pub uri: Uri,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryFileContentResult {
    /// The file's text. A library view is always Java source — materialized
    /// from an archive or produced by a decompiler.
    pub content: String,
}

impl Request for LibraryFileContent {
    type Params = LibraryFileContentParams;
    type Result = LibraryFileContentResult;
    const METHOD: LspRequestMethod<'static> =
        LspRequestMethod::Custom("caffeine_ls/libraryFileContent");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Reads a library view out of the cache. The URI is client input, so it is
/// validated like any other: a scheme, view or library this server does not
/// serve, or a `..` on the way out of the cache, is an error. A missing file is
/// one too — the view was pruned, and the client shows that as a document
/// saying so.
pub fn on_library_file_content(
    state: GlobalStateSnapshot,
    params: LibraryFileContentParams,
) -> anyhow::Result<LibraryFileContentResult> {
    tracing::info!(uri = ?params.uri, "request library file content");

    let Some(scheme) = state.config.library_uri_scheme() else {
        anyhow::bail!("no library view scheme is configured");
    };
    let Some(path) = crate::library_view::view_path(
        &state.config.get_cache_dir(),
        scheme,
        &params.uri,
        crate::decompiler::is_backend,
    ) else {
        anyhow::bail!("{} is not a library view this server serves", params.uri);
    };

    let target: &std::path::Path = path.as_ref();
    let content = std::fs::read_to_string(target)
        .map_err(|err| anyhow::anyhow!("failed to read {path}: {err}"))?;
    Ok(LibraryFileContentResult { content })
}
