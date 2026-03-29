use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::index::{
    ArtifactClassHandle, BucketIndex, ClassMetadata, FieldSummary, MethodSummary, NameTable,
    ScopeLayer, ScopeSnapshot,
};
use crate::request_metrics::RequestMetrics;

#[derive(Clone)]
enum ClassHandleRef {
    Overlay {
        bucket: Arc<BucketIndex>,
        internal_name: Arc<str>,
    },
    Artifact(ArtifactClassHandle),
}

#[derive(Default)]
struct IndexViewCaches {
    class_by_internal: DashMap<Arc<str>, Option<ClassHandleRef>>,
    source_type_names: DashMap<Arc<str>, Arc<str>>,
    classes_by_simple_name: DashMap<Arc<str>, Arc<Vec<ClassHandleRef>>>,
    classes_in_package: DashMap<Arc<str>, Arc<Vec<ClassHandleRef>>>,
    direct_inner_classes: DashMap<Arc<str>, Arc<Vec<ClassHandleRef>>>,
    hierarchy_order: DashMap<Arc<str>, Arc<Vec<ClassHandleRef>>>,
    methods_by_name: DashMap<(Arc<str>, Arc<str>), Arc<Vec<Arc<MethodSummary>>>>,
    fields_by_name: DashMap<(Arc<str>, Arc<str>), Option<Arc<FieldSummary>>>,
    declaring_method_owner: DashMap<(Arc<str>, Arc<str>, Arc<str>), Option<Arc<str>>>,
    all_classes: OnceLock<Arc<Vec<ClassHandleRef>>>,
}

#[derive(Clone)]
pub struct IndexView {
    scope: Arc<ScopeSnapshot>,
    caches: Arc<IndexViewCaches>,
    request_metrics: Option<Arc<RequestMetrics>>,
}

impl IndexView {
    fn layers(&self) -> &[ScopeLayer] {
        self.scope.layers()
    }

    fn overlay_ref(bucket: &Arc<BucketIndex>, class: Arc<ClassMetadata>) -> ClassHandleRef {
        ClassHandleRef::Overlay {
            bucket: Arc::clone(bucket),
            internal_name: Arc::clone(&class.internal_name),
        }
    }

    fn overlay_refs(
        bucket: &Arc<BucketIndex>,
        classes: Vec<Arc<ClassMetadata>>,
    ) -> Vec<ClassHandleRef> {
        classes
            .into_iter()
            .map(|class| Self::overlay_ref(bucket, class))
            .collect()
    }

    fn merge_class_refs<F>(&self, mut fetch: F) -> Vec<ClassHandleRef>
    where
        F: FnMut(&ScopeLayer) -> Vec<ClassHandleRef>,
    {
        let mut positions: FxHashMap<Arc<str>, usize> = Default::default();
        let mut merged = Vec::new();
        for layer in self.layers() {
            for class_ref in fetch(layer) {
                let Some(key) = self.class_ref_internal_name(&class_ref) else {
                    continue;
                };
                if let Some(existing) = positions.get(key.as_ref()).copied() {
                    if self.should_replace_ref(&merged[existing], &class_ref) {
                        merged[existing] = class_ref;
                    }
                } else {
                    positions.insert(Arc::clone(&key), merged.len());
                    merged.push(class_ref);
                }
            }
        }
        merged
    }

    fn resolve_class_refs(&self, class_refs: &[ClassHandleRef]) -> Vec<Arc<ClassMetadata>> {
        class_refs
            .iter()
            .filter_map(|class_ref| self.resolve_class_ref(class_ref))
            .collect()
    }

    fn get_class_ref_by_internal(&self, internal_name: &str) -> Option<ClassHandleRef> {
        if let Some(cached) = self.caches.class_by_internal.get(internal_name) {
            return cached.value().clone();
        }

        let mut best: Option<ClassHandleRef> = None;
        for layer in self.layers() {
            let Some(candidate) = self.layer_get_class_ref(layer, internal_name) else {
                continue;
            };
            if let Some(current) = &best {
                if self.should_replace_ref(current, &candidate) {
                    best = Some(candidate);
                }
            } else {
                best = Some(candidate);
            }
        }
        self.caches
            .class_by_internal
            .insert(Arc::from(internal_name), best.clone());
        best
    }

    fn hierarchy_order(&self, class_internal: &str) -> Arc<Vec<ClassHandleRef>> {
        if let Some(cached) = self.caches.hierarchy_order.get(class_internal) {
            return Arc::clone(cached.value());
        }

        let mut order = Vec::new();
        let mut seen: FxHashSet<Arc<str>> = Default::default();
        let mut queue: VecDeque<Arc<str>> = VecDeque::new();

        queue.push_back(Arc::from(class_internal));
        while let Some(internal) = queue.pop_front() {
            if !seen.insert(Arc::clone(&internal)) {
                continue;
            }
            let class_ref = match self.get_class_ref_by_internal(&internal) {
                Some(class_ref) => class_ref,
                None => continue,
            };

            order.push(class_ref.clone());

            for parent in self.class_ref_parent_internals(&class_ref) {
                if !parent.is_empty() {
                    queue.push_back(parent);
                }
            }
        }

        let order = Arc::new(order);
        self.caches
            .hierarchy_order
            .insert(Arc::from(class_internal), Arc::clone(&order));
        order
    }

    fn project_hierarchy_classes(
        &self,
        hierarchy_order: &[ClassHandleRef],
    ) -> Vec<Arc<ClassMetadata>> {
        let mut result = Vec::new();
        let mut seen_methods: FxHashSet<(Arc<str>, Arc<str>)> = Default::default();
        let mut seen_fields: FxHashSet<Arc<str>> = Default::default();

        for class_ref in hierarchy_order {
            let Some(meta) = self.resolve_class_ref(class_ref) else {
                continue;
            };

            let mut projected = (*meta).clone();
            projected
                .methods
                .retain(|method| seen_methods.insert(Self::method_shadow_key(method)));
            projected
                .fields
                .retain(|field| seen_fields.insert(Arc::clone(&field.name)));
            result.push(Arc::new(projected));
        }

        result
    }

