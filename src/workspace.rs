use anyhow::Result;
use lru::LruCache;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{RwLock, watch};
use tower_lsp::lsp_types::Url;
use tracing::info;

use crate::build_integration::{SourceRootId, WorkspaceModelSnapshot, WorkspaceRootKind};
use crate::index::cache;
use crate::index::codebase::{
    SourceScanMode, collect_source_files, collect_source_files_for_root, load_source_inputs,
    should_index_source_path,
};
use crate::index::incremental::{SourceTextInput, prepare_source_inputs};
use crate::index::{
    ArtifactKind, ArtifactMetadata, ClassMetadata, ClassOrigin, ClasspathId, IndexScope,
    IndexedArchiveData, IndexedJavaModule, ModuleId, StoredArtifact, WorkspaceIndex,
    WorkspaceIndexHandle,
};
use crate::language::Language;
use crate::language::java::module_info::JavaModuleDescriptor;
use crate::salsa_db::{Database as SalsaDatabase, FileId};
use crate::salsa_queries::Db;
use crate::salsa_queries::semantic::CachedMethodLocal;
use crate::semantic::context::CurrentClassMember;
use document::DocumentStore;

pub mod document;

const METHOD_LOCALS_CACHE_LIMIT: usize = 512;
const CLASS_MEMBERS_CACHE_LIMIT: usize = 512;

/// Cache for parsed semantic data (IntelliJ-style PSI cache)
///
/// This stores parsed locals and class members keyed by content hash.
/// When file content changes, the hash changes and cache is automatically invalidated.
struct SemanticCache {
    /// Cached parsed method locals per method, keyed by content hash
    method_locals: LruCache<u64, Vec<CachedMethodLocal>>,
    /// Cached class members per class, keyed by content hash
    class_members: LruCache<u64, Vec<CurrentClassMember>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SemanticCacheStats {
    method_local_entries: usize,
    method_local_items: usize,
    class_member_entries: usize,
    class_member_items: usize,
}

impl Default for SemanticCache {
    fn default() -> Self {
        Self {
            method_locals: LruCache::new(
                NonZeroUsize::new(METHOD_LOCALS_CACHE_LIMIT)
                    .expect("method locals cache limit must be non-zero"),
            ),
            class_members: LruCache::new(
                NonZeroUsize::new(CLASS_MEMBERS_CACHE_LIMIT)
                    .expect("class members cache limit must be non-zero"),
            ),
        }
    }
}

impl SemanticCache {
    fn stats(&self) -> SemanticCacheStats {
        SemanticCacheStats {
            method_local_entries: self.method_locals.len(),
            method_local_items: self
                .method_locals
                .iter()
                .map(|(_, locals)| locals.len())
                .sum(),
            class_member_entries: self.class_members.len(),
            class_member_items: self
                .class_members
                .iter()
                .map(|(_, members)| members.len())
                .sum(),
        }
    }

    fn clear(&mut self) {
        self.method_locals.clear();
        self.class_members.clear();
    }
}

#[derive(Default)]
struct JavaModuleRegistry {
    by_uri: HashMap<Url, Arc<JavaModuleDescriptor>>,
    by_name: HashMap<Arc<str>, Vec<Url>>,
}

impl JavaModuleRegistry {
    fn rebuild_name_index(&mut self) {
        self.by_name.clear();
        for (uri, descriptor) in &self.by_uri {
            self.by_name
                .entry(Arc::clone(&descriptor.name))
                .or_default()
                .push(uri.clone());
        }
    }

    fn upsert(&mut self, uri: Url, descriptor: Arc<JavaModuleDescriptor>) {
        self.by_uri.insert(uri, descriptor);
        self.rebuild_name_index();
    }

    fn remove(&mut self, uri: &Url) {
        if self.by_uri.remove(uri).is_some() {
            self.rebuild_name_index();
        }
    }

    fn replace_all(&mut self, descriptors: Vec<(Url, Arc<JavaModuleDescriptor>)>) {
        self.by_uri.clear();
        for (uri, descriptor) in descriptors {
            self.by_uri.insert(uri, descriptor);
        }
        self.rebuild_name_index();
    }

