use anyhow::Result;
use parking_lot::RwLock as ParkingRwLock;
use salsa::Setter;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tracing::info;

use crate::build_integration::{SourceRootId, WorkspaceModelSnapshot, WorkspaceRootKind};
use crate::index::codebase::{index_codebase, index_codebase_paths};
use crate::index::{ClassMetadata, ClassOrigin, ClasspathId, IndexScope, ModuleId, WorkspaceIndex};
use crate::salsa_db::{Database as SalsaDatabase, FileId};
use crate::semantic::{LocalVar, context::CurrentClassMember};
use document::DocumentStore;

pub mod document;

/// Cache for parsed semantic data (IntelliJ-style PSI cache)
///
/// This stores parsed locals and class members keyed by content hash.
/// When file content changes, the hash changes and cache is automatically invalidated.
#[derive(Default)]
struct SemanticCache {
    /// Cached local variables per method, keyed by content hash
    method_locals: HashMap<u64, Vec<LocalVar>>,
    /// Cached class members per class, keyed by content hash
    class_members: HashMap<u64, HashMap<Arc<str>, CurrentClassMember>>,
}

#[derive(Debug, Clone, Copy)]
pub struct AnalysisContext {
    pub module: ModuleId,
    pub classpath: ClasspathId,
    pub source_root: Option<SourceRootId>,
    pub root_kind: Option<WorkspaceRootKind>,
}

impl AnalysisContext {
    pub fn scope(self) -> IndexScope {
        IndexScope {
            module: self.module,
        }
    }
}

pub struct Workspace {
    pub documents: DocumentStore,
    /// Workspace index shared between async code and Salsa
    /// Uses parking_lot::RwLock for sync access (required by Salsa)
    pub index: Arc<parking_lot::RwLock<WorkspaceIndex>>,
    /// Salsa database for incremental computation
    pub salsa_db: Arc<parking_lot::Mutex<SalsaDatabase>>,
    /// Mapping from URI to Salsa SourceFile input
    salsa_files: Arc<parking_lot::RwLock<HashMap<Url, crate::salsa_db::SourceFile>>>,
    /// IntelliJ-style semantic cache for parsed locals and members
    /// Keyed by content hash, automatically invalidated when content changes
    semantic_cache: Arc<parking_lot::RwLock<SemanticCache>>,
    model: ParkingRwLock<Option<WorkspaceModelSnapshot>>,
    jdk_classes: RwLock<Vec<ClassMetadata>>,
}

impl Workspace {
    pub fn new() -> Self {
        // Create a single WorkspaceIndex shared by both async code and Salsa
        // Use parking_lot::RwLock for sync access (required by Salsa)
        let index = Arc::new(parking_lot::RwLock::new(WorkspaceIndex::new()));

        // Create Salsa database with the same workspace index reference
        let salsa_db = SalsaDatabase::with_workspace_index(Arc::clone(&index));

        Self {
            documents: DocumentStore::new(),
            index,
            salsa_db: Arc::new(parking_lot::Mutex::new(salsa_db)),
            salsa_files: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            semantic_cache: Arc::new(parking_lot::RwLock::new(SemanticCache::default())),
            model: ParkingRwLock::new(None),
            jdk_classes: RwLock::new(Vec::new()),
        }
    }

    /// Get cached method locals by content hash (IntelliJ-style PSI cache)
    pub fn get_cached_method_locals(&self, content_hash: u64) -> Option<Vec<LocalVar>> {
        self.semantic_cache
            .read()
            .method_locals
            .get(&content_hash)
            .cloned()
    }

    /// Cache method locals by content hash
    pub fn cache_method_locals(&self, content_hash: u64, locals: Vec<LocalVar>) {
        self.semantic_cache
            .write()
            .method_locals
            .insert(content_hash, locals);
    }

    /// Get cached class members by content hash (IntelliJ-style PSI cache)
    pub fn get_cached_class_members(
        &self,
        content_hash: u64,
    ) -> Option<HashMap<Arc<str>, CurrentClassMember>> {
        self.semantic_cache
            .read()
            .class_members
            .get(&content_hash)
            .cloned()
    }

    /// Cache class members by content hash
    pub fn cache_class_members(
        &self,
        content_hash: u64,
        members: HashMap<Arc<str>, CurrentClassMember>,
    ) {
        self.semantic_cache
            .write()
            .class_members
            .insert(content_hash, members);
    }

