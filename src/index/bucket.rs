use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use nucleo::Nucleo;
use nucleo::pattern::{CaseMatching, Normalization};
use parking_lot::RwLock;
use rustc_hash::FxHashSet;

use crate::index::{
    ClassMetadata, ClassOrigin, FieldSummary, MethodSummary, NameTable, intern_str,
};

type MroCacheMap = HashMap<Arc<str>, (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>)>;

struct BucketState {
    classes: HashMap<Arc<str>, Arc<ClassMetadata>>,
    by_origin: HashMap<ClassOrigin, Vec<Arc<str>>>,
    simple_name_index: HashMap<Arc<str>, Vec<Arc<ClassMetadata>>>,
    package_index: HashMap<Arc<str>, Vec<Arc<ClassMetadata>>>,
    name_table: NameTable,
    mro_cache: MroCacheMap,
    fuzzy_matcher: Nucleo<Arc<str>>,
}

pub struct BucketIndex {
    inner: RwLock<BucketState>,
}

impl BucketIndex {
    pub fn new() -> Self {
        let waker = Arc::new(|| {});
        let state = BucketState {
            classes: HashMap::with_capacity(100_000),
            by_origin: HashMap::new(),
            simple_name_index: HashMap::with_capacity(100_000),
            package_index: HashMap::with_capacity(10_000),
            name_table: NameTable(FxHashSet::default()),
            mro_cache: HashMap::new(),
            fuzzy_matcher: Nucleo::new(nucleo::Config::DEFAULT, waker, None, 1),
        };

        Self {
            inner: RwLock::new(state),
        }
    }

    pub fn add_classes(&self, classes: Vec<ClassMetadata>) {
        let mut inner = self.inner.write();
        let injector = inner.fuzzy_matcher.injector();

        for mut class in classes {
            Self::intern_class(&mut class);

            let internal = Arc::clone(&class.internal_name);
            let simple = Arc::clone(&class.name);
            let pkg = class.package.clone();
            let origin = class.origin.clone();
            let rc = Arc::new(class);

            inner.classes.insert(Arc::clone(&internal), Arc::clone(&rc));
            inner
                .simple_name_index
                .entry(Arc::clone(&simple))
                .or_default()
                .push(Arc::clone(&rc));

            if let Some(p) = pkg {
                inner
                    .package_index
                    .entry(Arc::clone(&p))
                    .or_default()
                    .push(Arc::clone(&rc));
            }

            inner
                .by_origin
                .entry(origin)
                .or_default()
                .push(Arc::clone(&internal));

            inner.name_table.0.insert(Arc::clone(&internal));

            injector.push(simple, |item: &Arc<str>, cols| {
                cols[0] = item.as_ref().into();
            });
        }

        inner.mro_cache.clear();
    }

    pub fn update_source(&self, origin: ClassOrigin, classes: Vec<ClassMetadata>) {
        let mut filtered = Vec::new();

        for c in classes {
            if let Some(existing) = self.get_class(&c.internal_name)
                && matches!(existing.origin, ClassOrigin::Jar(_))
            {
                // the LSP only trusts .class
                tracing::warn!(class = %c.internal_name, "BLOCKED: Prevented source AST from corrupting bytecode index!");
                continue;
            }

            filtered.push(c);
        }

        if filtered.is_empty() {
            return;
        }

        self.remove_by_origin(&origin);
        self.add_classes(filtered);
    }

    pub fn get_class(&self, internal_name: &str) -> Option<Arc<ClassMetadata>> {
        let inner = self.inner.read();
        inner.classes.get(internal_name).cloned()
    }

    pub fn get_source_type_name(&self, internal: &str) -> Option<String> {
        self.get_class(internal).map(|meta| meta.source_name())
    }

    pub fn get_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassMetadata>> {
        let inner = self.inner.read();
        inner
            .simple_name_index
            .get(simple_name)
            .cloned()
            .unwrap_or_default()
    }