    fn module_names(&self) -> Vec<Arc<str>> {
        let mut names = self.by_name.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    fn descriptor_for_uri(&self, uri: &Url) -> Option<Arc<JavaModuleDescriptor>> {
        self.by_uri.get(uri).cloned()
    }

    fn first_uri_for_name(&self, name: &str) -> Option<Url> {
        self.by_name
            .get(name)
            .and_then(|uris| uris.first())
            .cloned()
    }
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

#[derive(Debug, Clone)]
pub(crate) enum JavaModuleTarget {
    Source { uri: Url },
    Bytecode { module: Arc<IndexedJavaModule> },
}

#[derive(Debug, Clone)]
struct ActiveJdkArtifact {
    metadata: ArtifactMetadata,
    class_count: usize,
    module_count: usize,
}

impl ActiveJdkArtifact {
    fn from_stored(artifact: &StoredArtifact) -> Self {
        Self {
            metadata: artifact.metadata.clone(),
            class_count: artifact.data.classes.len(),
            module_count: artifact.data.modules.len(),
        }
    }
}

pub struct Workspace {
    pub documents: DocumentStore,
    /// Published workspace state. Reads load the current index snapshot, and
    /// full workspace-model publishes swap index+model together.
    pub index: WorkspaceIndexHandle,
    /// Salsa database for incremental computation
    pub salsa_db: Arc<parking_lot::Mutex<SalsaDatabase>>,
    /// Mapping from URI to Salsa SourceFile input
    salsa_files: Arc<parking_lot::RwLock<HashMap<Url, crate::salsa_db::SourceFile>>>,
    /// File URIs currently managed by workspace/fallback bulk indexing.
    indexed_salsa_uris: Arc<parking_lot::RwLock<HashSet<Url>>>,
    /// Root directory used by fallback indexing and source watching when no
    /// managed workspace model is available.
    workspace_root: Arc<parking_lot::RwLock<Option<PathBuf>>>,
    /// IntelliJ-style semantic cache for parsed locals and members
    /// Keyed by content hash, automatically invalidated when content changes
    semantic_cache: Arc<parking_lot::RwLock<SemanticCache>>,
    java_modules: Arc<parking_lot::RwLock<JavaModuleRegistry>>,
    active_jdk: RwLock<Option<ActiveJdkArtifact>>,
    full_reindex_in_progress: Arc<AtomicUsize>,
    full_reindex_serial: Arc<AtomicUsize>,
    watched_roots_tx: watch::Sender<Vec<WatchedSourceRoot>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchedSourceRoot {
    pub path: PathBuf,
    pub scan_mode: SourceScanMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FilesystemChangeKind {
    Upsert,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilesystemChange {
    pub path: PathBuf,
    pub kind: FilesystemChangeKind,
}

impl FilesystemChange {
    pub(crate) fn upsert(path: PathBuf) -> Self {
        Self {
            path,
            kind: FilesystemChangeKind::Upsert,
        }
    }

    pub(crate) fn remove(path: PathBuf) -> Self {
        Self {
            path,
            kind: FilesystemChangeKind::Remove,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilesystemApplySummary {
    pub applied: usize,
    pub removed: usize,
    pub skipped_open_documents: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileApplyState {
    Applied,
    Unchanged,
    SkippedOpenDocument,
    SkippedUntracked,
}

struct FullReindexGuard {
    counter: Arc<AtomicUsize>,
    serial: Arc<AtomicUsize>,
}

impl Drop for FullReindexGuard {
    fn drop(&mut self) {
        self.serial.fetch_add(1, Ordering::AcqRel);
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

impl Workspace {
    pub fn new() -> Self {
        // Create a single WorkspaceIndex handle shared by both async code and Salsa.
        let index = WorkspaceIndexHandle::new(WorkspaceIndex::new());

        // Create Salsa database with the same workspace index reference
        let salsa_db = SalsaDatabase::with_workspace_index(index.clone());
        let (watched_roots_tx, _) = watch::channel(Vec::new());

        Self {
            documents: DocumentStore::new(),
            index,
            salsa_db: Arc::new(parking_lot::Mutex::new(salsa_db)),
            salsa_files: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            indexed_salsa_uris: Arc::new(parking_lot::RwLock::new(HashSet::new())),
            workspace_root: Arc::new(parking_lot::RwLock::new(None)),
            semantic_cache: Arc::new(parking_lot::RwLock::new(SemanticCache::default())),
            java_modules: Arc::new(parking_lot::RwLock::new(JavaModuleRegistry::default())),
            active_jdk: RwLock::new(None),
            full_reindex_in_progress: Arc::new(AtomicUsize::new(0)),
            full_reindex_serial: Arc::new(AtomicUsize::new(0)),
            watched_roots_tx,
        }
    }

    fn begin_full_reindex(&self) -> FullReindexGuard {
        self.full_reindex_in_progress.fetch_add(1, Ordering::AcqRel);
        self.full_reindex_serial.fetch_add(1, Ordering::AcqRel);
        FullReindexGuard {
            counter: Arc::clone(&self.full_reindex_in_progress),
            serial: Arc::clone(&self.full_reindex_serial),
        }
    }

    pub fn full_reindex_in_progress(&self) -> bool {
        self.full_reindex_in_progress.load(Ordering::Acquire) != 0
    }

    pub fn full_reindex_serial(&self) -> usize {
        self.full_reindex_serial.load(Ordering::Acquire)
    }

    pub fn set_workspace_root(&self, root: PathBuf) {
        *self.workspace_root.write() = Some(root);
        self.publish_watched_source_roots();
    }

    pub fn workspace_root(&self) -> Option<PathBuf> {
        self.workspace_root.read().clone()
    }

    pub(crate) fn subscribe_watched_source_roots(&self) -> watch::Receiver<Vec<WatchedSourceRoot>> {
        self.watched_roots_tx.subscribe()
    }

    /// Get cached method locals by content hash (IntelliJ-style PSI cache)
    pub fn get_cached_method_locals(&self, content_hash: u64) -> Option<Vec<CachedMethodLocal>> {
        self.semantic_cache
            .write()
            .method_locals
            .get(&content_hash)
            .cloned()
    }

    /// Cache parsed method locals by content hash
    pub fn cache_method_locals(&self, content_hash: u64, locals: Vec<CachedMethodLocal>) {
        self.semantic_cache
            .write()
            .method_locals
            .put(content_hash, locals);
    }

    /// Get cached class members by content hash (IntelliJ-style PSI cache)
    pub fn get_cached_class_members(&self, content_hash: u64) -> Option<Vec<CurrentClassMember>> {
        self.semantic_cache
            .write()
            .class_members
            .get(&content_hash)
            .cloned()
    }

    /// Cache class members by content hash
    pub fn cache_class_members(&self, content_hash: u64, members: Vec<CurrentClassMember>) {
        self.semantic_cache
            .write()
            .class_members
            .put(content_hash, members);
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

    pub fn document_snapshot(&self, uri: &Url) -> Option<Arc<SourceFile>> {
        self.documents.with_doc(uri, |doc| doc.snapshot())
    }

    pub fn ensure_tree(&self, uri: &Url, lang: &dyn Language) -> Option<Arc<SourceFile>> {
        let has_tree = self
            .documents
            .with_doc(uri, |doc| doc.source().tree.is_some())
            .unwrap_or(false);

        if !has_tree {
            self.documents.with_doc_mut(uri, |doc| {
                if doc.source().tree.is_some() {
                    return;
                }
                let tree = lang.parse_tree(doc.source().text(), None);
                doc.set_tree(tree);
            });
        }

        self.document_snapshot(uri)
    }

    /// Get or update a Salsa SourceFile for a URI
    ///
    /// This ensures the Salsa file is synchronized with the document content.
    /// If the file exists but content has changed, it updates the Salsa file.
    pub fn get_or_update_salsa_file(&self, uri: &Url) -> Option<crate::salsa_db::SourceFile> {
        let source = self.document_snapshot(uri)?;
        Some(self.get_or_update_salsa_file_for_snapshot(source.as_ref()))
    }

    pub fn get_or_update_salsa_file_for_snapshot(
        &self,
        source: &SourceFile,
    ) -> crate::salsa_db::SourceFile {
        // Check if file exists
        {
            let files = self.salsa_files.read();
            if let Some(&file) = files.get(source.uri.as_ref()) {
                let in_sync = {
                    let db = self.salsa_db.lock();
                    file.content(&*db).as_str() == source.text()
                        && file.language_id(&*db).as_ref() == source.language_id.as_ref()
                };

                if in_sync {
                    // Already in sync
                    return file;
                }

                // Update the existing Salsa input to match the current document snapshot.
                drop(files); // Release read lock
                self.update_existing_salsa_file_from_source(file, source);
                self.seed_salsa_parse_tree_from_source(file, source);
                return file;
            }
        }

        // Create new file
        let file = self.create_salsa_file_from_source(source);
        self.salsa_files
            .write()
            .insert(source.uri.as_ref().clone(), file);
        self.seed_salsa_parse_tree_from_source(file, source);
        file
    }

    pub fn scope_for_uri(&self, uri: &Url) -> IndexScope {
        self.resolve_analysis_context_for_path(uri.to_file_path().ok().as_deref())
            .scope()
    }

    pub(crate) fn load_analysis_state_for_uri(
        &self,
        uri: &Url,
    ) -> (AnalysisContext, Option<Arc<str>>, Arc<WorkspaceIndex>) {
        let (index, model) = self.index.snapshot();
        let analysis = Self::resolve_analysis_context_for_snapshot(
            model.as_ref(),
            uri.to_file_path().ok().as_deref(),
        );
        let inferred_package =
            Self::infer_java_package_for_uri_with_model(uri, analysis.source_root, model.as_ref());
        (analysis, inferred_package, index)
    }

    pub fn infer_java_package_for_uri(
        &self,
        uri: &Url,
        source_root: Option<SourceRootId>,
    ) -> Option<Arc<str>> {
        let (_, model) = self.index.snapshot();
        Self::infer_java_package_for_uri_with_model(uri, source_root, model.as_ref())
    }

    fn infer_java_package_for_uri_with_model(
        uri: &Url,
        source_root: Option<SourceRootId>,
        model: Option<&WorkspaceModelSnapshot>,
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

        let Some(model) = model else {
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
        let (ctx, _, _) = self.load_analysis_state_for_uri(uri);
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
        self.index.current_model()
    }

    pub async fn memory_report(&self) -> String {
        let document_stats = self.documents.stats();
        let semantic_stats = self.semantic_cache.read().stats();
        let salsa_file_count = self.salsa_files.read().len();
        let tracked_indexed_salsa_uri_count = self.indexed_salsa_uris.read().len();
        let salsa_cache_stats = {
            let db = self.salsa_db.lock();
            db.cache_stats()
        };
        let index_stats = self.index.load().memory_stats();
        let active_jdk = self.active_jdk.read().await.clone();
        let active_jdk_artifact = usize::from(active_jdk.is_some());
        let active_jdk_byte_len = active_jdk
            .as_ref()
            .map(|artifact| artifact.metadata.byte_len)
            .unwrap_or_default();
        let active_jdk_class_count = active_jdk
            .as_ref()
            .map(|artifact| artifact.class_count)
            .unwrap_or_default();
        let active_jdk_module_count = active_jdk
            .as_ref()
            .map(|artifact| artifact.module_count)
            .unwrap_or_default();
        let interned_string_count = crate::index::intern_pool_len();

        format!(
            concat!(
                "Java Analyzer Memory Status\n",
                "documents.open={open_documents}\n",
                "documents.text_bytes={document_text_bytes}\n",
                "documents.rope_bytes={document_rope_bytes}\n",
                "documents.trees={document_trees}\n",
                "documents.semantic_token_entries={semantic_token_entries}\n",
                "documents.semantic_token_bytes_approx={semantic_token_bytes}\n",
                "documents.semantic_context_entries={semantic_context_entries}\n",
                "workspace.semantic_method_local_entries={method_local_entries}/{method_local_capacity}\n",
                "workspace.semantic_method_local_items={method_local_items}\n",
                "workspace.class_member_entries={class_member_entries}/{class_member_capacity}\n",
                "workspace.class_member_items={class_member_items}\n",
                "salsa.files={salsa_file_count}\n",
                "salsa.indexed_uris={tracked_indexed_salsa_uri_count}\n",
                "salsa.parse_tree_entries={parse_tree_entries}\n",
                "salsa.parse_tree_text_bytes={parse_tree_text_bytes}\n",
                "salsa.class_extraction_entries={class_extraction_entries}\n",
                "salsa.class_extraction_text_bytes={class_extraction_text_bytes}\n",
                "salsa.extracted_classes={extracted_class_count}\n",
                "index.modules={index_modules}\n",
                "index.jar_cache_entries={jar_cache_entries}\n",
                "index.artifact_reader_entries={artifact_reader_entries}\n",
                "index.scope_cache_entries={scope_cache_entries}\n",
                "index.classpath_jar_refs={classpath_jar_refs}\n",
                "index.unique_buckets={unique_bucket_count}\n",
                "index.classes={index_class_count}\n",
                "index.java_modules={index_java_module_count}\n",
                "index.origins={index_origin_count}\n",
                "index.simple_name_entries={index_simple_name_entry_count}\n",
                "index.package_entries={index_package_entry_count}\n",
                "index.owner_entries={index_owner_entry_count}\n",
                "index.name_table_entries={index_name_table_entries}\n",
                "index.mro_cache_entries={index_mro_cache_entries}\n",
                "index.artifact_materialized_classes={artifact_materialized_class_count}\n",
                "index.active_jdk_artifact={active_jdk_artifact}\n",
                "index.active_jdk_bytes={active_jdk_byte_len}\n",
                "index.active_jdk_classes={active_jdk_class_count}\n",
                "index.active_jdk_modules={active_jdk_module_count}\n",
                "interned_strings={interned_string_count}\n"
            ),
            open_documents = document_stats.open_document_count,
            document_text_bytes = document_stats.text_bytes,
            document_rope_bytes = document_stats.rope_bytes,
            document_trees = document_stats.tree_count,
            semantic_token_entries = document_stats.semantic_token_entries,
            semantic_token_bytes = document_stats.semantic_token_bytes,
            semantic_context_entries = document_stats.semantic_context_entries,
            method_local_entries = semantic_stats.method_local_entries,
            method_local_capacity = METHOD_LOCALS_CACHE_LIMIT,
            method_local_items = semantic_stats.method_local_items,
            class_member_entries = semantic_stats.class_member_entries,
            class_member_capacity = CLASS_MEMBERS_CACHE_LIMIT,
            class_member_items = semantic_stats.class_member_items,
            salsa_file_count = salsa_file_count,
            tracked_indexed_salsa_uri_count = tracked_indexed_salsa_uri_count,
            parse_tree_entries = salsa_cache_stats.parse_tree_entries,
            parse_tree_text_bytes = salsa_cache_stats.parse_tree_text_bytes,
            class_extraction_entries = salsa_cache_stats.class_extraction_entries,
            class_extraction_text_bytes = salsa_cache_stats.class_extraction_text_bytes,
            extracted_class_count = salsa_cache_stats.extracted_class_count,
            index_modules = index_stats.module_count,
            jar_cache_entries = index_stats.jar_cache_entries,
            artifact_reader_entries = index_stats.artifact_reader_entries,
            scope_cache_entries = index_stats.scope_cache_entries,
            classpath_jar_refs = index_stats.classpath_jar_refs,
            unique_bucket_count = index_stats.unique_bucket_count,
            index_class_count = index_stats.class_count,
            index_java_module_count = index_stats.java_module_count,
            index_origin_count = index_stats.origin_count,
            index_simple_name_entry_count = index_stats.simple_name_entry_count,
            index_package_entry_count = index_stats.package_entry_count,
            index_owner_entry_count = index_stats.owner_entry_count,
            index_name_table_entries = index_stats.name_table_entries,
            index_mro_cache_entries = index_stats.mro_cache_entries,
            artifact_materialized_class_count = index_stats.artifact_materialized_class_count,
            active_jdk_artifact = active_jdk_artifact,
            active_jdk_byte_len = active_jdk_byte_len,
            active_jdk_class_count = active_jdk_class_count,
            active_jdk_module_count = active_jdk_module_count,
            interned_string_count = interned_string_count,
        )
    }

    pub async fn clear_ephemeral_caches(&self) -> String {
        self.documents.clear_lsp_caches();
        self.semantic_cache.write().clear();
        {
            let db = self.salsa_db.lock();
            db.clear_cached_snapshots();
        }
        self.index.update(|index| index.clear_analysis_caches());

        let document_stats = self.documents.stats();
        let semantic_stats = self.semantic_cache.read().stats();
        let salsa_cache_stats = {
            let db = self.salsa_db.lock();
            db.cache_stats()
        };
        let index_stats = self.index.load().memory_stats();

        format!(
            concat!(
                "Cleared ephemeral caches.\n",
                "documents.semantic_token_entries={semantic_token_entries}\n",
                "documents.semantic_context_entries={semantic_context_entries}\n",
                "workspace.semantic_method_local_entries={method_local_entries}\n",
                "workspace.class_member_entries={class_member_entries}\n",
                "salsa.parse_tree_entries={parse_tree_entries}\n",
                "salsa.class_extraction_entries={class_extraction_entries}\n",
                "index.scope_cache_entries={scope_cache_entries}\n",
                "index.mro_cache_entries={mro_cache_entries}\n"
            ),
            semantic_token_entries = document_stats.semantic_token_entries,
            semantic_context_entries = document_stats.semantic_context_entries,
            method_local_entries = semantic_stats.method_local_entries,
            class_member_entries = semantic_stats.class_member_entries,
            parse_tree_entries = salsa_cache_stats.parse_tree_entries,
            class_extraction_entries = salsa_cache_stats.class_extraction_entries,
            scope_cache_entries = index_stats.scope_cache_entries,
            mro_cache_entries = index_stats.mro_cache_entries,
        )
    }

    pub async fn set_jdk_classes(&self, classes: Vec<ClassMetadata>) {
        self.set_jdk_archive(IndexedArchiveData {
            classes,
            modules: Vec::new(),
        })
        .await;
    }

    pub async fn set_jdk_archive(&self, data: IndexedArchiveData) {
        match cache::store_generated_artifact(
            "workspace://active-jdk",
            ArtifactKind::Unknown,
            &data,
        ) {
            Some(artifact) => self.set_jdk_artifact(artifact).await,
            None => {
                tracing::warn!(
                    "failed to persist active JDK artifact, falling back to in-memory activation"
                );
                *self.active_jdk.write().await = None;
                self.index.update(|index| index.add_jdk_archive(data));
            }
        }
    }

    pub async fn set_jdk_artifact(&self, artifact: StoredArtifact) {
        let active = ActiveJdkArtifact::from_stored(&artifact);
        *self.active_jdk.write().await = Some(active.clone());
        self.index
            .update(|index| index.set_jdk_artifact(active.metadata.id));
    }

    pub async fn apply_workspace_model(&self, snapshot: WorkspaceModelSnapshot) -> Result<()> {
        let _reindex_guard = self.begin_full_reindex();
        self.set_workspace_root(snapshot.root.path.clone());
        let active_jdk = self.active_jdk.read().await.clone();
        let open_doc_overlays = self
            .documents
            .snapshot_documents()
            .into_iter()
            .map(|snapshot| {
                (
                    snapshot.uri.to_string(),
                    (snapshot.language_id, snapshot.text),
                )
            })
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
                        Some((
                            module.id,
                            root.id,
                            root.classpath,
                            root.path.clone(),
                            root.kind,
                        ))
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();

        let indexed_roots = tokio::task::spawn_blocking(move || {
            root_inputs
                .into_iter()
                .map(|(module_id, root_id, classpath, root_path, root_kind)| {
                    let mode = if matches!(root_kind, WorkspaceRootKind::Generated) {
                        SourceScanMode::IncludeGenerated
                    } else {
                        SourceScanMode::Default
                    };
                    let source_files = collect_source_files_for_root(root_path.clone(), mode);
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
        if let Some(active_jdk) = active_jdk.as_ref() {
            new_index.set_jdk_artifact(active_jdk.metadata.id);
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
        let mut java_module_descriptors = HashMap::new();
        for (module_id, root_id, classpath, root_path, source_inputs) in indexed_roots {
            let (classes, indexed_uris, descriptors) =
                self.index_source_inputs_with_shared_salsa(source_inputs, None);
            current_indexed_uris.extend(indexed_uris);
            java_module_descriptors.extend(descriptors);
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

        let open_docs = self.documents.snapshot_documents();
        for open_doc in open_docs {
            let context = Self::resolve_analysis_context_for_snapshot(
                Some(&snapshot),
                open_doc.uri.to_file_path().ok().as_deref(),
            );
            let origin = ClassOrigin::SourceFile(Arc::<str>::from(open_doc.uri.to_string()));

            // Get or create Salsa file for this document
            let salsa_file = self.get_or_create_salsa_file(
                &open_doc.uri,
                open_doc.text.as_ref(),
                open_doc.language_id.as_ref(),
            );

            // Use Salsa queries for incremental parsing
            let classes = {
                let db = self.salsa_db.lock();

                // Trigger parse query (memoized) - this tracks changes
                let _result = crate::salsa_queries::index::extract_classes(&*db, salsa_file);

                self.extract_salsa_classes_for_index_context(
                    &db, salsa_file, &origin, &new_index, context,
                )
            };

            tracing::debug!(
                uri = %open_doc.uri,
                module = context.module.0,
                classpath = ?context.classpath,
                source_root = ?context.source_root.map(|id| id.0),
                class_count = classes.len(),
                "reindexing open document against workspace model (using Salsa)"
            );
            new_index.update_source_in_context(
                context.module,
                context.source_root,
                origin,
                classes,
            );

            match Self::extract_java_module_descriptor_for_source(
                &open_doc.uri,
                open_doc.language_id.as_ref(),
                open_doc.text.as_ref(),
            ) {
                Some(descriptor) => {
                    java_module_descriptors.insert(open_doc.uri, descriptor);
                }
                None => {
                    java_module_descriptors.remove(&open_doc.uri);
                }
            }
        }

        self.prune_indexed_salsa_files(&current_indexed_uris);
        self.replace_java_module_registry(java_module_descriptors.into_iter().collect());
        self.index.replace(new_index, Some(snapshot.clone()));
        self.publish_watched_source_roots();

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
        self.index.update_model(|model| {
            if let Some(model) = model.as_mut() {
                model.freshness = crate::build_integration::ModelFreshness::Stale;
            }
        });
    }

    pub async fn index_fallback_root(&self, root: PathBuf) -> Result<()> {
        let _reindex_guard = self.begin_full_reindex();
        self.set_workspace_root(root.clone());
        let open_doc_overlays = self
            .documents
            .snapshot_documents()
            .into_iter()
            .map(|snapshot| {
                (
                    snapshot.uri.to_string(),
                    (snapshot.language_id, snapshot.text),
                )
            })
            .collect::<HashMap<_, _>>();
        let source_inputs = tokio::task::spawn_blocking(move || {
            let source_files = collect_source_files([root]);
            let mut source_inputs = load_source_inputs(source_files);
            overlay_open_document_inputs(&mut source_inputs, &open_doc_overlays);
            source_inputs
        })
        .await?;
        let (classes, current_indexed_uris, java_module_descriptors) =
            self.index_source_inputs_with_shared_salsa(source_inputs, None);
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let index = WorkspaceIndex::new();
        if let Some(active_jdk) = self.active_jdk.read().await.clone() {
            index.set_jdk_artifact(active_jdk.metadata.id);
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
        self.replace_java_module_registry(java_module_descriptors);
        self.index.replace(index, None);
        self.publish_watched_source_roots();
        Ok(())
    }

    pub(crate) fn watched_source_roots(&self) -> Vec<WatchedSourceRoot> {
        let (_, model) = self.index.snapshot();
        let mut roots = Vec::new();
        let mut seen = HashSet::new();

        if let Some(model) = model.as_ref() {
            for root in model
                .modules
                .iter()
                .flat_map(|module| module.roots.iter())
                .filter(|root| {
                    matches!(
                        root.kind,
                        WorkspaceRootKind::Sources
                            | WorkspaceRootKind::Tests
                            | WorkspaceRootKind::Generated
                    )
                })
            {
                let mode = scan_mode_for_root_kind(root.kind);
                if seen.insert(root.path.clone()) {
                    roots.push(WatchedSourceRoot {
                        path: root.path.clone(),
                        scan_mode: mode,
                    });
                }
            }
        }

        if roots.is_empty()
            && let Some(root) = self.workspace_root()
            && seen.insert(root.clone())
        {
            roots.push(WatchedSourceRoot {
                path: root,
                scan_mode: SourceScanMode::Default,
            });
        }

        roots
    }

    pub(crate) fn apply_filesystem_changes_blocking(
        &self,
        changes: Vec<FilesystemChange>,
    ) -> Result<FilesystemApplySummary> {
        let mut summary = FilesystemApplySummary::default();
        let mut removals = Vec::new();
        let mut upserts = Vec::new();

        for change in changes {
            match change.kind {
                FilesystemChangeKind::Remove => removals.push(change.path),
                FilesystemChangeKind::Upsert => upserts.push(change.path),
            }
        }

        removals.sort();
        removals.dedup();
        upserts.sort();
        upserts.dedup();

        for path in removals {
            match self.remove_disk_source_path_blocking(&path)? {
                FileApplyState::Applied => {
                    summary.applied += 1;
                    summary.removed += 1;
                }
                FileApplyState::SkippedOpenDocument => {
                    summary.skipped_open_documents += 1;
                }
                FileApplyState::Unchanged | FileApplyState::SkippedUntracked => {}
            }
        }

        for path in upserts {
            match self.upsert_disk_source_path_blocking(&path)? {
                FileApplyState::Applied => {
                    summary.applied += 1;
                }
                FileApplyState::SkippedOpenDocument => {
                    summary.skipped_open_documents += 1;
                }
                FileApplyState::Unchanged | FileApplyState::SkippedUntracked => {}
            }
        }

        Ok(summary)
    }

    pub(crate) fn reconcile_closed_document_blocking(&self, uri: &Url) -> Result<bool> {
        if self.documents.with_doc(uri, |_| ()).is_some() {
            return Ok(false);
        }

        if let Ok(path) = uri.to_file_path() {
            let (_, model) = self.index.snapshot();
            if path.exists() && self.should_track_disk_path_with_model(&path, model.as_ref()) {
                return Ok(matches!(
                    self.upsert_disk_source_path_blocking(&path)?,
                    FileApplyState::Applied | FileApplyState::Unchanged
                ));
            }
        }

        Ok(self.remove_source_origin_for_uri_blocking(uri))
    }

    pub(crate) fn rescan_watched_roots_blocking(
        &self,
        roots: Vec<WatchedSourceRoot>,
    ) -> Result<FilesystemApplySummary> {
        let current_paths = roots
            .iter()
            .flat_map(|root| collect_source_files_for_root(root.path.clone(), root.scan_mode))
            .collect::<HashSet<_>>();
        let current_uris = current_paths
            .iter()
            .filter_map(|path| file_url_from_path(path))
            .collect::<HashSet<_>>();

        let removals = {
            let tracked = self.indexed_salsa_uris.read();
            tracked
                .iter()
                .filter(|uri| self.documents.with_doc(uri, |_| ()).is_none())
                .filter_map(|uri| {
                    let path = uri.to_file_path().ok()?;
                    matches_watched_root(&path, &roots)
                        .filter(|_| !current_uris.contains(uri))
                        .map(|_| FilesystemChange::remove(path))
                })
                .collect::<Vec<_>>()
        };

        let upserts = current_paths.into_iter().map(FilesystemChange::upsert);
        self.apply_filesystem_changes_blocking(removals.into_iter().chain(upserts).collect())
    }

    /// Get or create a Salsa SourceFile for the given URI
    /// This is the bridge between LSP and Salsa
    pub fn get_or_create_salsa_file(
        &self,
        uri: &Url,
        content: &str,
        language_id: &str,
    ) -> crate::salsa_db::SourceFile {
        if let Some(file) = self.get_salsa_file(uri) {
            self.update_existing_salsa_file(file, content, language_id);
            self.seed_salsa_parse_tree_from_document(uri, file);
            file
        } else {
            let created = self.create_salsa_file(uri.clone(), content, language_id);
            let file = {
                let mut files = self.salsa_files.write();
                *files.entry(uri.clone()).or_insert(created)
            };
            self.update_existing_salsa_file(file, content, language_id);
            self.seed_salsa_parse_tree_from_document(uri, file);
            file
        }
    }

    /// Get an existing Salsa SourceFile for the given URI
    pub fn get_salsa_file(&self, uri: &Url) -> Option<crate::salsa_db::SourceFile> {
        let files = self.salsa_files.read();
        files.get(uri).copied()
    }

    pub(crate) fn refresh_java_module_descriptor_for_salsa_file(
        &self,
        db: &crate::salsa_db::Database,
        file: crate::salsa_db::SourceFile,
    ) {
        let uri = file.file_id(db).uri().clone();
        if file.language_id(db).as_ref() != "java" {
            self.java_modules.write().remove(&uri);
            return;
        }

        let descriptor = crate::salsa_queries::java::extract_java_module_descriptor(db, file);
        let mut modules = self.java_modules.write();
        if let Some(descriptor) = descriptor {
            modules.upsert(uri, descriptor);
        } else {
            modules.remove(&uri);
        }
    }

    pub(crate) fn refresh_java_module_descriptor_for_source(
        &self,
        uri: &Url,
        language_id: &str,
        content: &str,
    ) -> bool {
        let descriptor = Self::extract_java_module_descriptor_for_source(uri, language_id, content);
        let mut modules = self.java_modules.write();
        let changed = match descriptor.as_ref() {
            Some(descriptor) => modules.by_uri.get(uri) != Some(descriptor),
            None => modules.by_uri.contains_key(uri),
        };
        if let Some(descriptor) = descriptor {
            modules.upsert(uri.clone(), descriptor);
        } else {
            modules.remove(uri);
        }
        changed
    }

    fn replace_java_module_registry(&self, descriptors: Vec<(Url, Arc<JavaModuleDescriptor>)>) {
        self.java_modules.write().replace_all(descriptors);
    }

    fn extract_java_module_descriptor_for_source(
        uri: &Url,
        language_id: &str,
        content: &str,
    ) -> Option<Arc<JavaModuleDescriptor>> {
        if language_id != "java" {
            return None;
        }

        let is_module_info = uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .is_some_and(|segment| segment == "module-info.java");
        if !is_module_info {
            return None;
        }

        crate::language::java::module_info::extract_module_descriptor_from_source(content)
    }

    pub fn java_module_names(&self) -> Vec<Arc<str>> {
        self.java_modules.read().module_names()
    }

    pub fn visible_java_module_names(&self, context: AnalysisContext) -> Vec<Arc<str>> {
        let mut names = BTreeSet::new();
        for name in self.java_modules.read().module_names() {
            names.insert(name);
        }
        let index = self.index.load();
        for name in index.visible_bytecode_module_names_for_analysis_context(
            context.module,
            context.classpath,
            context.source_root,
        ) {
            names.insert(name);
        }
        names.into_iter().collect()
    }

    pub fn java_module_descriptor_for_uri(&self, uri: &Url) -> Option<Arc<JavaModuleDescriptor>> {
        self.java_modules.read().descriptor_for_uri(uri)
    }

    pub fn java_module_uri(&self, module_name: &str) -> Option<Url> {
        self.java_modules.read().first_uri_for_name(module_name)
    }

    pub(crate) fn resolve_java_module_target(
        &self,
        context: AnalysisContext,
        module_name: &str,
    ) -> Option<JavaModuleTarget> {
        if let Some(uri) = self.java_module_uri(module_name) {
            return Some(JavaModuleTarget::Source { uri });
        }

        let index = self.index.load();
        index
            .find_visible_bytecode_module_for_analysis_context(
                context.module,
                context.classpath,
                context.source_root,
                module_name,
            )
            .map(|module| JavaModuleTarget::Bytecode { module })
    }

    pub(crate) fn extract_salsa_classes_for_index_context(
        &self,
        db: &crate::salsa_db::Database,
        salsa_file: crate::salsa_db::SourceFile,
        origin: &ClassOrigin,
        index: &WorkspaceIndex,
        context: AnalysisContext,
    ) -> Vec<ClassMetadata> {
        let Some(language) = crate::language::lookup_language(salsa_file.language_id(db).as_ref())
        else {
            return crate::salsa_queries::index::get_extracted_classes(db, salsa_file);
        };

        let live_index = db.workspace_index();
        let can_use_live_salsa_context = std::ptr::eq(index, live_index.as_ref());

        if salsa_file.language_id(db).as_ref() == "java" && can_use_live_salsa_context {
            return crate::salsa_queries::java::parse_java_classes_for_analysis_context(
                db,
                salsa_file,
                context.module,
                context.classpath,
                context.source_root,
                index.version(),
            )
            .as_ref()
            .clone();
        }

        let view =
            index.view_for_analysis_context(context.module, context.classpath, context.source_root);
        if salsa_file.language_id(db).as_ref() == "java" {
            return language.extract_classes_with_index_salsa(
                db,
                salsa_file,
                origin,
                None,
                Some(&view),
            );
        }
        let name_table = index.build_name_table_for_analysis_context(
            context.module,
            context.classpath,
            context.source_root,
        );

        language.extract_classes_with_index_salsa(
            db,
            salsa_file,
            origin,
            Some(name_table),
            Some(&view),
        )
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
        crate::salsa_queries::Db::remove_class_extraction(&*db, &file_id);
        drop(db);
        self.java_modules.write().remove(uri);
    }

    fn create_salsa_file(
        &self,
        uri: Url,
        content: &str,
        language_id: &str,
    ) -> crate::salsa_db::SourceFile {
        let db = self.salsa_db.lock();
        crate::salsa_db::SourceFile::new(
            &*db,
            FileId::new(uri),
            content.to_owned(),
            Arc::from(language_id),
        )
    }

    fn create_salsa_file_from_source(&self, source: &SourceFile) -> crate::salsa_db::SourceFile {
        let db = self.salsa_db.lock();
        crate::salsa_db::SourceFile::new(
            &*db,
            FileId::new(source.uri.as_ref().clone()),
            source.text().to_owned(),
            Arc::clone(&source.language_id),
        )
    }

    fn update_existing_salsa_file(
        &self,
        file: crate::salsa_db::SourceFile,
        content: &str,
        language_id: &str,
    ) {
        use salsa::Setter;

        let mut db = self.salsa_db.lock();
        if file.content(&*db).as_str() != content {
            file.set_content(&mut *db).to(content.to_owned());
        }
        if file.language_id(&*db).as_ref() != language_id {
            file.set_language_id(&mut *db).to(Arc::from(language_id));
        }
    }

    fn update_existing_salsa_file_from_source(
        &self,
        file: crate::salsa_db::SourceFile,
        source: &SourceFile,
    ) {
        use salsa::Setter;

        let mut db = self.salsa_db.lock();
        if file.content(&*db).as_str() != source.text() {
            file.set_content(&mut *db).to(source.text().to_owned());
        }
        if file.language_id(&*db).as_ref() != source.language_id.as_ref() {
            file.set_language_id(&mut *db)
                .to(Arc::clone(&source.language_id));
        }
    }

    fn seed_salsa_parse_tree_from_document(&self, uri: &Url, file: crate::salsa_db::SourceFile) {
        let Some(source) = self.document_snapshot(uri) else {
            return;
        };
        self.seed_salsa_parse_tree_from_source(file, source.as_ref());
    }

    fn seed_salsa_parse_tree_from_source(
        &self,
        file: crate::salsa_db::SourceFile,
        source: &SourceFile,
    ) {
        let Some(tree) = source.tree.as_deref().cloned() else {
            return;
        };

        let db = self.salsa_db.lock();
        if file.content(&*db).as_str() != source.text()
            || file.language_id(&*db).as_ref() != source.language_id.as_ref()
        {
            return;
        }

        crate::salsa_queries::parse::seed_parse_tree(&*db, file, &tree);
    }

    fn index_source_inputs_with_shared_salsa(
        &self,
        source_inputs: Vec<SourceTextInput>,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> (
        Vec<ClassMetadata>,
        HashSet<Url>,
        Vec<(Url, Arc<JavaModuleDescriptor>)>,
    ) {
        let mut prepared = Vec::with_capacity(source_inputs.len());
        let mut transient_inputs = Vec::new();
        let mut indexed_uris = HashSet::new();
        let mut java_module_descriptors = HashMap::new();

        for mut source in source_inputs {
            let Ok(uri) = Url::parse(source.uri.as_ref()) else {
                continue;
            };
            indexed_uris.insert(uri.clone());

            let source_snapshot = self.document_snapshot(&uri);
            if let Some(snapshot) = source_snapshot.as_ref() {
                source.language_id = Arc::clone(&snapshot.language_id);
                source.content = Arc::clone(&snapshot.text);
            }

            if let Some(descriptor) = Self::extract_java_module_descriptor_for_source(
                &uri,
                source.language_id.as_ref(),
                source.content.as_ref(),
            ) {
                java_module_descriptors.insert(uri.clone(), descriptor);
            }

            if crate::language::lookup_language(source.language_id.as_ref()).is_none() {
                continue;
            }

            if let Some(snapshot) = source_snapshot {
                let salsa_file = self.get_or_update_salsa_file_for_snapshot(snapshot.as_ref());
                prepared.push(IndexedWorkspaceSource {
                    language_id: Arc::clone(&snapshot.language_id),
                    content: Arc::clone(&snapshot.text),
                    origin: source.origin,
                    salsa_file: Some(salsa_file),
                });
            } else {
                transient_inputs.push(source);
            }
        }

        let transient_sources = prepare_source_inputs(transient_inputs);

        let discovered_names = {
            let db = self.salsa_db.lock();
            let mut names = prepared
                .iter()
                .flat_map(|source| source.discover_internal_names(&db))
                .collect::<Vec<_>>();
            names.extend(
                transient_sources
                    .iter()
                    .flat_map(|source| source.discover_internal_names()),
            );
            names
        };

        let enriched_name_table = match name_table {
            Some(existing) => existing.extend_with(discovered_names),
            None => crate::index::NameTable::from_names(discovered_names),
        };

        let mut classes = transient_sources
            .into_iter()
            .flat_map(|source| source.extract_classes(Some(enriched_name_table.clone())))
            .collect::<Vec<_>>();

        classes.extend({
            let db = self.salsa_db.lock();
            prepared
                .into_iter()
                .flat_map(|source| source.extract_classes(&db, Some(enriched_name_table.clone())))
                .collect::<Vec<_>>()
        });

        (
            classes,
            indexed_uris,
            java_module_descriptors.into_iter().collect(),
        )
    }

    fn prune_indexed_salsa_files(&self, current_indexed_uris: &HashSet<Url>) {
        let retained_uris = self.salsa_files.read().keys().cloned().collect::<Vec<_>>();
        for uri in retained_uris {
            if self.documents.with_doc(&uri, |_| ()).is_none() {
                self.remove_salsa_file(&uri);
            }
        }

        *self.indexed_salsa_uris.write() = current_indexed_uris.clone();
    }

    fn publish_watched_source_roots(&self) {
        let _ = self.watched_roots_tx.send(self.watched_source_roots());
    }

    fn should_track_disk_path_with_model(
        &self,
        path: &Path,
        model: Option<&WorkspaceModelSnapshot>,
    ) -> bool {
        if let Some(model) = model
            && let Some(root) = model.source_root_for_path(path, None)
        {
            return matches!(
                root.kind,
                WorkspaceRootKind::Sources
                    | WorkspaceRootKind::Tests
                    | WorkspaceRootKind::Generated
            ) && should_index_source_path(path, scan_mode_for_root_kind(root.kind));
        }

        self.workspace_root().is_some_and(|root| {
            path.starts_with(&root) && should_index_source_path(path, SourceScanMode::Default)
        })
    }

    fn upsert_disk_source_path_blocking(&self, path: &Path) -> Result<FileApplyState> {
        let Some(language_id) = language_id_for_path(path) else {
            return Ok(FileApplyState::SkippedUntracked);
        };
        let Some(uri) = file_url_from_path(path) else {
            return Ok(FileApplyState::SkippedUntracked);
        };
        if self.documents.with_doc(&uri, |_| ()).is_some() {
            return Ok(FileApplyState::SkippedOpenDocument);
        }

        let (index_snapshot, model) = self.index.snapshot();
        if !self.should_track_disk_path_with_model(path, model.as_ref()) {
            return Ok(FileApplyState::SkippedUntracked);
        }

        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.remove_disk_source_path_blocking(path);
            }
            Err(error) => return Err(error.into()),
        };

        let context = Self::resolve_analysis_context_for_snapshot(model.as_ref(), Some(path));
        let origin = ClassOrigin::SourceFile(Arc::from(uri.as_str()));
        let classes = crate::language::lookup_language(language_id)
            .map(|language| {
                let view = index_snapshot.view_for_analysis_context(
                    context.module,
                    context.classpath,
                    context.source_root,
                );
                let name_table = if language_id == "java" {
                    None
                } else {
                    let base_name_table = index_snapshot.build_name_table_for_analysis_context(
                        context.module,
                        context.classpath,
                        context.source_root,
                    );
                    let discovered_names = language.discover_internal_names(&content, None);
                    if discovered_names.is_empty() {
                        Some(base_name_table)
                    } else {
                        Some(base_name_table.extend_with(discovered_names))
                    }
                };

                language.extract_classes_from_source(
                    &content,
                    &origin,
                    None,
                    name_table,
                    Some(&view),
                )
            })
            .unwrap_or_default();

        let changed = self.index.update(|index| {
            index.update_source_in_context(context.module, context.source_root, origin, classes)
        });
        self.track_indexed_uri(&uri);
        self.remove_salsa_file(&uri);
        let module_changed =
            self.refresh_java_module_descriptor_for_source(&uri, language_id, &content);

        Ok(if changed || module_changed {
            FileApplyState::Applied
        } else {
            FileApplyState::Unchanged
        })
    }

    fn remove_disk_source_path_blocking(&self, path: &Path) -> Result<FileApplyState> {
        let Some(uri) = file_url_from_path(path) else {
            return Ok(FileApplyState::SkippedUntracked);
        };
        if self.documents.with_doc(&uri, |_| ()).is_some() {
            return Ok(FileApplyState::SkippedOpenDocument);
        }

        Ok(if self.remove_source_origin_for_uri_blocking(&uri) {
            FileApplyState::Applied
        } else {
            FileApplyState::Unchanged
        })
    }

    fn remove_source_origin_for_uri_blocking(&self, uri: &Url) -> bool {
        let (_, model) = self.index.snapshot();
        let path = uri.to_file_path().ok();
        let context = Self::resolve_analysis_context_for_snapshot(model.as_ref(), path.as_deref());
        let origin = ClassOrigin::SourceFile(Arc::from(uri.as_str()));
        let changed = self.index.update(|index| {
            index.remove_source_origin_in_context(context.module, context.source_root, &origin)
        });
        self.untrack_indexed_uri(uri);
        self.remove_salsa_file(uri);
        changed
    }

    fn track_indexed_uri(&self, uri: &Url) {
        self.indexed_salsa_uris.write().insert(uri.clone());
    }

    fn untrack_indexed_uri(&self, uri: &Url) {
        self.indexed_salsa_uris.write().remove(uri);
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

fn scan_mode_for_root_kind(kind: WorkspaceRootKind) -> SourceScanMode {
    if matches!(kind, WorkspaceRootKind::Generated) {
        SourceScanMode::IncludeGenerated
    } else {
        SourceScanMode::Default
    }
}

fn language_id_for_path(path: &Path) -> Option<&'static str> {
    crate::language::infer_language_id_from_path(path)
}

fn file_url_from_path(path: &Path) -> Option<Url> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Url::from_file_path(absolute).ok()
}

fn matches_watched_root<'a>(
    path: &Path,
    roots: &'a [WatchedSourceRoot],
) -> Option<&'a WatchedSourceRoot> {
    roots
        .iter()
        .filter(|root| path.starts_with(&root.path))
        .max_by_key(|root| root.path.components().count())
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
    content: Arc<str>,
    origin: ClassOrigin,
    salsa_file: Option<crate::salsa_db::SourceFile>,
}

impl IndexedWorkspaceSource {
    fn discover_internal_names(&self, db: &crate::salsa_db::Database) -> Vec<Arc<str>> {
        let Some(language) = crate::language::lookup_language(self.language_id.as_ref()) else {
            return vec![];
        };

        if let Some(file) = self.salsa_file
            && let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file)
        {
            return language.discover_internal_names(self.content.as_ref(), Some(&tree));
        }

        language.discover_internal_names(self.content.as_ref(), None)
    }

    fn extract_classes(
        self,
        db: &crate::salsa_db::Database,
        name_table: Option<Arc<crate::index::NameTable>>,
    ) -> Vec<ClassMetadata> {
        let Some(language) = crate::language::lookup_language(self.language_id.as_ref()) else {
            return vec![];
        };

        if let Some(file) = self.salsa_file
            && let Some(tree) = crate::salsa_queries::parse::parse_tree(db, file)
        {
            return language.extract_classes_from_source(
                self.content.as_ref(),
                &self.origin,
                Some(&tree),
                name_table,
                None,
            );
        }

        language.extract_classes_from_source(
            self.content.as_ref(),
            &self.origin,
            None,
            name_table,
            None,
        )
    }
}

fn overlay_open_document_inputs(
    source_inputs: &mut [SourceTextInput],
    open_doc_overlays: &HashMap<String, (Arc<str>, Arc<str>)>,
) {
    for source in source_inputs {
        if let Some((language_id, content)) = open_doc_overlays.get(source.uri.as_ref()) {
            source.language_id = Arc::clone(language_id);
            source.content = Arc::clone(content);
        }
    }
}

impl Workspace {
    fn resolve_analysis_context_for_path(&self, path: Option<&Path>) -> AnalysisContext {
        let (_, model) = self.index.snapshot();
        Self::resolve_analysis_context_for_snapshot(model.as_ref(), path)
    }

    fn resolve_analysis_context_for_snapshot(
        model: Option<&WorkspaceModelSnapshot>,
        path: Option<&Path>,
    ) -> AnalysisContext {
        if let Some(path) = path
            && let Some(model) = model
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
    use crate::language::LanguageRegistry;
    use crate::salsa_db::{FileId, ParseTreeOrigin};
    use crate::semantic::context::{CursorLocation, SemanticContext};
    use crate::workspace::document::Document;
    use crate::workspace::document::SemanticContextCacheKey;
    use tempfile::tempdir;
    use tower_lsp::lsp_types::SemanticToken;

    fn make_bytecode_module(name: &str, origin: ClassOrigin) -> IndexedJavaModule {
        IndexedJavaModule {
            descriptor: Arc::new(JavaModuleDescriptor {
                name: Arc::from(name),
                is_open: false,
                requires: vec![],
                exports: vec![],
                opens: vec![],
                uses: vec![],
                provides: vec![],
            }),
            origin,
        }
    }

    #[test]
    fn managed_workspace_prefers_imported_source_root_context() {
        let workspace = Workspace::new();
        workspace.index.replace_model(Some(WorkspaceModelSnapshot {
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
        }));

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
        workspace.index.replace_model(Some(WorkspaceModelSnapshot {
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
        }));

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

    #[tokio::test]
    async fn clear_ephemeral_caches_clears_document_workspace_and_salsa_caches() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/Test.java").expect("valid uri");
        let source = "class Test {}";

        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                uri.clone(),
                "java",
                1,
                source,
                None,
            )));

        workspace.documents.with_doc_mut(&uri, |doc| {
            doc.semantic_token_cache = Some((
                "1".into(),
                vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 4,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            ));
            doc.cache_semantic_context(
                SemanticContextCacheKey {
                    document_version: 1,
                    workspace_version: 0,
                    module: ModuleId::ROOT,
                    classpath: ClasspathId::Main,
                    source_root: None,
                    overlay_class_count: 0,
                    offset: 0,
                    trigger: None,
                },
                Arc::new(SemanticContext::new(
                    CursorLocation::Unknown,
                    "",
                    vec![],
                    None,
                    None,
                    None,
                    vec![],
                )),
            );
        });

        workspace.cache_method_locals(1, vec![]);
        workspace.cache_class_members(1, vec![]);

        let registry = LanguageRegistry::new();
        let java = registry.find("java").expect("java language");
        let tree = java
            .parse_tree(source, None)
            .expect("tree-sitter should parse a trivial source file");
        let salsa_file = workspace
            .get_or_update_salsa_file(&uri)
            .expect("salsa file should exist");
        {
            let db = workspace.salsa_db.lock();
            crate::salsa_queries::parse::seed_parse_tree(&*db, salsa_file, &tree);
            let _ = crate::salsa_queries::index::extract_classes(&*db, salsa_file);
        }

        let before = workspace.memory_report().await;
        assert!(before.contains("documents.semantic_token_entries=1"));
        assert!(before.contains("documents.semantic_context_entries=1"));
        assert!(before.contains("workspace.semantic_method_local_entries=1/"));
        assert!(before.contains("workspace.class_member_entries=1/"));
        assert!(before.contains("salsa.parse_tree_entries=1"));
        assert!(before.contains("salsa.class_extraction_entries=1"));

        let after = workspace.clear_ephemeral_caches().await;
        assert!(after.contains("documents.semantic_token_entries=0"));
        assert!(after.contains("documents.semantic_context_entries=0"));
        assert!(after.contains("workspace.semantic_method_local_entries=0"));
        assert!(after.contains("workspace.class_member_entries=0"));
        assert!(after.contains("salsa.parse_tree_entries=0"));
        assert!(after.contains("salsa.class_extraction_entries=0"));
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
        assert_eq!(
            snapshot.source_hash,
            crate::salsa_db::source_content_hash(text)
        );
        assert_eq!(db.cache_stats().parse_tree_text_bytes, 0);
    }

    #[test]
    fn java_module_registry_tracks_module_info_source_files() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/module-info.java").expect("valid uri");
        let salsa_file = workspace.get_or_create_salsa_file(
            &uri,
            "module com.example.app { requires com.example.shared; }",
            "java",
        );

        {
            let db = workspace.salsa_db.lock();
            workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
        }

        assert_eq!(
            workspace.java_module_names(),
            vec![Arc::from("com.example.app")]
        );
        assert_eq!(
            workspace
                .java_module_descriptor_for_uri(&uri)
                .expect("module descriptor")
                .name
                .as_ref(),
            "com.example.app"
        );
        assert_eq!(
            workspace
                .java_module_uri("com.example.app")
                .expect("module uri"),
            uri
        );
    }

    #[test]
    fn visible_java_module_names_merge_source_and_bytecode_modules() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/module-info.java").expect("valid uri");
        let salsa_file =
            workspace.get_or_create_salsa_file(&uri, "module com.example.app { }", "java");

        {
            let db = workspace.salsa_db.lock();
            workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
        }

        workspace.index.update(|index| {
            index.add_jdk_archive(IndexedArchiveData {
                classes: vec![],
                modules: vec![make_bytecode_module(
                    "com.example.lib",
                    ClassOrigin::Jar(Arc::from("jdk://builtin")),
                )],
            });
        });

        let names = workspace.visible_java_module_names(AnalysisContext {
            module: ModuleId::ROOT,
            classpath: ClasspathId::Main,
            source_root: None,
            root_kind: None,
        });

        assert_eq!(
            names,
            vec![
                Arc::<str>::from("com.example.app"),
                Arc::<str>::from("com.example.lib")
            ]
        );
    }

    #[test]
    fn resolve_java_module_target_prefers_source_over_bytecode() {
        let workspace = Workspace::new();
        let uri = Url::parse("file:///workspace/shared/module-info.java").expect("valid uri");
        let salsa_file =
            workspace.get_or_create_salsa_file(&uri, "module com.example.shared { }", "java");

        {
            let db = workspace.salsa_db.lock();
            workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
        }

        workspace.index.update(|index| {
            index.add_jdk_archive(IndexedArchiveData {
                classes: vec![],
                modules: vec![make_bytecode_module(
                    "com.example.shared",
                    ClassOrigin::Jar(Arc::from("/tmp/shared.jar")),
                )],
            });
        });

        let target = workspace
            .resolve_java_module_target(
                AnalysisContext {
                    module: ModuleId::ROOT,
                    classpath: ClasspathId::Main,
                    source_root: None,
                    root_kind: None,
                },
                "com.example.shared",
            )
            .expect("module target");

        match target {
            JavaModuleTarget::Source { uri: target_uri } => assert_eq!(target_uri, uri),
            JavaModuleTarget::Bytecode { .. } => {
                panic!("source module should win over bytecode module")
            }
        }
    }

    #[test]
    fn ensure_tree_materializes_tree_on_document_snapshot_boundary() {
        let workspace = Workspace::new();
        let registry = LanguageRegistry::new();
        let uri = Url::parse("file:///workspace/Test.java").expect("valid uri");
        let text = "class Test { void demo() {} }";
        let lang = registry.find("java").expect("java language");

        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                uri.clone(),
                "java",
                1,
                text,
                None,
            )));

        let snapshot = workspace
            .ensure_tree(&uri, lang)
            .expect("snapshot with tree should exist");

        assert!(
            snapshot.tree.is_some(),
            "returned snapshot should have a tree"
        );
        assert!(
            workspace
                .document_snapshot(&uri)
                .expect("stored snapshot")
                .tree
                .is_some(),
            "document snapshot boundary should publish the tree back to the workspace"
        );
    }

    #[tokio::test]
    async fn fallback_root_indexing_drops_closed_file_salsa_state() {
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

        {
            let db = workspace.salsa_db.lock();
            assert!(
                db.cached_parse_tree(&FileId::new(uri.clone())).is_none(),
                "closed fallback sources should not retain cached parse trees"
            );
        }
        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed fallback sources should not retain workspace Salsa files"
        );

        fs::write(&path, "class Demo { String value; }").expect("rewrite source");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("second index");

        {
            let db = workspace.salsa_db.lock();
            assert!(
                db.cached_parse_tree(&FileId::new(uri.clone())).is_none(),
                "closed fallback sources should clear cached parse trees after reindex"
            );
        }
        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed fallback sources should remain transient after reindex"
        );

        let index = workspace.index.load();
        let view = index.view(IndexScope {
            module: ModuleId::ROOT,
        });
        let demo = view.get_class("Demo").expect("indexed Demo");
        assert!(
            demo.fields[0].descriptor.as_ref().ends_with("String;"),
            "reindexed field should reflect the rewritten String type"
        );
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

    #[tokio::test]
    async fn fallback_root_indexing_tracks_closed_module_info_without_retaining_salsa() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("module-info.java");
        fs::write(&path, "module com.example.app { requires java.base; }")
            .expect("write module descriptor");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("index root");

        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed module-info indexing should not retain workspace Salsa files"
        );
        assert_eq!(
            workspace.java_module_names(),
            vec![Arc::<str>::from("com.example.app")]
        );
        assert_eq!(
            workspace
                .java_module_uri("com.example.app")
                .expect("module uri"),
            uri
        );
    }

    #[tokio::test]
    async fn filesystem_apply_skips_open_document_snapshot() {
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

        fs::write(&path, "class Demo { boolean disk; }").expect("rewrite source");

        let summary = workspace
            .apply_filesystem_changes_blocking(vec![FilesystemChange::upsert(path.clone())])
            .expect("apply filesystem change");
        assert_eq!(summary.applied, 0);
        assert_eq!(summary.skipped_open_documents, 1);

        let file = workspace.get_salsa_file(&uri).expect("tracked salsa file");
        let db = workspace.salsa_db.lock();
        assert_eq!(file.content(&*db), "class Demo { String live; }");
    }

    #[tokio::test]
    async fn watched_root_subscription_publishes_fallback_root() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let mut roots_rx = workspace.subscribe_watched_source_roots();

        workspace.set_workspace_root(dir.path().to_path_buf());

        roots_rx.changed().await.expect("watched root update");
        assert_eq!(
            roots_rx.borrow().clone(),
            vec![WatchedSourceRoot {
                path: dir.path().to_path_buf(),
                scan_mode: SourceScanMode::Default,
            }]
        );
    }

    #[tokio::test]
    async fn rescan_watched_roots_removes_deleted_disk_source() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Demo.java");
        fs::write(&path, "class Demo { int disk; }").expect("write source");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("index root");
        assert!(
            workspace.indexed_salsa_uris.read().contains(&uri),
            "tracked source URI should be recorded before delete"
        );
        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed watched-root sources should not retain workspace Salsa files"
        );

        fs::remove_file(&path).expect("remove source");

        let summary = workspace
            .rescan_watched_roots_blocking(workspace.watched_source_roots())
            .expect("rescan roots");
        assert_eq!(summary.applied, 1);
        assert_eq!(summary.removed, 1);
        assert!(
            !workspace.indexed_salsa_uris.read().contains(&uri),
            "deleted source should be removed from tracked URIs"
        );
        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "deleted source should not retain workspace Salsa files"
        );
    }

    #[tokio::test]
    async fn reconcile_closed_document_restores_disk_snapshot() {
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

        workspace.documents.close(&uri);
        workspace
            .reconcile_closed_document_blocking(&uri)
            .expect("reconcile closed document");

        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed document reconciliation should drop workspace Salsa retention"
        );

        let index = workspace.index.load();
        let view = index.view(IndexScope {
            module: ModuleId::ROOT,
        });
        let demo = view.get_class("Demo").expect("indexed Demo");
        assert_eq!(demo.fields[0].descriptor.as_ref(), "I");
    }

    #[tokio::test]
    async fn managed_workspace_indexes_generated_root_sources() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let app_dir = dir.path().join("app");
        let generated_root = app_dir.join("build/generated/sources/annotationProcessor/java/main");
        let pkg_dir = generated_root.join("org/example");
        fs::create_dir_all(&pkg_dir).expect("create generated package dir");
        let path = pkg_dir.join("GeneratedDemo.java");
        fs::write(
            &path,
            "package org.example; public class GeneratedDemo { int value; }",
        )
        .expect("write generated source");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .apply_workspace_model(WorkspaceModelSnapshot {
                generation: 1,
                root: WorkspaceRoot {
                    path: dir.path().to_path_buf(),
                },
                name: "demo".into(),
                modules: vec![WorkspaceModule {
                    id: ModuleId(1),
                    name: "app".into(),
                    directory: app_dir.clone(),
                    roots: vec![WorkspaceSourceRoot {
                        id: SourceRootId(11),
                        path: generated_root.clone(),
                        kind: WorkspaceRootKind::Generated,
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
                    tool_version: Some("8.10".into()),
                    imported_at: SystemTime::UNIX_EPOCH,
                },
                freshness: ModelFreshness::Fresh,
                fidelity: ModelFidelity::Full,
            })
            .await
            .expect("apply workspace model");

        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed generated-root sources should not retain workspace Salsa files"
        );

        let index = workspace.index.load();
        let view =
            index.view_for_analysis_context(ModuleId(1), ClasspathId::Main, Some(SourceRootId(11)));
        assert!(
            view.get_class("org/example/GeneratedDemo").is_some(),
            "generated Java source roots should still be indexed"
        );
    }

    #[tokio::test]
    async fn filesystem_apply_updates_closed_module_info_without_retaining_salsa() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("module-info.java");
        fs::write(&path, "module com.example.app { }").expect("write module descriptor");
        let uri =
            Url::from_file_path(path.canonicalize().expect("canonical path")).expect("file uri");

        workspace
            .index_fallback_root(dir.path().to_path_buf())
            .await
            .expect("index root");
        assert_eq!(
            workspace.java_module_names(),
            vec![Arc::<str>::from("com.example.app")]
        );

        fs::write(&path, "module com.example.renamed { }").expect("rewrite module descriptor");

        let summary = workspace
            .apply_filesystem_changes_blocking(vec![FilesystemChange::upsert(path.clone())])
            .expect("apply filesystem change");
        assert_eq!(summary.applied, 1);
        assert!(
            workspace.get_salsa_file(&uri).is_none(),
            "closed module-info updates should not retain workspace Salsa files"
        );
        assert_eq!(
            workspace.java_module_names(),
            vec![Arc::<str>::from("com.example.renamed")]
        );
        assert_eq!(
            workspace
                .java_module_descriptor_for_uri(&uri)
                .expect("module descriptor")
                .name
                .as_ref(),
            "com.example.renamed"
        );
    }

    #[tokio::test]
    async fn managed_workspace_open_document_reindex_preserves_fully_qualified_super_name() {
        let workspace = Workspace::new();
        let dir = tempdir().expect("tempdir");
        let app_dir = dir.path().join("app");
        let src_root = app_dir.join("src/main/java");
        let pkg_dir = src_root.join("org/example");
        fs::create_dir_all(&pkg_dir).expect("create package dir");

        fs::write(
            pkg_dir.join("SomeRandomClass.java"),
            "package org.example; public class SomeRandomClass {}",
        )
        .expect("write base source");

        let main_source = "package org.example; public class Main extends SomeRandomClass {}";
        let main_path = pkg_dir.join("Main.java");
        fs::write(&main_path, main_source).expect("write main source");

        let main_uri = Url::from_file_path(main_path.canonicalize().expect("canonical path"))
            .expect("main uri");
        workspace
            .documents
            .open(Document::new(crate::workspace::SourceFile::new(
                main_uri,
                "java",
                1,
                main_source,
                None,
            )));

        workspace
            .apply_workspace_model(WorkspaceModelSnapshot {
                generation: 1,
                root: WorkspaceRoot {
                    path: dir.path().to_path_buf(),
                },
                name: "demo".into(),
                modules: vec![WorkspaceModule {
                    id: ModuleId(1),
                    name: "app".into(),
                    directory: app_dir,
                    roots: vec![WorkspaceSourceRoot {
                        id: SourceRootId(11),
                        path: src_root,
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
            })
            .await
            .expect("apply workspace model");

        let index = workspace.index.load();
        let view =
            index.view_for_analysis_context(ModuleId(1), ClasspathId::Main, Some(SourceRootId(11)));
        let main = view.get_class("org/example/Main").expect("indexed Main");

        assert_eq!(
            main.super_name.as_deref(),
            Some("org/example/SomeRandomClass"),
            "open-document reindex should preserve the fully qualified superclass from the analysis view",
        );
    }
}
pub mod source_file;
pub use source_file::SourceFile;
