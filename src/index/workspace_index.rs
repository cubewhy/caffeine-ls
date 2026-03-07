use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::index::{
    index_jar, BucketIndex, ClassMetadata, ClassOrigin, ClasspathId, IndexScope, ModuleGraph,
    ModuleId, ModuleIndex, NameTable,
};
use crate::index::view::IndexView;

pub struct WorkspaceIndex {
    modules: DashMap<ModuleId, Arc<ModuleIndex>>,
    jdk: Arc<BucketIndex>,
    jar_cache: DashMap<Arc<str>, Arc<BucketIndex>>,
    graph: RwLock<ModuleGraph>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        let modules = DashMap::new();
        let root = Arc::new(ModuleIndex::new(ModuleId::ROOT, Arc::from("root")));
        modules.insert(ModuleId::ROOT, root);
        Self {
            modules,
            jdk: Arc::new(BucketIndex::new()),
            jar_cache: DashMap::new(),
            graph: RwLock::new(ModuleGraph::new()),
        }
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
    ) {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        module.update_source(origin, classes);
    }

    pub fn remove_source_origin(&self, scope: IndexScope, origin: &ClassOrigin) {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        module.source.remove_by_origin(origin);
    }

    pub fn add_jdk_classes(&self, classes: Vec<ClassMetadata>) {
        self.jdk.add_classes(classes);
    }

    pub fn add_jar_classes(&self, scope: IndexScope, classes: Vec<ClassMetadata>) {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));
        let bucket = Arc::new(BucketIndex::new());
        bucket.add_classes(classes);
        module.add_classpath_bucket(ClasspathId::Main, bucket);
    }

    pub fn get_or_index_jar(&self, path: Arc<str>) -> Arc<BucketIndex> {
        if let Some(existing) = self.jar_cache.get(&path) {
            return Arc::clone(existing.value());
        }

        let bucket = Arc::new(BucketIndex::new());
        match index_jar(Path::new(path.as_ref())) {
            Ok(classes) => bucket.add_classes(classes),
            Err(err) => {
                tracing::warn!(path = path.as_ref(), error = %err, "failed to index jar");
            }
        }

        self.jar_cache.insert(Arc::clone(&path), Arc::clone(&bucket));
        bucket
    }

    pub fn set_module_classpath(
        &self,
        module: ModuleId,
        classpath_id: ClasspathId,
        jar_paths: Vec<Arc<str>>,
    ) {
        let buckets = jar_paths
            .iter()
            .map(|p| self.get_or_index_jar(Arc::clone(p)))
            .collect();
        let module = self.ensure_module(module, default_module_name(module));
        module.set_classpath(classpath_id, jar_paths, buckets);
    }

    pub fn view(&self, scope: IndexScope) -> IndexView {
        let module = self.ensure_module(scope.module, default_module_name(scope.module));

        let mut layers: SmallVec<Arc<BucketIndex>, 8> = SmallVec::new();
        layers.push(Arc::clone(&module.source));
        for bucket in module.active_classpath_layers() {
            layers.push(bucket);
        }

        let graph = self.graph.read();
        let mut queue: VecDeque<ModuleId> = VecDeque::new();
        let mut seen: FxHashSet<ModuleId> = Default::default();
        seen.insert(scope.module);
        queue.extend(graph.deps_of(scope.module).iter().copied());

        while let Some(dep) = queue.pop_front() {
            if !seen.insert(dep) {
                continue;
            }
            let dep_module = self.ensure_module(dep, default_module_name(dep));
            layers.push(Arc::clone(&dep_module.source));
            for bucket in dep_module.active_classpath_layers() {
                layers.push(bucket);
            }
            queue.extend(graph.deps_of(dep).iter().copied());
        }

        layers.push(Arc::clone(&self.jdk));
        IndexView::new(layers)
    }

    pub fn build_name_table(&self, scope: IndexScope) -> Arc<NameTable> {
        self.view(scope).build_name_table()
    }

    pub fn add_classes(&self, classes: Vec<ClassMetadata>) {
        self.add_jar_classes(IndexScope { module: ModuleId::ROOT }, classes);
    }

    #[allow(dead_code)]
    pub fn graph_version(&self) -> u64 {
        self.graph.read().version()
    }
}

impl Default for WorkspaceIndex {
    fn default() -> Self {
        Self::new()
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
    use crate::index::{FieldSummary, MethodParams, MethodSummary};
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
        jdk_array.methods.push(make_method("add", "(Ljava/lang/Object;)Z"));
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

        let mut jdk_list = make_class("java/util/List", ClassOrigin::Jar(Arc::from("jdk://builtin")));
        jdk_list.methods.push(make_method("add", "(Ljava/lang/Object;)Z"));
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
}
