use anyhow::Result;
use parking_lot::RwLock as ParkingRwLock;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tracing::info;

use crate::build_integration::{SourceRootId, WorkspaceModelSnapshot, WorkspaceRootKind};
use crate::index::codebase::{collect_source_files, load_source_inputs};
use crate::index::incremental::SourceTextInput;
use crate::index::{ClassMetadata, ClassOrigin, ClasspathId, IndexScope, ModuleId, WorkspaceIndex};
use crate::salsa_db::{Database as SalsaDatabase, FileId};
use crate::salsa_queries::semantic::CachedMethodLocal;
use crate::semantic::context::CurrentClassMember;
use document::DocumentStore;

pub mod document;

/// Cache for parsed semantic data (IntelliJ-style PSI cache)
///
/// This stores parsed locals and class members keyed by content hash.
/// When file content changes, the hash changes and cache is automatically invalidated.
#[derive(Default)]
struct SemanticCache {
    /// Cached parsed method locals per method, keyed by content hash
    method_locals: HashMap<u64, Vec<CachedMethodLocal>>,
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
    /// File URIs currently managed by workspace/fallback bulk indexing.
    indexed_salsa_uris: Arc<parking_lot::RwLock<HashSet<Url>>>,
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
            indexed_salsa_uris: Arc::new(parking_lot::RwLock::new(HashSet::new())),
            semantic_cache: Arc::new(parking_lot::RwLock::new(SemanticCache::default())),
            model: ParkingRwLock::new(None),
            jdk_classes: RwLock::new(Vec::new()),
        }
    }

    /// Get cached method locals by content hash (IntelliJ-style PSI cache)
    pub fn get_cached_method_locals(&self, content_hash: u64) -> Option<Vec<CachedMethodLocal>> {
        self.semantic_cache
            .read()
            .method_locals
            .get(&content_hash)
            .cloned()
    }

    /// Cache parsed method locals by content hash
    pub fn cache_method_locals(&self, content_hash: u64, locals: Vec<CachedMethodLocal>) {
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
        self.get_or_update_salsa_file(&url)
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
                let (salsa_content, salsa_language_id) = {
                    let db = self.salsa_db.lock();
                    (
                        file.content(&*db).to_string(),
                        file.language_id(&*db).to_string(),
                    )
                };

                if salsa_content == content && salsa_language_id == language_id {
                    // Already in sync
                    return Some(file);
                }

                // Update the existing Salsa input to match the current document snapshot.
                drop(files); // Release read lock
                self.update_existing_salsa_file(file, content, language_id);
                self.seed_salsa_parse_tree_from_document(uri, file);
                return Some(file);
            }
        }

        // Create new file
        let file = self.create_salsa_file(uri.clone(), content, language_id);
        self.salsa_files.write().insert(uri.clone(), file);
        self.seed_salsa_parse_tree_from_document(uri, file);
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
        let open_doc_overlays = self
            .documents
            .snapshot_documents()
            .into_iter()
            .map(|(uri, language_id, content)| (uri.to_string(), (language_id, content)))
            .collect::<HashMap<_, _>>();
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
                    let source_files = collect_source_files([root_path.clone()]);
                    let mut source_inputs = load_source_inputs(source_files);
                    overlay_open_document_inputs(&mut source_inputs, &open_doc_overlays);
                    (
                        module_id,
                        root_id,
                        classpath,
                        root_path.clone(),
                        source_inputs,
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

        let mut current_indexed_uris = HashSet::new();
        for (module_id, root_id, classpath, root_path, source_inputs) in indexed_roots {
            let (classes, indexed_uris) =
                self.index_source_inputs_with_shared_salsa(source_inputs, None);
            current_indexed_uris.extend(indexed_uris);
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
        self.prune_indexed_salsa_files(&current_indexed_uris);

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
        let open_doc_overlays = self
            .documents
            .snapshot_documents()
            .into_iter()
            .map(|(uri, language_id, content)| (uri.to_string(), (language_id, content)))
            .collect::<HashMap<_, _>>();
        let source_inputs = tokio::task::spawn_blocking(move || {
            let source_files = collect_source_files([root]);
            let mut source_inputs = load_source_inputs(source_files);
            overlay_open_document_inputs(&mut source_inputs, &open_doc_overlays);
            source_inputs
        })
        .await?;
        let (classes, current_indexed_uris) =
            self.index_source_inputs_with_shared_salsa(source_inputs, None);
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
        for class in classes {
            by_origin
                .entry(class.origin.clone())
                .or_default()
                .push(class);
        }
        for (origin, classes) in by_origin {
            index.update_source(scope, origin, classes);
        }
        self.prune_indexed_salsa_files(&current_indexed_uris);
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
        let mut files = self.salsa_files.write();

        if let Some(file) = files.get(uri) {
            self.update_existing_salsa_file(*file, content.to_string(), language_id.to_string());
            self.seed_salsa_parse_tree_from_document(uri, *file);
            *file
        } else {
            // Create new file
            let file =
                self.create_salsa_file(uri.clone(), content.to_string(), language_id.to_string());
            files.insert(uri.clone(), file);
            self.seed_salsa_parse_tree_from_document(uri, file);
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
        let removed = files.remove(uri);
        drop(files);

        let Some(file) = removed else {
            return;
        };

        let db = self.salsa_db.lock();
        let file_id = file.file_id(&*db);
        crate::salsa_queries::Db::remove_parse_tree(&*db, &file_id);
    }

    fn create_salsa_file(
        &self,
        uri: Url,
        content: String,
        language_id: String,
    ) -> crate::salsa_db::SourceFile {
        let db = self.salsa_db.lock();
        crate::salsa_db::SourceFile::new(&*db, FileId::new(uri), content, Arc::from(language_id))
    }

    fn update_existing_salsa_file(
        &self,
        file: crate::salsa_db::SourceFile,
        content: String,
        language_id: String,
    ) {
        use salsa::Setter;

        let mut db = self.salsa_db.lock();
        if file.content(&*db).as_str() != content.as_str() {
            file.set_content(&mut *db).to(content);
        }
        if file.language_id(&*db).as_ref() != language_id {
            file.set_language_id(&mut *db).to(Arc::from(language_id));
        }
    }

    fn seed_salsa_parse_tree_from_document(&self, uri: &Url, file: crate::salsa_db::SourceFile) {
        let db = self.salsa_db.lock();
        let tree = self
            .documents
            .with_doc(uri, |doc| {
                let source = doc.source();
                (source.text() == file.content(&*db)
                    && source.language_id.as_ref() == file.language_id(&*db).as_ref())
                .then(|| source.tree.as_deref().cloned())
                .flatten()
            })
            .flatten();
        let Some(tree) = tree else {
            return;
        };

        crate::salsa_queries::parse::seed_parse_tree(&*db, file, &tree);
    }

    fn index_source_inputs_with_shared_salsa(
        &self,
        source_inputs: Vec<SourceTextInput>,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> (Vec<ClassMetadata>, HashSet<Url>) {
        let mut prepared = Vec::with_capacity(source_inputs.len());
        let mut indexed_uris = HashSet::new();

        for source in source_inputs {
            let Ok(uri) = Url::parse(source.uri.as_ref()) else {
                continue;
            };
            indexed_uris.insert(uri.clone());
            let salsa_file = if source.language_id.as_ref() == "java" {
                Some(self.get_or_create_salsa_file(
                    &uri,
                    source.content.as_str(),
                    source.language_id.as_ref(),
                ))
            } else {
                None
            };
            prepared.push(IndexedWorkspaceSource {
                language_id: source.language_id,
                content: source.content,
                origin: source.origin,
                salsa_file,
            });
        }

        let discovered_names = {
            let db = self.salsa_db.lock();
            prepared
                .iter()
                .flat_map(|source| source.discover_internal_names(&db))
                .collect::<Vec<_>>()
        };

        let enriched_name_table = match name_table {
            Some(existing) => existing.extend_with(discovered_names),
            None => crate::index::NameTable::from_names(discovered_names),
        };

        let classes = {
            let db = self.salsa_db.lock();
            prepared
                .into_iter()
                .flat_map(|source| source.extract_classes(&db, Some(enriched_name_table.clone())))
                .collect::<Vec<_>>()
        };

        (classes, indexed_uris)
    }

    fn prune_indexed_salsa_files(&self, current_indexed_uris: &HashSet<Url>) {
        let stale_uris = {
            let tracked = self.indexed_salsa_uris.read();
            tracked
                .difference(current_indexed_uris)
                .cloned()
                .collect::<Vec<_>>()
        };

        for uri in &stale_uris {
            if self.documents.with_doc(uri, |_| ()).is_none() {
                self.remove_salsa_file(uri);
            }
        }

        *self.indexed_salsa_uris.write() = current_indexed_uris.clone();
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

struct IndexedWorkspaceSource {
    language_id: Arc<str>,
    content: String,
    origin: ClassOrigin,
    salsa_file: Option<crate::salsa_db::SourceFile>,
}

impl IndexedWorkspaceSource {
    fn discover_internal_names(&self, db: &crate::salsa_db::Database) -> Vec<Arc<str>> {
        if let Some(file) = self.salsa_file
            && let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file)
        {
            return crate::language::java::class_parser::discover_java_names_from_tree(
                self.content.as_str(),
                &tree,
            );
        }

        crate::index::source::discover_internal_names_str(
            self.content.as_str(),
            self.language_id.as_ref(),
        )
    }

    fn extract_classes(
        self,
        db: &crate::salsa_db::Database,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> Vec<ClassMetadata> {
        if let Some(file) = self.salsa_file
            && let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file)
        {
            return crate::language::java::class_parser::extract_java_classes_from_tree(
                self.content.as_str(),
                &tree,
                &self.origin,
                name_table,
                None,
            );
        }

        crate::index::source::parse_source_str(
            self.content.as_str(),
            self.language_id.as_ref(),
            self.origin,
            name_table,
        )
    }
}

fn overlay_open_document_inputs(
    source_inputs: &mut [SourceTextInput],
    open_doc_overlays: &HashMap<String, (String, String)>,
) {
    for source in source_inputs {
        if let Some((language_id, content)) = open_doc_overlays.get(source.uri.as_ref()) {
            source.language_id = Arc::from(language_id.as_str());
            source.content = content.clone();
        }
    }
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
    use std::fs;
    use std::time::SystemTime;

    use crate::build_integration::{
        DetectedBuildToolKind, JavaToolchainInfo, ModelFidelity, ModelFreshness,
        WorkspaceModelProvenance, WorkspaceModule, WorkspaceRoot, WorkspaceSourceRoot,
    };
    use crate::salsa_db::ParseTreeOrigin;
    use crate::workspace::document::Document;
    use tempfile::tempdir;

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

    #[test]
    fn get_or_update_salsa_file_keeps_language_and_content_in_sync() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/Test.java").expect("valid uri");
        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                uri.clone(),
                "java",
                1,
                "class Test {}",
                None,
            )));

        let file = workspace
            .get_or_update_salsa_file(&uri)
            .expect("salsa file should exist");
        {
            let db = workspace.salsa_db.lock();
            assert_eq!(file.content(&*db), "class Test {}");
            assert_eq!(file.language_id(&*db).as_ref(), "java");
        }

        workspace.documents.with_doc_mut(&uri, |doc| {
            doc.update_source(crate::workspace::SourceFile::new(
                uri.clone(),
                "kotlin",
                2,
                "class Test",
                None,
            ));
        });

        let same_file = workspace
            .get_or_update_salsa_file(&uri)
            .expect("salsa file should stay addressable");
        assert!(file == same_file);

        let db = workspace.salsa_db.lock();
        assert_eq!(same_file.content(&*db), "class Test");
        assert_eq!(same_file.language_id(&*db).as_ref(), "kotlin");
    }

    #[test]
    fn get_or_update_salsa_file_seeds_parse_cache_from_document_tree() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/Test.java").expect("valid uri");
        let text = "class Test { void demo() {} }";
        let mut parser = crate::language::java::make_java_parser();
        let tree = parser.parse(text, None).expect("tree");

        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                uri.clone(),
                "java",
                1,
                text,
                Some(tree),
            )));

        let file = workspace
            .get_or_update_salsa_file(&uri)
            .expect("salsa file should exist");

        let db = workspace.salsa_db.lock();
        let file_id = file.file_id(&*db);
        let snapshot = db
            .cached_parse_tree(&file_id)
            .expect("parse snapshot should be seeded");

        assert_eq!(snapshot.origin, ParseTreeOrigin::Seeded);
        assert_eq!(snapshot.content.as_ref(), text);
    }

    #[tokio::test]
    async fn fallback_root_indexing_reuses_shared_salsa_parse_tree() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Demo.java");
        fs::write(&path, "class Demo { int value; }").expect("write source");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("first index");

        let file = workspace.get_salsa_file(&uri).expect("tracked salsa file");
        {
            let db = workspace.salsa_db.lock();
            let snapshot = db
                .cached_parse_tree(&file.file_id(&*db))
                .expect("cached parse tree");
            assert_eq!(snapshot.origin, ParseTreeOrigin::Full);
        }

        fs::write(&path, "class Demo { String value; }").expect("rewrite source");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("second index");

        let file = workspace.get_salsa_file(&uri).expect("tracked salsa file");
        let db = workspace.salsa_db.lock();
        let snapshot = db
            .cached_parse_tree(&file.file_id(&*db))
            .expect("cached parse tree");
        assert_eq!(snapshot.origin, ParseTreeOrigin::Incremental);
    }

    #[tokio::test]
    async fn fallback_root_indexing_prefers_open_document_snapshot() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Demo.java");
        fs::write(&path, "class Demo { int disk; }").expect("write source");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                uri.clone(),
                "java",
                1,
                "class Demo { String live; }",
                None,
            )));

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("index root");

        let file = workspace.get_salsa_file(&uri).expect("tracked salsa file");
        let db = workspace.salsa_db.lock();
        assert_eq!(file.content(&*db), "class Demo { String live; }");
    }
}
pub mod source_file;
pub use source_file::SourceFile;
