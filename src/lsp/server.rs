use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{error, info};
use tree_sitter::{InputEdit, Point};

use super::capabilities::server_capabilities;
use super::handlers::completion::handle_completion;
use crate::build_integration::{BuildIntegrationService, ReloadReason};
use crate::completion::engine::CompletionEngine;
use crate::decompiler::cache::DecompilerCache;
use crate::fs_watcher::SourceWatchService;
use crate::index::ClassOrigin;
use crate::index::jdk::JdkIndexer;
use crate::language::LanguageRegistry;
use crate::language::rope_utils::rope_line_col_to_offset;
use crate::lsp::commands::{COMMAND_CLEAR_CACHES, COMMAND_SHOW_MEMORY_STATUS};
use crate::lsp::config::JavaAnalyzerConfig;
use crate::lsp::handlers::goto_definition::handle_goto_definition;
use crate::lsp::handlers::inlay_hints::handle_inlay_hints;
use crate::lsp::handlers::semantic_tokens::{
    handle_semantic_tokens, handle_semantic_tokens_full_delta, handle_semantic_tokens_range,
};
use crate::lsp::request_cancellation::{RequestCancellationManager, RequestFamily, RequestGuard};
use crate::lsp::request_context::RequestContext;
use crate::workspace::{Workspace, document::Document};

const DID_CHANGE_REINDEX_DEBOUNCE: Duration = Duration::from_millis(75);

pub struct Backend {
    client: Client,
    pub workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    pub registry: Arc<LanguageRegistry>,
    pub config: tokio::sync::RwLock<JavaAnalyzerConfig>,
    pub decompiler_cache: crate::decompiler::cache::DecompilerCache,
    request_cancellation: Arc<RequestCancellationManager>,
    build_services: tokio::sync::RwLock<Vec<BuildIntegrationService>>,
    pending_document_reindex_versions: Arc<dashmap::DashMap<Url, i32>>,
    source_watch_service: tokio::sync::RwLock<Option<SourceWatchService>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let decompiler_cache_dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("java-analyzer")
            .join("decompiled");

