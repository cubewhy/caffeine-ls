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

/// The reference sites of the declaration(s) at a position, as LSP locations.
///
/// A query that resolves into a library whose declaring source is not loaded
/// yet defers exactly like [`on_goto_definition`]: the main loop materializes
/// (and, for a library without sources, decompiles) the declaring file and
/// re-runs the request. Only then does the workspace sweep — whose sites
/// resolve through the loaded declaration — compare against the query's.
pub fn on_references(
    state: GlobalStateSnapshot,
    params: ReferenceParams,
) -> anyhow::Result<Option<Vec<Location>>> {
    let pos = params.text_document_position_params;
    tracing::info!(uri = ?pos.text_document.uri, "request references");
    // A whole-workspace sweep runs for seconds on a cold database: abort on
    // entry when the client has already cancelled, like the workspace
    // diagnostic pull (crate::diagnostics::check_cancelled).
    crate::diagnostics::check_cancelled(&state)?;

    let Some(file_id) = state.url_to_file_id(&pos.text_document.uri)? else {
        return Ok(None);
    };
    let line_index = state.file_line_index(file_id)?;
    let offset = crate::lsp::from_proto::offset(&line_index, pos.position)?;
    let references =
        state
            .analysis
            .references(file_id, offset, params.context.include_declaration)?;
    if references.is_empty() {
        let files = state.analysis.pending_library_files(file_id, offset)?;
        if !files.is_empty() {
            return Err(DeferForLibraryFiles(files).into());
        }
        return Ok(None);
    }

    let mut locations = Vec::with_capacity(references.len());
    for reference in references {
        let uri = state.file_id_to_url(reference.file)?;
        let line_index = state.file_line_index(reference.file)?;
        locations.push(Location {
            uri,
            range: to_proto::range(&line_index, reference.range),
        });
    }
    Ok(Some(locations))
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
    // The signature in a Java fence, the declaration's documentation behind it
    // as Markdown. Only Java produces a hover today, and the Kotlin arm
    // answers `None`, so the fence is unconditional.
    let mut value = format!("```java\n{}\n```", info.value);
    if let Some(docs) = info.docs {
        value.push_str("\n\n");
        value.push_str(&docs);
    }
    Ok(Some(Hover {
        contents: Contents::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }))
}

/// `textDocument/semanticTokens/full`: the file's semantic tokens — the
/// resolved classification of every identifier plus the lexical layer
/// (keywords, modifiers, literals, operators, comments). The result carries a
/// `result_id` the client sends back in a `full/delta` request.
pub fn on_semantic_tokens(
    state: GlobalStateSnapshot,
    params: SemanticTokensParams,
) -> anyhow::Result<Option<SemanticTokens>> {
    tracing::info!(uri = ?params.text_document.uri, "request semantic tokens");
    let Some((file_id, tokens)) = encode_tokens(&state, &params.text_document.uri, None)? else {
        return Ok(None);
    };
    let result_id = state
        .semantic_tokens()
        .write()
        .store(file_id, tokens.clone());
    Ok(Some(SemanticTokens {
        result_id: Some(result_id),
        data: tokens,
    }))
}

/// `textDocument/inlayHint`: the hints the requested range contains — the
/// inferred types, parameter names and chain types of `ide`'s hint model, in
/// the client's own `InlayHint` shape. A resolve is asked for separately, so
/// this answer carries only what a client renders immediately.
pub fn on_inlay_hint(
    state: GlobalStateSnapshot,
    params: InlayHintParams,
) -> anyhow::Result<Option<Vec<InlayHint>>> {
    tracing::info!(uri = ?params.text_document.uri, "request inlay hints");

    // A file this server does not know answers nothing rather than failing the
    // request, like every other document request.
    let Some(file_id) = state.url_to_file_id(&params.text_document.uri)? else {
        return Ok(None);
    };
    let line_index = state.file_line_index(file_id)?;
    let range = crate::lsp::from_proto::text_range(&line_index, params.range)?;
    let config = state.config.inlay_hints();
    let hints = state.analysis.inlay_hints(file_id, range, &config)?;
    let hints: Vec<InlayHint> = hints
        .iter()
        .map(|hint| crate::lsp::inlay_hints::to_proto(hint, file_id, &line_index))
        .collect();
    tracing::debug!(hints = hints.len(), "computed inlay hints");
    Ok(Some(hints))
}

