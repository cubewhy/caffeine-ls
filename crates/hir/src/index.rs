//! In-memory library index: the always-resident tier-1 [`NameIndex`] plus
//! the lazy tier-2 [`LibraryIndex`] member cache.
//!
//! The name index supports FQN and package lookups and super-type queries;
//! full member stubs are loaded on demand through an LRU cache that reads
//! records back from `stubs.caf` on a miss (without ever touching the
//! archive again).

use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};

use camino::Utf8PathBuf;
use lasso::ThreadedRodeo;
use lru::LruCache;
use parking_lot::Mutex;
use postcard::from_bytes;
use rustc_hash::FxHashMap;

use crate::{
    db::{LibraryId, LibraryKind},
    disk,
    stubs::{ClassKind, ClassOrModuleRecord, DiskClassOrModuleRecord, DiskResolver, Symbol},
};

/// How many full class records to keep in memory per library.
pub const DEFAULT_CLASS_CACHE_CAPACITY: usize = 512;

/// Tier-1 entry: everything needed for name-based queries and hierarchy
/// edges, without any member data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassEntry {
    pub fqn: Symbol,
    pub package: Symbol,
    pub kind: ClassKind,
    pub flags: u16,
    pub super_class: Option<Symbol>,
    pub interfaces: Vec<Symbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    pub name: Symbol,
    pub flags: u16,
    pub version: Option<Symbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameIndex {
    entries: Vec<ClassEntry>,
    name_to_entry: FxHashMap<Symbol, u32>,
    package_to_entries: FxHashMap<Symbol, Vec<u32>>,
    modules: Vec<ModuleEntry>,
    module_name_to_entry: FxHashMap<Symbol, u32>,
}

impl NameIndex {
    pub fn new(entries: Vec<ClassEntry>, modules: Vec<ModuleEntry>) -> Self {
        let mut name_to_entry =
            FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        let mut package_to_entries: FxHashMap<Symbol, Vec<u32>> =
            FxHashMap::with_capacity_and_hasher(entries.len() / 2, Default::default());
        for (idx, entry) in entries.iter().enumerate() {
            name_to_entry.insert(entry.fqn, idx as u32);
            package_to_entries
                .entry(entry.package)
                .or_default()
                .push(idx as u32);
        }
        let mut module_name_to_entry =
            FxHashMap::with_capacity_and_hasher(modules.len(), Default::default());
        for (idx, module) in modules.iter().enumerate() {
            module_name_to_entry.insert(module.name, idx as u32);
        }
        Self {
            entries,
            name_to_entry,
            package_to_entries,
            modules,
            module_name_to_entry,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new(), Vec::new())
    }

    pub fn entries(&self) -> &[ClassEntry] {
        &self.entries
    }

    pub fn class_count(&self) -> u32 {
        self.entries.len() as u32
    }

    pub fn entry(&self, idx: u32) -> Option<&ClassEntry> {
        self.entries.get(idx as usize)
    }

    /// Looks up a class by its fully qualified name.
    pub fn lookup(&self, fqn: Symbol) -> Option<(u32, &ClassEntry)> {
        let idx = *self.name_to_entry.get(&fqn)?;
        Some((idx, &self.entries[idx as usize]))
    }

    pub fn classes_in_package(&self, package: Symbol) -> impl Iterator<Item = (u32, &ClassEntry)> {
        self.package_to_entries
            .get(&package)
            .into_iter()
            .flatten()
            .filter_map(|&idx| Some((idx, self.entries.get(idx as usize)?)))
    }

    pub fn modules(&self) -> &[ModuleEntry] {
        &self.modules
    }

    pub fn module(&self, name: Symbol) -> Option<(u32, &ModuleEntry)> {
        let idx = *self.module_name_to_entry.get(&name)?;
        Some((idx, &self.modules[idx as usize]))
    }
}

/// Per-library index. Shared behind an `Arc`; the LRU member cache makes it
/// cheap to hand out clones.
pub struct LibraryIndex {
    pub id: LibraryId,
    pub kind: LibraryKind,
    pub archive: Utf8PathBuf,
    pub names: Arc<NameIndex>,
    strings: Arc<Vec<String>>,
    offsets: Arc<Vec<u64>>,
    stubs_path: PathBuf,
    records: Mutex<LruCache<u32, Arc<ClassOrModuleRecord>>>,
}