        Self {
            client,
            workspace: Arc::new(Workspace::new()),
            engine: Arc::new(CompletionEngine::new()),
            registry: Arc::new(LanguageRegistry::new()),
            config: tokio::sync::RwLock::new(JavaAnalyzerConfig::default()),
            decompiler_cache: DecompilerCache::new(decompiler_cache_dir),
            request_cancellation: Arc::new(RequestCancellationManager::new()),
            build_services: tokio::sync::RwLock::new(Vec::new()),
            pending_document_reindex_versions: Arc::new(dashmap::DashMap::new()),
            source_watch_service: tokio::sync::RwLock::new(None),
        }
    }

    fn begin_request(
        &self,
        family: RequestFamily,
        request_kind: &'static str,
        uri: &Url,
    ) -> (RequestGuard, Arc<RequestContext>) {
        let guard = self.request_cancellation.begin(family, uri);
        let request = RequestContext::new(
            request_kind,
            uri,
            guard.family(),
            guard.generation(),
            guard.token().clone(),
        );
        (guard, request)
    }

    async fn configure_workspace_root(&self, root: std::path::PathBuf) {
        let workspace = Arc::clone(&self.workspace);
        let client = self.client.clone();
        workspace.set_workspace_root(root.clone());

        let config = self.config.read().await;
        let jdk_path = config.jdk_path.clone();

        if let Some(jdk_path) = jdk_path.clone() {
            // JDK
            with_progress(
                &client,
                "java-analyzer/index/jdk",
                "Indexing JDK",
                || async {
                    let jdk_index = tokio::task::spawn_blocking(|| {
                        let indexer = JdkIndexer::new(jdk_path);

                        indexer.index()
                    })
                    .await;
                    match jdk_index {
                        Ok(indexed)
                            if !indexed.classes.is_empty() || !indexed.modules.is_empty() =>
                        {
                            let msg = format!(
                                "✓ JDK: {} classes, {} modules",
                                indexed.classes.len(),
                                indexed.modules.len()
                            );
                            workspace.set_jdk_archive(indexed).await;
                            client.log_message(MessageType::INFO, msg).await;
                        }
                        Ok(_) => {
                            client
                                .log_message(
                                    MessageType::WARNING,
                                    "JDK not found — set JAVA_HOME for JDK completion",
                                )
                                .await;
                        }
                        Err(e) => error!(error = %e, "JDK indexing panicked"),
                    }
                },
            )
            .await;
        }
        drop(config);

        // build tools
        let service =
            BuildIntegrationService::new(root, Arc::clone(&workspace), client.clone(), jdk_path);
        service.schedule_reload(ReloadReason::Initialize);
        self.build_services.write().await.push(service);

        let mut source_watch_service = self.source_watch_service.write().await;
        if source_watch_service.is_none() {
            *source_watch_service = Some(SourceWatchService::new(
                Arc::clone(&workspace),
                client.clone(),
            ));
        }
    }

    pub async fn update_config(&self, params: serde_json::Value) {
        let mut config_guard = self.config.write().await;
        match serde_json::from_value::<JavaAnalyzerConfig>(params) {
            Ok(new_config) => {
                tracing::info!(config = ?new_config, "Config updated");
                self.decompiler_cache
                    .set_decompiler(&new_config.decompiler_backend);
                *config_guard = new_config;
            }
            Err(e) => {
                tracing::error!("Failed to parse incoming config: {e:#}");
            }
        }
    }
    async fn notify_build_file_change(&self, uri: &Url) {
        if let Ok(path) = uri.to_file_path() {
            let services = self.build_services.read().await.clone();
            for service in services {
                service.notify_paths_changed([path.clone()]).await;
            }
        }
    }

    async fn register_build_watchers(&self) {
        let options = serde_json::json!({
            "watchers": [
                { "globPattern": "**/build.gradle" },
                { "globPattern": "**/build.gradle.kts" },
                { "globPattern": "**/settings.gradle" },
                { "globPattern": "**/settings.gradle.kts" },
                { "globPattern": "**/gradle.properties" },
                { "globPattern": "**/gradle/libs.versions.toml" }
            ]
        });

        self.client
            .send_request::<tower_lsp::lsp_types::request::RegisterCapability>(RegistrationParams {
                registrations: vec![Registration {
                    id: "java-analyzer-build-watchers".into(),
                    method: "workspace/didChangeWatchedFiles".into(),
                    register_options: Some(options),
                }],
            })
            .await
            .ok();
    }

    fn reindex_document_from_workspace(
        workspace: &Workspace,
        uri: &Url,
        reason: &'static str,
    ) -> Option<()> {
        let started = Instant::now();
        let source = workspace.document_snapshot(uri)?;
        let salsa_file = workspace.get_or_update_salsa_file_for_snapshot(source.as_ref());
        let sync_elapsed = started.elapsed();
        let analysis = workspace.analysis_context_for_uri(uri);
        let index_snapshot = workspace.index.load();
        let uri_str = uri.to_string();

        let (
            classes,
            parse_origin_before,
            parse_origin_after,
            extract_tracked_elapsed,
            extract_materialize_elapsed,
            extraction_result,
        ) = {
            let db = workspace.salsa_db.lock();
            let parse_origin_before =
                crate::salsa_queries::parse::cached_parse_tree_origin(&*db, salsa_file);
            let tracked_started = Instant::now();

            // Trigger the tracked extraction so dependent Salsa queries observe content changes.
            let extraction_result = crate::salsa_queries::index::extract_classes(&*db, salsa_file);
            let extract_tracked_elapsed = tracked_started.elapsed();
            let parse_origin_after =
                crate::salsa_queries::parse::cached_parse_tree_origin(&*db, salsa_file);
            let materialize_started = Instant::now();
            let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
            let classes = workspace.extract_salsa_classes_for_index_context(
                &db,
                salsa_file,
                &origin,
                index_snapshot.as_ref(),
                analysis,
            );
            let extract_materialize_elapsed = materialize_started.elapsed();
            (
                classes,
                parse_origin_before,
                parse_origin_after,
                extract_tracked_elapsed,
                extract_materialize_elapsed,
                extraction_result,
            )
        };

        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        let index_update_started = Instant::now();
        tracing::debug!(
            uri = %uri,
            reason,
            module = analysis.module.0,
            classpath = ?analysis.classpath,
            source_root = ?analysis.source_root.map(|id| id.0),
            class_count = classes.len(),
            language = source.language_id.as_ref(),
            parse_origin_before = parse_origin_before.map(|origin| origin.as_str()),
            parse_origin_after = parse_origin_after.map(|origin| origin.as_str()),
            tracked_class_count = extraction_result.class_count,
            tracked_content_hash = extraction_result.content_hash,
            sync_document_ms = sync_elapsed.as_secs_f64() * 1000.0,
            tracked_extract_ms = extract_tracked_elapsed.as_secs_f64() * 1000.0,
            materialize_classes_ms = extract_materialize_elapsed.as_secs_f64() * 1000.0,
            "document indexing with Salsa"
        );

        let index_updated = workspace.index.update(|index| {
            index.update_source_in_context(analysis.module, analysis.source_root, origin, classes)
        });
        {
            let db = workspace.salsa_db.lock();
            workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
        }
        tracing::debug!(
            uri = %uri,
            reason,
            module = analysis.module.0,
            classpath = ?analysis.classpath,
            source_root = ?analysis.source_root.map(|id| id.0),
            index_updated,
            index_update_ms = index_update_started.elapsed().as_secs_f64() * 1000.0,
            total_reindex_ms = started.elapsed().as_secs_f64() * 1000.0,
            "document indexing timing"
        );

        Some(())
    }

    fn reindex_document_from_salsa(&self, uri: &Url, reason: &'static str) -> Option<()> {
        Self::reindex_document_from_workspace(self.workspace.as_ref(), uri, reason)
    }

    fn schedule_did_change_reindex(&self, uri: Url, version: i32) {
        self.pending_document_reindex_versions
            .insert(uri.clone(), version);

        let workspace = Arc::clone(&self.workspace);
        let pending_versions = Arc::clone(&self.pending_document_reindex_versions);

        tokio::spawn(async move {
            tokio::time::sleep(DID_CHANGE_REINDEX_DEBOUNCE).await;

            if pending_versions.get(&uri).map(|entry| *entry.value()) != Some(version) {
                tracing::debug!(uri = %uri, version, "skipping stale debounced document reindex");
                return;
            }

            if workspace.documents.with_doc(&uri, |doc| doc.version()) != Some(version) {
                tracing::debug!(
                    uri = %uri,
                    version,
                    "skipping debounced document reindex due to document version mismatch"
                );
                return;
            }

            let uri_for_reindex = uri.clone();
            let workspace_for_reindex = Arc::clone(&workspace);
            let reindex_result = tokio::task::spawn_blocking(move || {
                Self::reindex_document_from_workspace(
                    workspace_for_reindex.as_ref(),
                    &uri_for_reindex,
                    "did_change_debounced",
                )
            })
            .await;

            match reindex_result {
                Ok(Some(())) => {
                    tracing::debug!(uri = %uri, version, "completed debounced document reindex");
                }
                Ok(None) => {
                    tracing::debug!(uri = %uri, version, "debounced document reindex skipped");
                }
                Err(error) => {
                    tracing::error!(%error, uri = %uri, version, "debounced document reindex task panicked");
                }
            }

            if pending_versions.get(&uri).map(|entry| *entry.value()) == Some(version) {
                pending_versions.remove(&uri);
            }
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        info!("LSP initialize");

        if let Some(options) = params.initialization_options {
            self.update_config(options).await;
        }

        // Trigger workspace index
        if let Some(root) = params.root_uri.as_ref().and_then(|u| u.to_file_path().ok()) {
            self.configure_workspace_root(root).await;
        } else if let Some(root) = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|folder| folder.uri.to_file_path().ok())
        {
            self.configure_workspace_root(root).await;
        } else if let Some(folders) = params.workspace_folders
            && folders.len() > 1
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "java-analyzer build import currently uses the first workspace folder only",
                )
                .await;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "java-analyzer".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: server_capabilities(),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("LSP initialized");
        self.register_build_watchers().await;
        self.client
            .log_message(MessageType::INFO, "java-analyzer ready")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        info!("LSP shutdown");
        if let Some(source_watch_service) = self.source_watch_service.write().await.take() {
            source_watch_service.stop();
        }
        Ok(())
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> LspResult<Option<Value>> {
        match params.command.as_str() {
            COMMAND_SHOW_MEMORY_STATUS => {
                let summary = self.workspace.memory_report().await;
                self.client
                    .log_message(MessageType::INFO, summary.clone())
                    .await;
                Ok(Some(json!({ "summary": summary })))
            }
            COMMAND_CLEAR_CACHES => {
                let summary = self.workspace.clear_ephemeral_caches().await;
                self.client
                    .log_message(MessageType::INFO, summary.clone())
                    .await;
                Ok(Some(json!({ "summary": summary })))
            }
            command => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "unsupported command: {command}"
            ))),
        }
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> LspResult<Option<Vec<InlayHint>>> {
        let (guard, request) = self.begin_request(
            RequestFamily::InlayHints,
            "inlay_hints",
            &params.text_document.uri,
        );
        let response = handle_inlay_hints(
            Arc::clone(&self.workspace),
            Arc::clone(&self.registry),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;

        let lang_id = match resolve_supported_language_id(&self.registry, &td.uri, &td.language_id)
        {
            Some(id) => id,
            None => return,
        };
        let lang = self
            .registry
            .find(lang_id)
            .expect("resolved language must be registered");

        tracing::info!(
            uri = %td.uri,
            reported_lang = %td.language_id,
            resolved_lang = lang_id,
            "did_open"
        );

        self.workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                td.uri.clone(),
                lang_id,
                td.version,
                td.text.as_str(),
                None,
            )));

        let mut parser = lang.make_parser();
        // the old_tree param is ok to be none since this is the first time parsing the file.
        let tree = parser.parse(&td.text, None);

        self.workspace.documents.with_doc_mut(&td.uri, |doc| {
            doc.set_tree(tree);
        });

        self.reindex_document_from_salsa(&td.uri, "did_open");

        self.client.semantic_tokens_refresh().await.ok();
        self.notify_build_file_change(&td.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;

        let Some(lang_id) = self
            .workspace
            .documents
            .with_doc(uri, |d| d.language_id().to_owned())
        else {
            return;
        };

        let lang = match self.registry.find(&lang_id) {
            Some(l) => l,
            None => return,
        };

        let mut changed_text_for_index = false;

        let ok = self.workspace.documents.with_doc_mut(uri, |doc| {
            let new_version = params.text_document.version;
            let mut text: String = doc.source().text().to_owned();
            let mut rope: ropey::Rope = (*doc.source().rope).clone();

            let base_tree: Option<tree_sitter::Tree> = match &doc.source().tree {
                Some(t) => Some((**t).clone()),
                None => {
                    let mut p = lang.make_parser();
                    p.parse(&text, None)
                }
            };
            let Some(mut tree) = base_tree else {
                return;
            };

            for ch in &params.content_changes {
                let Some(range) = ch.range else {
                    text = ch.text.clone();
                    let mut p = lang.make_parser();
                    let new_tree = p.parse(&text, None);
                    let uri_c = doc.source().uri.as_ref().clone();
                    let lid_c = doc.source().language_id.clone();
                    doc.update_source(crate::workspace::SourceFile::new(
                        uri_c,
                        lid_c.as_ref(),
                        new_version,
                        text.as_str(),
                        new_tree,
                    ));
                    changed_text_for_index = true;
                    return;
                };

                let start_byte =
                    match rope_line_col_to_offset(&rope, range.start.line, range.start.character) {
                        Some(x) => x,
                        None => continue,
                    };
                let old_end_byte =
                    match rope_line_col_to_offset(&rope, range.end.line, range.end.character) {
                        Some(x) => x,
                        None => continue,
                    };

                let start_line = range.start.line as usize;
                let end_line = range.end.line as usize;
                let start_line_byte = rope.line_to_byte(start_line);
                let end_line_byte = rope.line_to_byte(end_line);
                let start_position =
                    Point::new(start_line, start_byte.saturating_sub(start_line_byte));
                let old_end_position =
                    Point::new(end_line, old_end_byte.saturating_sub(end_line_byte));

                text.replace_range(start_byte..old_end_byte, &ch.text);

                let start_char = rope.byte_to_char(start_byte);
                let old_end_char = rope.byte_to_char(old_end_byte);
                rope.remove(start_char..old_end_char);
                rope.insert(start_char, &ch.text);

                let new_end_byte = start_byte + ch.text.len();
                let (new_end_row, new_end_col_bytes) =
                    point_after_insert_bytes(start_position.row, start_position.column, &ch.text);
                let new_end_position = Point::new(new_end_row, new_end_col_bytes);

                tree.edit(&InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position,
                    old_end_position,
                    new_end_position,
                });
            }

            let mut parser = lang.make_parser();
            let new_tree = parser.parse(&text, Some(&tree));
            let uri_c = doc.source().uri.as_ref().clone();
            let lid_c = doc.source().language_id.clone();
            doc.update_source(crate::workspace::SourceFile::new(
                uri_c,
                lid_c.as_ref(),
                new_version,
                text.as_str(),
                new_tree,
            ));
            changed_text_for_index = true;
        });
        if ok.is_none() {
            return;
        }

        if !changed_text_for_index {
            return;
        }

        self.schedule_did_change_reindex(uri.clone(), params.text_document.version);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = &params.text_document.uri;

        let Some(lang_id) = self
            .workspace
            .documents
            .with_doc(uri, |d| d.language_id().to_owned())
        else {
            return;
        };

        let lang = match self.registry.find(&lang_id) {
            Some(l) => l,
            None => return,
        };

        let mut content_for_index = false;

        self.workspace.documents.with_doc_mut(uri, |doc| {
            if let Some(text) = params.text.as_ref() {
                // TODO: should we use increment parsing here?
                // 规范：如果 didSave 携带 text，以它为准（可能与内存不同步）
                let mut parser = lang.make_parser();
                let new_tree = parser.parse(text.as_str(), None);
                let uri_c = doc.source().uri.as_ref().clone();
                let lid_c = doc.source().language_id.clone();
                let ver = doc.version();
                doc.update_source(crate::workspace::SourceFile::new(
                    uri_c,
                    lid_c.as_ref(),
                    ver,
                    text.as_str(),
                    new_tree,
                ));
                content_for_index = true;
                return;
            }

            // No text payload: just re-parse the current source.
            let cur_text = doc.source().text().to_owned();
            let mut parser = lang.make_parser();
            let new_tree = parser.parse(&cur_text, None);
            doc.set_tree(new_tree);
            content_for_index = true;
        });

        if !content_for_index {
            return;
        }
        self.reindex_document_from_salsa(uri, "did_save");

        self.client.semantic_tokens_refresh().await.ok();
        self.notify_build_file_change(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;
        info!(uri = %uri, "did_close");
        self.workspace.documents.close(uri);

        let workspace = Arc::clone(&self.workspace);
        let uri_for_reconcile = uri.clone();
        match tokio::task::spawn_blocking(move || {
            workspace.reconcile_closed_document_blocking(&uri_for_reconcile)
        })
        .await
        {
            Ok(Ok(true)) => {
                self.client.semantic_tokens_refresh().await.ok();
            }
            Ok(Ok(false)) => {}
            Ok(Err(error)) => {
                tracing::error!(%error, uri = %uri, "did_close reconcile failed");
            }
            Err(error) => {
                tracing::error!(%error, uri = %uri, "did_close reconcile task panicked");
            }
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let paths = params
            .changes
            .into_iter()
            .filter_map(|event| event.uri.to_file_path().ok())
            .collect::<Vec<_>>();
        let services = self.build_services.read().await.clone();
        for service in services {
            service.notify_paths_changed(paths.clone()).await;
        }
    }

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let (guard, request) = self.begin_request(
            RequestFamily::Completion,
            "completion",
            &params.text_document_position.text_document.uri,
        );
        let response = handle_completion(
            Arc::clone(&self.workspace),
            Arc::clone(&self.engine),
            Arc::clone(&self.registry),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }

    // ── 预留：hover ───────────────────────────────────────────────────────────

    async fn hover(&self, _params: HoverParams) -> LspResult<Option<Hover>> {
        Ok(None) // TODO
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        info!("LSP goto_definition request received");
        let (guard, request) = self.begin_request(
            RequestFamily::GotoDefinition,
            "goto_definition",
            &params.text_document_position_params.text_document.uri,
        );

        let result = handle_goto_definition(self, params, request).await;
        guard.finish();

        if result.as_ref().ok().is_some_and(|value| value.is_none()) {
            tracing::warn!("Goto definition could not resolve any target");
        }

        result
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> LspResult<Option<SemanticTokensResult>> {
        let (guard, request) = self.begin_request(
            RequestFamily::SemanticTokensFull,
            "semantic_tokens_full",
            &params.text_document.uri,
        );
        let response = handle_semantic_tokens(
            Arc::clone(&self.registry),
            Arc::clone(&self.workspace),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> LspResult<Option<SemanticTokensFullDeltaResult>> {
        let (guard, request) = self.begin_request(
            RequestFamily::SemanticTokensFull,
            "semantic_tokens_full_delta",
            &params.text_document.uri,
        );
        let response = handle_semantic_tokens_full_delta(
            Arc::clone(&self.registry),
            Arc::clone(&self.workspace),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> LspResult<Option<SemanticTokensRangeResult>> {
        let (guard, request) = self.begin_request(
            RequestFamily::SemanticTokensRange,
            "semantic_tokens_range",
            &params.text_document.uri,
        );
        let response = handle_semantic_tokens_range(
            Arc::clone(&self.registry),
            Arc::clone(&self.workspace),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> LspResult<Option<DocumentSymbolResponse>> {
        let (guard, request) = self.begin_request(
            RequestFamily::DocumentSymbol,
            "document_symbol",
            &params.text_document.uri,
        );
        let response = super::handlers::symbols::handle_document_symbol(
            self.registry.clone(),
            self.workspace.clone(),
            params,
            request,
        )
        .await;
        guard.finish();
        response
    }
}

async fn with_progress<F, Fut>(client: &Client, token: &str, title: &str, f: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // 创建进度 token
    let token = NumberOrString::String(token.to_string());
    client
        .send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
            WorkDoneProgressCreateParams {
                token: token.clone(),
            },
        )
        .await
        .ok();

    // Begin
    client
        .send_notification::<tower_lsp::lsp_types::notification::Progress>(ProgressParams {
            token: token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: title.to_string(),
                cancellable: Some(false),
                message: None,
                percentage: None,
            })),
        })
        .await;

    f().await;

    // End
    client
        .send_notification::<tower_lsp::lsp_types::notification::Progress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: None,
            })),
        })
        .await;
}

