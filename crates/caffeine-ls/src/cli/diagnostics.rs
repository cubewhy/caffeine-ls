//! The `caffeine-ls diagnostics` subcommand: analyzes a repository headlessly
//! by driving the real server lifecycle over an in-memory LSP connection.

use std::path::{Path, PathBuf};

use anyhow::Context;
use lsp_types::{
    DocumentDiagnosticParams, DocumentDiagnosticReport, PartialResultParams,
    TextDocumentIdentifier, Uri, WorkDoneProgressParams,
};
use vfs::AbsPathBuf;

use crate::{
    cli::{headless::HeadlessServer, report},
    flags::{BuildSystemChoice, DiagnosticsArgs, OutputFormat},
};

/// Outcome of pulling diagnostics for a single file.
enum PullOutcome {
    Reported(Vec<lsp_types::Diagnostic>),
    /// The server does not know the file: outside any loaded source root or
    /// filtered by gitignore. Expected for candidate files that live outside
    /// the build system's source sets.
    Skipped,
}

pub fn run(args: &DiagnosticsArgs) -> anyhow::Result<i32> {
    let root = resolve_root(&args.path)?;
    let select_build_system = resolve_build_system(root.as_ref(), args.build_system)?;

    let files = report::discover_files(root.as_ref());
    tracing::debug!(count = files.len(), "discovered candidate source files");

    let server = HeadlessServer::start(
        &root,
        select_build_system.map(str::to_string),
        args.java_home.as_deref(),
    )
    .context("failed to start headless language server")?;

    let analysis = analyze(&server, root.as_ref(), &files, args);
    let shutdown = server.shutdown();

    let report = analysis?;
    shutdown.context("headless language server failed")?;

    if report.files_analyzed == 0 {
        anyhow::bail!(
            "no source files were analyzed under {} (nothing matched the workspace source roots)",
            root.as_str()
        );
    }

    let rendered = match args.format {
        OutputFormat::Text => report::render_text(&report),
        OutputFormat::Json => report::render_json(&report)?,
    };

    match &args.output {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::write(path, rendered)
                .with_context(|| format!("failed to write report to {}", path.display()))?;
        }
        None => print!("{rendered}"),
    }

    Ok(if report.diagnostics.is_empty() {
        crate::cli::EXIT_CLEAN
    } else {
        crate::cli::EXIT_FINDINGS
    })
}

fn resolve_root(path: &Option<PathBuf>) -> anyhow::Result<AbsPathBuf> {
    let raw = match path {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("failed to determine the current directory")?,
    };
    let canonical = raw
        .canonicalize()
        .with_context(|| format!("workspace path {} does not exist", raw.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );

    let canonical = crate::cli::serve::patch_path_prefix(canonical);
    let utf8 = camino::Utf8PathBuf::from_path_buf(canonical)
        .map_err(|p| anyhow::format_err!("non-UTF-8 workspace path {}", p.display()))?;
    AbsPathBuf::try_from(utf8).map_err(|p| anyhow::format_err!("non-absolute path {p}"))
}

/// Probes the workspace layout upfront so ambiguity can be resolved (or
/// rejected) before the server starts — otherwise the server would block on
/// a selection dialog no human is there to answer.
///
/// Returns the action title to auto-answer selection dialogs with, if one
/// was configured.
fn resolve_build_system(
    root: &Path,
    choice: Option<BuildSystemChoice>,
) -> anyhow::Result<Option<&'static str>> {
    use project_model::ProbeResult;

    match project_model::probe_workspace_layout(root) {
        ProbeResult::None => {
            if let Some(choice) = choice {
                tracing::warn!(
                    "no build system detected, ignoring --build-system {}",
                    choice.action_title()
                );
            }
            Ok(None)
        }
        ProbeResult::Single(system) => {
            if let Some(choice) = choice
                && choice.action_title() != system.name()
            {
                tracing::warn!(
                    "--build-system {} ignored: workspace uses {}",
                    choice.action_title(),
                    system.name()
                );
            }
            Ok(None)
        }
        ProbeResult::Ambiguous(systems) => {
            let names: Vec<&str> = systems.iter().map(|s| s.name()).collect();
            match choice.filter(|c| names.contains(&c.action_title())) {
                Some(choice) => Ok(Some(choice.action_title())),
                None => anyhow::bail!(
                    "multiple build systems detected at {}: {}. \
                     Re-run with --build-system <{}>",
                    root.display(),
                    names.join(", "),
                    ["gradle", "maven", "eclipse", "idea"].join("|")
                ),
            }
        }
    }
}

fn analyze(
    server: &HeadlessServer,
    root: &Path,
    files: &[PathBuf],
    args: &DiagnosticsArgs,
) -> anyhow::Result<report::DiagnosticReport> {
    server.wait_workspace_ready()?;
    check_server_errors(server)?;

    let max_rank = args.min_severity.max_rank();
    let mut report = report::DiagnosticReport::default();

    for file in files {
        match pull_document(server, file)? {
            PullOutcome::Skipped => {
                report.files_skipped += 1;
            }
            PullOutcome::Reported(diagnostics) => {
                report.files_analyzed += 1;
                let display = report::display_path(root, file);
                report.diagnostics.extend(report::collect_entries(
                    &display,
                    file,
                    &diagnostics,
                    max_rank,
                ));
            }
        }
    }

    check_server_errors(server)?;
    Ok(report)
}

fn pull_document(server: &HeadlessServer, file: &Path) -> anyhow::Result<PullOutcome> {
    let uri: Uri = Uri::from_file_path(file)
        .map_err(|_| anyhow::format_err!("failed to build URI for {}", file.display()))?;
    let params = serde_json::to_value(DocumentDiagnosticParams {
        text_document: TextDocumentIdentifier { uri },
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: PartialResultParams {
            partial_result_token: None,
        },
    })?;

    let mut last_error = String::new();
    for attempt in 0..crate::cli::PULL_RETRIES {
        let response = server.request("textDocument/diagnostic", params.clone())?;

        if let Some(err) = response.error {
            // A file outside the loaded source roots is not an error for the
            // overall run; it is simply not part of the analysis.
            if err.message.contains("vfs path") || err.message.contains("file not found") {
                return Ok(PullOutcome::Skipped);
            }
            last_error = err.message;
            tracing::debug!(
                file = %file.display(),
                attempt,
                "diagnostics pull failed, retrying: {last_error}"
            );
            continue;
        }

        let Some(result) = response.result else {
            anyhow::bail!("empty diagnostic report for {}", file.display());
        };
        let report: DocumentDiagnosticReport = serde_json::from_value(result)
            .map_err(|e| anyhow::format_err!("invalid diagnostic report for {file:?}: {e}"))?;

        let items = match report {
            DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(full) => {
                full.full_document_diagnostic_report.items
            }
            DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_) => Vec::new(),
        };
        return Ok(PullOutcome::Reported(items));
    }

    anyhow::bail!(
        "failed to pull diagnostics for {}: {last_error}",
        file.display()
    )
}

/// Surfaces messages the server sent via `window/showMessage` with severity
/// error (broken JDK setup, failed build-system sync, ...) as tool failures.
fn check_server_errors(server: &HeadlessServer) -> anyhow::Result<()> {
    let errors = server.take_error_messages();
    if errors.is_empty() {
        return Ok(());
    }
    anyhow::bail!("{}", errors.join("\n"))
}