impl LibraryIndex {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: LibraryId,
        kind: LibraryKind,
        archive: Utf8PathBuf,
        names: Arc<NameIndex>,
        strings: Arc<Vec<String>>,
        offsets: Arc<Vec<u64>>,
        stubs_path: PathBuf,
    ) -> Self {
        Self {
            id,
            kind,
            archive,
            names,
            strings,
            offsets,
            stubs_path,
            records: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEFAULT_CLASS_CACHE_CAPACITY).expect("non-zero capacity"),
            )),
        }
    }

    /// An index with no content, used when an archive fails to load.
    pub fn empty(id: LibraryId, kind: LibraryKind, archive: Utf8PathBuf) -> Self {
        Self::new(
            id,
            kind,
            archive,
            Arc::new(NameIndex::empty()),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            PathBuf::new(),
        )
    }

    pub fn lookup_fqn(&self, fqn: Symbol) -> Option<(u32, &ClassEntry)> {
        self.names.lookup(fqn)
    }

    /// Tier-2 access: returns the full member stubs for a class, loading
    /// the record from the on-disk cache on a miss.
    pub fn class_record(
        &self,
        interner: &ThreadedRodeo,
        entry_idx: u32,
    ) -> Option<Arc<ClassOrModuleRecord>> {
        self.record(interner, entry_idx)
    }

    /// Tier-2 access for module records.
    pub fn module_record(
        &self,
        interner: &ThreadedRodeo,
        module_idx: u32,
    ) -> Option<Arc<ClassOrModuleRecord>> {
        self.record(interner, self.names.class_count() + module_idx)
    }

    fn record(
        &self,
        interner: &ThreadedRodeo,
        record_idx: u32,
    ) -> Option<Arc<ClassOrModuleRecord>> {
        let mut cache = self.records.lock();
        if let Some(record) = cache.get(&record_idx) {
            return Some(record.clone());
        }
        let offset = *self.offsets.get(record_idx as usize)?;
        let bytes = disk::read_record_bytes(&self.stubs_path, offset).ok()?;
        let disk_record: DiskClassOrModuleRecord = from_bytes(&bytes).ok()?;
        let resolver = DiskResolver::new(&self.strings, interner);
        let record = Arc::new(resolver.class_or_module(&disk_record));
        cache.push(record_idx, record.clone());
        Some(record)
    }

    /// Drops all cached member records (the name index stays resident).
    pub fn clear_record_cache(&self) {
        self.records.lock().clear();
    }
}

impl std::fmt::Debug for LibraryIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryIndex")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("archive", &self.archive)
            .field("classes", &self.names.class_count())
            .field("modules", &self.names.modules().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stubs::ClassKind;
    use lasso::ThreadedRodeo;

    fn symbol(interner: &ThreadedRodeo, s: &str) -> Symbol {
        interner.get_or_intern(s)
    }

    #[test]
    fn name_index_lookup_and_packages() {
        let interner = ThreadedRodeo::default();
        let entries = vec![
            ClassEntry {
                fqn: symbol(&interner, "java.lang.String"),
                package: symbol(&interner, "java.lang"),
                kind: ClassKind::Class,
                flags: 0x0021,
                super_class: Some(symbol(&interner, "java.lang.Object")),
                interfaces: vec![symbol(&interner, "java.lang.CharSequence")],
            },
            ClassEntry {
                fqn: symbol(&interner, "java.util.List"),
                package: symbol(&interner, "java.util"),
                kind: ClassKind::Interface,
                flags: 0x0601,
                super_class: None,
                interfaces: Vec::new(),
            },
        ];
        let index = NameIndex::new(entries, Vec::new());

        let (idx, entry) = index.lookup(symbol(&interner, "java.lang.String")).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(entry.kind, ClassKind::Class);

        assert!(
            index
                .lookup(symbol(&interner, "com.example.Missing"))
                .is_none()
        );

        let classes: Vec<_> = index
            .classes_in_package(symbol(&interner, "java.lang"))
            .map(|(idx, _)| idx)
            .collect();
        assert_eq!(classes, vec![0]);

        assert_eq!(index.class_count(), 2);
        assert_eq!(index.entry(1).unwrap().kind, ClassKind::Interface);
    }

    #[test]
    fn library_index_empty() {
        let interner = ThreadedRodeo::default();
        let index = LibraryIndex::empty(
            LibraryId(1),
            LibraryKind::Jar,
            Utf8PathBuf::from("/nonexistent.jar"),
        );
        assert_eq!(index.names.class_count(), 0);
        assert!(index.class_record(&interner, 0).is_none());
    }
}
