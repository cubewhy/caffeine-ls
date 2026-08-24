//! Headless subcommands for the `caffeine-ls` binary. The default (no
//! subcommand) path runs the stdio language server; see [`serve`].

pub(crate) mod diagnostics;
pub(crate) mod headless;
pub(crate) mod report;
pub mod serve;

pub use diagnostics::run;

/// Process exit code: analysis succeeded and no findings passed the filter.
pub const EXIT_CLEAN: i32 = 0;

/// Process exit code: at least one diagnostic passed the severity filter.
pub const EXIT_FINDINGS: i32 = 1;

/// Process exit code: the analysis itself failed (bad workspace, broken JDK
/// setup, timed-out load, internal error).
pub const EXIT_TOOL_FAILURE: i32 = 2;

/// Retries for a single diagnostics pull before giving up on the file.
pub(crate) const PULL_RETRIES: usize = 5;
