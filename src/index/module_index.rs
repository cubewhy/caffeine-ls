use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::index::scope::{ClasspathId, ModuleId};
use crate::index::{BucketIndex, ClassMetadata, ClassOrigin};

pub struct ClasspathIndex {
    pub jars: Vec<Arc<str>>,
    pub buckets: Vec<Arc<BucketIndex>>,
}

impl ClasspathIndex {
    fn new(jars: Vec<Arc<str>>, buckets: Vec<Arc<BucketIndex>>) -> Self {
        Self { jars, buckets }
    }
}

pub struct ModuleQueryCache {
    by_internal: RwLock<HashMap<Arc<str>, Arc<ClassMetadata>>>,
}

impl ModuleQueryCache {
    fn new() -> Self {
        Self {
            by_internal: RwLock::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub fn get(&self, internal: &str) -> Option<Arc<ClassMetadata>> {
        self.by_internal.read().get(internal).cloned()
    }

    #[allow(dead_code)]
    pub fn insert(&self, internal: Arc<str>, class: Arc<ClassMetadata>) {
        self.by_internal.write().insert(internal, class);
    }
}

struct ModuleState {
    classpaths: HashMap<ClasspathId, ClasspathIndex>,
    active_classpath: ClasspathId,
    deps: Vec<ModuleId>,
}

pub struct ModuleIndex {
    pub id: ModuleId,
    pub name: Arc<str>,
    pub source: Arc<BucketIndex>,
    state: RwLock<ModuleState>,
    pub query_cache: ModuleQueryCache,
}

impl ModuleIndex {
    pub fn new(id: ModuleId, name: Arc<str>) -> Self {
        let state = ModuleState {
            classpaths: HashMap::new(),
            active_classpath: ClasspathId::Main,
            deps: Vec::new(),
        };
        Self {
            id,
            name,
            source: Arc::new(BucketIndex::new()),
            state: RwLock::new(state),
            query_cache: ModuleQueryCache::new(),
        }
    }

    pub fn update_source(&self, origin: ClassOrigin, classes: Vec<ClassMetadata>) {
        self.source.update_source(origin, classes);
    }

    pub fn set_classpath(
        &self,
        id: ClasspathId,
        jars: Vec<Arc<str>>,
        buckets: Vec<Arc<BucketIndex>>,
    ) {
        let mut state = self.state.write();
        state
            .classpaths
            .insert(id, ClasspathIndex::new(jars, buckets));
    }

    pub fn add_classpath_bucket(&self, id: ClasspathId, bucket: Arc<BucketIndex>) {
        let mut state = self.state.write();
        let entry = state
            .classpaths
            .entry(id)
            .or_insert_with(|| ClasspathIndex::new(Vec::new(), Vec::new()));
        entry.buckets.push(bucket);
    }

    pub fn set_active_classpath(&self, id: ClasspathId) {
        self.state.write().active_classpath = id;
    }

    pub fn active_classpath_layers(&self) -> Vec<Arc<BucketIndex>> {
        let state = self.state.read();
        state
            .classpaths
            .get(&state.active_classpath)
            .map(|cp| cp.buckets.clone())
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub fn set_deps(&self, deps: Vec<ModuleId>) {
        self.state.write().deps = deps;
    }
}
