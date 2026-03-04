use std::sync::Arc;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use tracing::{error, info};
use tree_sitter::{InputEdit, Point};

use super::capabilities::server_capabilities;
use super::handlers::completion::handle_completion;
use crate::completion::engine::CompletionEngine;
use crate::decompiler::cache::DecompilerCache;
use crate::index::ClassOrigin;
use crate::index::codebase::{index_codebase, index_source_text};
use crate::language::LanguageRegistry;
use crate::language::rope_utils::rope_line_col_to_offset;
use crate::lsp::config::JavaAnalyzerConfig;
use crate::lsp::handlers::goto_definition::handle_goto_definition;
use crate::lsp::handlers::semantic_tokens::handle_semantic_tokens;
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

        // 用 registry 判断是否支持
        let lang = match self.registry.find(&td.language_id) {
            Some(l) => l,
            None => return,
        };

        info!(uri = %td.uri, lang = %td.language_id, "did_open");

        // 先存 document（Document::new 需要是 text+rope+tree=None 的新结构）
        self.workspace.documents.open(Document::new(
            td.uri.clone(),
            td.language_id.clone(),
            td.version,
            td.text.clone(),
        ));

        // 立刻 parse 一次，缓存 tree（避免 completion/semantic_tokens 每次 parse）
        let mut parser = lang.make_parser();
        let tree = parser.parse(&td.text, None);

        self.workspace.documents.with_doc_mut(&td.uri, |doc| {
            doc.tree = tree;
        });

        // 增量更新索引（保持你原逻辑不变）
        let uri_str = td.uri.to_string();
        let name_table = self.workspace.index.read().await.build_name_table();
        let classes = index_source_text(&uri_str, &td.text, &td.language_id, Some(name_table));
        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        self.workspace
            .index
            .write()
            .await
            .update_source(origin, classes);

        self.client.semantic_tokens_refresh().await.ok();
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;

        // 只处理已打开文档
        let Some(lang_id) = self
            .workspace
            .documents
            .with_doc(uri, |d| d.language_id.clone())
        else {
            return;
        };

        let lang = match self.registry.find(&lang_id) {
            Some(l) => l,
            None => return,
        };

        // 在 doc 上完成：应用 edits -> tree.edit -> parse(Some(old))
        // 注意：闭包里不能 await
        let mut changed_text_for_index: Option<String> = None;

        let ok = self.workspace.documents.with_doc_mut(uri, |doc| {
            // 版本更新
            doc.version = params.text_document.version;

            // 如果 tree 还没有（比如之前没 parse 成功），先 full parse 一次兜底
            if doc.tree.is_none() {
                let mut parser = lang.make_parser();
                doc.tree = parser.parse(&doc.text, None);
            }

            // 必须有 tree 才能增量
            let Some(old_tree) = doc.tree.as_ref() else {
                // 解析失败就只能退化：把 change 当 full 文本替换（这里按协议一般不会发生）
                return;
            };

            let mut tree = old_tree.clone();

            // 逐个应用 change（INCREMENTAL 可能一次发多个）
            for ch in &params.content_changes {
                let Some(range) = ch.range else {
                    // 客户端可能发 full text（range=None），那就退化成 full replace + full parse
                    doc.text = ch.text.clone();
                    doc.rope = ropey::Rope::from_str(&doc.text);

                    let mut parser = lang.make_parser();
                    doc.tree = parser.parse(&doc.text, None);
                    changed_text_for_index = Some(doc.text.clone());
                    return;
                };

                // 1) 旧 rope 上算 byte offsets（你已有 rope_line_col_to_offset）
                let start_byte = match rope_line_col_to_offset(
                    &doc.rope,
                    range.start.line,
                    range.start.character,
                ) {
                    Some(x) => x,
                    None => continue,
                };
                let old_end_byte =
                    match rope_line_col_to_offset(&doc.rope, range.end.line, range.end.character) {
                        Some(x) => x,
                        None => continue,
                    };

                // 2) old positions (tree-sitter Point 的 column 用“字节列”)
                let start_line = range.start.line as usize;
                let end_line = range.end.line as usize;

                let start_line_byte = doc.rope.line_to_byte(start_line);
                let end_line_byte = doc.rope.line_to_byte(end_line);

                let start_position =
                    Point::new(start_line, start_byte.saturating_sub(start_line_byte));
                let old_end_position =
                    Point::new(end_line, old_end_byte.saturating_sub(end_line_byte));

                // 3) 更新 doc.text（byte range）
                doc.text.replace_range(start_byte..old_end_byte, &ch.text);

                // 4) 更新 rope（char range）
                let start_char = doc.rope.byte_to_char(start_byte);
                let old_end_char = doc.rope.byte_to_char(old_end_byte);
                doc.rope.remove(start_char..old_end_char);
                doc.rope.insert(start_char, &ch.text);

                // 5) new end byte / new end point（按插入文本计算）
                let new_end_byte = start_byte + ch.text.len();
                let (new_end_row, new_end_col_bytes) =
                    point_after_insert_bytes(start_position.row, start_position.column, &ch.text);
                let new_end_position = Point::new(new_end_row, new_end_col_bytes);

                // 6) tree.edit
                tree.edit(&InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position,
                    old_end_position,
                    new_end_position,
                });
            }

            // 7) incremental parse（复用 edited old tree）
            let mut parser = lang.make_parser();
            let new_tree = parser.parse(&doc.text, Some(&tree));
            doc.tree = new_tree;

            changed_text_for_index = Some(doc.text.clone());
        });

        if ok.is_none() {
            return;
        }

        // 下面可以 await：更新索引 + refresh
        let Some(content) = changed_text_for_index else {
            return;
        };

        let uri_str = uri.to_string();
        let name_table = self.workspace.index.read().await.build_name_table();
        let classes = index_source_text(&uri_str, &content, &lang_id, Some(name_table));
        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        self.workspace
            .index
            .write()
            .await
            .update_source(origin, classes);

        self.client.semantic_tokens_refresh().await.ok();
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = &params.text_document.uri;

        // 只处理已打开文档
        let Some(lang_id) = self
            .workspace
            .documents
            .with_doc(uri, |d| d.language_id.clone())
        else {
            return;
        };

        let lang = match self.registry.find(&lang_id) {
            Some(l) => l,
            None => return,
        };

        // 在 doc 内更新内容 + rope + tree（闭包内不能 await）
        // 最终用于索引更新的内容
        let mut content_for_index: Option<String> = None;

        self.workspace.documents.with_doc_mut(uri, |doc| {
            if let Some(text) = params.text.as_ref() {
                // 规范：如果 didSave 携带 text，以它为准（可能与内存不同步）
                doc.text = text.clone();
                doc.rope = ropey::Rope::from_str(&doc.text);
            }

            // 保存时重建树：稳定可靠（不依赖 edit ranges）
            let mut parser = lang.make_parser();
            doc.tree = parser.parse(&doc.text, None);

            content_for_index = Some(doc.text.clone());
        });

        let Some(content) = content_for_index else {
            return;
        };

        // 重新索引（你原有逻辑）
        let uri_str = uri.to_string();
        let name_table = self.workspace.index.read().await.build_name_table();
        let classes = index_source_text(&uri_str, &content, &lang_id, Some(name_table));
        let origin = ClassOrigin::SourceFile(Arc::from(uri_str.as_str()));
        self.workspace
            .index
            .write()
            .await
            .update_source(origin, classes);

        // 刷新语义高亮
        self.client.semantic_tokens_refresh().await.ok();
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
        let response = handle_semantic_tokens(
            Arc::clone(&self.registry),
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
        let response = super::handlers::symbols::handle_document_symbol(
            self.registry.clone(),
            self.workspace.clone(),
            params,
        )
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
