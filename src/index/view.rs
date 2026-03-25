use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::index::{BucketIndex, ClassMetadata, FieldSummary, MethodSummary, NameTable};

#[derive(Default)]
struct IndexViewCaches {
    class_by_internal: DashMap<Arc<str>, Option<Arc<ClassMetadata>>>,
    classes_by_simple_name: DashMap<Arc<str>, Arc<Vec<Arc<ClassMetadata>>>>,
    classes_in_package: DashMap<Arc<str>, Arc<Vec<Arc<ClassMetadata>>>>,
    direct_inner_classes: DashMap<(Option<Arc<str>>, Arc<str>), Arc<Vec<Arc<ClassMetadata>>>>,
    inherited_members: DashMap<Arc<str>, Arc<InheritedMembers>>,
    mro: DashMap<Arc<str>, Arc<Vec<Arc<ClassMetadata>>>>,
    methods_by_name: DashMap<(Arc<str>, Arc<str>), Arc<Vec<Arc<MethodSummary>>>>,
    fields_by_name: DashMap<(Arc<str>, Arc<str>), Option<Arc<FieldSummary>>>,
    declaring_method_owner: DashMap<(Arc<str>, Arc<str>, Arc<str>), Option<Arc<ClassMetadata>>>,
    all_classes: OnceLock<Arc<Vec<Arc<ClassMetadata>>>>,
    name_table: OnceLock<Arc<NameTable>>,
}

struct InheritedMembers {
    methods: Vec<Arc<MethodSummary>>,
    fields: Vec<Arc<FieldSummary>>,
}

#[derive(Clone)]
pub struct IndexView {
    layers: SmallVec<Arc<BucketIndex>, 8>,
    caches: Arc<IndexViewCaches>,
}

