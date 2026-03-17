use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tower_lsp::Client;
use tower_lsp::lsp_types::MessageType;

use crate::build_integration::progress::ImportProgress;
use crate::workspace::Workspace;

use super::detection::{BuildWatchInterest, DetectedBuildTool};
use super::gradle::GradleIntegration;
use super::maven::MavenIntegration;
use super::status::{BuildCapability, BuildIntegrationStatus, BuildReloadState};
use super::tool::{BuildToolImportRequest, BuildToolRegistry};

const DEFAULT_RELOAD_DEBOUNCE: Duration = Duration::from_millis(900);

#[derive(Debug, Clone)]
pub enum ReloadReason {
    Initialize,
    FileChanged(PathBuf),
    Manual,
}

pub enum ReloadCommand {
    Trigger(ReloadReason),
}

#[derive(Clone)]
pub struct BuildIntegrationService {
    root: PathBuf,
    tx: mpsc::UnboundedSender<ReloadCommand>,
    status: Arc<RwLock<BuildIntegrationStatus>>,
    watch_interest: Arc<RwLock<Option<BuildWatchInterest>>>,
    fallback_watch_interest: BuildWatchInterest,
}

impl BuildIntegrationService {
    pub fn new(
        root: PathBuf,
        workspace: Arc<Workspace>,
        client: Client,
        java_home: Option<PathBuf>,
    ) -> Self {
        let registry = BuildToolRegistry::new(vec![
            Arc::new(GradleIntegration::default()),
            Arc::new(MavenIntegration::default()),
        ]);
        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(RwLock::new(BuildIntegrationStatus::default()));
        let watch_interest = Arc::new(RwLock::new(None));

        tokio::spawn(run_reload_loop(RunReloadLoopRequest {
            root: root.clone(),
            workspace,
            client,
            registry: registry.clone(),
            java_home,
            rx,
            status: status.clone(),
            watch_interest: watch_interest.clone(),
        }));

        Self {
            root,
            tx,
            status,
            watch_interest,
            fallback_watch_interest: registry.fallback_watch_interest().clone(),
        }
    }

    pub fn schedule_reload(&self, reason: ReloadReason) {
        let _ = self.tx.send(ReloadCommand::Trigger(reason));
    }