    fn resolve_internal_hint_to_class(&self, internal_hint: &str) -> Option<Arc<ClassMetadata>> {
        let (pkg, simple) = internal_hint.rsplit_once('/')?;
        let package_refs = self.classes_in_package_refs(pkg);
        let mut matches = package_refs
            .iter()
            .filter(|class_ref| self.class_ref_matches_internal_name_tail(class_ref, simple));
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        self.resolve_class_ref(first)
    }

    fn origin_precedence(class: &ClassMetadata) -> u8 {
        match class.origin {
            crate::index::ClassOrigin::SourceFile(_) => 2,
            _ => 1,
        }
    }

    fn should_replace(current: &Arc<ClassMetadata>, candidate: &Arc<ClassMetadata>) -> bool {
        Self::origin_precedence(candidate) > Self::origin_precedence(current)
    }

    fn should_replace_ref(&self, current: &ClassHandleRef, candidate: &ClassHandleRef) -> bool {
        self.class_ref_origin_precedence(candidate) > self.class_ref_origin_precedence(current)
    }

    fn method_shadow_key(method: &MethodSummary) -> (Arc<str>, Arc<str>) {
        (
            Arc::clone(&method.name),
            method
                .generic_signature
                .clone()
                .unwrap_or_else(|| method.desc()),
        )
    }

    pub fn new(layers: SmallVec<Arc<BucketIndex>, 8>) -> Self {
        Self::from_scope(Arc::new(ScopeSnapshot::from_layers(layers)))
    }

    pub fn from_scope(scope: Arc<ScopeSnapshot>) -> Self {
        Self {
            scope,
            caches: Arc::new(IndexViewCaches::default()),
            request_metrics: None,
        }
    }

    pub fn with_request_metrics(mut self, metrics: Arc<RequestMetrics>) -> Self {
        self.request_metrics = Some(metrics);
        self
    }

    pub fn with_overlay_classes(&self, classes: Vec<ClassMetadata>) -> Self {
        if classes.is_empty() {
            return self.clone();
        }

        let overlay = Arc::new(BucketIndex::new());
        overlay.add_classes(classes);

        Self::from_scope(Arc::new(self.scope.with_prepended_overlay(overlay)))
    }

    /// Get the number of layers in this view
    pub fn layer_count(&self) -> usize {
        self.scope.layer_count()
    }

    pub fn get_class(&self, internal_name: &str) -> Option<Arc<ClassMetadata>> {
        self.get_class_ref_by_internal(internal_name)
            .and_then(|class_ref| self.resolve_class_ref(&class_ref))
    }

    pub fn get_source_type_name(&self, internal: &str) -> Option<String> {
        if let Some(cached) = self.caches.source_type_names.get(internal) {
            return Some(cached.value().to_string());
        }

        let class = self.get_class(internal)?;
        let source_name =
            class.qualified_source_name_with(|owner_internal| self.get_class(owner_internal));
        self.caches
            .source_type_names
            .insert(Arc::from(internal), Arc::from(source_name.as_str()));
        Some(source_name)
    }

    /// Resolve a simple inner-class name within the current enclosing-class scope.
    /// Uses `inner_class_of` metadata as the primary relation source.
    pub fn resolve_scoped_inner_class(
        &self,
        enclosing_internal: &str,
        simple_name: &str,
    ) -> Option<Arc<ClassMetadata>> {
        let enclosing = self
            .get_class(enclosing_internal)
            .or_else(|| self.resolve_internal_hint_to_class(enclosing_internal))?;
        let enclosing_pkg = enclosing.package.clone();

        let mut scope_chain: Vec<Arc<str>> = vec![Arc::clone(&enclosing.internal_name)];
        let mut cur = enclosing;
        let mut depth = 0usize;
        while let Some(parent_internal) = cur.inner_class_of.clone() {
            scope_chain.push(Arc::clone(&parent_internal));
            let Some(parent) = self.get_class(parent_internal.as_ref()) else {
                break;
            };
            cur = parent;
            depth += 1;
            if depth > 64 {
                break;
            }
        }

        let candidates = if let Some(pkg) = enclosing_pkg.as_deref() {
            self.classes_in_package(pkg)
        } else {
            self.get_classes_by_simple_name(simple_name)
        };

        let mut best: Option<(usize, Arc<ClassMetadata>)> = None;
        for class in candidates {
            if class.package != enclosing_pkg {
                continue;
            }
            if !class.matches_simple_name(simple_name) {
                continue;
            }
            if let Some(parent) = class.inner_class_of.clone()
                && let Some(pos) = scope_chain
                    .iter()
                    .position(|n| n.as_ref() == parent.as_ref())
            {
                match &best {
                    Some((best_pos, _)) if *best_pos <= pos => {}
                    _ => best = Some((pos, class)),
                }
            }
        }

        best.map(|(_, c)| c)
    }

    pub fn get_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassMetadata>> {
        if let Some(cached) = self.caches.classes_by_simple_name.get(simple_name) {
            return self.resolve_class_refs(cached.value().as_ref());
        }

        let merged =
            Arc::new(self.merge_class_refs(|layer| {
                self.layer_classes_by_simple_name_refs(layer, simple_name)
            }));
        self.caches
            .classes_by_simple_name
            .insert(Arc::from(simple_name), Arc::clone(&merged));
        self.resolve_class_refs(merged.as_ref())
    }

