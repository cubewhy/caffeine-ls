//! In-memory library index: the always-resident tier-1 [`NameIndex`] plus
//! the lazy tier-2 [`LibraryIndex`] member lookups.
//!
//! The name index supports FQN and package lookups and super-type queries;
//! full member stubs are loaded on demand from the persistent LMDB stub
//! cache (without ever touching the archive again).

use std::sync::Arc;

use camino::Utf8PathBuf;
use lasso::ThreadedRodeo;
use postcard::from_bytes;
use rustc_hash::FxHashMap;

use crate::{
    db::{LibraryId, LibraryKind},
    lmdb_store,
    stubs::{ClassKind, ClassOrModuleRecord, DiskClassOrModuleRecord, DiskResolver, Symbol},
};

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
    /// The JPMS module owning this class, if the containing archive is
    /// modular (`module-info.class` present) or a JDK jimage.
    pub module: Option<Symbol>,
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

    /// Whether any class of this archive belongs to `package`. Backs the
    /// on-demand-import validation
    /// ([JLS §7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)):
    /// `import pkg.*;` is a compile-time error when the package is not
    /// observable on the classpath.
    pub fn has_class_in_package(&self, package: Symbol) -> bool {
        self.package_to_entries.contains_key(&package)
    }

    pub fn modules(&self) -> &[ModuleEntry] {
        &self.modules
    }

    pub fn module(&self, name: Symbol) -> Option<(u32, &ModuleEntry)> {
        let idx = *self.module_name_to_entry.get(&name)?;
        Some((idx, &self.modules[idx as usize]))
    }
}

/// Per-library index. Shared behind an `Arc`; cheap to hand out clones.
///
/// Tier-1 data ([`NameIndex`] plus the per-library string table) is
/// resident; tier-2 member records are read from the persistent stub cache
/// on demand.
pub struct LibraryIndex {
    pub id: LibraryId,
    pub kind: LibraryKind,
    pub archive: Utf8PathBuf,
    pub names: Arc<NameIndex>,
    strings: Arc<Vec<String>>,
    /// The full tier-2 member records of every class, retained in memory.
    /// Kept alongside (and preferred over) the persistent LMDB cache so that
    /// member lookups — constructors, methods, static fields — work even when
    /// the persistent cache is unavailable (no cache dir, read-only
    /// filesystem): the records a freshly built index needs are the same ones
    /// [`loader`](crate::loader) produced, so they are held here instead of
    /// being dropped after a best-effort persist. Cache-loaded indexes keep
    /// this empty and read tier-2 from the store.
    records: Arc<Vec<DiskClassOrModuleRecord>>,
    store: lmdb_store::StubStore,
}

impl LibraryIndex {
    pub fn new(
        id: LibraryId,
        kind: LibraryKind,
        archive: Utf8PathBuf,
        names: Arc<NameIndex>,
        strings: Arc<Vec<String>>,
        records: Arc<Vec<DiskClassOrModuleRecord>>,
        store: lmdb_store::StubStore,
    ) -> Self {
        Self {
            id,
            kind,
            archive,
            names,
            strings,
            records,
            store,
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
            lmdb_store::StubStore::default(),
        )
    }

    pub fn lookup_fqn(&self, fqn: Symbol) -> Option<(u32, &ClassEntry)> {
        self.names.lookup(fqn)
    }

    /// Tier-2 access: returns the full member stubs for a class, loading
    /// the record from the persistent cache on demand.
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
        // Prefer the retained in-memory records (available even without the
        // persistent cache); fall back to the LMDB store for cache-loaded
        // indexes that kept their tier-2 only on disk.
        let disk_record = self.records.get(record_idx as usize).cloned().or_else(|| {
            self.store.with_record_bytes(self.id, record_idx, |bytes| {
                from_bytes::<DiskClassOrModuleRecord>(bytes).ok()
            })
        })?;
        let resolver = DiskResolver::new(&self.strings, interner);
        Some(Arc::new(resolver.class_or_module(&disk_record)))
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
                module: Some(symbol(&interner, "java.base")),
            },
            ClassEntry {
                fqn: symbol(&interner, "java.util.List"),
                package: symbol(&interner, "java.util"),
                kind: ClassKind::Interface,
                flags: 0x0601,
                super_class: None,
                interfaces: Vec::new(),
                module: Some(symbol(&interner, "java.base")),
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
