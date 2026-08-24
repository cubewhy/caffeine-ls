use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, clap::Parser)]
pub struct Flags {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, default_value = "false", global = true)]
    pub wait_dbg: bool,

    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the language server over stdio (default when no subcommand is given)
    Serve,

    /// Analyze a repository headlessly and report its diagnostics
    Diagnostics(DiagnosticsArgs),
}

#[derive(Debug, Args)]
pub struct DiagnosticsArgs {
    /// Repository directory to analyze (defaults to the current directory)
    pub path: Option<PathBuf>,

    /// Output format of the report
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Write the report to this file instead of stdout
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Minimum severity a diagnostic must have to be reported and counted for
    /// the exit code
    #[arg(long, value_enum, default_value_t = SeverityFilter::Error)]
    pub min_severity: SeverityFilter,

    /// Build system to pick when the workspace layout is ambiguous
    #[arg(long, value_enum)]
    pub build_system: Option<BuildSystemChoice>,

    /// JDK home directory used for workspace loading and library indexing
    #[arg(long)]
    pub java_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

/// Ordered by increasing inclusion: `warning` keeps errors and warnings,
/// `all` keeps everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum SeverityFilter {
    Error,
    Warning,
    All,
}

impl SeverityFilter {
    /// The highest [`SeverityRank`] that passes this filter.
    pub fn max_rank(self) -> u32 {
        match self {
            SeverityFilter::Error => SeverityRank::ERROR,
            SeverityFilter::Warning => SeverityRank::WARNING,
            SeverityFilter::All => SeverityRank::HINT,
        }
    }
}

pub(crate) struct SeverityRank;

impl SeverityRank {
    pub const ERROR: u32 = 1;
    pub const WARNING: u32 = 2;
    pub const INFORMATION: u32 = 3;
    pub const HINT: u32 = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BuildSystemChoice {
    Gradle,
    Maven,
    Eclipse,
    Idea,
}

impl BuildSystemChoice {
    /// The title the server uses for this build system in selection dialogs.
    pub fn action_title(self) -> &'static str {
        match self {
            BuildSystemChoice::Gradle => "Gradle",
            BuildSystemChoice::Maven => "Maven",
            BuildSystemChoice::Eclipse => "Eclipse Classpath",
            BuildSystemChoice::Idea => "Intellij IDEA",
        }
    }
}