impl IndexView {
    fn merge_classes<F>(&self, mut fetch: F) -> Vec<Arc<ClassMetadata>>
    where
        F: FnMut(&BucketIndex) -> Vec<Arc<ClassMetadata>>,
    {
        let mut by_internal: FxHashMap<Arc<str>, Arc<ClassMetadata>> = Default::default();
        for layer in &self.layers {
            for class in fetch(layer) {
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

    fn resolve_internal_hint_to_class(&self, internal_hint: &str) -> Option<Arc<ClassMetadata>> {
        let (pkg, simple) = internal_hint.rsplit_once('/')?;
        let mut matches = self
            .classes_in_package(pkg)
            .into_iter()
            .filter(|c| c.matches_internal_name_tail(simple));
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
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
        Self {
            layers,
            caches: Arc::new(IndexViewCaches::default()),
        }
    }

    pub fn with_overlay_classes(&self, classes: Vec<ClassMetadata>) -> Self {
        if classes.is_empty() {
            return self.clone();
        }

        let overlay = Arc::new(BucketIndex::new());
        overlay.add_classes(classes);

        let mut layers = SmallVec::with_capacity(self.layers.len() + 1);
        layers.push(overlay);
        layers.extend(self.layers.iter().cloned());
        Self::new(layers)
    }

    /// Get the number of layers in this view
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn get_class(&self, internal_name: &str) -> Option<Arc<ClassMetadata>> {
        if let Some(cached) = self.caches.class_by_internal.get(internal_name) {
            return cached.clone();
        }

        let mut best: Option<Arc<ClassMetadata>> = None;
        for layer in &self.layers {
            if let Some(class) = layer.get_class(internal_name) {
                if let Some(current) = &best {
                    if Self::should_replace(current, &class) {
                        best = Some(class);
                    }
                } else {
                    best = Some(class);
                }
            }
        }
        self.caches
            .class_by_internal
            .insert(Arc::from(internal_name), best.clone());
        best
    }

    pub fn get_source_type_name(&self, internal: &str) -> Option<String> {
        let class = self.get_class(internal)?;
        if class.inner_class_of.is_some() || class.internal_name.contains('$') {
            // For nested/inner classes, internal name already encodes the owner chain.
            // Converting separators is O(length) and avoids global index scans.
            return Some(class.internal_name.replace(['/', '$'], "."));
        }
        Some(class.source_name())
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

        let mut scope_chain: Vec<Arc<str>> = vec![Arc::clone(&enclosing.name)];
        let mut cur = enclosing;
        while let Some(parent_name) = cur.inner_class_of.clone() {
            scope_chain.push(Arc::clone(&parent_name));
            let parent = cur
                .internal_name
                .rsplit_once('$')
                .and_then(|(owner_internal, _)| self.get_class(owner_internal))
                .or_else(|| {
                    self.get_classes_by_simple_name(parent_name.as_ref())
                        .into_iter()
                        .find(|c| {
                            c.name.as_ref() == parent_name.as_ref() && c.package == enclosing_pkg
                        })
                });
            match parent {
                Some(p) => cur = p,
                None => break,
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
            } else if class.inner_class_of.is_none()
                && class.internal_name.contains('$')
                && class
                    .internal_name
                    .rsplit('$')
                    .next()
                    .is_some_and(|tail| tail == simple_name)
            {
                // Compatibility fallback when inner_class_of is missing.
                let owner_tail = class
                    .internal_name
                    .rsplit_once('$')
                    .and_then(|(owner, _)| owner.rsplit('/').next());
                if let Some(owner_tail) = owner_tail
                    && let Some(pos) = scope_chain.iter().position(|n| n.as_ref() == owner_tail)
                {
                    match &best {
                        Some((best_pos, _)) if *best_pos <= pos => {}
                        _ => best = Some((pos, class)),
                    }
                }
            }
        }

        best.map(|(_, c)| c)
    }

    pub fn get_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassMetadata>> {
        if let Some(cached) = self.caches.classes_by_simple_name.get(simple_name) {
            return cached.value().as_ref().clone();
        }

        let merged =
            Arc::new(self.merge_classes(|layer| layer.get_classes_by_simple_name(simple_name)));
        self.caches
            .classes_by_simple_name
            .insert(Arc::from(simple_name), Arc::clone(&merged));
        merged.as_ref().clone()
    }

    pub fn classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        if let Some(cached) = self.caches.classes_in_package.get(pkg) {
            return cached.value().as_ref().clone();
        }

        let merged = Arc::new(self.merge_classes(|layer| layer.classes_in_package(pkg)));
        self.caches
            .classes_in_package
            .insert(Arc::from(pkg), Arc::clone(&merged));
        merged.as_ref().clone()
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
        let Some(owner) = self.get_class(owner_internal) else {
            return vec![];
        };
        let key = (owner.package.clone(), Arc::clone(&owner.name));
        if let Some(cached) = self.caches.direct_inner_classes.get(&key) {
            return cached.value().as_ref().clone();
        }

        let owner_pkg = owner.package.as_deref();
        let owner_name = owner.name.as_ref();
        let merged = Arc::new(
            self.merge_classes(|layer| layer.direct_inner_classes_by_owner(owner_pkg, owner_name)),
        );
        self.caches
            .direct_inner_classes
            .insert(key, Arc::clone(&merged));
        merged.as_ref().clone()
    }

    /// Resolves a direct nested class by simple name under `owner_internal`.
    pub fn resolve_direct_inner_class(
        &self,
        owner_internal: &str,
        simple_name: &str,
    ) -> Option<Arc<ClassMetadata>> {
        let owner = self.get_class(owner_internal)?;
        let owner_pkg = owner.package.as_deref();
        let owner_name = owner.name.as_ref();
        let mut best: Option<Arc<ClassMetadata>> = None;
        for layer in &self.layers {
            for candidate in layer.direct_inner_classes_by_owner(owner_pkg, owner_name) {
                if !candidate.matches_simple_name(simple_name) {
                    continue;
                }
                if let Some(current) = &best {
                    if Self::should_replace(current, &candidate) {
                        best = Some(candidate);
                    }
                } else {
                    best = Some(candidate);
                }
            }
        }
        best
    }

    /// Resolves the direct owner of `class_internal` using authoritative `inner_class_of`
    /// metadata. Returns `None` when ownership cannot be proven.
    pub fn resolve_owner_class(&self, class_internal: &str) -> Option<Arc<ClassMetadata>> {
        let class = self.get_class(class_internal)?;
        let owner_name = class.inner_class_of.as_deref()?;
        let mut best: Option<Arc<ClassMetadata>> = None;

        for candidate in self.get_classes_by_simple_name(owner_name) {
            if candidate.package != class.package {
                continue;
            }
            if !self
                .direct_inner_classes_of(candidate.internal_name.as_ref())
                .into_iter()
                .any(|inner| inner.internal_name == class.internal_name)
            {
                continue;
            }

            match &best {
                Some(current) if !Self::should_replace(current, &candidate) => {}
                _ => best = Some(candidate),
            }
        }

        best
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
        self.layers.iter().any(|layer| layer.has_package(pkg))
    }

    pub fn has_classes_in_package(&self, pkg: &str) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.has_classes_in_package(pkg))
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
        if let Some(cached) = self.caches.inherited_members.get(class_internal) {
            return (cached.methods.clone(), cached.fields.clone());
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
        let cached = Arc::new(InheritedMembers {
            methods: methods.clone(),
            fields: fields.clone(),
        });
        self.caches
            .inherited_members
            .insert(Arc::from(class_internal), cached);
        (methods, fields)
    }

    pub fn mro(&self, class_internal: &str) -> Vec<Arc<ClassMetadata>> {
        if let Some(cached) = self.caches.mro.get(class_internal) {
            return cached.as_ref().clone();
        }

        let mut result = Vec::new();
        let mut seen: std::collections::HashSet<Arc<str>> = std::collections::HashSet::new();
        let mut seen_methods: FxHashSet<(Arc<str>, Arc<str>)> = Default::default();
        let mut seen_fields: FxHashSet<Arc<str>> = Default::default();
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
            let mut projected = (*meta).clone();
            projected
                .methods
                .retain(|m| seen_methods.insert(Self::method_shadow_key(m)));
            projected
                .fields
                .retain(|f| seen_fields.insert(Arc::clone(&f.name)));
            result.push(Arc::new(projected));
        }
        self.caches
            .mro
            .insert(Arc::from(class_internal), Arc::new(result.clone()));
        result
    }

