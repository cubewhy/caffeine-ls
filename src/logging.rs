use tracing_subscriber::{EnvFilter, fmt, prelude::*};

pub(crate) fn init_logger() {
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .with(
            EnvFilter::try_from_env("JAVA_ANALYZER_LOG")
                .unwrap_or_else(|_| EnvFilter::new("debug")),
        )
        .init();
}
