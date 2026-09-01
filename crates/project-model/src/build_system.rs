use std::{
    collections::VecDeque,
    fmt,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
};

use serde::Serialize;

use crate::{
    EclipseBuildSystem, GradleBuildSystem, IdeaBuildSystem, MavenBuildSystem,
    workspace::WorkspaceGraph,
};

/// Marker lines delimiting the structural workspace model JSON printed by the
/// build tool init scripts.
pub const WORKSPACE_MODEL_BEGIN: &str = "WORKSPACE_MODEL_BEGIN";
pub const WORKSPACE_MODEL_END: &str = "WORKSPACE_MODEL_END";

/// A structured, tool-agnostic progress event emitted during a build-system
/// sync. Unlike raw output lines, these carry semantics the LSP layer can turn
/// into a phase-budgeted percentage and the headless CLI can render live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncProgress {
    /// The build tool entered a coarse lifecycle phase.
    Phase(SyncPhase),
    /// A dependency download began/made progress. `bytes_total` is `None` when
    /// the tool does not report a size (older Maven/Gradle versions).
    Download {
        dependency: String,
        bytes_downloaded: u64,
        bytes_total: Option<u64>,
    },
    /// The build tool is working on one module of a multi-module build.
    Project {
        name: String,
        index: u32,
        total: u32,
        action: String,
    },
    /// Free-form status text without structured meaning.
    Info(String),
}

/// Coarse lifecycle phases a build-system sync moves through. Kept in a fixed
/// order so a consumer can map them onto bounded percentage ranges.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SyncPhase {
    Resolving,
    Downloading,
    Configuring,
    Compiling,
    Exporting,
    Done,
    Failed,
}

impl SyncPhase {
    pub fn label(self) -> &'static str {
        match self {
            SyncPhase::Resolving => "Resolving dependencies",
            SyncPhase::Downloading => "Downloading dependencies",
            SyncPhase::Configuring => "Configuring projects",
            SyncPhase::Compiling => "Compiling projects",
            SyncPhase::Exporting => "Exporting workspace model",
            SyncPhase::Done => "Sync complete",
            SyncPhase::Failed => "Sync failed",
        }
    }
}

/// Maximum number of tail lines kept in memory for error messages. The full
/// output is streamed to a log file instead.
const TAIL_MAX_LINES: usize = 20;

/// Hard cap on the in-memory model JSON size. Realistic workspace models are
/// far smaller; anything larger is treated as a failure rather than risking
/// unbounded memory usage.
const MODEL_JSON_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Copy, Clone, Serialize)]
pub enum BuildSystemType {
    Gradle,
    Maven,
    Eclipse,
    Idea,
}

impl BuildSystemType {
    pub fn name(&self) -> &'static str {
        match self {
            BuildSystemType::Gradle => "Gradle",
            BuildSystemType::Maven => "Maven",
            BuildSystemType::Eclipse => "Eclipse Classpath",
            BuildSystemType::Idea => "Intellij IDEA",
        }
    }

    pub fn get_executor(&self) -> Box<dyn BuildSystem> {
        match self {
            BuildSystemType::Gradle => Box::new(GradleBuildSystem),
            BuildSystemType::Maven => Box::new(MavenBuildSystem),
            BuildSystemType::Eclipse => Box::new(EclipseBuildSystem),
            BuildSystemType::Idea => Box::new(IdeaBuildSystem),
        }
    }
}

/// The result of streaming a build tool to completion. No unbounded output is
/// retained: lines are written straight to the log file, and only a bounded
/// tail plus the model JSON are kept in memory.
pub struct CommandOutcome {
    pub status: ExitStatus,
    /// Bounded tail of the merged stdout/stderr output, for error messages.
    pub tail: String,
    /// The workspace model JSON captured between the `WORKSPACE_MODEL_BEGIN`
    /// and `WORKSPACE_MODEL_END` markers (stdout only), if found.
    pub model_json: Option<String>,
    /// Whether the model JSON exceeded [`MODEL_JSON_MAX_BYTES`] and was
    /// truncated; callers should treat this as a sync failure.
    pub model_truncated: bool,
}