    pub fn classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        let normalized = pkg.replace('.', "/");
        let inner = self.inner.read();
        inner
            .package_index
            .get(normalized.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn has_package(&self, pkg: &str) -> bool {
        let normalized = pkg.replace('.', "/");
        let inner = self.inner.read();
        inner.package_index.contains_key(normalized.as_str())
    }

    pub fn has_classes_in_package(&self, pkg: &str) -> bool {
        let normalized = pkg.replace('.', "/");
        let inner = self.inner.read();
        inner
            .package_index
            .get(normalized.as_str())
            .is_some_and(|v| !v.is_empty())
    }

    pub fn resolve_imports(&self, imports: &[Arc<str>]) -> Vec<Arc<ClassMetadata>> {
        let mut result = Vec::new();
        for import in imports {
            if import.ends_with(".*") {
                let pkg = import.trim_end_matches(".*").replace('.', "/");
                let classes = self.classes_in_package(&pkg);
                tracing::debug!(
                    import = import.as_ref(),
                    pkg,
                    count = classes.len(),
                    "wildcard import expanded"
                );
                result.extend(classes.into_iter());
            } else {
                let internal = import.replace('.', "/");
                tracing::debug!(import = import.as_ref(), internal, "exact import lookup");
                if let Some(cls) = self.get_class(&internal) {
                    tracing::debug!(internal, "exact import found");
                    result.push(cls);
                } else {
                    tracing::debug!(internal, "exact import NOT FOUND");
                }
            }
        }
        result
    }

    pub fn collect_inherited_members(
        &self,
        class_internal: &str,
    ) -> (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>) {
        if let Some(cached) = {
            let inner = self.inner.read();
            inner.mro_cache.get(class_internal).cloned()
        } {
            return cached;
        }

        let mut methods: Vec<Arc<MethodSummary>> = Vec::new();
        let mut fields: Vec<Arc<FieldSummary>> = Vec::new();
        let mut seen_methods: FxHashSet<(Arc<str>, Arc<str>)> = Default::default();
        let mut seen_fields: FxHashSet<Arc<str>> = Default::default();
        let mut seen_classes: FxHashSet<Arc<str>> = Default::default();
        let mut queue: VecDeque<Arc<str>> = Default::default();

        queue.push_back(Arc::from(class_internal));

        while let Some(internal) = queue.pop_front() {
            if !seen_classes.insert(Arc::clone(&internal)) {
                continue;
            }

            let meta = match self.get_class(&internal) {
                Some(m) => m,
                None => continue,
            };

            for method in &meta.methods {
                let key = (Arc::clone(&method.name), Arc::clone(&method.desc()));
                if seen_methods.insert(key) {
                    methods.push(Arc::new(method.clone()));
                }
            }
            for field in &meta.fields {
                if seen_fields.insert(Arc::clone(&field.name)) {
                    fields.push(Arc::new(field.clone()));
                }
            }

            if let Some(ref super_name) = meta.super_name
                && !super_name.is_empty()
            {
                queue.push_back(super_name.clone());
            }
            for iface in &meta.interfaces {
                if !iface.is_empty() {
                    queue.push_back(Arc::clone(iface));
                }
            }
        }
        let result = (methods, fields);
        self.inner
            .write()
            .mro_cache
            .insert(Arc::from(class_internal), result.clone());

        result
    }

    pub fn mro(&self, class_internal: &str) -> Vec<Arc<ClassMetadata>> {
        let mut result = Vec::new();
        let mut seen: std::collections::HashSet<Arc<str>> = std::collections::HashSet::new();
        let mut queue: VecDeque<Arc<str>> = VecDeque::new();

        queue.push_back(Arc::from(class_internal));
        while let Some(internal) = queue.pop_front() {
            if !seen.insert(internal.clone()) {
                continue;
            }
            let meta = match self.get_class(&internal) {
                Some(m) => m,
                None => continue,
            };
            if let Some(ref super_name) = meta.super_name
                && !super_name.is_empty()
            {
                queue.push_back(super_name.clone());
            }
            for iface in &meta.interfaces {
                if !iface.is_empty() {
                    queue.push_back(iface.clone());
                }
            }
            result.push(meta);
        }
        result
    }

    pub fn fuzzy_autocomplete(&self, query: &str, limit: usize) -> Vec<Arc<str>> {
        let mut inner = self.inner.write();
        inner.fuzzy_matcher.pattern.reparse(
            0,
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
        let _status = inner.fuzzy_matcher.tick(10);
        let snapshot = inner.fuzzy_matcher.snapshot();
        let count = snapshot.matched_item_count();
        let end_bound = (limit as u32).min(count);
        snapshot
            .matched_items(..end_bound)
            .map(|item| Arc::clone(item.data))
            .collect()
    }

    pub fn fuzzy_search_classes(&self, query: &str, limit: usize) -> Vec<Arc<ClassMetadata>> {
        if query.is_empty() {
            return self.iter_all_classes().into_iter().take(limit).collect();
        }
        let simple_names = self.fuzzy_autocomplete(query, limit);
        simple_names
            .into_iter()
            .flat_map(|name| self.get_classes_by_simple_name(&name).into_iter())
            .collect()
    }

    pub fn exact_match_keys(&self) -> Vec<Arc<str>> {
        let inner = self.inner.read();
        inner.classes.keys().cloned().collect()
    }

    pub fn class_count(&self) -> usize {
        let inner = self.inner.read();
        inner.classes.len()
    }

    pub fn iter_all_classes(&self) -> Vec<Arc<ClassMetadata>> {
        let inner = self.inner.read();
        inner.classes.values().cloned().collect()
    }

    pub fn build_name_table(&self) -> Arc<NameTable> {
        let inner = self.inner.read();
        Arc::new(inner.name_table.clone())
    }

    pub fn remove_by_origin(&self, origin: &ClassOrigin) {
        let mut inner = self.inner.write();
        let internals = match inner.by_origin.remove(origin) {
            Some(v) => v,
            None => return,
        };

        for internal in &internals {
            if let Some(meta) = inner.classes.remove(internal) {
                if let Some(v) = inner.simple_name_index.get_mut(&meta.name) {
                    v.retain(|meta: &Arc<ClassMetadata>| meta.internal_name != *internal);
                    if v.is_empty() {
                        inner.simple_name_index.remove(&meta.name);
                    }
                }
                if let Some(pkg) = &meta.package
                    && let Some(v) = inner.package_index.get_mut(pkg)
                {
                    v.retain(|meta: &Arc<ClassMetadata>| meta.internal_name != *internal);
                    if v.is_empty() {
                        inner.package_index.remove(pkg);
                    }
                }
            }
        }

        inner.mro_cache.clear();
        Self::rebuild_fuzzy_locked(&mut inner);
        Self::rebuild_name_table_locked(&mut inner);
    }

    fn rebuild_fuzzy_locked(inner: &mut BucketState) {
        let waker = Arc::new(|| {});
        inner.fuzzy_matcher = Nucleo::new(nucleo::Config::DEFAULT, waker, None, 1);
        let injector = inner.fuzzy_matcher.injector();
        for name in inner.simple_name_index.keys() {
            let n = Arc::clone(name);
            injector.push(n, |item: &Arc<str>, cols| {
                cols[0] = item.as_ref().into();
            });
        }
    }

    fn rebuild_name_table_locked(inner: &mut BucketState) {
        inner.name_table = NameTable(inner.classes.keys().cloned().collect());
    }

    fn intern_class(class: &mut ClassMetadata) {
        class.name = intern_str(&class.name);
        class.internal_name = intern_str(&class.internal_name);
        if let Some(pkg) = &class.package {
            class.package = Some(intern_str(pkg));
        }
        for m in &mut class.methods {
            m.name = intern_str(&m.name);
            if let Some(rt) = &m.return_type {
                m.return_type = Some(intern_str(rt));
            }
            if let Some(gs) = &m.generic_signature {
                m.generic_signature = Some(intern_str(gs));
            }
        }
        for f in &mut class.fields {
            f.name = intern_str(&f.name);
            f.descriptor = intern_str(&f.descriptor);
        }

        match &class.origin {
            ClassOrigin::Jar(j) => {
                class.origin = ClassOrigin::Jar(intern_str(j));
            }
            ClassOrigin::SourceFile(s) => {
                class.origin = ClassOrigin::SourceFile(intern_str(s));
            }
            ClassOrigin::ZipSource {
                zip_path,
                entry_name,
            } => {
                class.origin = ClassOrigin::ZipSource {
                    zip_path: intern_str(zip_path),
                    entry_name: intern_str(entry_name),
                };
            }
            _ => {
                tracing::error!("Unknown class source found for class {}", class.name);
            }
        }
    }
}

impl Default for BucketIndex {
    fn default() -> Self {
        Self::new()
    }
}
