use std::{collections::HashSet, process, sync::Arc};

use line_index::{LineIndex, WideEncoding, WideLineCol};
use lsp_types::*;
use vfs::{AbsPathBuf, VfsPath};

use crate::{GlobalState, global_state::BackgroundTaskEvent};

pub fn on_initialized(state: &mut GlobalState, _: InitializedParams) -> anyhow::Result<()> {
    // load workspaces
    for workspace_root in state.config.workspace_folders.iter() {
        state.spawn_task(BackgroundTaskEvent::ProbeWorkspace {
            root: workspace_root.clone(),
        });
    }

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

pub fn on_did_change_watched_files(
    state: &mut GlobalState,
    params: DidChangeWatchedFilesParams,
) -> anyhow::Result<()> {
    let mut roots_to_reload = HashSet::new();

    for event in params.changes {
        let Ok(path) = event.uri.to_file_path() else {
            continue;
        };
        let Ok(abs_path) = AbsPathBuf::try_from(path) else {
            continue;
        };

        if is_build_configuration_file(&abs_path)
            && let Some(root) = state
                .config
                .workspace_folders
                .iter()
                .find(|root| abs_path.starts_with(root))
        {
            roots_to_reload.insert(root.clone());
        }
    }

    for root in roots_to_reload {
        tracing::info!(
            ?root,
            "Build configuration changed, re-triggering workspace probe"
        );

        state.spawn_task(BackgroundTaskEvent::ProbeWorkspace { root });
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

pub fn on_did_change_configuration(
    state: &mut GlobalState,
    params: DidChangeConfigurationParams,
) -> anyhow::Result<()> {
    tracing::info!("Processing didChangeConfiguration notification");

    let mut full_settings = params.settings;

    tracing::debug!(?full_settings, "updated config");

    let mut extracted_config = match full_settings.get_mut("caffeine_ls") {
        Some(value) if !value.is_null() => value.take(),
        _ => {
            tracing::info!("Section key not found or null. Falling back to flat topology parsing");
            full_settings
        }
    };

    let is_valid_payload =
        serde_json::from_value::<crate::config::ClientConfig>(extracted_config.clone()).is_ok();

    if !is_valid_payload {
        tracing::warn!("Push-based payload doesn't match ClientConfig schema.");

        if let Some(workspace_caps) = state.config.client_capabilities.workspace.as_ref()
            && workspace_caps.configuration.unwrap_or(false)
        {
            tracing::info!(
                "Client supports Pull Model. Dispatched dynamic configuration pull request"
            );
            let pull_params = lsp_types::ConfigurationParams {
                items: vec![lsp_types::ConfigurationItem {
                    scope_uri: None,
                    section: Some("caffeine-ls".to_string()),
                }],
            };
            state.send_request::<request::WorkspaceConfiguration>(
                pull_params,
                crate::global_state::OutgoingRequest::WorkspaceConfiguration,
            );
            return Ok(());
        }

        extracted_config = serde_json::Value::Object(serde_json::Map::new());
    }

    let mut change = crate::config::ConfigChange::default();
    change.change_client_config(extracted_config);

    let old_config = Arc::clone(&state.config);
    let current_config = (*old_config).clone();

    let (new_config, errors, config_changed) = current_config.apply_change(change);

    if !errors.is_empty() {
        state.show_message(lsp_types::MessageType::WARNING, errors.to_string());
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