    pub async fn notify_paths_changed<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let watch_interest = self.watch_interest.read().await.clone();
        for path in paths {
            let is_relevant = watch_interest
                .as_ref()
                .map(|interest| interest.matches_path(&path))
                .unwrap_or_else(|| self.fallback_watch_interest.matches_path(&path));
            if is_relevant {
                self.schedule_reload(ReloadReason::FileChanged(path));
            }
        }
    }

    pub async fn status(&self) -> BuildIntegrationStatus {
        self.status.read().await.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub struct RunReloadLoopRequest {
    pub root: PathBuf,
    pub workspace: Arc<Workspace>,
    pub client: Client,
    pub registry: BuildToolRegistry,
    pub java_home: Option<PathBuf>,
    pub rx: mpsc::UnboundedReceiver<ReloadCommand>,
    pub status: Arc<RwLock<BuildIntegrationStatus>>,
    pub watch_interest: Arc<RwLock<Option<BuildWatchInterest>>>,
}

async fn run_reload_loop(req: RunReloadLoopRequest) {
    let RunReloadLoopRequest {
        root,
        workspace,
        client,
        registry,
        java_home,
        mut rx,
        status,
        watch_interest,
    } = req;

    let mut generation = 0_u64;
    let mut dirty = false;
    let mut debounce: Option<tokio::time::Instant> = None;
    let mut in_flight: Option<
        JoinHandle<Result<crate::build_integration::WorkspaceModelSnapshot>>,
    > = None;

    loop {
        let debounce_sleep = async {
            if let Some(deadline) = debounce {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            Some(command) = rx.recv() => {
                match command {
                    ReloadCommand::Trigger(reason) => {
                        dirty = true;
                        debounce = Some(tokio::time::Instant::now() + DEFAULT_RELOAD_DEBOUNCE);
                        let mut guard = status.write().await;
                        guard.reload_state = if in_flight.is_some() {
                            BuildReloadState::Importing
                        } else {
                            BuildReloadState::Debouncing
                        };
                        if let ReloadReason::FileChanged(path) = &reason {
                            tracing::debug!(path = %path.display(), "build-relevant file change queued");
                        }
                    }
                }
            }
            _ = debounce_sleep, if debounce.is_some() && in_flight.is_none() => {
                if !dirty {
                    debounce = None;
                    continue;
                }

                dirty = false;
                debounce = None;
                generation = generation.wrapping_add(1);

                let resolved = registry.detect(&root);
                publish_detection_status(
                    &status,
                    &watch_interest,
                    resolved.as_ref().map(|resolved| &resolved.detected),
                ).await;

                let Some(resolved) = resolved else {
                    let mut guard = status.write().await;
                    guard.capability = BuildCapability::Unsupported;
                    guard.reload_state = BuildReloadState::Unmanaged;
                    guard.detected_tool = None;
                    guard.last_error = None;
                    drop(guard);
                    if let Err(err) = workspace.index_fallback_root(root.clone()).await {
                        let mut guard = status.write().await;
                        guard.reload_state = BuildReloadState::Failed;
                        guard.last_error = Some(err.to_string());
                        client.log_message(MessageType::ERROR, format!("Fallback indexing failed: {err:#}")).await;
                    } else {
                        client.semantic_tokens_refresh().await.ok();
                    }
                    continue;
                };

                let detected = resolved.detected.clone();
                let integration = Arc::clone(&resolved.integration);
                let labels = integration.labels();
                let java_home = java_home.clone();
                let progress_client = client.clone();

                {
                    let mut guard = status.write().await;
                    guard.capability = BuildCapability::Supported;
                    guard.detected_tool = Some(detected.kind);
                    guard.tool_version = None;
                    guard.reload_state = BuildReloadState::Importing;
                    guard.last_error = None;
                }

                client
                    .log_message(MessageType::INFO, labels.importing_workspace)
                    .await;

                in_flight = Some(tokio::spawn(async move {
                    integration
                        .import_workspace(BuildToolImportRequest {
                            root: detected.root,
                            generation,
                            java_home,
                            client: progress_client,
                        })
                        .await
                }));
            }
            result = async { in_flight.as_mut().unwrap().await }, if in_flight.is_some() => {
                in_flight = None;
                match result {
                    Ok(Ok(snapshot)) => {
                        let version = snapshot.provenance.tool_version.clone();
                        let progress = ImportProgress::begin(
                            client.clone(),
                            format!("java-analyzer/build-apply/{}", snapshot.generation),
                            "Applying workspace model",
                            "Applying imported workspace model",
                        )
                        .await
                        .ok();
                        if let Err(err) = workspace.apply_workspace_model(snapshot.clone()).await {
                            let mut guard = status.write().await;
                            guard.reload_state = BuildReloadState::Failed;
                            guard.last_error = Some(err.to_string());
                            workspace.mark_model_stale().await;
                            if let Some(progress) = progress {
                                progress.finish("Workspace model apply failed").await;
                            }
                            client.log_message(MessageType::ERROR, format!("Workspace reload failed: {err:#}")).await;
                        } else {
                            let mut guard = status.write().await;
                            guard.capability = BuildCapability::Supported;
                            guard.detected_tool = Some(snapshot.provenance.tool);
                            guard.tool_version = version;
                            guard.reload_state = if dirty { BuildReloadState::Debouncing } else { BuildReloadState::Idle };
                            guard.generation = snapshot.generation;
                            guard.freshness = Some(snapshot.freshness);
                            guard.fidelity = Some(snapshot.fidelity);
                            guard.last_error = None;
                            if let Some(progress) = progress {
                                progress.finish("Workspace model applied").await;
                            }
                            client.semantic_tokens_refresh().await.ok();
                        }
                    }
                    Ok(Err(err)) => {
                        workspace.mark_model_stale().await;
                        let mut guard = status.write().await;
                        guard.reload_state = BuildReloadState::Failed;
                        guard.last_error = Some(err.to_string());
                        client.log_message(MessageType::ERROR, format!("Workspace import failed: {err:#}")).await;
                    }
                    Err(err) => {
                        workspace.mark_model_stale().await;
                        let mut guard = status.write().await;
                        guard.reload_state = BuildReloadState::Failed;
                        guard.last_error = Some(err.to_string());
                        client.log_message(MessageType::ERROR, format!("Workspace import task failed: {err}")).await;
                    }
                }

                if dirty {
                    debounce = Some(tokio::time::Instant::now() + DEFAULT_RELOAD_DEBOUNCE);
                    let mut guard = status.write().await;
                    guard.reload_state = BuildReloadState::Debouncing;
                }
            }
        }
    }
}

async fn publish_detection_status(
    status: &Arc<RwLock<BuildIntegrationStatus>>,
    watch_interest: &Arc<RwLock<Option<BuildWatchInterest>>>,
    detection: Option<&DetectedBuildTool>,
) {
    *watch_interest.write().await = detection.map(|tool| tool.watch_interest.clone());

    let mut guard = status.write().await;
    guard.detected_tool = detection.map(|tool| tool.kind);
    if detection.is_none() {
        guard.tool_version = None;
    }
    guard.capability = if detection.is_some() {
        BuildCapability::Supported
    } else {
        BuildCapability::Unsupported
    };
}
