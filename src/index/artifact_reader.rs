use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use nucleo_matcher::{
    Config as MatcherConfig, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::index::archive_stub::{ArchiveFieldStub, ArchiveMethodStub};
use crate::index::cache;
use crate::index::store::StoredArtifactArchive;
use crate::index::{
    ArchiveClassStub, ArtifactId, ArtifactMetadata, ClassMetadata, ClassOrigin, FieldSummary,
    IndexedJavaModule, MethodSummary,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactClassHandle {
    pub artifact_id: ArtifactId,
    pub slot: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactMethodHandle {
    pub class: ArtifactClassHandle,
    pub slot: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactFieldHandle {
    pub class: ArtifactClassHandle,
    pub slot: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactReaderMemoryStats {
    pub reader_entries: usize,
    pub class_count: usize,
    pub module_count: usize,
    pub simple_name_entry_count: usize,
    pub package_entry_count: usize,
    pub owner_entry_count: usize,
    pub materialized_class_count: usize,
}

struct ArtifactClassRecord {
    stub: Arc<ArchiveClassStub>,
    materialized: OnceLock<Arc<ClassMetadata>>,
}

impl ArtifactClassRecord {
    fn new(stub: ArchiveClassStub) -> Self {
        Self {
            stub: Arc::new(stub),
            materialized: OnceLock::new(),
        }
    }
}

#[derive(Default)]
struct ArtifactReaderStats {
    materialized_classes: AtomicUsize,
}

pub struct ArtifactScopeReader {
    metadata: ArtifactMetadata,
    classes: Vec<ArtifactClassRecord>,
    by_internal: HashMap<Arc<str>, u32>,
    simple_name_index: HashMap<Arc<str>, Vec<u32>>,
    package_index: HashMap<Arc<str>, Vec<u32>>,
    owner_index: HashMap<Arc<str>, Vec<u32>>,
    modules: HashMap<Arc<str>, Arc<IndexedJavaModule>>,
    stats: ArtifactReaderStats,
}

impl ArtifactScopeReader {
    pub fn from_archive(archive: StoredArtifactArchive) -> Self {
        let mut classes = Vec::with_capacity(archive.classes.len());
        let mut by_internal = HashMap::with_capacity(archive.classes.len());
        let mut simple_name_index = HashMap::with_capacity(archive.classes.len());
        let mut package_index = HashMap::new();
        let mut owner_index = HashMap::new();

        for (idx, stub) in archive.classes.into_iter().enumerate() {
            let slot = u32::try_from(idx).expect("artifact class count exceeded u32");
            by_internal.insert(Arc::clone(&stub.internal_name), slot);
            simple_name_index
                .entry(Arc::clone(&stub.name))
                .or_insert_with(Vec::new)
                .push(slot);
            if let Some(pkg) = stub.package.as_ref() {
                package_index
                    .entry(Arc::clone(pkg))
                    .or_insert_with(Vec::new)
                    .push(slot);
            }
            if let Some(owner_internal) = stub.inner_class_of.as_ref() {
                owner_index
                    .entry(Arc::clone(owner_internal))
                    .or_insert_with(Vec::new)
                    .push(slot);
            }
            classes.push(ArtifactClassRecord::new(stub));
        }

        let modules = archive
            .modules
            .into_iter()
            .map(|module| (Arc::from(module.name()), Arc::new(module)))
            .collect();

        Self {
            metadata: archive.metadata,
            classes,
            by_internal,
            simple_name_index,
            package_index,
            owner_index,
            modules,
            stats: ArtifactReaderStats::default(),
        }
    }

    pub fn artifact_id(&self) -> ArtifactId {
        self.metadata.id
    }

    pub fn get_class_handle(&self, internal_name: &str) -> Option<ArtifactClassHandle> {
        self.by_internal
            .get(internal_name)
            .copied()
            .map(|slot| ArtifactClassHandle {
                artifact_id: self.metadata.id,
                slot,
            })
    }

    pub fn get_class(&self, handle: ArtifactClassHandle) -> Option<Arc<ClassMetadata>> {
        let record = self.record(handle)?;
        Some(Arc::clone(record.materialized.get_or_init(|| {
            self.stats
                .materialized_classes
                .fetch_add(1, Ordering::Relaxed);
            Arc::new(record.stub.materialize())
        })))
    }

    pub fn class_internal_name(&self, handle: ArtifactClassHandle) -> Option<Arc<str>> {
        Some(Arc::clone(&self.stub(handle)?.internal_name))
    }

    pub fn class_origin(&self, handle: ArtifactClassHandle) -> Option<ClassOrigin> {
        Some(self.stub(handle)?.origin.clone())
    }

    pub fn class_package(&self, handle: ArtifactClassHandle) -> Option<Option<Arc<str>>> {
        Some(self.stub(handle)?.package.clone())
    }

    pub fn class_owner(&self, handle: ArtifactClassHandle) -> Option<Option<Arc<str>>> {
        Some(self.stub(handle)?.inner_class_of.clone())
    }

    pub fn class_super_name(&self, handle: ArtifactClassHandle) -> Option<Option<Arc<str>>> {
        Some(self.stub(handle)?.super_name.clone())
    }

    pub fn class_interfaces(&self, handle: ArtifactClassHandle) -> Option<Vec<Arc<str>>> {
        Some(self.stub(handle)?.interfaces.clone())
    }

    pub fn class_name(&self, handle: ArtifactClassHandle) -> Option<Arc<str>> {
        Some(Arc::clone(&self.stub(handle)?.name))
    }

    pub fn class_matches_simple_name(
        &self,
        handle: ArtifactClassHandle,
        simple_name: &str,
    ) -> bool {
        self.stub(handle)
            .is_some_and(|stub| stub.name.as_ref() == simple_name)
    }

    pub fn class_matches_internal_name_tail(
        &self,
        handle: ArtifactClassHandle,
        tail: &str,
    ) -> bool {
        self.stub(handle).is_some_and(|stub| {
            stub.internal_name
                .rsplit('/')
                .next()
                .is_some_and(|internal_tail| internal_tail == tail)
                || stub.name.as_ref() == tail
        })
    }

    pub fn class_origin_precedence(&self, handle: ArtifactClassHandle) -> Option<u8> {
        Some(match self.stub(handle)?.origin {
            ClassOrigin::SourceFile(_) => 2,
            _ => 1,
        })
    }

    pub fn materialize_methods(
        &self,
        handle: ArtifactClassHandle,
    ) -> Option<Vec<Arc<MethodSummary>>> {
        Some(
            self.stub(handle)?
                .methods
                .iter()
                .map(|method| Arc::new(method.materialize()))
                .collect(),
        )
    }

    pub fn materialize_method(&self, handle: ArtifactMethodHandle) -> Option<Arc<MethodSummary>> {
        Some(Arc::new(self.method_stub(handle)?.materialize()))
    }

    pub fn materialize_fields(
        &self,
        handle: ArtifactClassHandle,
    ) -> Option<Vec<Arc<FieldSummary>>> {
        Some(
            self.stub(handle)?
                .fields
                .iter()
                .map(|field| Arc::new(field.materialize()))
                .collect(),
        )
    }

    pub fn materialize_field(&self, handle: ArtifactFieldHandle) -> Option<Arc<FieldSummary>> {
        Some(Arc::new(self.field_stub(handle)?.materialize()))
    }

    pub fn has_method_named_desc(
        &self,
        handle: ArtifactClassHandle,
        method_name: &str,
        method_desc: &str,
    ) -> Option<bool> {
        Some(self.stub(handle)?.methods.iter().any(|method| {
            method.name.as_ref() == method_name && method.descriptor.as_ref() == method_desc
        }))
    }

    pub fn method_handles(&self, handle: ArtifactClassHandle) -> Option<Vec<ArtifactMethodHandle>> {
        let methods = &self.stub(handle)?.methods;
        Some(
            (0..methods.len())
                .map(|slot| ArtifactMethodHandle {
                    class: handle,
                    slot: u32::try_from(slot).expect("artifact method count exceeded u32"),
                })
                .collect(),
        )
    }

    pub fn method_handles_named(
        &self,
        handle: ArtifactClassHandle,
        method_name: &str,
    ) -> Option<Vec<ArtifactMethodHandle>> {
        Some(
            self.stub(handle)?
                .methods
                .iter()
                .enumerate()
                .filter(|(_, method)| method.name.as_ref() == method_name)
                .map(|(slot, _)| ArtifactMethodHandle {
                    class: handle,
                    slot: u32::try_from(slot).expect("artifact method count exceeded u32"),
                })
                .collect(),
        )
    }

    pub fn method_handle_by_name_desc(
        &self,
        handle: ArtifactClassHandle,
        method_name: &str,
        method_desc: &str,
    ) -> Option<ArtifactMethodHandle> {
        self.stub(handle)?
            .methods
            .iter()
            .enumerate()
            .find(|(_, method)| {
                method.name.as_ref() == method_name && method.descriptor.as_ref() == method_desc
            })
            .map(|(slot, _)| ArtifactMethodHandle {
                class: handle,
                slot: u32::try_from(slot).expect("artifact method count exceeded u32"),
            })
    }

    pub fn project_method_stub(&self, handle: ArtifactMethodHandle) -> Option<ArchiveMethodStub> {
        Some(self.method_stub(handle)?.clone())
    }

    pub fn field_handles(&self, handle: ArtifactClassHandle) -> Option<Vec<ArtifactFieldHandle>> {
        let fields = &self.stub(handle)?.fields;
        Some(
            (0..fields.len())
                .map(|slot| ArtifactFieldHandle {
                    class: handle,
                    slot: u32::try_from(slot).expect("artifact field count exceeded u32"),
                })
                .collect(),
        )
    }

    pub fn field_handle_by_name(
        &self,
        handle: ArtifactClassHandle,
        field_name: &str,
    ) -> Option<ArtifactFieldHandle> {
        self.stub(handle)?
            .fields
            .iter()
            .enumerate()
            .find(|(_, field)| field.name.as_ref() == field_name)
            .map(|(slot, _)| ArtifactFieldHandle {
                class: handle,
                slot: u32::try_from(slot).expect("artifact field count exceeded u32"),
            })
    }

    pub fn project_field_stub(&self, handle: ArtifactFieldHandle) -> Option<ArchiveFieldStub> {
        Some(self.field_stub(handle)?.clone())
    }

    pub fn class_handles_by_simple_name(&self, simple_name: &str) -> Vec<ArtifactClassHandle> {
        self.simple_name_index
            .get(simple_name)
            .into_iter()
            .flatten()
            .copied()
            .map(|slot| ArtifactClassHandle {
                artifact_id: self.metadata.id,
                slot,
            })
            .collect()
    }

    pub fn class_handles_in_package(&self, pkg: &str) -> Vec<ArtifactClassHandle> {
        let normalized = pkg.replace('.', "/");
        self.package_index
            .get(normalized.as_str())
            .into_iter()
            .flatten()
            .copied()
            .map(|slot| ArtifactClassHandle {
                artifact_id: self.metadata.id,
                slot,
            })
            .collect()
    }

    pub fn direct_inner_class_handles(&self, owner_internal: &str) -> Vec<ArtifactClassHandle> {
        self.owner_index
            .get(owner_internal)
            .into_iter()
            .flatten()
            .copied()
            .map(|slot| ArtifactClassHandle {
                artifact_id: self.metadata.id,
                slot,
            })
            .collect()
    }

    pub fn has_package(&self, pkg: &str) -> bool {
        let normalized = pkg.replace('.', "/");
        self.package_index.contains_key(normalized.as_str())
    }

    pub fn has_classes_in_package(&self, pkg: &str) -> bool {
        let normalized = pkg.replace('.', "/");
        self.package_index
            .get(normalized.as_str())
            .is_some_and(|ids| !ids.is_empty())
    }

    pub fn module_names(&self) -> Vec<Arc<str>> {
        self.modules.keys().cloned().collect()
    }

    pub fn get_module(&self, module_name: &str) -> Option<Arc<IndexedJavaModule>> {
        self.modules.get(module_name).cloned()
    }

    pub fn exact_match_keys(&self) -> Vec<Arc<str>> {
        self.by_internal.keys().cloned().collect()
    }

    pub fn iter_all_class_handles(&self) -> Vec<ArtifactClassHandle> {
        (0..self.classes.len())
            .map(|slot| ArtifactClassHandle {
                artifact_id: self.metadata.id,
                slot: u32::try_from(slot).expect("artifact class count exceeded u32"),
            })
            .collect()
    }

    pub fn fuzzy_autocomplete(&self, query: &str, limit: usize) -> Vec<Arc<str>> {
        if limit == 0 {
            return vec![];
        }

        if query.is_empty() {
            let mut names: Vec<_> = self.simple_name_index.keys().cloned().collect();
            names.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
            names.truncate(limit);
            return names;
        }

        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT);
        let mut utf32_buf = Vec::new();
        let mut scored = Vec::new();

        for name in self.simple_name_index.keys() {
            utf32_buf.clear();
            let haystack = Utf32Str::new(name.as_ref(), &mut utf32_buf);
            if let Some(score) = pattern.score(haystack, &mut matcher) {
                scored.push((Arc::clone(name), score));
            }
        }

        scored.sort_unstable_by(|(left_name, left_score), (right_name, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_name.as_ref().cmp(right_name.as_ref()))
        });
        scored.truncate(limit);
        scored.into_iter().map(|(name, _)| name).collect()
    }

    pub fn class_access_flags(&self, handle: ArtifactClassHandle) -> Option<u16> {
        self.stub(handle).map(|stub| stub.access_flags)
    }

    pub fn memory_stats(&self) -> ArtifactReaderMemoryStats {
        ArtifactReaderMemoryStats {
            reader_entries: 1,
            class_count: self.classes.len(),
            module_count: self.modules.len(),
            simple_name_entry_count: self.simple_name_index.len(),
            package_entry_count: self.package_index.len(),
            owner_entry_count: self.owner_index.len(),
            materialized_class_count: self.stats.materialized_classes.load(Ordering::Relaxed),
        }
    }

    fn record(&self, handle: ArtifactClassHandle) -> Option<&ArtifactClassRecord> {
        if handle.artifact_id != self.metadata.id {
            return None;
        }
        self.classes.get(handle.slot as usize)
    }

    fn stub(&self, handle: ArtifactClassHandle) -> Option<&ArchiveClassStub> {
        Some(self.record(handle)?.stub.as_ref())
    }

    fn method_stub(&self, handle: ArtifactMethodHandle) -> Option<&ArchiveMethodStub> {
        self.stub(handle.class)?.methods.get(handle.slot as usize)
    }

    fn field_stub(&self, handle: ArtifactFieldHandle) -> Option<&ArchiveFieldStub> {
        self.stub(handle.class)?.fields.get(handle.slot as usize)
    }
}

#[derive(Default)]
pub struct ArtifactReaderCache {
    readers: DashMap<ArtifactId, Arc<ArtifactScopeReader>>,
}

impl ArtifactReaderCache {
    pub fn get(&self, artifact_id: ArtifactId) -> Option<Arc<ArtifactScopeReader>> {
        if let Some(existing) = self.readers.get(&artifact_id) {
            return Some(Arc::clone(existing.value()));
        }

        let archive = cache::load_artifact_archive_by_id(artifact_id)?;
        let reader = Arc::new(ArtifactScopeReader::from_archive(archive));
        self.readers.insert(artifact_id, Arc::clone(&reader));
        Some(reader)
    }

    pub fn insert_preloaded(&self, reader: Arc<ArtifactScopeReader>) {
        self.readers.insert(reader.artifact_id(), reader);
    }

    pub fn clear(&self) {
        self.readers.clear();
    }

    pub fn memory_stats(&self) -> ArtifactReaderMemoryStats {
        let mut stats = ArtifactReaderMemoryStats {
            reader_entries: self.readers.len(),
            ..ArtifactReaderMemoryStats::default()
        };
        for reader in &self.readers {
            let reader_stats = reader.value().memory_stats();
            stats.class_count += reader_stats.class_count;
            stats.module_count += reader_stats.module_count;
            stats.simple_name_entry_count += reader_stats.simple_name_entry_count;
            stats.package_entry_count += reader_stats.package_entry_count;
            stats.owner_entry_count += reader_stats.owner_entry_count;
            stats.materialized_class_count += reader_stats.materialized_class_count;
        }
        stats
    }
}
