use std::sync::Arc;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{error, info};

use super::capabilities::server_capabilities;
use super::handlers::completion::handle_completion;
use crate::completion::engine::CompletionEngine;
use crate::decompiler::cache::DecompilerCache;
use crate::index::ClassOrigin;
use crate::index::codebase::{index_codebase, index_source_text};
use crate::language::LanguageRegistry;
use crate::lsp::config::JavaAnalyzerConfig;
use crate::lsp::handlers::goto_definition::handle_goto_definition;
use crate::workspace::{Workspace, document::Document};

pub struct Backend {
    client: Client,
    pub workspace: Arc<Workspace>,
    engine: Arc<CompletionEngine>,
    pub registry: Arc<LanguageRegistry>,
    pub config: tokio::sync::RwLock<JavaAnalyzerConfig>,
    pub decompiler_cache: crate::decompiler::cache::DecompilerCache,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("java-analyzer")
            .join("decompiled");

        Self {
            client,
            workspace: Arc::new(Workspace::new()),
            engine: Arc::new(CompletionEngine::new()),
            registry: Arc::new(LanguageRegistry::new()),
            config: tokio::sync::RwLock::new(JavaAnalyzerConfig::default()),
            decompiler_cache: DecompilerCache::new(cache_dir),
        }
    }

    /// Language ID determination
    fn is_supported(lang_id: &str) -> bool {
        matches!(lang_id, "java" | "kotlin")
    }

    /// Trigger background indexing (without blocking the response)
    fn spawn_index_workspace(&self, root: std::path::PathBuf) {
        let workspace = Arc::clone(&self.workspace);
        let client = self.client.clone();
        tokio::spawn(async move {
            // JDK
            with_progress(
                &client,
                "java-analyzer/index/jdk",
                "Indexing JDK",
                || async {
                    let jdk_classes =
                        tokio::task::spawn_blocking(crate::index::jdk::JdkIndexer::index).await;
                    match jdk_classes {
                        Ok(classes) if !classes.is_empty() => {
                            let msg = format!("✓ JDK: {} classes", classes.len());
                            workspace.index.write().await.add_classes(classes);
                            client.log_message(MessageType::INFO, msg).await;
                            client.semantic_tokens_refresh().await.ok();
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

            // JARs
            with_progress(
                &client,
                "java-analyzer/index/jars",
                "Indexing JARs",
                || async {
                    for jar_dir in find_jar_dirs(&root) {
                        workspace.load_jars_from_dir(jar_dir).await;
                    }
                },
            )
            .await;

            let name_table = workspace.index.read().await.build_name_table();

            // Codebase
            with_progress(
                &client,
                "java-analyzer/index/codebase",
                "Indexing workspace",
                || async {
                    let codebase = tokio::task::spawn_blocking({
                        let root = root.clone();
                        move || index_codebase(&root, Some(name_table))
                    })
                    .await;
                    match codebase {
                        Ok(result) => {
                            let msg = format!(
                                "✓ Codebase: {} files, {} classes",
                                result.file_count,
                                result.classes.len()
                            );
                            workspace.index.write().await.add_classes(result.classes);
                            client.log_message(MessageType::INFO, msg).await;
                            client.semantic_tokens_refresh().await.ok();
                        }
                        Err(e) => error!(error = %e, "codebase indexing panicked"),
                    }
                },
            )
            .await;

            client
                .log_message(MessageType::INFO, "✓ Indexing complete")
                .await;
        });
    }

    pub async fn update_config(&self, params: serde_json::Value) {
        let mut config_guard = self.config.write().await;
        if let Ok(new_config) = serde_json::from_value::<JavaAnalyzerConfig>(params) {
            info!(config = ?new_config, "Config updated");
            *config_guard = new_config;
        } else {
            error!("Failed to parse incoming config");
        }
    }
}

/// Locate common JAR directories within the workspace
fn find_jar_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let candidates = [
        ".gradle/caches",
        ".m2/repository",
        "build/libs",
        "out/artifacts",
        "libs",
        "lib",
    ];
    candidates
        .iter()
        .map(|rel| root.join(rel))
        .filter(|p| p.exists())
        .collect()
}

/// Infer Language ID from LSP URI
pub(crate) fn language_id_from_uri(uri: &Url) -> &'static str {
    match uri.path().rsplit('.').next() {
        Some("kt") | Some("kts") => "kotlin",
        _ => "java",
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
            self.spawn_index_workspace(root);
        } else if let Some(folders) = params.workspace_folders {
            for folder in folders {
                if let Ok(root) = folder.uri.to_file_path() {
                    self.spawn_index_workspace(root);
                }
            }
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
        self.client
            .log_message(MessageType::INFO, "java-analyzer ready")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        info!("LSP shutdown");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;
        if !Self::is_supported(&td.language_id) {
            return;
        }

        info!(uri = %td.uri, lang = %td.language_id, "did_open");

        // 存入文档
        self.workspace.documents.open(Document::new(
            td.uri.clone(),
            td.language_id.clone(),
            td.version,
            td.text.clone(),
        ));

        // 增量更新索引
        let uri_str = td.uri.to_string();
        let name_table = self.workspace.index.read().await.build_name_table();
        let classes = index_source_text(&uri_str, &td.text, &td.language_id, Some(name_table));
        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        self.workspace
            .index
            .write()
            .await
            .update_source(origin, classes);

        // update syntax highlight
        self.client.semantic_tokens_refresh().await.ok();
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;

        // 全量同步：取最后一次变更
        let content = match params.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };

        let lang_id = self
            .workspace
            .documents
            .get(uri)
            .map(|d| d.language_id.clone())
            .unwrap_or_else(|| language_id_from_uri(uri).to_string());

        if !Self::is_supported(&lang_id) {
            return;
        }

        self.workspace
            .documents
            .update(uri, params.text_document.version, content.clone());

        // 增量更新索引（去抖动 TODO: 生产实现可加 500ms debounce）
        let uri_str = uri.to_string();
        let name_table = self.workspace.index.read().await.build_name_table();
        let classes = index_source_text(&uri_str, &content, &lang_id, Some(name_table));
        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        self.workspace
            .index
            .write()
            .await
            .update_source(origin, classes);

        // update syntax highlight
        self.client.semantic_tokens_refresh().await.ok();
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Re-parse upon saving (if there is content).
        if let Some(text) = params.text {
            let uri = &params.text_document.uri;
            let lang_id = self
                .workspace
                .documents
                .get(uri)
                .map(|d| d.language_id.clone())
                .unwrap_or_else(|| language_id_from_uri(uri).to_string());

            if Self::is_supported(&lang_id) {
                let uri_str = uri.to_string();
                let name_table = self.workspace.index.read().await.build_name_table();
                let classes = index_source_text(&uri_str, &text, &lang_id, Some(name_table));
                let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
                self.workspace
                    .index
                    .write()
                    .await
                    .update_source(origin, classes);

                // update syntax highlight
                self.client.semantic_tokens_refresh().await.ok();
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;
        info!(uri = %uri, "did_close");
        self.workspace.documents.close(uri);
    }

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let response = handle_completion(
            Arc::clone(&self.workspace),
            Arc::clone(&self.engine),
            Arc::clone(&self.registry),
            params,
        )
        .await;
        Ok(response)
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

        let result = handle_goto_definition(self, params).await;

        if result.is_none() {
            tracing::warn!("Goto definition could not resolve any target");
        }

        Ok(result)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> LspResult<Option<SemanticTokensResult>> {
        let response = super::handlers::semantic_tokens::handle_semantic_tokens_full(
            Arc::clone(&self.workspace),
            params,
        )
        .await;
        Ok(response)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> LspResult<Option<DocumentSymbolResponse>> {
        let response =
            super::handlers::symbols::handle_document_symbol(Arc::clone(&self.workspace), params)
                .await;
        Ok(response)
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
