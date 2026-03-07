use std::collections::VecDeque;
use std::sync::Arc;

use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::index::{BucketIndex, ClassMetadata, FieldSummary, MethodSummary, NameTable};

pub struct IndexView {
    layers: SmallVec<Arc<BucketIndex>, 8>,
}

impl IndexView {
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
        Self { layers }
    }

    pub fn get_class(&self, internal_name: &str) -> Option<Arc<ClassMetadata>> {
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
        best
    }

    pub fn get_source_type_name(&self, internal: &str) -> Option<String> {
        self.get_class(internal).map(|meta| meta.source_name())
    }

    pub fn get_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassMetadata>> {
        let mut by_internal: rustc_hash::FxHashMap<Arc<str>, Arc<ClassMetadata>> =
            Default::default();
        for layer in &self.layers {
            for class in layer.get_classes_by_simple_name(simple_name) {
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

    pub fn classes_in_package(&self, pkg: &str) -> Vec<Arc<ClassMetadata>> {
        let mut by_internal: rustc_hash::FxHashMap<Arc<str>, Arc<ClassMetadata>> =
            Default::default();
        for layer in &self.layers {
            for class in layer.classes_in_package(pkg) {
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
        (methods, fields)
    }

    pub fn mro(&self, class_internal: &str) -> Vec<Arc<ClassMetadata>> {
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
        result
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
        let mut out = Vec::new();
        let mut seen: FxHashSet<Arc<str>> = Default::default();
        for layer in &self.layers {
            for key in layer.exact_match_keys() {
                if seen.insert(Arc::clone(&key)) {
                    out.push(key);
                }
            }
        }
        out
    }

    pub fn iter_all_classes(&self) -> Vec<Arc<ClassMetadata>> {
        let mut by_internal: rustc_hash::FxHashMap<Arc<str>, Arc<ClassMetadata>> =
            Default::default();
        for layer in &self.layers {
            for class in layer.iter_all_classes() {
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

    pub fn build_name_table(&self) -> Arc<NameTable> {
        let names = self.exact_match_keys();
        NameTable::from_names(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ClassOrigin, MethodParams};
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
}
