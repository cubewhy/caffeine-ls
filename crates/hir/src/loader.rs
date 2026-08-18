//! Enumerates and parses JVM archives (jars and JDK jimages) into stub
//! records, and maintains the persistent two-tier cache.

use std::{
    fs::File,
    io::Read as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Context as _;
use camino::Utf8Path;
use jimage_rs::JImage;
use lasso::ThreadedRodeo;
use rayon::prelude::*;
use syntax::{
    class_parser::ClassParser,
    stub::{ClassOrModuleStub, Symbol, TypeRef as SyntaxTypeRef},
};
use zip::ZipArchive;

use crate::{
    db::{LibraryId, LibraryKind},
    disk::{self, DiskClassEntry, DiskModuleEntry, NamesBlob, StubsWriter},
    index::{ClassEntry, LibraryIndex, ModuleEntry, NameIndex},
    stubs::{DiskClassOrModuleRecord, StubStringTable},
};

/// A single parsed declaration, with its fully qualified name.
pub struct StubRecord {
    pub fqn: String,
    pub package: String,
    /// The JPMS module owning the declaration, if the containing archive is
    /// modular. `None` for classpath (unnamed module) declarations and for
    /// module descriptors themselves.
    pub module: Option<Symbol>,
    pub stub: ClassOrModuleStub<Symbol>,
}

impl StubRecord {
    fn new(interner: &ThreadedRodeo, stub: ClassOrModuleStub<Symbol>) -> Self {
        let fqn = interner.resolve(&stub.fqn()).to_owned();
        let package = fqn
            .rsplit_once('.')
            .map(|(package, _)| package.to_owned())
            .unwrap_or_default();
        Self {
            fqn,
            package,
            module: None,
            stub,
        }
    }
}

/// Parses all classes of a jar archive. The zip file is read sequentially
/// (its API requires `&mut self`), the class files are then parsed in
/// parallel.
pub fn parse_jar(
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
) -> anyhow::Result<Vec<StubRecord>> {
    let file = File::open(archive).with_context(|| format!("failed to open jar {archive}"))?;
    let mut zip = ZipArchive::new(file).with_context(|| format!("invalid jar {archive}"))?;

    let mut class_bytes = Vec::new();
    for i in 0..zip.len() {
        cancel_check();
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_owned();
        if entry.is_dir() || !name.ends_with(".class") || name.starts_with("META-INF/") {
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes)?;
        class_bytes.push(bytes);
    }
    drop(zip);

    let failed = AtomicUsize::new(0);
    let records: Vec<StubRecord> = class_bytes
        .into_par_iter()
        .filter_map(
            |bytes| match ClassParser::new(interner).parse_cafebabe(&bytes) {
                Ok(stub) => Some(StubRecord::new(interner, stub)),
                Err(_) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    None
                }
            },
        )
        .collect();
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        tracing::warn!(%failed, archive = %archive, "failed to parse class files");
    }
    Ok(tag_jar_module(interner, records))
}

/// Associates every class of a modular jar with its module (JLS: the module
/// consists of all packages contained in the jar). Non-modular jars keep
/// `module = None`, which corresponds to the unnamed module.
fn tag_jar_module(interner: &ThreadedRodeo, records: Vec<StubRecord>) -> Vec<StubRecord> {
    let Some(module_name) = records.iter().find_map(|record| match &record.stub {
        ClassOrModuleStub::Module(module) => Some(module.name),
        _ => None,
    }) else {
        return records;
    };
    let module_name = interner.get_or_intern(interner.resolve(&module_name));
    records
        .into_iter()
        .map(|mut record| {
            if matches!(record.stub, ClassOrModuleStub::Class(_)) {
                record.module = Some(module_name);
            }
            record
        })
        .collect()
}