    pub fn classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        let refs = self.classes_in_package_refs(pkg);
        self.resolve_class_refs(refs.as_ref())
    }

    /// Returns classes directly declared in the package (excludes nested/inner classes).
    /// Ownership is determined by authoritative `inner_class_of` metadata.
    pub fn top_level_classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        self.classes_in_package(pkg)
            .into_iter()
            .filter(|c| c.inner_class_of.is_none())
            .collect()
    }

    /// Returns direct nested classes whose owner is `owner_internal`.
    /// Ownership is determined by authoritative `inner_class_of` metadata.
    pub fn direct_inner_classes_of(&self, owner_internal: &str) -> Vec<Arc<ClassMetadata>> {
        let key = Arc::from(owner_internal);
        if let Some(cached) = self.caches.direct_inner_classes.get(&key) {
            return self.resolve_class_refs(cached.value().as_ref());
        }

        let merged =
            Arc::new(self.merge_class_refs(|layer| {
                self.layer_direct_inner_class_refs(layer, owner_internal)
            }));
        self.caches
            .direct_inner_classes
            .insert(Arc::clone(&key), Arc::clone(&merged));
        self.resolve_class_refs(merged.as_ref())
    }

    /// Resolves a direct nested class by simple name under `owner_internal`.
    pub fn resolve_direct_inner_class(
        &self,
        owner_internal: &str,
        simple_name: &str,
    ) -> Option<Arc<ClassMetadata>> {
        let mut best: Option<ClassHandleRef> = None;
        for layer in self.layers() {
            for candidate in self.layer_direct_inner_class_refs(layer, owner_internal) {
                if !self.class_ref_matches_simple_name(&candidate, simple_name) {
                    continue;
                }
                if let Some(current) = &best {
                    if self.should_replace_ref(current, &candidate) {
                        best = Some(candidate);
                    }
                } else {
                    best = Some(candidate);
                }
            }
        }
        best.and_then(|class_ref| self.resolve_class_ref(&class_ref))
    }

    /// Resolves the direct owner of `class_internal` using authoritative `inner_class_of`
    /// metadata. Returns `None` when ownership cannot be proven.
    pub fn resolve_owner_class(&self, class_internal: &str) -> Option<Arc<ClassMetadata>> {
        let class = self.get_class(class_internal)?;
        let owner_internal = class.inner_class_of.as_deref()?;
        self.get_class(owner_internal)
    }

    /// Resolve a potentially-qualified nested type path (e.g. `Outer.Inner`, `a.b.Outer.Inner`)
    /// by first resolving a head owner type, then following direct nested-owner edges.
    /// Ownership is resolved only through authoritative `inner_class_of` metadata.
    pub fn resolve_qualified_type_path(
        &self,
        path: &str,
        resolve_head: &dyn Fn(&str) -> Option<Arc<str>>,
    ) -> Option<Arc<ClassMetadata>> {
        let text = path.trim();
        if text.is_empty() {
            return None;
        }
        if text.contains('/') {
            return self.get_class(text);
        }
        let parts: Vec<&str> = text.split('.').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return None;
        }

        for split in (1..=parts.len()).rev() {
            let head = parts[..split].join(".");
            let Some(mut current_internal) = resolve_head(&head) else {
                continue;
            };

            let mut ok = true;
            for seg in &parts[split..] {
                let Some(inner) = self.resolve_direct_inner_class(&current_internal, seg) else {
                    ok = false;
                    break;
                };
                current_internal = Arc::clone(&inner.internal_name);
            }

            if ok && let Some(meta) = self.get_class(&current_internal) {
                return Some(meta);
            }
        }
        None
    }

    pub fn has_package(&self, pkg: &str) -> bool {
        self.layers().iter().any(|layer| match layer {
            ScopeLayer::Overlay(bucket) => bucket.has_package(pkg),
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .is_some_and(|reader| reader.has_package(pkg)),
        })
    }

    pub fn has_classes_in_package(&self, pkg: &str) -> bool {
        self.layers().iter().any(|layer| match layer {
            ScopeLayer::Overlay(bucket) => bucket.has_classes_in_package(pkg),
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .is_some_and(|reader| reader.has_classes_in_package(pkg)),
        })
    }

    pub fn resolve_imports(&self, imports: &[Arc<str>]) -> Vec<Arc<ClassMetadata>> {
        let mut result = Vec::new();
        let mut seen: FxHashSet<Arc<str>> = Default::default();
        for import in imports {
            if import.ends_with(".*") {
                let pkg = import.trim_end_matches(".*").replace('.', "/");
                for class in self.classes_in_package(&pkg) {
                    if seen.insert(Arc::clone(&class.internal_name)) {
                        result.push(class);
                    }
                }
            } else {
                let internal = import.replace('.', "/");
                if let Some(cls) = self.get_class(&internal)
                    && seen.insert(Arc::clone(&cls.internal_name))
                {
                    result.push(cls);
                }
            }
        }
        result
    }

    pub fn collect_inherited_members(
        &self,
        class_internal: &str,
    ) -> (Vec<Arc<MethodSummary>>, Vec<Arc<FieldSummary>>) {
        let mut methods: Vec<Arc<MethodSummary>> = Vec::new();
        let mut fields: Vec<Arc<FieldSummary>> = Vec::new();
        let mut seen_methods: FxHashSet<(Arc<str>, Arc<str>)> = Default::default();
        let mut seen_fields: FxHashSet<Arc<str>> = Default::default();
        let hierarchy_order = self.hierarchy_order(class_internal);

        for class_ref in hierarchy_order.iter() {
            match class_ref {
                ClassHandleRef::Overlay { .. } => {
                    let Some(meta) = self.resolve_class_ref(class_ref) else {
                        continue;
                    };

                    for method in &meta.methods {
                        let key = Self::method_shadow_key(method);
                        if seen_methods.insert(key) {
                            methods.push(Arc::new(method.clone()));
                        }
                    }
                    for field in &meta.fields {
                        if seen_fields.insert(Arc::clone(&field.name)) {
                            fields.push(Arc::new(field.clone()));
                        }
                    }
                }
                ClassHandleRef::Artifact(handle) => {
                    let Some(reader) = self.scope.artifact_reader(handle.artifact_id) else {
                        continue;
                    };
                    let artifact_methods = reader.materialize_methods(*handle).unwrap_or_default();
                    let artifact_fields = reader.materialize_fields(*handle).unwrap_or_default();

                    if let Some(metrics) = self.request_metrics() {
                        metrics.record_artifact_method_materialization(artifact_methods.len());
                        metrics.record_artifact_field_materialization(artifact_fields.len());
                    }

                    for method in artifact_methods {
                        let key = Self::method_shadow_key(method.as_ref());
                        if seen_methods.insert(key) {
                            methods.push(method);
                        }
                    }
                    for field in artifact_fields {
                        if seen_fields.insert(Arc::clone(&field.name)) {
                            fields.push(field);
                        }
                    }
                }
            }
        }
        (methods, fields)
    }

    pub fn mro(&self, class_internal: &str) -> Vec<Arc<ClassMetadata>> {
        let hierarchy_order = self.hierarchy_order(class_internal);
        self.project_hierarchy_classes(hierarchy_order.as_ref())
    }

    pub fn lookup_methods_in_hierarchy(
        &self,
        class_internal: &str,
        method_name: &str,
    ) -> Vec<Arc<MethodSummary>> {
        let key = (Arc::from(class_internal), Arc::from(method_name));
        if let Some(cached) = self.caches.methods_by_name.get(&key) {
            return cached.value().as_ref().clone();
        }

        let methods = self
            .collect_inherited_members(class_internal)
            .0
            .into_iter()
            .filter(|method| method.name.as_ref() == method_name)
            .collect::<Vec<_>>();
        self.caches
            .methods_by_name
            .insert(key, Arc::new(methods.clone()));
        methods
    }

    pub fn lookup_field_in_hierarchy(
        &self,
        class_internal: &str,
        field_name: &str,
    ) -> Option<Arc<FieldSummary>> {
        let key = (Arc::from(class_internal), Arc::from(field_name));
        if let Some(cached) = self.caches.fields_by_name.get(&key) {
            return cached.value().clone();
        }

        let field = self
            .collect_inherited_members(class_internal)
            .1
            .into_iter()
            .find(|field| field.name.as_ref() == field_name);
        self.caches.fields_by_name.insert(key, field.clone());
        field
    }

    pub fn lookup_member_type_in_hierarchy(
        &self,
        class_internal: &str,
        simple_name: &str,
    ) -> Option<Arc<ClassMetadata>> {
        let hierarchy_order = self.hierarchy_order(class_internal);
        for owner_ref in hierarchy_order.iter() {
            let Some(owner_internal) = self.class_ref_internal_name(owner_ref) else {
                continue;
            };
            if let Some(inner) =
                self.resolve_direct_inner_class(owner_internal.as_ref(), simple_name)
            {
                return Some(inner);
            }
        }
        None
    }

    pub fn find_declaring_method_owner(
        &self,
        class_internal: &str,
        method_name: &str,
        method_desc: &str,
    ) -> Option<Arc<ClassMetadata>> {
        let key = (
            Arc::from(class_internal),
            Arc::from(method_name),
            Arc::from(method_desc),
        );
        if let Some(cached) = self.caches.declaring_method_owner.get(&key) {
            return cached
                .value()
                .clone()
                .and_then(|owner_internal| self.get_class(&owner_internal));
        }

        let hierarchy_order = self.hierarchy_order(class_internal);
        let owner_internal = hierarchy_order
            .iter()
            .find_map(|class_ref| match class_ref {
                ClassHandleRef::Overlay { .. } => {
                    let class = self.resolve_class_ref(class_ref)?;
                    class
                        .methods
                        .iter()
                        .any(|method| {
                            method.name.as_ref() == method_name
                                && method.desc().as_ref() == method_desc
                        })
                        .then(|| Arc::clone(&class.internal_name))
                }
                ClassHandleRef::Artifact(handle) => {
                    let reader = self.scope.artifact_reader(handle.artifact_id)?;
                    reader
                        .has_method_named_desc(*handle, method_name, method_desc)?
                        .then(|| reader.class_internal_name(*handle))
                        .flatten()
                }
            });
        self.caches
            .declaring_method_owner
            .insert(key, owner_internal.clone());
        owner_internal.and_then(|owner_internal| self.get_class(&owner_internal))
    }

    pub fn get_unique_class_by_simple_name(&self, simple_name: &str) -> Option<Arc<ClassMetadata>> {
        let mut matches = self.get_classes_by_simple_name(simple_name).into_iter();
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    pub fn fuzzy_autocomplete(&self, query: &str, limit: usize) -> Vec<Arc<str>> {
        let mut out = Vec::new();
        let mut seen: FxHashSet<Arc<str>> = Default::default();
        for layer in self.layers() {
            let matches = match layer {
                ScopeLayer::Overlay(bucket) => bucket.fuzzy_autocomplete(query, limit),
                ScopeLayer::Artifact(artifact_id) => self
                    .scope
                    .artifact_reader(*artifact_id)
                    .map(|reader| reader.fuzzy_autocomplete(query, limit))
                    .unwrap_or_default(),
            };
            for name in matches {
                if seen.insert(Arc::clone(&name)) {
                    out.push(name);
                }
            }
        }
        out
    }

    pub fn fuzzy_search_classes(&self, query: &str, limit: usize) -> Vec<Arc<ClassMetadata>> {
        let mut by_internal: rustc_hash::FxHashMap<Arc<str>, Arc<ClassMetadata>> =
            Default::default();
        for name in self.fuzzy_autocomplete(query, limit) {
            for class in self.get_classes_by_simple_name(&name) {
                let key = Arc::clone(&class.internal_name);
                if let Some(current) = by_internal.get(&key) {
                    if Self::should_replace(current, &class) {
                        by_internal.insert(key, class);
                    }
                } else {
                    by_internal.insert(key, class);
                }
            }
        }
        by_internal.into_values().collect()
    }

    pub fn exact_match_keys(&self) -> Vec<Arc<str>> {
        let name_table = self.build_name_table();
        name_table.iter().cloned().collect()
    }

    pub fn iter_all_classes(&self) -> Vec<Arc<ClassMetadata>> {
        let all_classes = self.caches.all_classes.get_or_init(|| {
            Arc::new(self.merge_class_refs(|layer| self.layer_iter_all_class_refs(layer)))
        });
        self.resolve_class_refs(all_classes.as_ref())
    }

    pub fn build_name_table(&self) -> Arc<NameTable> {
        self.scope.build_name_table()
    }

    fn request_metrics(&self) -> Option<&Arc<RequestMetrics>> {
        self.request_metrics.as_ref()
    }

    fn classes_in_package_refs(&self, pkg: &str) -> Arc<Vec<ClassHandleRef>> {
        if let Some(cached) = self.caches.classes_in_package.get(pkg) {
            return Arc::clone(cached.value());
        }

        let merged =
            Arc::new(self.merge_class_refs(|layer| self.layer_classes_in_package_refs(layer, pkg)));
        self.caches
            .classes_in_package
            .insert(Arc::from(pkg), Arc::clone(&merged));
        merged
    }

    fn resolve_class_ref(&self, class_ref: &ClassHandleRef) -> Option<Arc<ClassMetadata>> {
        match class_ref {
            ClassHandleRef::Overlay {
                bucket,
                internal_name,
            } => bucket.get_class(internal_name.as_ref()),
            ClassHandleRef::Artifact(handle) => self
                .scope
                .artifact_reader(handle.artifact_id)
                .and_then(|reader| {
                    let class = reader.get_class(*handle)?;
                    if let Some(metrics) = self.request_metrics() {
                        metrics.record_artifact_class_projection(1);
                    }
                    Some(class)
                }),
        }
    }

    fn class_ref_internal_name(&self, class_ref: &ClassHandleRef) -> Option<Arc<str>> {
        match class_ref {
            ClassHandleRef::Overlay { internal_name, .. } => Some(Arc::clone(internal_name)),
            ClassHandleRef::Artifact(handle) => self
                .scope
                .artifact_reader(handle.artifact_id)
                .and_then(|reader| reader.class_internal_name(*handle)),
        }
    }

    fn class_ref_origin_precedence(&self, class_ref: &ClassHandleRef) -> u8 {
        match class_ref {
            ClassHandleRef::Overlay {
                bucket,
                internal_name,
            } => bucket
                .get_class(internal_name.as_ref())
                .map(|class| Self::origin_precedence(&class))
                .unwrap_or_default(),
            ClassHandleRef::Artifact(handle) => self
                .scope
                .artifact_reader(handle.artifact_id)
                .and_then(|reader| reader.class_origin_precedence(*handle))
                .unwrap_or_default(),
        }
    }

    fn class_ref_matches_simple_name(&self, class_ref: &ClassHandleRef, simple_name: &str) -> bool {
        match class_ref {
            ClassHandleRef::Overlay {
                bucket,
                internal_name,
            } => bucket
                .get_class(internal_name.as_ref())
                .is_some_and(|class| class.matches_simple_name(simple_name)),
            ClassHandleRef::Artifact(handle) => self
                .scope
                .artifact_reader(handle.artifact_id)
                .is_some_and(|reader| reader.class_matches_simple_name(*handle, simple_name)),
        }
    }

    fn class_ref_matches_internal_name_tail(&self, class_ref: &ClassHandleRef, tail: &str) -> bool {
        match class_ref {
            ClassHandleRef::Overlay {
                bucket,
                internal_name,
            } => bucket
                .get_class(internal_name.as_ref())
                .is_some_and(|class| class.matches_internal_name_tail(tail)),
            ClassHandleRef::Artifact(handle) => self
                .scope
                .artifact_reader(handle.artifact_id)
                .is_some_and(|reader| reader.class_matches_internal_name_tail(*handle, tail)),
        }
    }

    fn class_ref_parent_internals(&self, class_ref: &ClassHandleRef) -> Vec<Arc<str>> {
        match class_ref {
            ClassHandleRef::Overlay {
                bucket,
                internal_name,
            } => {
                let Some(class) = bucket.get_class(internal_name.as_ref()) else {
                    return Vec::new();
                };
                let mut parents = Vec::new();
                if let Some(super_name) = class.super_name.as_ref() {
                    parents.push(Arc::clone(super_name));
                }
                parents.extend(class.interfaces.iter().cloned());
                parents
            }
            ClassHandleRef::Artifact(handle) => {
                let Some(reader) = self.scope.artifact_reader(handle.artifact_id) else {
                    return Vec::new();
                };
                let mut parents = Vec::new();
                if let Some(super_name) = reader.class_super_name(*handle).flatten() {
                    parents.push(super_name);
                }
                if let Some(interfaces) = reader.class_interfaces(*handle) {
                    parents.extend(interfaces);
                }
                parents
            }
        }
    }

    fn layer_get_class_ref(
        &self,
        layer: &ScopeLayer,
        internal_name: &str,
    ) -> Option<ClassHandleRef> {
        match layer {
            ScopeLayer::Overlay(bucket) => bucket
                .get_class(internal_name)
                .map(|class| Self::overlay_ref(bucket, class)),
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .and_then(|reader| reader.get_class_handle(internal_name))
                .map(ClassHandleRef::Artifact),
        }
    }

    fn layer_classes_by_simple_name_refs(
        &self,
        layer: &ScopeLayer,
        simple_name: &str,
    ) -> Vec<ClassHandleRef> {
        match layer {
            ScopeLayer::Overlay(bucket) => {
                Self::overlay_refs(bucket, bucket.get_classes_by_simple_name(simple_name))
            }
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .map(|reader| {
                    reader
                        .class_handles_by_simple_name(simple_name)
                        .into_iter()
                        .map(ClassHandleRef::Artifact)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn layer_classes_in_package_refs(&self, layer: &ScopeLayer, pkg: &str) -> Vec<ClassHandleRef> {
        match layer {
            ScopeLayer::Overlay(bucket) => {
                Self::overlay_refs(bucket, bucket.classes_in_package(pkg))
            }
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .map(|reader| {
                    reader
                        .class_handles_in_package(pkg)
                        .into_iter()
                        .map(ClassHandleRef::Artifact)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn layer_direct_inner_class_refs(
        &self,
        layer: &ScopeLayer,
        owner_internal: &str,
    ) -> Vec<ClassHandleRef> {
        match layer {
            ScopeLayer::Overlay(bucket) => {
                Self::overlay_refs(bucket, bucket.direct_inner_classes_by_owner(owner_internal))
            }
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .map(|reader| {
                    reader
                        .direct_inner_class_handles(owner_internal)
                        .into_iter()
                        .map(ClassHandleRef::Artifact)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn layer_iter_all_class_refs(&self, layer: &ScopeLayer) -> Vec<ClassHandleRef> {
        match layer {
            ScopeLayer::Overlay(bucket) => Self::overlay_refs(bucket, bucket.iter_all_classes()),
            ScopeLayer::Artifact(artifact_id) => self
                .scope
                .artifact_reader(*artifact_id)
                .map(|reader| {
                    reader
                        .iter_all_class_handles()
                        .into_iter()
                        .map(ClassHandleRef::Artifact)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{
        ArchiveClassStub, ArtifactKind, ArtifactMetadata, ArtifactReaderCache, ArtifactScopeReader,
        ClasspathId, IndexScope, IndexedJavaModule, ModuleId, ScopeLayer, StoredArtifactArchive,
        WorkspaceIndex,
    };
    use crate::index::{ClassOrigin, MethodParams};
    use crate::language::java::module_info::JavaModuleDescriptor;
    use crate::request_metrics::RequestMetrics;
    use rust_asm::constants::ACC_PUBLIC;
    use tower_lsp::lsp_types::Url;

    fn make_class(
        internal: &str,
        origin: ClassOrigin,
        method_descs: &[&str],
    ) -> Arc<ClassMetadata> {
        let (pkg, name) = internal
            .rsplit_once('/')
            .map(|(p, n)| (Some(Arc::from(p)), Arc::from(n)))
            .unwrap_or((None, Arc::from(internal)));
        Arc::new(ClassMetadata {
            package: pkg,
            name,
            internal_name: Arc::from(internal),
            super_name: None,
            interfaces: vec![],
            annotations: vec![],
            methods: method_descs
                .iter()
                .map(|d| MethodSummary {
                    name: Arc::from("add"),
                    params: MethodParams::from_method_descriptor(d),
                    annotations: vec![],
                    access_flags: ACC_PUBLIC,
                    is_synthetic: false,
                    generic_signature: None,
                    return_type: crate::semantic::types::parse_return_type_from_descriptor(d),
                })
                .collect(),
            fields: vec![],
            access_flags: ACC_PUBLIC,
            generic_signature: None,
            inner_class_of: None,
            origin,
        })
    }

    fn make_artifact_view(classes: Vec<ClassMetadata>) -> IndexView {
        let artifact_id = crate::index::ArtifactId(7);
        let archive = StoredArtifactArchive {
            metadata: ArtifactMetadata {
                id: artifact_id,
                kind: ArtifactKind::Jar,
                source_path: "memory://artifact.jar".to_string(),
                content_hash: 1,
                byte_len: 1,
                stored_at_unix_secs: 0,
            },
            classes: classes
                .into_iter()
                .map(ArchiveClassStub::from_class_metadata)
                .collect(),
            modules: vec![IndexedJavaModule {
                descriptor: Arc::new(JavaModuleDescriptor {
                    name: Arc::from("demo.module"),
                    is_open: false,
                    requires: vec![],
                    exports: vec![],
                    opens: vec![],
                    uses: vec![],
                    provides: vec![],
                }),
                origin: ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            }],
        };
        let readers = Arc::new(ArtifactReaderCache::default());
        readers.insert_preloaded(Arc::new(ArtifactScopeReader::from_archive(archive)));

        let mut layers: SmallVec<ScopeLayer, 8> = SmallVec::new();
        layers.push(ScopeLayer::Artifact(artifact_id));

        IndexView::from_scope(Arc::new(ScopeSnapshot::new(
            ModuleId::ROOT,
            ClasspathId::Main,
            None,
            layers,
            Vec::new(),
            readers,
        )))
    }

    #[test]
    fn test_source_class_shadows_base_for_same_internal() {
        let base_bucket = Arc::new(BucketIndex::new());
        base_bucket.add_classes(vec![
            (*make_class(
                "java/util/ArrayList",
                ClassOrigin::Jar(Arc::from("jdk://builtin")),
                &["(Ljava/lang/Object;)Z", "(ILjava/lang/Object;)V"],
            ))
            .clone(),
        ]);

        let source_bucket = Arc::new(BucketIndex::new());
        let source_origin = ClassOrigin::SourceFile(Arc::from("file:///X.java"));
        source_bucket.add_classes(vec![
            (*make_class(
                "java/util/ArrayList",
                source_origin.clone(),
                &["(LE;)Z", "(ILE;)V"],
            ))
            .clone(),
        ]);

        // Intentionally place base before source to verify precedence is origin-based, not order-based.
        let mut layers: SmallVec<Arc<BucketIndex>, 8> = SmallVec::new();
        layers.push(base_bucket);
        layers.push(source_bucket);
        let view = IndexView::new(layers);

        let cls = view.get_class("java/util/ArrayList").unwrap();
        assert!(matches!(cls.origin, ClassOrigin::SourceFile(_)));
        let descs: Vec<_> = cls
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert!(descs.contains(&"(LE;)Z".to_string()));
        assert!(!descs.contains(&"(Ljava/lang/Object;)Z".to_string()));
    }

    #[test]
    fn test_mro_hides_mixed_add_families_when_generic_signature_matches() {
        let base_bucket = Arc::new(BucketIndex::new());
        let mut base_array = (*make_class(
            "java/util/ArrayList",
            ClassOrigin::Jar(Arc::from("jdk://builtin")),
            &["(Ljava/lang/Object;)Z", "(ILjava/lang/Object;)V"],
        ))
        .clone();
        base_array.interfaces.push(Arc::from("java/util/List"));
        for method in &mut base_array.methods {
            if method.desc().as_ref() == "(Ljava/lang/Object;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILjava/lang/Object;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }
        let mut base_list = (*make_class(
            "java/util/List",
            ClassOrigin::Jar(Arc::from("jdk://builtin")),
            &["(Ljava/lang/Object;)Z", "(ILjava/lang/Object;)V"],
        ))
        .clone();
        for method in &mut base_list.methods {
            if method.desc().as_ref() == "(Ljava/lang/Object;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILjava/lang/Object;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }
        base_bucket.add_classes(vec![base_array, base_list]);

        let source_bucket = Arc::new(BucketIndex::new());
        let source_origin = ClassOrigin::SourceFile(Arc::from("file:///X.java"));
        let mut source_array =
            (*make_class("java/util/ArrayList", source_origin, &["(LE;)Z", "(ILE;)V"])).clone();
        source_array.interfaces.push(Arc::from("java/util/List"));
        for method in &mut source_array.methods {
            if method.desc().as_ref() == "(LE;)Z" {
                method.generic_signature = Some(Arc::from("(TE;)Z"));
            } else if method.desc().as_ref() == "(ILE;)V" {
                method.generic_signature = Some(Arc::from("(ITE;)V"));
            }
        }
        source_bucket.add_classes(vec![source_array]);

        let mut layers: SmallVec<Arc<BucketIndex>, 8> = SmallVec::new();
        layers.push(source_bucket);
        layers.push(base_bucket);
        let view = IndexView::new(layers);

        let mro = view.mro("java/util/ArrayList");
        let add_descs: Vec<_> = mro
            .iter()
            .flat_map(|c| c.methods.iter())
            .filter(|m| m.name.as_ref() == "add")
            .map(|m| m.desc().to_string())
            .collect();
        assert!(add_descs.contains(&"(LE;)Z".to_string()));
        assert!(!add_descs.contains(&"(Ljava/lang/Object;)Z".to_string()));
    }

    #[test]
    fn test_get_source_type_name_reconstructs_nested_owner_chain() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };

        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ClassWithGenerics"),
                internal_name: Arc::from("org/cubewhy/ClassWithGenerics"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: Some(Arc::from("<B:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ClassWithGenerics$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: Some(Arc::from("<T:Ljava/lang/Object;>Ljava/lang/Object;")),
                inner_class_of: Some(Arc::from("org/cubewhy/ClassWithGenerics")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("TopLevel"),
                internal_name: Arc::from("org/cubewhy/TopLevel"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(scope);
        assert_eq!(
            view.get_source_type_name("org/cubewhy/ClassWithGenerics$Box")
                .as_deref(),
            Some("org.cubewhy.ClassWithGenerics.Box")
        );
        assert_eq!(
            view.get_source_type_name("org/cubewhy/TopLevel").as_deref(),
            Some("org.cubewhy.TopLevel")
        );
    }

    #[test]
    fn test_get_source_type_name_avoids_global_scan_hot_path() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        let mut classes = vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Outer"),
                internal_name: Arc::from("org/cubewhy/Outer"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Middle"),
                internal_name: Arc::from("org/cubewhy/Outer$Middle"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/Outer")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Inner"),
                internal_name: Arc::from("org/cubewhy/Outer$Middle$Inner"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/Outer$Middle")),
                origin: ClassOrigin::Unknown,
            },
        ];
        for i in 0..12_000 {
            classes.push(ClassMetadata {
                package: Some(Arc::from("bench/p")),
                name: Arc::from(format!("Dummy{i:05}")),
                internal_name: Arc::from(format!("bench/p/Dummy{i:05}")),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            });
        }
        idx.add_classes(classes);
        let view = idx.view(scope);
        let target = view
            .get_class("org/cubewhy/Outer$Middle$Inner")
            .expect("target class");

        fn old_style_source_name(view: &IndexView, class: &Arc<ClassMetadata>) -> Option<String> {
            let mut package_prefix = String::new();
            if let Some(ref pkg) = class.package {
                package_prefix.push_str(&pkg.replace('/', "."));
                package_prefix.push('.');
            }
            if class.inner_class_of.is_some() {
                let mut chain = vec![class.name.to_string()];
                let mut current = Arc::clone(class);
                while let Some(parent_internal) = current.inner_class_of.clone() {
                    let parent = view.get_class(parent_internal.as_ref());
                    match parent {
                        Some(p) => {
                            chain.push(p.name.to_string());
                            current = p;
                        }
                        None => {
                            chain.clear();
                            break;
                        }
                    }
                }
                if !chain.is_empty() {
                    chain.reverse();
                    return Some(format!("{package_prefix}{}", chain.join(".")));
                }
            }
            if class.internal_name.contains('$') {
                return Some(class.internal_name.replace(['/', '$'], "."));
            }
            Some(class.source_name())
        }

        let t_old = std::time::Instant::now();
        let mut old_last = None;
        for _ in 0..60 {
            old_last = old_style_source_name(&view, &target);
        }
        let old_ms = t_old.elapsed().as_secs_f64() * 1000.0;

        let t_new = std::time::Instant::now();
        let mut new_last = None;
        for _ in 0..60 {
            new_last = view.get_source_type_name(target.internal_name.as_ref());
        }
        let new_ms = t_new.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "source_type_name_perf: old_ms={old_ms:.3} new_ms={new_ms:.3} old={:?} new={:?}",
            old_last, new_last
        );

        assert_eq!(
            new_last.as_deref(),
            Some("org.cubewhy.Outer.Middle.Inner"),
            "nested source name must preserve owner chain"
        );
        assert_eq!(new_last, old_last);
        assert!(
            new_ms < old_ms,
            "optimized path should beat global-scan reconstruction"
        );
    }

    #[test]
    fn test_top_level_classes_in_package_excludes_nested() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Outer"),
                internal_name: Arc::from("org/cubewhy/Outer"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Inner"),
                internal_name: Arc::from("org/cubewhy/Outer$Inner"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/Outer")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(scope);
        let names: Vec<String> = view
            .top_level_classes_in_package("org/cubewhy")
            .into_iter()
            .map(|c| c.name.to_string())
            .collect();
        assert_eq!(names, vec!["Outer".to_string()]);
    }

    #[test]
    fn test_direct_inner_classes_of_returns_owner_children() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("BoxV"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box$BoxV"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck$Box")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(scope);
        let outer_children: Vec<String> = view
            .direct_inner_classes_of("org/cubewhy/ChainCheck")
            .into_iter()
            .map(|c| c.name.to_string())
            .collect();
        assert_eq!(outer_children, vec!["Box".to_string()]);
        let box_children: Vec<String> = view
            .direct_inner_classes_of("org/cubewhy/ChainCheck$Box")
            .into_iter()
            .map(|c| c.name.to_string())
            .collect();
        assert_eq!(box_children, vec!["BoxV".to_string()]);
    }

    #[test]
    fn test_resolve_direct_inner_class_matches_direct_name_for_raw_bytecode_name() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(scope);
        let resolved = view.resolve_direct_inner_class("org/cubewhy/ChainCheck", "Box");
        assert_eq!(
            resolved.map(|m| m.internal_name.to_string()),
            Some("org/cubewhy/ChainCheck$Box".to_string())
        );
    }

    #[test]
    fn test_resolve_scoped_inner_class_matches_direct_name_for_raw_bytecode_name() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(scope);
        let resolved = view.resolve_scoped_inner_class("org/cubewhy/ChainCheck", "Box");
        assert_eq!(
            resolved.map(|m| m.internal_name.to_string()),
            Some("org/cubewhy/ChainCheck$Box".to_string())
        );
    }

    #[test]
    fn test_resolve_qualified_type_path_follow_owner_chain() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("BoxV"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box$BoxV"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck$Box")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(scope);
        let resolved = view.resolve_qualified_type_path("ChainCheck.Box.BoxV", &|head| {
            if head == "ChainCheck" {
                Some(Arc::from("org/cubewhy/ChainCheck"))
            } else {
                None
            }
        });
        assert_eq!(
            resolved.map(|m| m.internal_name.to_string()),
            Some("org/cubewhy/ChainCheck$Box$BoxV".to_string())
        );
    }

    #[test]
    fn test_resolve_scoped_inner_class_accepts_unique_owner_internal_hint() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("ChainCheck"),
                internal_name: Arc::from("org/cubewhy/ChainCheck"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck")),
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("org/cubewhy")),
                name: Arc::from("BoxV"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box$BoxV"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("org/cubewhy/ChainCheck$Box")),
                origin: ClassOrigin::Unknown,
            },
        ]);
        let view = idx.view(scope);
        // owner internal hint uses source-like owner "org/cubewhy/Box" (missing outer owner path)
        let resolved = view.resolve_scoped_inner_class("org/cubewhy/Box", "BoxV");
        assert_eq!(
            resolved.map(|c| c.internal_name.to_string()),
            Some("org/cubewhy/ChainCheck$Box$BoxV".to_string())
        );
    }

    #[test]
    fn test_resolve_owner_class_handles_dollar_in_owner_name() {
        let idx = WorkspaceIndex::new();
        let scope = IndexScope {
            module: ModuleId::ROOT,
        };
        idx.add_classes(vec![
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("Outer$Class"),
                internal_name: Arc::from("com/example/Outer$Class"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: None,
                origin: ClassOrigin::Unknown,
            },
            ClassMetadata {
                package: Some(Arc::from("com/example")),
                name: Arc::from("Inner$Class"),
                internal_name: Arc::from("com/example/Outer$Class$Inner$Class"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("com/example/Outer$Class")),
                origin: ClassOrigin::Unknown,
            },
        ]);

        let view = idx.view(scope);
        let resolved = view.resolve_owner_class("com/example/Outer$Class$Inner$Class");
        assert_eq!(
            resolved.map(|class| class.internal_name.to_string()),
            Some("com/example/Outer$Class".to_string())
        );
    }

    #[test]
    fn test_artifact_layer_supports_lookup_name_table_and_hierarchy() {
        let mut base = (*make_class(
            "pkg/Base",
            ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            &["()V"],
        ))
        .clone();
        base.methods[0].name = Arc::from("ping");

        let mut child = (*make_class(
            "pkg/Child",
            ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            &["(I)V"],
        ))
        .clone();
        child.super_name = Some(Arc::from("pkg/Base"));
        child.methods[0].name = Arc::from("pong");

        let view = make_artifact_view(vec![base, child]);

        assert!(view.get_class("pkg/Child").is_some());
        assert_eq!(
            view.get_classes_by_simple_name("Child")
                .into_iter()
                .map(|class| class.internal_name.to_string())
                .collect::<Vec<_>>(),
            vec!["pkg/Child".to_string()]
        );
        assert_eq!(view.classes_in_package("pkg").len(), 2);
        assert!(view.build_name_table().exists("pkg/Base"));
        assert_eq!(
            view.lookup_methods_in_hierarchy("pkg/Child", "ping").len(),
            1
        );
        assert_eq!(
            view.scope
                .artifact_reader(crate::index::ArtifactId(7))
                .unwrap()
                .module_names(),
            vec![Arc::from("demo.module")]
        );
    }

    #[test]
    fn test_artifact_hierarchy_member_lookup_avoids_class_projection_until_needed() {
        let mut base = (*make_class(
            "pkg/Base",
            ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            &["()V"],
        ))
        .clone();
        base.methods[0].name = Arc::from("ping");

        let mut child = (*make_class(
            "pkg/Child",
            ClassOrigin::Jar(Arc::from("memory://artifact.jar")),
            &["(I)V"],
        ))
        .clone();
        child.super_name = Some(Arc::from("pkg/Base"));
        child.methods[0].name = Arc::from("pong");

        let metrics = RequestMetrics::new(
            "test-artifact-metrics",
            &Url::parse("file:///workspace/Test.java").expect("uri"),
        );
        let view = make_artifact_view(vec![base, child]).with_request_metrics(Arc::clone(&metrics));

        let inherited = view.lookup_methods_in_hierarchy("pkg/Child", "ping");
        assert_eq!(inherited.len(), 1);
        assert_eq!(metrics.artifact_class_projection_count(), 0);
        assert_eq!(metrics.artifact_method_materialization_count(), 2);

        assert!(view.get_class("pkg/Child").is_some());
        assert_eq!(metrics.artifact_class_projection_count(), 1);
    }
}