/// Runs `command` with stdout/stderr piped, streaming each merged line to
/// `log_file` and invoking `on_output` for every line as it arrives (serially,
/// from a single thread).
pub fn run_command_streaming(
    command: &mut Command,
    log_file: Option<&Path>,
    on_output: &mut (dyn FnMut(String) + Send),
) -> anyhow::Result<CommandOutcome> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout was piped for streaming");
    let stderr = child.stderr.take().expect("stderr was piped for streaming");

    let (stdout_tx, stdout_rx) = crossbeam_channel::bounded::<String>(128);
    let (stderr_tx, stderr_rx) = crossbeam_channel::bounded::<String>(128);

    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = line.unwrap_or_default();
            if stdout_tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let line = line.unwrap_or_default();
            if stderr_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut writer: Option<std::io::BufWriter<File>> = match log_file {
        Some(log_file) => {
            if let Some(parent) = log_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Some(std::io::BufWriter::new(File::create(log_file)?))
        }
        None => None,
    };

    let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_MAX_LINES);
    let mut model_json: Option<String> = None;
    let mut capturing_model = false;
    let mut model_truncated = false;

    let mut stdout_done = false;
    let mut stderr_done = false;

    while !(stdout_done && stderr_done) {
        crossbeam_channel::select! {
            recv(stdout_rx) -> line => match line {
                Ok(line) => {
                    if let Some(writer) = writer.as_mut() {
                        writer.write_all(line.as_bytes())?;
                        writer.write_all(b"\n")?;
                    }
                    tail.push_back(line.clone());
                    if tail.len() > TAIL_MAX_LINES {
                        tail.pop_front();
                    }
                    on_output(line.clone());

                    // Model markers are printed on stdout only. The marker
                    // lines themselves are excluded from the captured JSON.
                    if capturing_model {
                        if line.contains(WORKSPACE_MODEL_END) {
                            capturing_model = false;
                        } else if !model_truncated {
                            let buf = model_json
                                .as_mut()
                                .expect("model capture buffer exists while capturing");
                            buf.push_str(&line);
                            buf.push('\n');
                            if buf.len() > MODEL_JSON_MAX_BYTES {
                                model_truncated = true;
                            }
                        }
                    } else if line.contains(WORKSPACE_MODEL_BEGIN) {
                        capturing_model = true;
                        model_json = Some(String::new());
                    }
                }
                Err(_) => stdout_done = true,
            },
            recv(stderr_rx) -> line => match line {
                Ok(line) => {
                    if let Some(writer) = writer.as_mut() {
                        writer.write_all(line.as_bytes())?;
                        writer.write_all(b"\n")?;
                    }
                    tail.push_back(line.clone());
                    if tail.len() > TAIL_MAX_LINES {
                        tail.pop_front();
                    }
                    on_output(line);
                }
                Err(_) => stderr_done = true,
            },
        }
    }

    if let Some(writer) = writer.as_mut() {
        writer.flush()?;
    }
    drop(writer);
    stdout_thread.join().ok();
    stderr_thread.join().ok();
    let status = child.wait()?;

    Ok(CommandOutcome {
        status,
        tail: tail.into_iter().collect::<Vec<_>>().join("\n"),
        model_json,
        model_truncated,
    })
}

/// A build tool failure carrying a bounded tail of the tool's output so the
/// caller can include a snippet in error messages. The full log lives on disk.
#[derive(Debug)]
pub struct SyncError {
    pub message: String,
    pub tail: String,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SyncError {}

/// Represents a tool that can resolve the workspace structure.
pub trait BuildSystem: Send + Sync {
    /// The name of the build system (e.g., "Gradle", "Maven")
    fn name(&self) -> &'static str;

    /// Checks if this build system manages the given directory
    /// (e.g., by looking for build.gradle or pom.xml)
    fn is_applicable(&self, workspace_root: &Path) -> bool;

    /// Whether this build system runs an external tool whose output can be
    /// captured and logged.
    fn support_logging(&self) -> bool;

    /// Executes the tool to build and return the workspace graph.
    /// `on_output` is invoked for every line the build tool prints, and the
    /// full (merged) output is streamed to `log_file` if provided.
    fn sync(
        &self,
        workspace_root: &Path,
        java_home: &Path,
        log_file: Option<&Path>,
        on_output: &mut (dyn FnMut(String) + Send),
    ) -> anyhow::Result<WorkspaceGraph> {
        self.sync_with_progress(workspace_root, java_home, log_file, on_output, &mut |_| {})
    }

    /// Executes the tool like [`Self::sync`], additionally reporting structured
    /// [`SyncProgress`] events as they happen. The default implementation runs
    /// the sync while dropping all progress events, so tools that do not opt in
    /// (Eclipse, IDEA) keep working unchanged.
    fn sync_with_progress(
        &self,
        workspace_root: &Path,
        java_home: &Path,
        log_file: Option<&Path>,
        on_output: &mut (dyn FnMut(String) + Send),
        on_progress: &mut (dyn FnMut(SyncProgress) + Send),
    ) -> anyhow::Result<WorkspaceGraph> {
        let _ = on_progress;
        self.sync(workspace_root, java_home, log_file, on_output)
    }

    fn system_type(&self) -> BuildSystemType;
}

pub enum ProbeResult {
    None,
    Single(BuildSystemType),
    Ambiguous(Vec<BuildSystemType>),
}

pub fn probe_workspace_layout(root: &Path) -> ProbeResult {
    let managers: &[&dyn BuildSystem] = &[
        &GradleBuildSystem,
        &MavenBuildSystem,
        &EclipseBuildSystem,
        &IdeaBuildSystem,
    ];

    let detected_systems: Vec<BuildSystemType> = managers
        .iter()
        .filter(|sys| sys.is_applicable(root))
        .map(|sys| sys.system_type())
        .collect();

    match detected_systems.len() {
        0 => ProbeResult::None,
        1 => ProbeResult::Single(detected_systems[0]),
        _ => ProbeResult::Ambiguous(detected_systems),
    }
}