    /// Get or create a Salsa SourceFile for a URI string
    ///
    /// This is used by incremental parsing to get a Salsa file handle.
    /// Files are cached so repeated calls return the same SourceFile.
    pub fn get_or_create_salsa_file_by_uri_str(
        &self,
        uri: &str,
    ) -> Option<crate::salsa_db::SourceFile> {
        let url = Url::parse(uri).ok()?;

        // Check if already exists
        {
            let files = self.salsa_files.read();
            if let Some(file) = files.get(&url) {
                return Some(*file);
            }
        }

        // Create new file
        let content = self
            .documents
            .with_doc(&url, |doc| doc.source().text().to_string())?;
        let language_id = self
            .documents
            .with_doc(&url, |doc| doc.language_id().to_string())?;

        let db = self.salsa_db.lock();
        let file = crate::salsa_db::SourceFile::new(
            &*db,
            crate::salsa_db::FileId::new(url.clone()),
            content,
            Arc::from(language_id.as_str()),
        );

        // Cache it
        self.salsa_files.write().insert(url, file);

        Some(file)
    }

    /// Get or update a Salsa SourceFile for a URI
    ///
    /// This ensures the Salsa file is synchronized with the document content.
    /// If the file exists but content has changed, it updates the Salsa file.
    pub fn get_or_update_salsa_file(&self, uri: &Url) -> Option<crate::salsa_db::SourceFile> {
        // Get current document content
        let content = self
            .documents
            .with_doc(uri, |doc| doc.source().text().to_string())?;
        let language_id = self
            .documents
            .with_doc(uri, |doc| doc.language_id().to_string())?;

        // Check if file exists
        {
            let files = self.salsa_files.read();
            if let Some(&file) = files.get(uri) {
                // Check if content is in sync
                let salsa_content = {
                    let db = self.salsa_db.lock();
                    file.content(&*db).to_string()
                };

                if salsa_content == content {
                    // Already in sync
                    return Some(file);
                }

                // Update content
                drop(files); // Release read lock
                let mut db = self.salsa_db.lock();
                file.set_content(&mut *db).to(content);
                return Some(file);
            }
        }

        // Create new file
        let db = self.salsa_db.lock();
        let file = crate::salsa_db::SourceFile::new(
            &*db,
            crate::salsa_db::FileId::new(uri.clone()),
            content,
            Arc::from(language_id.as_str()),
        );

        drop(db);
        self.salsa_files.write().insert(uri.clone(), file);
        Some(file)
    }

    pub fn scope_for_uri(&self, uri: &Url) -> IndexScope {
        self.resolve_analysis_context_for_path(uri.to_file_path().ok().as_deref())
            .scope()
    }

    pub fn infer_java_package_for_uri(
        &self,
        uri: &Url,
        source_root: Option<SourceRootId>,
    ) -> Option<Arc<str>> {
        let Ok(path) = uri.to_file_path() else {
            tracing::debug!(
                uri = %uri,
                requested_source_root = ?source_root.map(|id| id.0),
                fallback_used = true,
                "java package inference skipped: URI is not a file path"
            );
            return None;
        };

        let Some(model) = self.model.read().clone() else {
            tracing::debug!(
                uri = %uri,
                path = %path.display(),
                requested_source_root = ?source_root.map(|id| id.0),
                fallback_used = true,
                "java package inference skipped: no managed workspace model"
            );
            return None;
        };

        match model.infer_java_package_for_file(&path, source_root) {
            Some(inference) => {
                tracing::debug!(
                    uri = %uri,
                    path = %path.display(),
                    resolved_source_root_id = inference.source_root_id.0,
                    resolved_source_root = %inference.source_root_path.display(),
                    relative_dir = %inference.relative_dir.display(),
                    inferred_package = %inference.package,
                    fallback_used = false,
                    "java package inference resolved from workspace source root"
                );
                Some(Arc::from(inference.package))
            }
            None => {
                tracing::debug!(
                    uri = %uri,
                    path = %path.display(),
                    requested_source_root = ?source_root.map(|id| id.0),
                    fallback_used = true,
                    "java package inference fell back"
                );
                None
            }
        }
    }

    pub fn analysis_context_for_uri(&self, uri: &Url) -> AnalysisContext {
        let ctx = self.resolve_analysis_context_for_path(uri.to_file_path().ok().as_deref());
        tracing::debug!(
            uri = %uri,
            module = ctx.module.0,
            classpath = ?ctx.classpath,
            source_root = ?ctx.source_root.map(|id| id.0),
            root_kind = ?ctx.root_kind,
            "resolved analysis context for URI"
        );
        if let Ok(path) = uri.to_file_path() {
            tracing::info!(
                uri = %uri,
                path = %path.display(),
                module = ctx.module.0,
                classpath = ?ctx.classpath,
                source_root = ?ctx.source_root.map(|id| id.0),
                root_kind = ?ctx.root_kind,
                "analysis context resolution"
            );
        }
        ctx
    }

