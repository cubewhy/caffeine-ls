use std::{fs::File, path::PathBuf, process::ExitCode};

use caffeine_ls::{
    cli,
    cli::serve,
    flags::{Command, Flags},
};
use clap::Parser as _;
use lsp_server::Connection;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

fn main() -> ExitCode {
    let flags = Flags::parse();

    #[cfg(debug_assertions)]
    if flags.wait_dbg {
        wait_for_debugger();
    }

    // The headless subcommands report through stdout; keep stderr logging
    // quiet unless asked otherwise.
    let default_filter = match &flags.command {
        Some(Command::Diagnostics(_)) => "warn",
        _ => "info",
    };
    if let Err(err) = setup_logging(flags.log_file, default_filter) {
        eprintln!("Failed to setup logger: {err:?}");
        return ExitCode::from(cli::EXIT_TOOL_FAILURE as u8);
    }

    let result = match flags.command {
        Some(Command::Diagnostics(args)) => {
            with_extra_thread("caffeine-diagnostics", move || cli::run(&args))
        }
        None | Some(Command::Serve) => with_extra_thread("lsp-main", run_stdio_server),
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(cli::EXIT_TOOL_FAILURE as u8)
        }
    }
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

fn setup_logging(log_file: Option<PathBuf>, default_filter: &str) -> anyhow::Result<()> {
    let file_layer = log_file.map(|path| {
        let file = File::create(path).expect("Failed to create log file");
        tracing_subscriber::fmt::layer()
            .with_writer(file)
            .with_ansi(false)
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_env("CAFFEINE_LS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with(stderr_layer)
        .with(file_layer)
        .try_init()?;

    Ok(())
}

fn with_extra_thread<F, T>(thread_name: impl Into<String>, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(thread_name.into())
        .stack_size(STACK_SIZE)
        .spawn(f)
        .expect("Failed to create thread")
        .join()
        .expect("thread panicked")
}

fn run_stdio_server() -> anyhow::Result<i32> {
    tracing::info!("server version {} will start", caffeine_ls::VERSION);

    let (connection, io_threads) = Connection::stdio();

    // If the io_threads have an error, there's usually an error on the main
    // loop too because the channels are closed. Ensure we report both errors.
    let result = serve::run(connection);
    let join_result = io_threads.join();

    match (result, join_result) {
        (Ok(()), Ok(())) => Ok(0),
        (Err(loop_e), Ok(())) => Err(loop_e),
        (Ok(()), Err(join_e)) => Err(join_e.into()),
        (Err(loop_e), Err(join_e)) => anyhow::bail!("{loop_e}\n{join_e}"),
    }
}