/// Parses all classes of a JDK jimage archive.
pub fn parse_jimage(
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
) -> anyhow::Result<Vec<StubRecord>> {
    let jimage =
        JImage::open(archive).with_context(|| format!("failed to open jimage {archive}"))?;
    let names = jimage
        .resource_names()
        .context("failed to list jimage resources")?;

    let failed = AtomicUsize::new(0);
    let records: Vec<StubRecord> = names
        .into_par_iter()
        .filter_map(|resource| {
            let (module, path) = resource.get_full_name();
            if !path.ends_with(".class") {
                return None;
            }
            let lookup = format!("/{module}/{path}");
            let bytes = jimage.find_resource(&lookup).ok().flatten()?;
            match ClassParser::new(interner).parse_cafebabe(&bytes) {
                Ok(stub) => {
                    let mut record = StubRecord::new(interner, stub);
                    if matches!(record.stub, ClassOrModuleStub::Class(_)) {
                        record.module = Some(interner.get_or_intern(&module));
                    }
                    Some(record)
                }
                Err(_) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        })
        .collect();
    cancel_check();
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        tracing::warn!(%failed, archive = %archive, "failed to parse class files");
    }
    Ok(records)
}

fn parse_archive(
    kind: LibraryKind,
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
) -> anyhow::Result<Vec<StubRecord>> {
    match kind {
        LibraryKind::Jar => parse_jar(archive, interner, cancel_check),
        LibraryKind::Jimage => parse_jimage(archive, interner, cancel_check),
    }
}

/// Builds the in-memory index and the serialized tier-1/tier-2 payloads.
fn build(
    mut records: Vec<StubRecord>,
    interner: &ThreadedRodeo,
) -> (NameIndex, NamesBlob, Vec<DiskClassOrModuleRecord>) {
    // Deterministic ordering keeps the on-disk cache stable across runs.
    records.sort_by(|a, b| a.fqn.cmp(&b.fqn));

    let mut table = StubStringTable::new(interner);
    let mut entries = Vec::new();
    let mut modules = Vec::new();
    let mut disk_class_records = Vec::with_capacity(records.len());
    let mut disk_module_records = Vec::new();
    let mut disk_entries = Vec::with_capacity(records.len());

    for record in records {
        let fqn = interner.get_or_intern(&record.fqn);
        let package = interner.get_or_intern(&record.package);
        match record.stub {
            ClassOrModuleStub::Class(class) => {
                entries.push(ClassEntry {
                    fqn,
                    package,
                    kind: crate::stubs::ClassKind::from_flags(class.flags, class.is_record),
                    flags: class.flags,
                    super_class: class.super_class.as_ref().and_then(|t| match t {
                        SyntaxTypeRef::Reference { name, .. } => Some(*name),
                        _ => None,
                    }),
                    interfaces: class
                        .interfaces
                        .iter()
                        .filter_map(|t| match t {
                            SyntaxTypeRef::Reference { name, .. } => Some(*name),
                            _ => None,
                        })
                        .collect(),
                    module: record.module,
                });
                let disk = table.class(&class);
                disk_entries.push(disk_entry(&mut table, entries.last().unwrap()));
                disk_class_records.push(DiskClassOrModuleRecord::Class(disk));
            }
            ClassOrModuleStub::Module(module) => {
                modules.push(ModuleEntry {
                    name: fqn,
                    flags: module.flags,
                    version: module.version,
                });
                let disk = table.module(&module);
                disk_module_records.push(DiskClassOrModuleRecord::Module(disk));
            }
        }
    }

    // Disk records must be partitioned (all classes, then all modules) so the
    // offsets table indexes module record `i` at `offsets[entries.len() + i]`
    // (see `LibraryIndex::module_record`).
    let mut disk_records = disk_class_records;
    disk_records.extend(disk_module_records);

    let mut disk_modules = Vec::with_capacity(modules.len());
    for module in &modules {
        disk_modules.push(DiskModuleEntry {
            name: table.symbol(module.name),
            flags: module.flags,
            version: module.version.map(|v| table.symbol(v)),
        });
    }

    let name_index = NameIndex::new(entries, modules);
    let blob = NamesBlob {
        strings: table.into_strings(),
        entries: disk_entries,
        modules: disk_modules,
        offsets: Vec::new(),
    };
    (name_index, blob, disk_records)
}

