use std::{fs::File, path::PathBuf};

use caffeine_ls::flags::Flags;
use clap::Parser;
use tower_lsp::{LspService, Server};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

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

    with_extra_thread("lsp-main", async {
        run_server()
            .await
            .expect("An error occurred on the LSP server");
    })
    .expect("Failed to create runtime");
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

    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(true);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("CAFFEINE_LS_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stderr_layer)
        .with(file_layer)
        .try_init()?;

    Ok(())
}

fn with_extra_thread<F>(thread_name: impl Into<String>, f: F) -> anyhow::Result<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .name(thread_name.into())
        .thread_stack_size(STACK_SIZE)
        .build()?;

    Ok(rt.block_on(f))
}

async fn run_server() -> anyhow::Result<()> {
    tracing::info!("server version {} will start", caffeine_ls::VERSION);

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(caffeine_ls::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;

    tracing::info!("server did shut down");
    Ok(())
}
