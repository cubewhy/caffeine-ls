use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::RwLock;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::build_integration::SourceRootId;
use crate::index::cache;
use crate::index::view::IndexView;
use crate::index::{
    AnalysisContextKey, ArtifactId, ArtifactReaderCache, BucketIndex, ClassMetadata, ClassOrigin,
    ClasspathEntry, ClasspathId, IndexScope, IndexedArchiveData, IndexedJavaModule, ModuleGraph,
    ModuleId, ModuleIndex, NameTable, ScopeLayer, ScopeSnapshot, index_jar,
};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorkspaceIndexStats {
    pub module_count: usize,
    pub jar_cache_entries: usize,
    pub artifact_reader_entries: usize,
    pub scope_cache_entries: usize,
    pub classpath_jar_refs: usize,
    pub unique_bucket_count: usize,
    pub class_count: usize,
    pub java_module_count: usize,
    pub origin_count: usize,
    pub simple_name_entry_count: usize,
    pub package_entry_count: usize,
    pub owner_entry_count: usize,
    pub name_table_entries: usize,
    pub mro_cache_entries: usize,
    pub artifact_materialized_class_count: usize,
}

#[derive(Default)]
struct JdkIndexState {
    overlay: Option<Arc<BucketIndex>>,
    artifact_id: Option<ArtifactId>,
}