fn disk_entry(table: &mut StubStringTable<'_>, entry: &ClassEntry) -> DiskClassEntry {
    DiskClassEntry {
        name: table.symbol(entry.fqn),
        package: table.symbol(entry.package),
        kind: entry.kind,
        flags: entry.flags,
        super_class: entry.super_class.map(|s| table.symbol(s)),
        interfaces: entry.interfaces.iter().map(|&s| table.symbol(s)).collect(),
        module: entry.module.map(|m| table.symbol(m)),
    }
}

/// Loads the index for a library, using the on-disk cache when present and
/// otherwise parsing the archive and (re)building the cache.
pub fn load_or_build(
    id: LibraryId,
    kind: LibraryKind,
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
) -> anyhow::Result<LibraryIndex> {
    load_or_build_with_cache(id, kind, archive, interner, cancel_check, disk::cache_dir())
}

/// Like [`load_or_build`], but with an injectable cache directory (used by
/// tests; `None` builds the index in memory only).
fn load_or_build_with_cache(
    id: LibraryId,
    kind: LibraryKind,
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cancel_check: &dyn Fn(),
    cache_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<LibraryIndex> {
    let Some(cache_dir) = cache_dir else {
        // No cache directory available: build the index in memory only
        // (tier-2 member loading is unavailable).
        let records = parse_archive(kind, archive, interner, cancel_check)?;
        let (names, blob, _) = build(records, interner);
        return Ok(LibraryIndex::new(
            id,
            kind,
            archive.to_owned(),
            Arc::new(names),
            Arc::new(blob.strings),
            Arc::new(Vec::new()),
            std::path::PathBuf::new(),
        ));
    };

    let names_path = disk::names_path(&cache_dir, id);
    let stubs_path = disk::stubs_path(&cache_dir, id);

    if names_path.exists() && stubs_path.exists() {
        if let Some(index) = load_from_cache(id, kind, archive, interner, &cache_dir) {
            return Ok(index);
        }
        tracing::warn!(library = %id, "failed to load stub cache, rebuilding");
    }

    let records = parse_archive(kind, archive, interner, cancel_check)?;
    let (names, mut blob, disk_records) = build(records, interner);

    let mut writer = StubsWriter::create(&stubs_path)?;
    for record in &disk_records {
        writer.push(record)?;
    }
    let offsets = writer.finish()?;
    blob.offsets = offsets.clone();

    disk::write_names(&names_path, &blob)?;

    Ok(LibraryIndex::new(
        id,
        kind,
        archive.to_owned(),
        Arc::new(names),
        Arc::new(blob.strings),
        Arc::new(offsets),
        stubs_path,
    ))
}

/// Loads the index from an existing on-disk cache. Returns `None` if the
/// cache is missing or corrupt.
pub fn load_from_cache(
    id: LibraryId,
    kind: LibraryKind,
    archive: &Utf8Path,
    interner: &ThreadedRodeo,
    cache_dir: &std::path::Path,
) -> Option<LibraryIndex> {
    let names_path = disk::names_path(cache_dir, id);
    let stubs_path = disk::stubs_path(cache_dir, id);

    let blob = disk::read_names(&names_path)
        .inspect_err(|err| tracing::warn!(library = %id, "failed to read name index: {err:#}"))
        .ok()?;

    let mut entries = Vec::with_capacity(blob.entries.len());
    for entry in &blob.entries {
        let symbol = |idx: u32| interner.get_or_intern(&blob.strings[idx as usize]);
        entries.push(ClassEntry {
            fqn: symbol(entry.name),
            package: symbol(entry.package),
            kind: entry.kind,
            flags: entry.flags,
            super_class: entry.super_class.map(symbol),
            interfaces: entry.interfaces.iter().map(|&i| symbol(i)).collect(),
            module: entry.module.map(symbol),
        });
    }
    let mut modules = Vec::with_capacity(blob.modules.len());
    for module in &blob.modules {
        let symbol = |idx: u32| interner.get_or_intern(&blob.strings[idx as usize]);
        modules.push(ModuleEntry {
            name: symbol(module.name),
            flags: module.flags,
            version: module.version.map(symbol),
        });
    }

    let names = NameIndex::new(entries, modules);
    Some(LibraryIndex::new(
        id,
        kind,
        archive.to_owned(),
        Arc::new(names),
        Arc::new(blob.strings),
        Arc::new(blob.offsets),
        stubs_path,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use camino::Utf8PathBuf;
    use rust_asm::class_writer::ClassWriter;
    use rust_asm::constants::*;
    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;
    use crate::db::{LibraryId, LibraryKind};
    use crate::disk::StubsWriter;

    const NOOP: fn() = || {};

    /// Builds a minimal, hand-encoded `com/example/Greeter` class file with two
    /// methods (`<init>` and `greet`) and one field (`name`), then packs it
    /// into a jar. Hand-encoding keeps the test self-contained and avoids
    /// depending on the (currently incomplete) rust-asm writer.
    fn greeter_class_bytes() -> Vec<u8> {
        fn utf8(bytes: &mut Vec<u8>, s: &str) {
            bytes.push(1);
            bytes.extend_from_slice(&(s.len() as u16).to_be_bytes());
            bytes.extend_from_slice(s.as_bytes());
        }
        fn class_ref(bytes: &mut Vec<u8>, idx: u16) {
            bytes.push(7);
            bytes.extend_from_slice(&idx.to_be_bytes());
        }

        let mut bytes = Vec::new();

        bytes.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        bytes.extend_from_slice(&0u16.to_be_bytes()); // minor version
        bytes.extend_from_slice(&52u16.to_be_bytes()); // major version
        bytes.extend_from_slice(&11u16.to_be_bytes()); // constant pool count (1-based)

        // cp #1
        utf8(&mut bytes, "com/example/Greeter");
        // cp #2
        class_ref(&mut bytes, 1);
        // cp #3
        utf8(&mut bytes, "java/lang/Object");
        // cp #4
        class_ref(&mut bytes, 3);
        // cp #5
        utf8(&mut bytes, "<init>");
        // cp #6
        utf8(&mut bytes, "()V");
        // cp #7
        utf8(&mut bytes, "greet");
        // cp #8
        utf8(&mut bytes, "()Ljava/lang/String;");
        // cp #9
        utf8(&mut bytes, "name");
        // cp #10
        utf8(&mut bytes, "Ljava/lang/String;");

        bytes.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
        bytes.extend_from_slice(&2u16.to_be_bytes()); // this_class
        bytes.extend_from_slice(&4u16.to_be_bytes()); // super_class
        bytes.extend_from_slice(&0u16.to_be_bytes()); // interfaces
        bytes.extend_from_slice(&1u16.to_be_bytes()); // fields
        // field `name` (cp #9) of type `Ljava/lang/String;` (cp #10)
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&9u16.to_be_bytes());
        bytes.extend_from_slice(&10u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes
        bytes.extend_from_slice(&2u16.to_be_bytes()); // methods
        // method `<init>` (cp #5) descriptor `()V` (cp #6)
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&5u16.to_be_bytes());
        bytes.extend_from_slice(&6u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes
        // method `greet` (cp #7) descriptor `()Ljava/lang/String;` (cp #8)
        bytes.extend_from_slice(&0x0001u16.to_be_bytes());
        bytes.extend_from_slice(&7u16.to_be_bytes());
        bytes.extend_from_slice(&8u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes()); // attributes
        bytes.extend_from_slice(&0u16.to_be_bytes()); // class attributes

        bytes
    }

    /// Builds a tiny, self-contained jar with a single class
    /// `com/example/Greeter`.
    fn build_jar(path: &Utf8Path) {
        let class_bytes = greeter_class_bytes();

        let file = File::create(path.as_std_path()).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("com/example/Greeter.class", options)
            .unwrap();
        zip.write_all(&class_bytes).unwrap();
        zip.finish().unwrap();
    }

    fn cache_dir_fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("caffeine-ls").join("stubs").join("v1");
        (dir, Utf8PathBuf::from_path_buf(path).unwrap())
    }

    #[test]
    fn full_pipeline_builds_and_loads_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar_path = Utf8PathBuf::from_path_buf(dir.path().join("test.jar")).unwrap();
        build_jar(&jar_path);

        let interner = ThreadedRodeo::default();
        let id = LibraryId(0xdeadbeef);
        let (_, cache_dir) = cache_dir_fixture();
        let cache_path = cache_dir.as_std_path().to_owned();

        // First run: build the index and write the cache.
        let index = load_or_build_with_cache(
            id,
            LibraryKind::Jar,
            &jar_path,
            &interner,
            &NOOP,
            Some(cache_path.clone()),
        )
        .unwrap();

        let fqn = interner.get_or_intern("com.example.Greeter");
        let (entry_idx, entry) = index.lookup_fqn(fqn).unwrap();
        assert_eq!(entry.kind, crate::stubs::ClassKind::Class);
        assert_eq!(
            entry.super_class,
            Some(interner.get_or_intern("java.lang.Object"))
        );

        // Tier-2: load the full class record with members.
        let record = index.class_record(&interner, entry_idx).unwrap();
        let crate::stubs::ClassOrModuleStub::Class(class) = record.as_ref() else {
            panic!("expected a class record");
        };
        assert_eq!(class.fqn, fqn);
        assert_eq!(class.methods.len(), 2);
        assert_eq!(class.fields.len(), 1);
        assert_eq!(class.methods[1].name, interner.get_or_intern("greet"));

        // Second run (new interner, same cache dir): load from the cache.
        let cached =
            load_from_cache(id, LibraryKind::Jar, &jar_path, &interner, &cache_path).unwrap();
        let (cached_idx, cached_entry) = cached.lookup_fqn(fqn).unwrap();
        assert_eq!(cached_entry.fqn, fqn);
        let cached_record = cached.class_record(&interner, cached_idx).unwrap();
        let crate::stubs::ClassOrModuleStub::Class(class) = cached_record.as_ref() else {
            panic!("expected a class record");
        };
        assert_eq!(class.methods.len(), 2);
    }

    #[test]
    fn cache_miss_reparses() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar_path = Utf8PathBuf::from_path_buf(dir.path().join("test.jar")).unwrap();
        build_jar(&jar_path);

        let interner = ThreadedRodeo::default();
        let id = LibraryId(0xcafebabe);

        let (_, cache_dir) = cache_dir_fixture();
        let cache_path = cache_dir.as_std_path().to_owned();
        let index = load_or_build_with_cache(
            id,
            LibraryKind::Jar,
            &jar_path,
            &interner,
            &NOOP,
            Some(cache_path),
        )
        .unwrap();
        assert!(
            index
                .lookup_fqn(interner.get_or_intern("com.example.Greeter"))
                .is_some()
        );
    }

    #[test]
    fn parse_jar_ignores_non_class_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar_path = Utf8PathBuf::from_path_buf(dir.path().join("test.jar")).unwrap();

        let file = File::create(jar_path.as_std_path()).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
        zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
        zip.finish().unwrap();

        let interner = ThreadedRodeo::default();
        let records = parse_jar(&jar_path, &interner, &NOOP).unwrap();
        assert!(records.is_empty());
    }

    /// Builds a modular jar: a `module-info.class` declaring module
    /// `com.example.app` (requiring `java.base`, exporting `com/example/api`)
    /// plus the `com/example/Greeter` class.
    fn modular_jar_bytes() -> Vec<u8> {
        let mut zip_buffer = Vec::new();
        let cursor = std::io::Cursor::new(&mut zip_buffer);
        let mut zip = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();

        let mut writer = ClassWriter::new(0);
        writer.visit(V9, 0, ACC_MODULE, "module-info", None, &[]);
        let mut module = writer.visit_module("com.example.app", 0, None);
        module.visit_require("java.base", ACC_MANDATED, None);
        module.visit_export("com/example/api", 0, &[]);
        module.visit_end(&mut writer);
        let module_bytes = writer.to_bytes().expect("module-info should encode");

        zip.start_file("module-info.class", options).unwrap();
        zip.write_all(&module_bytes).unwrap();
        zip.start_file("com/example/Greeter.class", options)
            .unwrap();
        zip.write_all(&greeter_class_bytes()).unwrap();
        zip.finish().unwrap();
        zip_buffer
    }

    #[test]
    fn modular_jar_tags_classes_and_indexes_module() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar_path = Utf8PathBuf::from_path_buf(dir.path().join("modular.jar")).unwrap();
        std::fs::write(jar_path.as_std_path(), modular_jar_bytes()).unwrap();

        let interner = ThreadedRodeo::default();
        let records = parse_jar(&jar_path, &interner, &NOOP).unwrap();

        let module_name = interner.get_or_intern("com.example.app");
        let greeter = records
            .iter()
            .find(|r| r.fqn == "com.example.Greeter")
            .expect("greeter should be parsed");
        assert_eq!(greeter.module, Some(module_name));

        // The module descriptor is retained as a Module stub.
        assert!(records.iter().any(|r| matches!(
            &r.stub,
            ClassOrModuleStub::Module(m)
                if interner.resolve(&m.name) == "com.example.app" && m.requires.len() == 1
        )));

        // Full pipeline: build the index + stubs, then read the module
        // record back through the (class_count-offset) tier-2 lookup.
        let (names, blob, disk_records) = build(records, &interner);
        assert_eq!(names.modules().len(), 1);
        assert_eq!(names.class_count(), 1);

        let stubs_path = dir.path().join("modular.stubs");
        let mut writer = StubsWriter::create(&stubs_path).unwrap();
        for record in &disk_records {
            writer.push(record).unwrap();
        }
        let offsets = writer.finish().unwrap();
        assert_eq!(offsets.len(), 2);

        let index = LibraryIndex::new(
            LibraryId(0xfeed),
            LibraryKind::Jar,
            jar_path.clone(),
            Arc::new(names),
            Arc::new(blob.strings),
            Arc::new(offsets),
            stubs_path,
        );

        let (module_idx, entry) = index
            .names
            .module(interner.get_or_intern("com.example.app"))
            .unwrap();
        assert_eq!(interner.resolve(&entry.name), "com.example.app");
        let record = index.module_record(&interner, module_idx).unwrap();
        let ClassOrModuleStub::Module(module) = record.as_ref() else {
            panic!("expected a module record");
        };
        assert_eq!(interner.resolve(&module.name), "com.example.app");
        assert_eq!(
            interner.resolve(&module.requires[0].module_name),
            "java.base"
        );
        assert_eq!(
            interner.resolve(&module.exports[0].package_name),
            "com.example.api"
        );
        assert!(module.exports[0].to_modules.is_empty());
    }

    #[test]
    fn non_modular_jar_has_unnamed_module_classes() {
        let dir = tempfile::TempDir::new().unwrap();
        let jar_path = Utf8PathBuf::from_path_buf(dir.path().join("plain.jar")).unwrap();
        build_jar(&jar_path);

        let interner = ThreadedRodeo::default();
        let records = parse_jar(&jar_path, &interner, &NOOP).unwrap();
        let greeter = records
            .iter()
            .find(|r| r.fqn == "com.example.Greeter")
            .expect("greeter should be parsed");
        assert_eq!(greeter.module, None);
    }

    #[test]
    fn missing_archive_errors() {
        let interner = ThreadedRodeo::default();
        let result = parse_jar(
            &Utf8PathBuf::from("/definitely/missing.jar"),
            &interner,
            &NOOP,
        );
        assert!(result.is_err());
    }
}
