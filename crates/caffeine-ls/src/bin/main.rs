use std::{env, fs::File, path::PathBuf};

use caffeine_ls::{
    config::{Config, ConfigChange, ConfigErrors},
    flags::Flags,
    from_json,
};
use camino::Utf8PathBuf;
use clap::Parser;
use lsp_server::Connection;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use vfs::AbsPathBuf;

const STACK_SIZE: usize = 1024 * 1024 * 8;

cfg_if::cfg_if! {
    if #[cfg(feature = "mimalloc")] {
        #[global_allocator]
        static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
    } else if #[cfg(all(feature = "jemalloc", not(target_env = "msvc")))] {
        #[global_allocator]
        static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;
    }
}

fn main() {
    let flags = Flags::parse();

    #[cfg(debug_assertions)]
    if flags.wait_dbg {
        wait_for_debugger();
    }

    setup_logging(flags.log_file).expect("Failed to setup logger");

    with_extra_thread("lsp-main", run_server).expect("An error occurred on the LSP server");
}

#[cfg(debug_assertions)]
fn wait_for_debugger() {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Diagnostics::Debug::IsDebuggerPresent;
        // SAFETY: WinAPI generated code that is defensively marked `unsafe` but
        // in practice can not be used in an unsafe way.
        while unsafe { IsDebuggerPresent() } == 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        #[allow(unused_mut)]
        let mut d = 4;
        while d == 4 {
            d = 4;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

fn setup_logging(log_file: Option<PathBuf>) -> anyhow::Result<()> {
    let file_layer = log_file.map(|path| {
        let file = File::create(path).expect("Failed to create log file");
        fmt::layer().with_writer(file).with_ansi(false)
    });

    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(false);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("CAFFEINE_LS_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stderr_layer)
        .with(file_layer)
        .try_init()?;

    Ok(())
}

fn with_extra_thread<F>(thread_name: impl Into<String>, f: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    std::thread::Builder::new()
        .name(thread_name.into())
        .stack_size(STACK_SIZE)
        .spawn(f)
        .expect("Failed to create thread")
        .join()
        .expect("thread panicked")
}

fn run_server() -> anyhow::Result<()> {
    tracing::info!("server version {} will start", caffeine_ls::VERSION);

    let (connection, io_threads) = Connection::stdio();

    let (initialize_id, initialize_params) = match connection.initialize_start() {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            return Err(e.into());
        }
    };

    tracing::info!("InitializeParams: {}", initialize_params);
    #[allow(deprecated)]
    let lsp_types::InitializeParams {
        root_uri,
        mut capabilities,
        workspace_folders,
        initialization_options,
        client_info,
        ..
    } = from_json::<lsp_types::InitializeParams>("InitializeParams", &initialize_params)?;

    // lsp-types has a typo in the `/capabilities/workspace/diagnostics` field, its typoed as `diagnostic`
    if let Some(val) = initialize_params.pointer("/capabilities/workspace/diagnostics")
        && let Ok(diag_caps) = from_json::<lsp_types::DiagnosticWorkspaceClientCapabilities>(
            "DiagnosticWorkspaceClientCapabilities",
            val,
        )
    {
        tracing::info!("Patching lsp-types workspace diagnostics capabilities: {diag_caps:#?}");
        capabilities
            .workspace
            .get_or_insert_default()
            .diagnostic
            .get_or_insert(diag_caps);
    }

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
            use lsp_types::{
                MessageType, ShowMessageParams,
                notification::{Notification, ShowMessage},
            };
            let not = lsp_server::Notification::new(
                ShowMessage::METHOD.to_owned(),
                ShowMessageParams {
                    typ: MessageType::WARNING,
                    message: error_sink.to_string(),
                },
            );
            connection
                .sender
                .send(lsp_server::Message::Notification(not))
                .unwrap();
        }
    }

    let server_capabilities = caffeine_ls::server_capabilities(&config);

    let initialize_result = lsp_types::InitializeResult {
        capabilities: server_capabilities,
        server_info: Some(lsp_types::ServerInfo {
            name: caffeine_ls::NAME.to_string(),
            version: Some(caffeine_ls::VERSION.to_string()),
        }),
        offset_encoding: None,
    };

    let initialize_result = serde_json::to_value(initialize_result).unwrap();

    if let Err(e) = connection.initialize_finish(initialize_id, initialize_result) {
        if e.channel_is_disconnected() {
            io_threads.join()?;
        }
        return Err(e.into());
    }

    rayon::ThreadPoolBuilder::new()
        .thread_name(|ix| format!("RayonWorker{}", ix))
        .build_global()
        .unwrap();

    // If the io_threads have an error, there's usually an error on the main
    // loop too because the channels are closed. Ensure we report both errors.
    match (
        caffeine_ls::main_loop(config, connection),
        io_threads.join(),
    ) {
        (Err(loop_e), Err(join_e)) => anyhow::bail!("{loop_e}\n{join_e}"),
        (Ok(_), Err(join_e)) => anyhow::bail!("{join_e}"),
        (Err(loop_e), Ok(_)) => anyhow::bail!("{loop_e}"),
        (Ok(_), Ok(_)) => {}
    }

    tracing::info!("server did shut down");
    Ok(())
}

fn patch_path_prefix(path: PathBuf) -> PathBuf {
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
