use std::collections::HashSet;
use triomphe::Arc;

use lsp_types::*;
use vfs::AbsPathBuf;

use crate::{
    GlobalState,
    global_state::BackgroundTaskEvent,
    lsp::{
        from_proto::{self, abs_path},
        utils::apply_document_changes,
    },
    mem_docs::DocumentData,
};

pub fn on_initialized(state: &mut GlobalState, _: InitializedParams) -> anyhow::Result<()> {
    // load workspaces
    state.trigger_workspace_probe();

    Ok(())
}

pub fn on_exit(state: &mut GlobalState, _: ()) -> anyhow::Result<()> {
    if !state.shutdown_requested {
        panic!("bad client! shutdown request not received.");
    }
    state.exit_requested = true;

    Ok(())
}

pub fn on_cancel(state: &mut GlobalState, params: CancelParams) -> anyhow::Result<()> {
    let id: lsp_server::RequestId = match params.id {
        lsp_types::Id::Int(n) => n.into(),
        lsp_types::Id::String(s) => s.into(),
    };

    state.cancel(id);

    Ok(())
}

pub fn on_did_open(
    state: &mut GlobalState,
    params: DidOpenTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didOpen {}", params.text_document.uri);

    if let Ok(path) = from_proto::vfs_path(&params.text_document.uri) {
        let already_exists = state
            .mem_docs
            .insert(
                path.clone(),
                DocumentData::new(
                    params.text_document.version,
                    params.text_document.text.clone().into_bytes(),
                ),
            )
            .is_err();
        if already_exists {
            tracing::error!("duplicate DidOpenTextDocument: {}", path);
        }

        let contents = params.text_document.text.into_bytes();
        state
            .vfs
            .write()
            .0
            .set_file_contents(path.clone(), Some(contents));
    }

    Ok(())
}

pub(crate) fn on_did_change(
    state: &mut GlobalState,
    params: DidChangeTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::debug!(
        "didChange {}",
        params.text_document.text_document_identifier.uri
    );

    if let Ok(path) = from_proto::vfs_path(&params.text_document.text_document_identifier.uri) {
        let Some(DocumentData { version, data }) = state.mem_docs.get_mut(&path) else {
            tracing::error!(?path, "unexpected DidChangeTextDocument");
            return Ok(());
        };
        // The version passed in DidChangeTextDocument is the version after all edits are applied
        // so we should apply it before the vfs is notified.
        *version = params.text_document.version;

        let new_contents = apply_document_changes(
            state.config.negotiated_encoding(),
            std::str::from_utf8(data).unwrap(),
            params.content_changes,
        )
        .into_bytes();
        if *data != new_contents {
            data.clone_from(&new_contents);
            state
                .vfs
                .write()
                .0
                .set_file_contents(path, Some(new_contents));
        }
    }

    Ok(())
}

pub fn on_did_save(
    _state: &mut GlobalState,
    params: DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didSave {}", params.text_document.uri);

    // NOTE: we sync file content with did_change notifications.

    Ok(())
}

pub fn on_did_close(
    state: &mut GlobalState,
    params: DidCloseTextDocumentParams,
) -> anyhow::Result<()> {
    tracing::info!("didClose {}", params.text_document.uri);
    if let Ok(path) = from_proto::vfs_path(&params.text_document.uri) {
        if state.mem_docs.remove(&path).is_err() {
            tracing::error!("orphan DidCloseTextDocument: {}", path);
        }

        if let Some(path) = path.as_path() {
            state.loader.handle.invalidate(path.to_path_buf());
        }
    }
    Ok(())
}

pub fn on_did_change_watched_files(
    state: &mut GlobalState,
    params: DidChangeWatchedFilesParams,
) -> anyhow::Result<()> {
    let mut roots_to_reload = HashSet::new();

    for event in params.changes {
        let Ok(abs_path) = abs_path(&event.uri) else {
            continue;
        };

        // An on-disk change to a build configuration requires re-syncing the
        // project layout with the build tool.
        if is_build_configuration_file(&abs_path)
            && let Some(root) = state
                .config
                .workspace_folders
                .iter()
                .find(|root| abs_path.starts_with(root))
        {
            roots_to_reload.insert(root.clone());
            continue;
        }

        // Source files are mirrored on disk by the client, so an editor-side
        // delete/create/edit must be re-read through the loader. This mirrors
        // rust-analyzer: every watched path is invalidated regardless of change
        // type; a deleted file re-reads as `None` and flows into
        // `Vfs::Change::Delete`, which drops the file from the source roots.
        if is_watched_source_file(&abs_path)
            || state
                .vfs
                .read()
                .0
                .file_id(&vfs::VfsPath::from(abs_path.clone()))
                .is_some()
        {
            state.loader.handle.invalidate(abs_path);
        }
    }

    for root in roots_to_reload {
        tracing::info!(
            ?root,
            "Build configuration changed, re-triggering workspace probe"
        );

        state
            .task_sender
            .send(BackgroundTaskEvent::ProbeWorkspace { root })
            .ok();
    }

    Ok(())
}

fn is_build_configuration_file(path: &AbsPathBuf) -> bool {
    if let Some(file_name) = path.file_name() {
        matches!(
            file_name,
            "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
                | "pom.xml"
        )
    } else {
        false
    }
}

/// The source extensions tracked as live workspace files. Changes to these are
/// re-read through the vfs loader so the database reflects on-disk deletion,
/// recreation and modification coming from the client.
fn is_watched_source_file(path: &AbsPathBuf) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "java" || ext == "kt" || ext == "kts")
}

pub fn on_did_change_configuration(
    state: &mut GlobalState,
    params: DidChangeConfigurationParams,
) -> anyhow::Result<()> {
    tracing::info!("Processing didChangeConfiguration notification");

    let mut full_settings = params.settings;

    tracing::debug!(?full_settings, "updated config");

    let extracted_config = match full_settings.get_mut("caffeine_ls") {
        Some(value) if !value.is_null() => value.take(),
        _ => {
            tracing::info!("Section key not found or null. Falling back to flat topology parsing");
            full_settings
        }
    };

    let mut change = crate::config::ConfigChange::default();
    change.change_client_config(extracted_config);

    let old_config = Arc::clone(&state.config);
    let current_config = (*old_config).clone();

    let (new_config, errors, config_changed) = current_config.apply_change(change);

    if !errors.is_empty() {
        state.show_message(lsp_types::MessageType::Warning, errors.to_string());
        state.config_errors = Some(errors);
    } else {
        state.config_errors = None;
    }

    if config_changed {
        let old_java_home = old_config.get_java_home();
        let new_java_home = new_config.get_java_home();

        state.config = Arc::new(new_config);

        if old_java_home != new_java_home {
            tracing::info!("Critical configuration updated. Re-probing project models.");
            state.trigger_workspace_probe();
        }
    }

    Ok(())
}
