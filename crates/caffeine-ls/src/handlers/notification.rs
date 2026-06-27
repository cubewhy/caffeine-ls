use std::process;

use line_index::{LineIndex, WideEncoding, WideLineCol};
use lsp_types::*;
use vfs::VfsPath;

use crate::GlobalState;

pub fn on_initialized(state: &mut GlobalState, _: InitializedParams) -> anyhow::Result<()> {
    tracing::info!("lsp initialized");
    Ok(())
}

pub fn on_exit(state: &mut GlobalState, _: ()) -> anyhow::Result<()> {
    if state.shutdown_requested {
        process::exit(0);
    } else {
        process::exit(1);
    }
}

pub fn on_cancel(state: &mut GlobalState, params: CancelParams) -> anyhow::Result<()> {
    let id: lsp_server::RequestId = match params.id {
        lsp_types::NumberOrString::Number(n) => n.into(),
        lsp_types::NumberOrString::String(s) => s.into(),
    };

    state.cancel(id);

    Ok(())
}

pub fn on_did_open(
    state: &mut GlobalState,
    params: DidOpenTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didOpen {}", params.text_document.uri);
    let text = params.text_document.text;
    let content = text.clone().into_bytes();

    let vfs_uri = VfsPath::from(&params.text_document.uri);
    state.vfs.write().set_file_contents(vfs_uri, Some(content));
    state.handle_vfs_change();

    Ok(())
}

pub fn on_did_change(
    state: &mut GlobalState,
    params: DidChangeTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::debug!("didChange {}", params.text_document.uri);

    let path = VfsPath::from(&params.text_document.uri);

    let mut text = {
        let vfs = state.vfs.read();
        let Some(file_id) = vfs.file_id(&path) else {
            anyhow::bail!("Internal error");
        };

        let content = match vfs.fetch_content(file_id) {
            Ok(content) => content,
            Err(err) => anyhow::bail!("Failed to get file content: {err:#}"),
        };
        String::from_utf8_lossy(&content).to_string()
    };

    // Apply edits
    for edit in params.content_changes {
        if let Some(range) = edit.range {
            let line_index = LineIndex::new(&text);

            let start_wide = WideLineCol {
                line: range.start.line,
                col: range.start.character,
            };
            let start_line_col = line_index.to_utf8(WideEncoding::Utf16, start_wide).unwrap();
            let start_offset = line_index.offset(start_line_col).unwrap();

            let end_wide = WideLineCol {
                line: range.end.line,
                col: range.end.character,
            };
            let end_line_col = line_index.to_utf8(WideEncoding::Utf16, end_wide).unwrap();
            let end_offset = line_index.offset(end_line_col).unwrap();

            let start = (u32::from(start_offset) as usize).min(text.len());
            let end = (u32::from(end_offset) as usize).max(start).min(text.len());

            text.replace_range(start..end, &edit.text);
        } else {
            text = edit.text; // Full edit
        }
    }

    state
        .vfs
        .write()
        .set_file_contents(path, Some(text.into_bytes()));

    state.handle_vfs_change();

    Ok(())
}

pub fn on_did_save(
    state: &mut GlobalState,
    params: DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didSave {}", params.text_document.uri);

    if let Some(text) = params.text {
        let path = VfsPath::from(&params.text_document.uri);
        state
            .vfs
            .write()
            .set_file_contents(path, Some(text.into_bytes()));
        state.handle_vfs_change();
    }

    Ok(())
}

pub fn on_did_close(
    state: &mut GlobalState,
    params: DidCloseTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didClose {}", params.text_document.uri);
    let path = VfsPath::from(&params.text_document.uri);
    state.vfs.write().set_file_contents(path, None);
    state.handle_vfs_change();

    Ok(())
}