/// `inlayHint/resolve`: what the first answer left out — the tooltip, the
/// declaration behind each label part that rendered a class name, and the edits
/// accepting the hint applies.
///
/// The hint is recomputed from the request's `data` rather than carried along,
/// so an answer can never describe a hint the document no longer has: when the
/// hint is gone the request is handed back unchanged.
pub fn on_inlay_hint_resolve(
    state: GlobalStateSnapshot,
    mut params: InlayHint,
) -> anyhow::Result<InlayHint> {
    let data: crate::lsp::inlay_hints::InlayHintData = params
        .data
        .clone()
        .ok_or_else(|| anyhow::anyhow!("inlay hint resolve missing data"))
        .and_then(|data| serde_json::from_value(data).map_err(Into::into))?;
    tracing::info!(?data, "resolve inlay hint");

    let file_id = FileId::from_raw(data.file_id);
    let config = state.config.inlay_hints();
    let Some(detail) = state.analysis.inlay_hint_resolve(
        file_id,
        rowan::TextSize::from(data.offset),
        data.kind.into(),
        &config,
    )?
    else {
        // The document changed under the hint the client holds.
        return Ok(params);
    };
    tracing::debug!(offset = ?detail.hint.offset, kind = ?detail.hint.kind, "resolved inlay hint");
    let line_index = state.file_line_index(file_id)?;
    let mut resolved =
        crate::lsp::inlay_hints::resolve_to_proto(&state, &detail, file_id, &line_index)?;
    // The handle is preserved: it is what a second resolve would name the hint
    // by, and the client echoes it back verbatim.
    resolved.data = params.data.take();
    Ok(resolved)
}

/// `textDocument/semanticTokens/full/delta`: the edit that turns the stream the
/// client holds into the current one, so an edit costs the client one small
/// token edit instead of a full re-tokenization of the document.
///
/// The client names the stream with the `result_id` of the last response it
/// received for the document; a request naming a stream this server did not
/// send — a restarted server, a dropped answer — is answered in full
/// ([`SemanticTokensDeltaResponse`] carries either shape).
pub fn on_semantic_tokens_delta(
    state: GlobalStateSnapshot,
    params: SemanticTokensDeltaParams,
) -> anyhow::Result<Option<SemanticTokensDeltaResponse>> {
    tracing::info!(uri = ?params.text_document.uri, "request semantic tokens delta");

    let uri = &params.text_document.uri;
    let Some((file_id, tokens)) = encode_tokens(&state, uri, None)? else {
        return Ok(None);
    };
    let mut cache = state.semantic_tokens().write();
    let Some(previous) = cache.previous(file_id, &params.previous_result_id) else {
        // Nothing to diff against: answer in full, which the client treats as a
        // fresh stream (`result_id` included).
        let result_id = cache.store(file_id, tokens.clone());
        return Ok(Some(
            SemanticTokens {
                result_id: Some(result_id),
                data: tokens,
            }
            .into(),
        ));
    };
    let edits = crate::lsp::semantic_tokens::delta_edits(previous, &tokens);
    let result_id = cache.store(file_id, tokens);
    Ok(Some(
        SemanticTokensDelta {
            result_id: Some(result_id),
            edits,
        }
        .into(),
    ))
}

/// `textDocument/semanticTokens/range`: the tokens of one requested range. The
/// file's tokens are computed whole and filtered to the highlights the range
/// *fully* contains, so the client's view of a window is exactly the window's
/// slice of the full answer. A range's tokens are a view of the document, not a
/// stream of it, so it carries no `result_id` and does not touch the delta
/// cache.
pub fn on_semantic_tokens_range(
    state: GlobalStateSnapshot,
    params: SemanticTokensRangeParams,
) -> anyhow::Result<Option<SemanticTokens>> {
    tracing::info!(uri = ?params.text_document.uri, "request semantic tokens");
    let Some((_, tokens)) = encode_tokens(&state, &params.text_document.uri, Some(params.range))?
    else {
        return Ok(None);
    };
    Ok(Some(SemanticTokens {
        result_id: None,
        data: tokens,
    }))
}

/// Encodes the tokens of `uri` — the whole document, or the highlights `range`
/// fully contains — with the file they belong to (the delta cache's key).
fn encode_tokens(
    state: &GlobalStateSnapshot,
    uri: &Uri,
    range: Option<Range>,
) -> anyhow::Result<Option<(FileId, Vec<SemanticToken>)>> {
    // The file may have been deleted since the request was queued; a client
    // showing the old buffer gets no tokens rather than an error.
    let Some(file_id) = state.url_to_file_id(uri)? else {
        return Ok(None);
    };
    let line_index = state.file_line_index(file_id)?;
    let range = range
        .map(|range| crate::lsp::from_proto::text_range(&line_index, range))
        .transpose()?;
    let highlights = state
        .analysis
        .highlight(file_id)?
        .into_iter()
        .filter(|highlight| range.is_none_or(|range| range.contains_range(highlight.range)))
        .collect();
    Ok(Some((
        file_id,
        crate::lsp::semantic_tokens::highlights_to_tokens(highlights, &line_index),
    )))
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