pub struct WorkspaceIndex {
    modules: DashMap<ModuleId, Arc<ModuleIndex>>,
    jdk: RwLock<JdkIndexState>,
    jar_cache: DashMap<Arc<str>, ClasspathEntry>,
    artifact_readers: Arc<ArtifactReaderCache>,
    scope_cache: DashMap<AnalysisContextKey, (u64, Arc<ScopeSnapshot>)>,
    graph: RwLock<ModuleGraph>,
    /// Version counter that increments on every mutation
    /// Used by Salsa to detect changes
    version: AtomicU64,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        let modules = DashMap::new();
        let root = Arc::new(ModuleIndex::new(ModuleId::ROOT, Arc::from("root")));
        modules.insert(ModuleId::ROOT, root);
        Self {
            modules,
            jdk: RwLock::new(JdkIndexState::default()),
            jar_cache: DashMap::new(),
            artifact_readers: Arc::new(ArtifactReaderCache::default()),
            scope_cache: DashMap::new(),
            graph: RwLock::new(ModuleGraph::new()),
            version: AtomicU64::new(0),
        }
    }

    /// Get the current version of the workspace index
    /// This increments whenever the index is mutated
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Increment the version counter (called on every mutation)
    fn increment_version(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    fn invalidate_analysis_caches(&self) {
        self.scope_cache.clear();
        self.increment_version();
    }

    fn invalidate_dependency_caches(&self) {
        self.scope_cache.clear();
        self.artifact_readers.clear();
        self.increment_version();
    }

    pub fn ensure_module(&self, id: ModuleId, name: Arc<str>) -> Arc<ModuleIndex> {
        match self.modules.entry(id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => Arc::clone(entry.get()),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let module = Arc::new(ModuleIndex::new(id, name));
                entry.insert(Arc::clone(&module));
                module
            }
        }
    }

    pub fn update_source(
        &self,
        scope: IndexScope,
        origin: ClassOrigin,
        classes: Vec<ClassMetadata>,
    ) -> bool {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        let changed = module.source.update_source(origin, classes);
        if changed {
            self.invalidate_analysis_caches();
        }
        changed
    }

    pub fn update_source_in_context(
        &self,
        module: ModuleId,
        source_root: Option<SourceRootId>,
        origin: ClassOrigin,
        classes: Vec<ClassMetadata>,
    ) -> bool {
        let module = self.ensure_module(module, default_module_name(module));
        let changed = module.update_source_in_root(source_root, origin, classes);
        if changed {
            self.invalidate_analysis_caches();
        }
        changed
    }

    pub fn remove_source_origin(&self, scope: IndexScope, origin: &ClassOrigin) -> bool {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        let changed = module.source.remove_by_origin(origin);
        if changed {
            self.invalidate_analysis_caches();
        }
        changed
    }

    pub fn remove_source_origin_in_context(
        &self,
        module: ModuleId,
        source_root: Option<SourceRootId>,
        origin: &ClassOrigin,
    ) -> bool {
        let module = self.ensure_module(module, default_module_name(module));
        let changed = module.remove_source_origin_in_root(source_root, origin);
        if changed {
            self.invalidate_analysis_caches();
        }
        changed
    }

    pub fn add_jdk_classes(&self, classes: Vec<ClassMetadata>) {
        let bucket = Arc::new(BucketIndex::new());
        bucket.add_archive_classes(classes);
        *self.jdk.write() = JdkIndexState {
            overlay: Some(bucket),
            artifact_id: None,
        };
        self.invalidate_dependency_caches();
    }

    pub fn add_jdk_archive(&self, data: IndexedArchiveData) {
        let bucket = Arc::new(BucketIndex::new());
        bucket.add_archive_classes(data.classes);
        bucket.add_modules(data.modules);
        *self.jdk.write() = JdkIndexState {
            overlay: Some(bucket),
            artifact_id: None,
        };
        self.invalidate_dependency_caches();
    }

    pub fn set_jdk_artifact(&self, artifact_id: ArtifactId) {
        *self.jdk.write() = JdkIndexState {
            overlay: None,
            artifact_id: Some(artifact_id),
        };
        self.invalidate_dependency_caches();
    }

    pub fn add_jar_classes(&self, scope: IndexScope, classes: Vec<ClassMetadata>) {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        let bucket = Arc::new(BucketIndex::new());
        bucket.add_archive_classes(classes);
        module.add_classpath_bucket(ClasspathId::Main, bucket);
        self.invalidate_analysis_caches();
    }

    pub fn get_or_index_jar(&self, path: Arc<str>) -> ClasspathEntry {
        if let Some(existing) = self.jar_cache.get(&path) {
            return existing.value().clone();
        }

        let resolved = match cache::load_cached_artifact(Path::new(path.as_ref())) {
            Some(artifact) => ClasspathEntry::Artifact(artifact.metadata.id),
            None => match index_jar(Path::new(path.as_ref())) {
                Ok(data) => match cache::save_cached_artifact(Path::new(path.as_ref()), &data) {
                    Some(artifact) => ClasspathEntry::Artifact(artifact.metadata.id),
                    None => {
                        tracing::warn!(
                            path = path.as_ref(),
                            "failed to persist jar artifact, falling back to in-memory bucket"
                        );
                        ClasspathEntry::Overlay(Self::bucket_from_archive_data(data))
                    }
                },
                Err(err) => {
                    tracing::warn!(path = path.as_ref(), error = %err, "failed to index jar");
                    ClasspathEntry::Overlay(Arc::new(BucketIndex::new()))
                }
            },
        };

        self.jar_cache.insert(Arc::clone(&path), resolved.clone());
        resolved
    }

    pub fn set_module_classpath(
        &self,
        module: ModuleId,
        classpath_id: ClasspathId,
        jar_paths: Vec<Arc<str>>,
    ) {
        let entries = jar_paths
            .iter()
            .map(|p| self.get_or_index_jar(Arc::clone(p)))
            .collect();
        let module = self.ensure_module(module, default_module_name(module));
        module.set_classpath(classpath_id, jar_paths, entries);
        self.invalidate_dependency_caches();
    }

    pub fn set_module_dependencies(&self, module: ModuleId, deps: Vec<ModuleId>) {
        self.graph.write().set_deps(module, deps);
        self.invalidate_analysis_caches();
    }

    pub fn register_module_source_roots(
        &self,
        module: ModuleId,
        roots: Vec<(SourceRootId, ClasspathId)>,
    ) {
        let module_index = self.ensure_module(module, default_module_name(module));
        module_index.set_source_roots(roots);
        self.invalidate_analysis_caches();
    }

    pub fn set_module_active_classpath(&self, module: ModuleId, classpath_id: ClasspathId) {
        let module_index = self.ensure_module(module, default_module_name(module));
        module_index.set_active_classpath(classpath_id);
        self.invalidate_analysis_caches();
    }

    pub fn module_classpath_jars(
        &self,
        module: ModuleId,
        classpath_id: ClasspathId,
    ) -> Vec<Arc<str>> {
        self.ensure_module(module, default_module_name(module))
            .classpath_jars(classpath_id)
    }

    pub fn module_source_package_names(
        &self,
        module: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> Vec<Arc<str>> {
        self.ensure_module(module, default_module_name(module))
            .visible_source_package_names(classpath_id, source_root)
    }

    pub fn module_classpath_summary(&self, module: ModuleId) -> Vec<(ClasspathId, Vec<Arc<str>>)> {
        self.ensure_module(module, default_module_name(module))
            .classpath_summary()
    }

    pub fn visible_bytecode_module_names_for_analysis_context(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> Vec<Arc<str>> {
        let mut names = BTreeSet::new();
        let scope = self.scope_for_analysis_context(module_id, classpath_id, source_root);
        for layer in scope.layers() {
            match layer {
                ScopeLayer::Overlay(bucket) => {
                    for name in bucket.module_names() {
                        names.insert(name);
                    }
                }
                ScopeLayer::Artifact(artifact_id) => {
                    if let Some(reader) = scope.artifact_reader(*artifact_id) {
                        for name in reader.module_names() {
                            names.insert(name);
                        }
                    }
                }
            }
        }
        names.into_iter().collect()
    }

    pub fn find_visible_bytecode_module_for_analysis_context(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
        module_name: &str,
    ) -> Option<Arc<IndexedJavaModule>> {
        let scope = self.scope_for_analysis_context(module_id, classpath_id, source_root);
        for layer in scope.layers() {
            let module = match layer {
                ScopeLayer::Overlay(bucket) => bucket.get_module(module_name),
                ScopeLayer::Artifact(artifact_id) => scope
                    .artifact_reader(*artifact_id)
                    .and_then(|reader| reader.get_module(module_name)),
            };
            if module.is_some() {
                return module;
            }
        }
        None
    }

    pub fn view(&self, scope: IndexScope) -> IndexView {
        self.view_for_classpath(scope, ClasspathId::Main)
    }

    pub fn view_for_classpath(&self, scope: IndexScope, classpath_id: ClasspathId) -> IndexView {
        self.view_for_analysis_context(scope.module, classpath_id, None)
    }

    pub fn scope(&self, scope: IndexScope) -> Arc<ScopeSnapshot> {
        self.scope_for_classpath(scope, ClasspathId::Main)
    }

    pub fn scope_for_classpath(
        &self,
        scope: IndexScope,
        classpath_id: ClasspathId,
    ) -> Arc<ScopeSnapshot> {
        self.scope_for_analysis_context(scope.module, classpath_id, None)
    }

    pub fn scope_for_analysis_context(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> Arc<ScopeSnapshot> {
        let key = (module_id, classpath_id, source_root);
        let started_version = self.version();
        if let Some(cached) = self.scope_cache.get(&key)
            && cached.value().0 == started_version
        {
            return Arc::clone(&cached.value().1);
        }

        let snapshot = Arc::new(self.build_scope_snapshot(module_id, classpath_id, source_root));
        if self.version() == started_version {
            self.scope_cache
                .insert(key, (started_version, Arc::clone(&snapshot)));
        }
        snapshot
    }

    pub fn view_for_analysis_context(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> IndexView {
        IndexView::from_scope(self.scope_for_analysis_context(module_id, classpath_id, source_root))
    }

    pub fn build_name_table(&self, scope: IndexScope) -> Arc<NameTable> {
        self.scope(scope).build_name_table()
    }

    pub fn build_name_table_for_classpath(
        &self,
        scope: IndexScope,
        classpath_id: ClasspathId,
    ) -> Arc<NameTable> {
        self.scope_for_classpath(scope, classpath_id)
            .build_name_table()
    }

    pub fn build_name_table_for_analysis_context(
        &self,
        module: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> Arc<NameTable> {
        self.scope_for_analysis_context(module, classpath_id, source_root)
            .build_name_table()
    }

    pub(crate) fn memory_stats(&self) -> WorkspaceIndexStats {
        let artifact_reader_stats = self.artifact_readers.memory_stats();
        let mut stats = WorkspaceIndexStats {
            module_count: self.modules.len(),
            jar_cache_entries: self.jar_cache.len(),
            artifact_reader_entries: artifact_reader_stats.reader_entries,
            scope_cache_entries: self.scope_cache.len(),
            class_count: artifact_reader_stats.class_count,
            java_module_count: artifact_reader_stats.module_count,
            simple_name_entry_count: artifact_reader_stats.simple_name_entry_count,
            package_entry_count: artifact_reader_stats.package_entry_count,
            owner_entry_count: artifact_reader_stats.owner_entry_count,
            artifact_materialized_class_count: artifact_reader_stats.materialized_class_count,
            ..WorkspaceIndexStats::default()
        };

        let mut seen_buckets = HashSet::new();
        let mut accumulate_bucket = |bucket: Arc<BucketIndex>| {
            let ptr = Arc::as_ptr(&bucket) as usize;
            if !seen_buckets.insert(ptr) {
                return;
            }

            let bucket_stats = bucket.stats();
            stats.unique_bucket_count += 1;
            stats.class_count += bucket_stats.class_count;
            stats.java_module_count += bucket_stats.module_count;
            stats.origin_count += bucket_stats.origin_count;
            stats.simple_name_entry_count += bucket_stats.simple_name_entry_count;
            stats.package_entry_count += bucket_stats.package_entry_count;
            stats.owner_entry_count += bucket_stats.owner_entry_count;
            stats.name_table_entries += bucket_stats.name_table_size;
            stats.mro_cache_entries += bucket_stats.mro_cache_entries;
        };

        if let Some(bucket) = self.jdk.read().overlay.as_ref() {
            accumulate_bucket(Arc::clone(bucket));
        }

        for module in &self.modules {
            let module = module.value();
            stats.classpath_jar_refs += module.classpath_jar_count();
            for bucket in module.all_overlay_bucket_refs() {
                accumulate_bucket(bucket);
            }
        }

        for cached in &self.jar_cache {
            if let ClasspathEntry::Overlay(bucket) = cached.value() {
                accumulate_bucket(Arc::clone(bucket));
            }
        }

        stats
    }

    pub(crate) fn clear_analysis_caches(&self) {
        self.scope_cache.clear();
        self.artifact_readers.clear();

        if let Some(bucket) = self.jdk.read().overlay.as_ref() {
            bucket.clear_query_caches();
        }

        for module in &self.modules {
            module.value().clear_query_caches();
        }
    }

    fn build_scope_snapshot(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> ScopeSnapshot {
        let module = self.ensure_module(module_id, default_module_name(module_id));
        let jar_paths = module.classpath_jars(classpath_id);
        let layers = self.build_scope_layers(module_id, classpath_id, source_root);

        tracing::debug!(
            module = module_id.0,
            requested_classpath = ?classpath_id,
            source_root = ?source_root.map(|id| id.0),
            module_classpath_jars = jar_paths.len(),
            layer_count = layers.len(),
            "building scope snapshot for analysis context"
        );

        ScopeSnapshot::new(
            module_id,
            classpath_id,
            source_root,
            layers,
            jar_paths,
            Arc::clone(&self.artifact_readers),
        )
    }

    fn build_scope_layers(
        &self,
        module_id: ModuleId,
        classpath_id: ClasspathId,
        source_root: Option<SourceRootId>,
    ) -> SmallVec<ScopeLayer, 8> {
        let module = self.ensure_module(module_id, default_module_name(module_id));

        let mut layers: SmallVec<ScopeLayer, 8> = SmallVec::new();
        for bucket in module.visible_source_layers(classpath_id, source_root) {
            layers.push(ScopeLayer::Overlay(bucket));
        }
        self.extend_classpath_layers(&mut layers, &module.classpath_entries(classpath_id));

        let graph = self.graph.read();
        let mut queue: VecDeque<ModuleId> = VecDeque::new();
        let mut seen: FxHashSet<ModuleId> = Default::default();
        seen.insert(module_id);
        queue.extend(graph.deps_of(module_id).iter().copied());

        while let Some(dep) = queue.pop_front() {
            if !seen.insert(dep) {
                continue;
            }
            let dep_module = self.ensure_module(dep, default_module_name(dep));
            for bucket in dep_module.visible_source_layers(ClasspathId::Main, None) {
                layers.push(ScopeLayer::Overlay(bucket));
            }
            self.extend_classpath_layers(
                &mut layers,
                &dep_module.classpath_entries(ClasspathId::Main),
            );
            queue.extend(graph.deps_of(dep).iter().copied());
        }

        self.extend_jdk_layer(&mut layers);
        layers
    }

    pub fn add_classes(&self, classes: Vec<ClassMetadata>) {
        self.add_jar_classes(
            IndexScope {
                module: ModuleId::ROOT,
            },
            classes,
        );
    }

    #[allow(dead_code)]
    pub fn graph_version(&self) -> u64 {
        self.graph.read().version()
    }

    #[cfg(test)]
    pub(crate) fn preload_artifact_reader(&self, reader: Arc<crate::index::ArtifactScopeReader>) {
        self.artifact_readers.insert_preloaded(reader);
    }
}

impl Default for WorkspaceIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceIndex {
    fn bucket_from_archive_data(data: IndexedArchiveData) -> Arc<BucketIndex> {
        let bucket = Arc::new(BucketIndex::new());
        bucket.add_archive_classes(data.classes);
        bucket.add_modules(data.modules);
        bucket
    }

    fn extend_classpath_layers(
        &self,
        layers: &mut SmallVec<ScopeLayer, 8>,
        entries: &[ClasspathEntry],
    ) {
        for entry in entries {
            match entry {
                ClasspathEntry::Artifact(artifact_id) => {
                    layers.push(ScopeLayer::Artifact(*artifact_id))
                }
                ClasspathEntry::Overlay(bucket) => {
                    layers.push(ScopeLayer::Overlay(Arc::clone(bucket)))
                }
            }
        }
    }

    fn extend_jdk_layer(&self, layers: &mut SmallVec<ScopeLayer, 8>) {
        let jdk = self.jdk.read();
        if let Some(artifact_id) = jdk.artifact_id {
            layers.push(ScopeLayer::Artifact(artifact_id));
        } else if let Some(bucket) = jdk.overlay.as_ref() {
            layers.push(ScopeLayer::Overlay(Arc::clone(bucket)));
        }
    }
}

fn default_module_name(id: ModuleId) -> Arc<str> {
    if id == ModuleId::ROOT {
        Arc::from("root")
    } else {
        Arc::from(format!("module-{}", id.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_integration::SourceRootId;
    use crate::index::{ArtifactScopeReader, FieldSummary, MethodParams, MethodSummary};
    use crate::language::java::module_info::JavaModuleDescriptor;
    use rust_asm::constants::ACC_PUBLIC;

    fn make_class(internal: &str, origin: ClassOrigin) -> ClassMetadata {
        let (pkg, name) = internal
            .rsplit_once('/')
            .map(|(p, n)| (Some(Arc::from(p)), Arc::from(n)))
            .unwrap_or((None, Arc::from(internal)));
        ClassMetadata {
            package: pkg,
            name,
            internal_name: Arc::from(internal),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: vec![],
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin,
        }
    }

    fn make_method(name: &str, desc: &str) -> MethodSummary {
        MethodSummary {
            name: Arc::from(name),
            params: MethodParams::from_method_descriptor(desc),
            annotations: vec![],
            access_flags: ACC_PUBLIC,
            is_synthetic: false,
            generic_signature: None,
            return_type: crate::semantic::types::parse_return_type_from_descriptor(desc),
        }
    }

    fn make_artifact_reader(
        artifact_id: ArtifactId,
        classes: Vec<ClassMetadata>,
        module_name: &str,
    ) -> Arc<ArtifactScopeReader> {
        Arc::new(ArtifactScopeReader::from_archive(
            crate::index::StoredArtifactArchive {
                metadata: crate::index::ArtifactMetadata {
                    id: artifact_id,
                    kind: crate::index::ArtifactKind::Jar,
                    source_path: "memory://artifact.jar".to_string(),
                    content_hash: 1,
                    byte_len: 1,
                    stored_at_unix_secs: 0,
                },
                classes: classes
                    .into_iter()
                    .map(crate::index::ArchiveClassStub::from_class_metadata)
                    .collect(),
                modules: vec![IndexedJavaModule {
                    descriptor: Arc::new(JavaModuleDescriptor {
                        name: Arc::from(module_name),
                        is_open: false,
                        requires: vec![],
                        exports: vec![],
                        opens: vec![],
                        uses: vec![],
                        provides: vec![],
                    }),
                    origin: ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
                }],
            },
        ))
    }

    #[test]
    fn test_close_cleanup_removes_temp_source_overlay_and_falls_back() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };

        let mut jdk_array = make_class(
            "java/util/ArrayList",
            ClassOrigin::Jar(Arc::from("jdk://builtin")),
        );
        jdk_array.interfaces.push(Arc::from("java/util/List"));
        jdk_array
            .methods
            .push(make_method("add", "(Ljava/lang/Object;)Z"));
        jdk_array
            .methods
            .push(make_method("add", "(ILjava/lang/Object;)V"));
        for method in &mut jdk_array.methods {
            if method.desc().as_ref() == "(Ljava/lang/Object;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILjava/lang/Object;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }

        let mut jdk_list = make_class(
            "java/util/List",
            ClassOrigin::Jar(Arc::from("jdk://builtin")),
        );
        jdk_list
            .methods
            .push(make_method("add", "(Ljava/lang/Object;)Z"));
        jdk_list
            .methods
            .push(make_method("add", "(ILjava/lang/Object;)V"));
        for method in &mut jdk_list.methods {
            if method.desc().as_ref() == "(Ljava/lang/Object;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILjava/lang/Object;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }

        idx.add_jdk_classes(vec![jdk_array, jdk_list]);

        let source_origin = ClassOrigin::SourceFile(Arc::from(
            "file:///tmp/java_analyzer_sources/java.base/java/util/ArrayList.java",
        ));
        let mut src_array = make_class("java/util/ArrayList", source_origin.clone());
        src_array.interfaces.push(Arc::from("java/util/List"));
        src_array.methods.push(make_method("add", "(LE;)Z"));
        src_array.methods.push(make_method("add", "(ILE;)V"));
        for method in &mut src_array.methods {
            if method.desc().as_ref() == "(LE;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILE;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }
        idx.update_source(scope, source_origin.clone(), vec![src_array]);

        let view_before = idx.view(scope);
        let (methods_before, _): (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>) =
            view_before.collect_inherited_members("java/util/ArrayList");
        let add_descs_before: Vec<_> = methods_before
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert!(
            add_descs_before.contains(&"(LE;)Z".to_string()),
            "expected source-shaped add family before cleanup: {:?}",
            add_descs_before
        );
        assert!(
            !add_descs_before.contains(&"(Ljava/lang/Object;)Z".to_string()),
            "view should not expose mixed add families simultaneously: {:?}",
            add_descs_before
        );

        idx.remove_source_origin(scope, &source_origin);

        let view_after = idx.view(scope);
        let (methods_after, _): (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>) =
            view_after.collect_inherited_members("java/util/ArrayList");
        let add_descs_after: Vec<_> = methods_after
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert!(
            !add_descs_after.iter().any(|d| d.contains("LE;")),
            "source overlay should be removed on close, got {:?}",
            add_descs_after
        );
        assert!(
            add_descs_after.contains(&"(Ljava/lang/Object;)Z".to_string()),
            "bytecode fallback should remain after cleanup: {:?}",
            add_descs_after
        );
    }

    #[test]
    fn test_analysis_context_view_uses_selected_root_visibility() {
        let idx = WorkspaceIndex::new();
        let module = ModuleId(1);
        idx.ensure_module(module, Arc::from("app"));
        idx.register_module_source_roots(
            module,
            vec![
                (SourceRootId(10), ClasspathId::Main),
                (SourceRootId(20), ClasspathId::Test),
            ],
        );

        let main_origin = ClassOrigin::SourceFile(Arc::from("file:///main/Foo.java"));
        let test_origin = ClassOrigin::SourceFile(Arc::from("file:///test/FooTest.java"));
        idx.update_source_in_context(
            module,
            Some(SourceRootId(10)),
            main_origin.clone(),
            vec![make_class("pkg/Foo", main_origin)],
        );
        idx.update_source_in_context(
            module,
            Some(SourceRootId(20)),
            test_origin.clone(),
            vec![make_class("pkg/FooTest", test_origin)],
        );

        let main_view =
            idx.view_for_analysis_context(module, ClasspathId::Main, Some(SourceRootId(10)));
        assert!(main_view.get_class("pkg/Foo").is_some());
        assert!(main_view.get_class("pkg/FooTest").is_none());

        let test_view =
            idx.view_for_analysis_context(module, ClasspathId::Test, Some(SourceRootId(20)));
        assert!(test_view.get_class("pkg/Foo").is_some());
        assert!(test_view.get_class("pkg/FooTest").is_some());
    }

    #[test]
    fn test_analysis_context_view_cache_reuses_name_table_until_mutation() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };

        idx.add_classes(vec![make_class("pkg/Alpha", ClassOrigin::Unknown)]);

        let view1 = idx.view(scope);
        let names1 = view1.build_name_table();
        let view2 = idx.view(scope);
        let names2 = view2.build_name_table();

        assert!(
            Arc::ptr_eq(&names1, &names2),
            "reused analysis context should share the cached name table"
        );

        let source_origin = ClassOrigin::SourceFile(Arc::from("file:///pkg/Beta.java"));
        idx.update_source(
            scope,
            source_origin.clone(),
            vec![make_class("pkg/Beta", source_origin)],
        );

        let view3 = idx.view(scope);
        let names3 = view3.build_name_table();

        assert!(
            !Arc::ptr_eq(&names1, &names3),
            "workspace mutations should invalidate cached analysis views"
        );
        assert!(names3.exists("pkg/Alpha"));
        assert!(names3.exists("pkg/Beta"));
    }

    #[test]
    fn test_visible_bytecode_module_lookup_uses_artifact_reader_layers() {
        let idx = WorkspaceIndex::new();
        let artifact_id = ArtifactId(11);
        idx.set_jdk_artifact(artifact_id);
        idx.preload_artifact_reader(make_artifact_reader(
            artifact_id,
            vec![make_class(
                "pkg/Dep",
                ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            )],
            "demo.module",
        ));

        let names = idx.visible_bytecode_module_names_for_analysis_context(
            ModuleId::ROOT,
            ClasspathId::Main,
            None,
        );
        assert_eq!(names, vec![Arc::from("demo.module")]);

        let module = idx
            .find_visible_bytecode_module_for_analysis_context(
                ModuleId::ROOT,
                ClasspathId::Main,
                None,
                "demo.module",
            )
            .expect("module from artifact reader");
        assert_eq!(module.name(), "demo.module");
        assert!(
            idx.view(IndexScope {
                module: ModuleId::ROOT
            })
            .get_class("pkg/Dep")
            .is_some()
        );
    }

    #[test]
    fn test_identical_source_update_does_not_bump_workspace_version() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let origin = ClassOrigin::SourceFile(Arc::from("file:///pkg/Alpha.java"));

        assert!(
            idx.update_source(
                scope,
                origin.clone(),
                vec![make_class("pkg/Alpha", origin.clone())]
            ),
            "initial source publish should mutate the workspace index",
        );
        let version_after_first_publish = idx.version();

        assert!(
            !idx.update_source(
                scope,
                origin.clone(),
                vec![make_class("pkg/Alpha", origin.clone())],
            ),
            "re-publishing the same extracted classes should be a no-op",
        );
        assert_eq!(
            idx.version(),
            version_after_first_publish,
            "workspace version should stay stable when the source structure is unchanged",
        );

        let mut changed = make_class("pkg/Alpha", origin.clone());
        changed.methods.push(make_method("ping", "()V"));
        assert!(
            idx.update_source(scope, origin, vec![changed]),
            "structural source changes should still invalidate cached analysis views",
        );
        assert!(
            idx.version() > version_after_first_publish,
            "workspace version should advance once the indexed class surface changes",
        );
    }
}