    pub fn lookup_methods_in_hierarchy(
        &self,
        class_internal: &str,
        method_name: &str,
    ) -> Vec<Arc<MethodSummary>> {
        let key = (Arc::from(class_internal), Arc::from(method_name));
        if let Some(cached) = self.caches.methods_by_name.get(&key) {
            return cached.as_ref().clone();
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
            return cached.clone();
        }

        let field = self
            .collect_inherited_members(class_internal)
            .1
            .into_iter()
            .find(|field| field.name.as_ref() == field_name);
        self.caches.fields_by_name.insert(key, field.clone());
        field
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
            return cached.clone();
        }

        let owner = self.mro(class_internal).into_iter().find(|class| {
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == method_name && m.desc().as_ref() == method_desc)
        });
        self.caches
            .declaring_method_owner
            .insert(key, owner.clone());
        owner
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
        for layer in &self.layers {
            for name in layer.fuzzy_autocomplete(query, limit) {
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
        for layer in &self.layers {
            for class in layer.fuzzy_search_classes(query, limit) {
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
        self.caches
            .all_classes
            .get_or_init(|| Arc::new(self.merge_classes(|layer| layer.iter_all_classes())))
            .as_ref()
            .clone()
    }

    pub fn build_name_table(&self) -> Arc<NameTable> {
        Arc::clone(self.caches.name_table.get_or_init(|| {
            let mut names = Vec::new();
            let mut seen: FxHashSet<Arc<str>> = Default::default();
            for layer in &self.layers {
                let layer_table = layer.build_name_table();
                for name in layer_table.iter() {
                    if seen.insert(Arc::clone(name)) {
                        names.push(Arc::clone(name));
                    }
                }
            }
            tracing::debug!(
                layer_count = self.layers.len(),
                name_count = names.len(),
                phase = "indexing_or_discovery",
                "build NameTable from IndexView"
            );
            NameTable::from_names(names)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassOrigin, MethodParams};
    use crate::index::{IndexScope, ModuleId, WorkspaceIndex};
    use rust_asm::constants::ACC_PUBLIC;

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
                inner_class_of: Some(Arc::from("ClassWithGenerics")),
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
                inner_class_of: Some(Arc::from("Outer")),
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
                inner_class_of: Some(Arc::from("Middle")),
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
                let pkg = class.package.clone();
                let mut current = Arc::clone(class);
                while let Some(parent_name) = current.inner_class_of.clone() {
                    chain.push(parent_name.to_string());
                    let parent = view
                        .iter_all_classes()
                        .into_iter()
                        .find(|c| c.name.as_ref() == parent_name.as_ref() && c.package == pkg);
                    match parent {
                        Some(p) => current = p,
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
                inner_class_of: Some(Arc::from("Outer")),
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
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                inner_class_of: Some(Arc::from("Box")),
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
                name: Arc::from("ChainCheck$Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                name: Arc::from("ChainCheck$Box"),
                internal_name: Arc::from("org/cubewhy/ChainCheck$Box"),
                super_name: None,
                interfaces: vec![],
                annotations: vec![],
                methods: vec![],
                fields: vec![],
                access_flags: rust_asm::constants::ACC_PUBLIC,
                generic_signature: None,
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                inner_class_of: Some(Arc::from("Box")),
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
                inner_class_of: Some(Arc::from("ChainCheck")),
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
                inner_class_of: Some(Arc::from("Box")),
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
                inner_class_of: Some(Arc::from("Outer$Class")),
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
}