fn point_after_insert_bytes(
    start_row: usize,
    start_col_bytes: usize,
    inserted: &str,
) -> (usize, usize) {
    if inserted.is_empty() {
        return (start_row, start_col_bytes);
    }

    // tree-sitter Point.column 是“从行首开始的字节数”
    let mut row = start_row;
    let mut col = start_col_bytes;

    // 按 '\n' 分行，column 取最后一行的字节数
    // 注意：这里用 bytes 计数，和 tree-sitter 的定义一致
    if let Some(last_nl) = inserted.rfind('\n') {
        row += inserted.as_bytes().iter().filter(|&&b| b == b'\n').count();
        col = inserted.len() - (last_nl + 1);
    } else {
        col += inserted.len();
    }

    (row, col)
}

fn resolve_supported_language_id<'a>(
    registry: &'a LanguageRegistry,
    uri: &Url,
    reported_language_id: &'a str,
) -> Option<&'a str> {
    if registry.find(reported_language_id).is_some() {
        return Some(reported_language_id);
    }

    infer_language_id_from_uri(uri).filter(|language_id| registry.find(language_id).is_some())
}

fn infer_language_id_from_uri(uri: &Url) -> Option<&'static str> {
    let path = uri.to_file_path().ok()?;
    crate::language::infer_language_id_from_path(&path)
}

#[cfg(test)]
mod tests {
    use super::{infer_language_id_from_uri, resolve_supported_language_id};
    use crate::language::LanguageRegistry;
    use tower_lsp::lsp_types::Url;

    #[test]
    fn resolves_kotlin_from_file_extension_when_language_id_is_unknown() {
        let registry = LanguageRegistry::new();
        let uri = Url::parse("file:///tmp/build.gradle.kts").unwrap();

        let resolved = resolve_supported_language_id(&registry, &uri, "plaintext");

        assert_eq!(resolved, Some("kotlin"));
    }

    #[test]
    fn infers_kotlin_from_kt_file_extension() {
        let uri = Url::parse("file:///tmp/App.kt").unwrap();

        assert_eq!(infer_language_id_from_uri(&uri), Some("kotlin"));
    }
}
