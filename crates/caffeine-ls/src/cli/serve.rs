use std::{env, path::PathBuf};

use camino::Utf8PathBuf;
use lsp_server::Connection;
use lsp_types::{
    Notification, ShowMessageNotification, WorkspaceFolders, WorkspaceFoldersInitializeParams,
};
use vfs::AbsPathBuf;

use crate::{config::Config, config::ConfigChange, config::ConfigErrors, from_json};

/// Performs the LSP initialize handshake on `connection`, builds the
/// [`Config`] from the client's `InitializeParams` and runs the main loop
/// until the client shuts the server down.
///
/// Shared by the stdio binary and in-memory drivers (tests and the headless
/// `diagnostics` subcommand), so every consumer exercises the same lifecycle.
pub fn run(connection: Connection) -> anyhow::Result<()> {
    let (initialize_id, initialize_params) = connection.initialize_start()?;

    tracing::info!("InitializeParams: {}", initialize_params);
    #[allow(deprecated)]
    let lsp_types::InitializeParams {
        root_uri,
        capabilities,
        workspace_folders_initialize_params: WorkspaceFoldersInitializeParams { workspace_folders },
        initialization_options,
        client_info,
        ..
    } = from_json::<lsp_types::InitializeParams>("InitializeParams", &initialize_params)?;

    let root_path = match root_uri
        .and_then(|it| it.to_file_path().ok())
        .map(patch_path_prefix)
        .and_then(|it| Utf8PathBuf::from_path_buf(it).ok())
        .and_then(|it| AbsPathBuf::try_from(it).ok())
    {
        Some(it) => it,
        None => {
            let cwd = env::current_dir()?;
            AbsPathBuf::assert_utf8(cwd)
        }
    };

    if let Some(client_info) = &client_info {
        tracing::info!(
            "Client '{}' {}",
            client_info.name,
            client_info.version.as_deref().unwrap_or_default()
        );
    }

    let workspace_folders = match workspace_folders {
        Some(WorkspaceFolders::WorkspaceFolderList(folders)) => Some(folders),
        _ => None,
    };

    let workspace_roots = workspace_folders
        .map(|workspaces| {
            workspaces
                .into_iter()
                .filter_map(|it| it.uri.to_file_path().ok())
                .map(patch_path_prefix)
                .filter_map(|it| Utf8PathBuf::from_path_buf(it).ok())
                .filter_map(|it| AbsPathBuf::try_from(it).ok())
                .collect::<Vec<_>>()
        })
        .filter(|workspaces| !workspaces.is_empty())
        .unwrap_or_else(|| vec![root_path.clone()]);
    let mut config = Config::new(capabilities, workspace_roots, client_info, None);
    if let Some(json) = initialization_options {
        let mut change = ConfigChange::default();
        change.change_client_config(json);

        let error_sink: ConfigErrors;
        (config, error_sink, _) = config.apply_change(change);

        if !error_sink.is_empty() {
            use lsp_types::{MessageType, ShowMessageParams};
            let not = lsp_server::Notification::new(
                ShowMessageNotification::METHOD.to_string(),
                ShowMessageParams {
                    kind: MessageType::Warning,
                    message: error_sink.to_string(),
                },
            );
            connection
                .sender
                .send(lsp_server::Message::Notification(not))
                .unwrap();
        }
    }

    let server_capabilities = crate::server_capabilities(&config);

    let initialize_result = lsp_types::InitializeResult {
        capabilities: server_capabilities,
        server_info: Some(lsp_types::ServerInfo {
            name: crate::NAME.to_string(),
            version: Some(crate::VERSION.to_string()),
        }),
    };

    let initialize_result = serde_json::to_value(initialize_result).unwrap();

    connection.initialize_finish(initialize_id, initialize_result)?;

    // A second call in the same process (e.g. an in-memory driver running in
    // the test binary alongside other suites) is a no-op error we can ignore.
    let _ = rayon::ThreadPoolBuilder::new()
        .thread_name(|ix| format!("RayonWorker{}", ix))
        .build_global();

    crate::main_loop(config, connection)?;

    tracing::info!("server did shut down");
    Ok(())
}

pub(crate) fn patch_path_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    if cfg!(windows) {
        // VSCode might report paths with the file drive in lowercase, but this can mess
        // with env vars set by tools and build scripts executed by r-a such that it invalidates
        // cargo's compilations unnecessarily. https://github.com/rust-lang/rust-analyzer/issues/14683
        // So we just uppercase the drive letter here unconditionally.
        // (doing it conditionally is a pain because std::path::Prefix always reports uppercase letters on windows)
        let mut comps = path.components();
        match comps.next() {
            Some(Component::Prefix(prefix)) => {
                let prefix = match prefix.kind() {
                    Prefix::Disk(d) => {
                        format!("{}:", d.to_ascii_uppercase() as char)
                    }
                    Prefix::VerbatimDisk(d) => {
                        format!(r"\\?\{}:", d.to_ascii_uppercase() as char)
                    }
                    _ => return path,
                };
                let mut path = PathBuf::new();
                path.push(prefix);
                path.extend(comps);
                path
            }
            _ => path,
        }
    } else {
        path
    }
}