    pub async fn current_model(&self) -> Option<WorkspaceModelSnapshot> {
        self.model.read().clone()
    }

    pub async fn set_jdk_classes(&self, classes: Vec<ClassMetadata>) {
        *self.jdk_classes.write().await = classes.clone();
        self.index.write().add_jdk_classes(classes);
    }

    pub async fn apply_workspace_model(&self, snapshot: WorkspaceModelSnapshot) -> Result<()> {
        let jdk_classes = self.jdk_classes.read().await.clone();
        let root_inputs = snapshot
            .modules
            .iter()
            .flat_map(|module| {
                module.roots.iter().filter_map(|root| {
                    if matches!(
                        root.kind,
                        WorkspaceRootKind::Sources
                            | WorkspaceRootKind::Tests
                            | WorkspaceRootKind::Generated
                    ) {
                        Some((module.id, root.id, root.classpath, root.path.clone()))
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();

        let indexed_roots = tokio::task::spawn_blocking(move || {
            root_inputs
                .into_iter()
                .map(|(module_id, root_id, classpath, root_path)| {
                    (
                        module_id,
                        root_id,
                        classpath,
                        root_path.clone(),
                        index_codebase_paths([root_path], None).classes,
                    )
                })
                .collect::<Vec<_>>()
        })
        .await?;

        let new_index = WorkspaceIndex::new();
        if !jdk_classes.is_empty() {
            new_index.add_jdk_classes(jdk_classes);
        }

        for module in &snapshot.modules {
            let main_roots = module
                .roots
                .iter()
                .filter(|root| root.classpath == ClasspathId::Main)
                .count();
            let test_roots = module
                .roots
                .iter()
                .filter(|root| root.classpath == ClasspathId::Test)
                .count();
            tracing::debug!(
                module = module.id.0,
                name = %module.name,
                roots = module.roots.len(),
                main_roots,
                test_roots,
                compile_classpath = module.compile_classpath.len(),
                test_classpath = module.test_classpath.len(),
                deps = module.dependency_modules.len(),
                "publishing normalized workspace module"
            );
            tracing::info!(
                module = %module.name,
                roots = ?module
                    .roots
                    .iter()
                    .map(|root| format!("{:?}:{:?}:{}", root.kind, root.classpath, root.path.display()))
                    .collect::<Vec<_>>(),
                compile_classpath = ?module
                    .compile_classpath
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
                test_classpath = ?module
                    .test_classpath
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
                dep_modules = ?module.dependency_modules.iter().map(|id| id.0).collect::<Vec<_>>(),
                "normalized workspace module classpath dump"
            );
            new_index.ensure_module(module.id, Arc::from(module.name.clone()));
            new_index.register_module_source_roots(
                module.id,
                module
                    .roots
                    .iter()
                    .filter(|root| {
                        matches!(
                            root.kind,
                            WorkspaceRootKind::Sources
                                | WorkspaceRootKind::Tests
                                | WorkspaceRootKind::Generated
                        )
                    })
                    .map(|root| (root.id, root.classpath))
                    .collect(),
            );

            let main_classpath = merge_classpath(module.compile_classpath.iter())
                .iter()
                .map(|path| Arc::<str>::from(path.to_string_lossy().into_owned()))
                .collect::<Vec<_>>();
            new_index.set_module_classpath(module.id, ClasspathId::Main, main_classpath);

            let test_classpath = merge_classpath(module.test_classpath.iter())
                .iter()
                .map(|path| Arc::<str>::from(path.to_string_lossy().into_owned()))
                .collect::<Vec<_>>();
            new_index.set_module_classpath(module.id, ClasspathId::Test, test_classpath);
            new_index.set_module_dependencies(module.id, module.dependency_modules.clone());
        }

        for (module_id, root_id, classpath, root_path, classes) in indexed_roots {
            let mut by_origin: std::collections::HashMap<ClassOrigin, Vec<_>> =
                std::collections::HashMap::new();
            for class in classes {
                by_origin
                    .entry(class.origin.clone())
                    .or_default()
                    .push(class);
            }
            for (origin, classes) in by_origin {
                tracing::debug!(
                    module = module_id.0,
                    source_root = root_id.0,
                    classpath = ?classpath,
                    root = %root_path.display(),
                    class_count = classes.len(),
                    "indexing imported workspace source root"
                );
                new_index.update_source_in_context(module_id, Some(root_id), origin, classes);
            }
        }

        {
            let mut guard = self.index.write();
            *guard = new_index;
        }
        *self.model.write() = Some(snapshot.clone());

        let open_docs = self.documents.snapshot_documents();
        for (uri, language_id, content) in open_docs {
            let context = self.analysis_context_for_uri(&uri);

            // Get or create Salsa file for this document
            let salsa_file = self.get_or_create_salsa_file(&uri, &content, &language_id);

            // Use Salsa queries for incremental parsing
            let classes = {
                let db = self.salsa_db.lock();

                // Trigger parse query (memoized) - this tracks changes
                let _result = crate::salsa_queries::index::extract_classes(&*db, salsa_file);

                // Get the actual classes (not memoized, but fast)
                crate::salsa_queries::index::get_extracted_classes(&*db, salsa_file)
            };

            let origin = ClassOrigin::SourceFile(Arc::from(uri.to_string().as_str()));
            tracing::debug!(
                uri = %uri,
                module = context.module.0,
                classpath = ?context.classpath,
                source_root = ?context.source_root.map(|id| id.0),
                class_count = classes.len(),
                "reindexing open document against workspace model (using Salsa)"
            );
            self.index.write().update_source_in_context(
                context.module,
                context.source_root,
                origin,
                classes,
            );
        }

        info!(
            generation = snapshot.generation,
            modules = snapshot.modules.len(),
            tool = ?snapshot.provenance.tool,
            tool_version = ?snapshot.provenance.tool_version,
            "workspace model applied"
        );
        Ok(())
    }

    pub async fn mark_model_stale(&self) {
        if let Some(model) = self.model.write().as_mut() {
            model.freshness = crate::build_integration::ModelFreshness::Stale;
        }
    }

    pub async fn index_fallback_root(&self, root: PathBuf) -> Result<()> {
        let result = tokio::task::spawn_blocking(move || index_codebase(root, None)).await?;
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let index = WorkspaceIndex::new();
        let jdk_classes = self.jdk_classes.read().await.clone();
        if !jdk_classes.is_empty() {
            index.add_jdk_classes(jdk_classes);
        }
        let mut by_origin: std::collections::HashMap<ClassOrigin, Vec<_>> =
            std::collections::HashMap::new();
        for class in result.classes {
            by_origin
                .entry(class.origin.clone())
                .or_default()
                .push(class);
        }
        for (origin, classes) in by_origin {
            index.update_source(scope, origin, classes);
        }
        *self.index.write() = index;
        *self.model.write() = None;
        Ok(())
    }

    /// Get or create a Salsa SourceFile for the given URI
    /// This is the bridge between LSP and Salsa
    pub fn get_or_create_salsa_file(
        &self,
        uri: &Url,
        content: &str,
        language_id: &str,
    ) -> crate::salsa_db::SourceFile {
        use salsa::Setter;

        let mut files = self.salsa_files.write();
        let mut db = self.salsa_db.lock();

        if let Some(file) = files.get(uri) {
            // Update existing file
            file.set_content(&mut *db).to(content.to_string());
            *file
        } else {
            // Create new file
            let file = crate::salsa_db::SourceFile::new(
                &*db,
                FileId::new(uri.clone()),
                content.to_string(),
                Arc::from(language_id),
            );
            files.insert(uri.clone(), file);
            file
        }
    }

    /// Get an existing Salsa SourceFile for the given URI
    pub fn get_salsa_file(&self, uri: &Url) -> Option<crate::salsa_db::SourceFile> {
        let files = self.salsa_files.read();
        files.get(uri).copied()
    }

    /// Remove a Salsa SourceFile (e.g., when file is deleted)
    pub fn remove_salsa_file(&self, uri: &Url) {
        let mut files = self.salsa_files.write();
        files.remove(uri);
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_classpath<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<&'a PathBuf> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for path in paths {
        if seen.insert(path.clone()) {
            merged.push(path);
        }
    }
    merged
}

impl Workspace {
    fn resolve_analysis_context_for_path(&self, path: Option<&Path>) -> AnalysisContext {
        if let Some(path) = path
            && let Some(model) = self.model.read().as_ref()
        {
            let best_root = model
                .modules
                .iter()
                .flat_map(|module| module.roots.iter().map(move |root| (module.id, root)))
                .filter(|(_, root)| path.starts_with(&root.path))
                .max_by_key(|(_, root)| root.path.components().count());

            if let Some((module, root)) = best_root {
                return AnalysisContext {
                    module,
                    classpath: root.classpath,
                    source_root: Some(root.id),
                    root_kind: Some(root.kind),
                };
            }

            if let Some(module) = model.module_for_path(path) {
                tracing::debug!(
                    path = %path.display(),
                    module = module.id.0,
                    "managed workspace path fell back to module-level analysis context"
                );
                return AnalysisContext {
                    module: module.id,
                    classpath: ClasspathId::Main,
                    source_root: None,
                    root_kind: None,
                };
            }
        }

        AnalysisContext {
            module: ModuleId::ROOT,
            classpath: ClasspathId::Main,
            source_root: None,
            root_kind: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::build_integration::{
        DetectedBuildToolKind, JavaToolchainInfo, ModelFidelity, ModelFreshness,
        WorkspaceModelProvenance, WorkspaceModule, WorkspaceRoot, WorkspaceSourceRoot,
    };

    #[test]
    fn managed_workspace_prefers_imported_source_root_context() {
        let workspace = Workspace::new();
        *workspace.model.write() = Some(WorkspaceModelSnapshot {
            generation: 1,
            root: WorkspaceRoot {
                path: PathBuf::from("/workspace"),
            },
            name: "demo".into(),
            modules: vec![WorkspaceModule {
                id: ModuleId(1),
                name: "app".into(),
                directory: PathBuf::from("/workspace/app"),
                roots: vec![
                    WorkspaceSourceRoot {
                        id: SourceRootId(11),
                        path: PathBuf::from("/workspace/app/src/main/java"),
                        kind: WorkspaceRootKind::Sources,
                        classpath: ClasspathId::Main,
                    },
                    WorkspaceSourceRoot {
                        id: SourceRootId(12),
                        path: PathBuf::from("/workspace/app/src/test/java"),
                        kind: WorkspaceRootKind::Tests,
                        classpath: ClasspathId::Test,
                    },
                ],
                compile_classpath: vec![],
                test_classpath: vec![],
                dependency_modules: vec![],
                java: JavaToolchainInfo {
                    language_version: None,
                },
            }],
            provenance: WorkspaceModelProvenance {
                tool: DetectedBuildToolKind::Gradle,
                tool_version: Some("9.2.0".into()),
                imported_at: SystemTime::UNIX_EPOCH,
            },
            freshness: ModelFreshness::Fresh,
            fidelity: ModelFidelity::Full,
        });

        let test_file = PathBuf::from("/workspace/app/src/test/java/demo/AppTest.java");
        let ctx = workspace.resolve_analysis_context_for_path(Some(&test_file));
        assert_eq!(ctx.module, ModuleId(1));
        assert_eq!(ctx.classpath, ClasspathId::Test);
        assert_eq!(ctx.source_root, Some(SourceRootId(12)));
        assert_eq!(ctx.root_kind, Some(WorkspaceRootKind::Tests));
    }

    #[test]
    fn infer_java_package_for_uri_uses_managed_source_root() {
        let workspace = Workspace::new();
        *workspace.model.write() = Some(WorkspaceModelSnapshot {
            generation: 1,
            root: WorkspaceRoot {
                path: PathBuf::from("/workspace"),
            },
            name: "demo".into(),
            modules: vec![WorkspaceModule {
                id: ModuleId(1),
                name: "app".into(),
                directory: PathBuf::from("/workspace/app"),
                roots: vec![WorkspaceSourceRoot {
                    id: SourceRootId(11),
                    path: PathBuf::from("/workspace/app/src/main/java"),
                    kind: WorkspaceRootKind::Sources,
                    classpath: ClasspathId::Main,
                }],
                compile_classpath: vec![],
                test_classpath: vec![],
                dependency_modules: vec![],
                java: JavaToolchainInfo {
                    language_version: None,
                },
            }],
            provenance: WorkspaceModelProvenance {
                tool: DetectedBuildToolKind::Gradle,
                tool_version: Some("9.2.0".into()),
                imported_at: SystemTime::UNIX_EPOCH,
            },
            freshness: ModelFreshness::Fresh,
            fidelity: ModelFidelity::Full,
        });

        let uri = Url::parse("file:///workspace/app/src/main/java/org/example/foo/Bar.java")
            .expect("valid uri");
        let inferred = workspace
            .infer_java_package_for_uri(&uri, Some(SourceRootId(11)))
            .expect("package");

        assert_eq!(inferred.as_ref(), "org.example.foo");
    }
}
pub mod source_file;
pub use source_file::SourceFile;
