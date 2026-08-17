//! Persistent, two-tier on-disk cache for library stubs.
//!
//! Each library is cached as a pair of files:
//!
//! * `names.caf` — the tier-1 "name index": a string table, one
//!   [`DiskClassEntry`] per class (fqn, package, kind, flags, super class
//!   and interfaces), the module entries, and the offsets table into the
//!   stubs file. It is small (a few MB for the whole JDK) and is loaded
//!   eagerly on cold start.
//! * `stubs.caf` — the tier-2 records: one length-prefixed postcard blob per
//!   class/module with the full member stubs. Records are read on demand, in
//!   O(1), through the offsets table.
//!
//! Both files start with a 4-byte magic followed by the cache format
//! version. Writes are atomic (temp file + rename), so a crash mid-write
//! never corrupts an existing cache.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use postcard::{from_bytes, to_allocvec};
use serde::{Deserialize, Serialize};

use crate::{
    db::LibraryId,
    stubs::{ClassKind, DiskClassOrModuleRecord},
};

pub const CACHE_FORMAT_VERSION: u32 = 1;

const NAMES_MAGIC: &[u8; 4] = b"CFLS";
const STUBS_MAGIC: &[u8; 4] = b"CFSR";
const HEADER_LEN: usize = 12; // magic (4) + version (4) + payload length (4)

/// The tier-1 payload of `names.caf`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamesBlob {
    /// Per-library string table. All `u32` names below are indices into
    /// this table.
    pub strings: Vec<String>,
    pub entries: Vec<DiskClassEntry>,
    pub modules: Vec<DiskModuleEntry>,
    /// Absolute byte offset of each record in `stubs.caf`. Class record `i`
    /// lives at `offsets[i]`, module record `i` at
    /// `offsets[entries.len() + i]`.
    pub offsets: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskClassEntry {
    /// FQN, string-table index.
    pub name: u32,
    /// Package name (possibly empty), string-table index.
    pub package: u32,
    pub kind: ClassKind,
    pub flags: u16,
    pub super_class: Option<u32>,
    pub interfaces: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskModuleEntry {
    pub name: u32,
    pub flags: u16,
    pub version: Option<u32>,
}

/// Platform-specific cache directory for library stubs
/// (`$XDG_CACHE_HOME/caffeine-ls/stubs/vN`, falling back to
/// `~/.cache` or `%LOCALAPPDATA%`).
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))?;
    Some(
        base.join("caffeine-ls")
            .join("stubs")
            .join(format!("v{CACHE_FORMAT_VERSION}")),
    )
}

pub fn names_path(cache_dir: &Path, id: LibraryId) -> PathBuf {
    cache_dir
        .join(format!("{:016x}", id.0))
        .with_extension("names")
}

pub fn stubs_path(cache_dir: &Path, id: LibraryId) -> PathBuf {
    cache_dir
        .join(format!("{:016x}", id.0))
        .with_extension("stubs")
}

pub fn write_names(path: &Path, blob: &NamesBlob) -> anyhow::Result<()> {
    let payload = to_allocvec(blob)
        .map_err(|err| anyhow::anyhow!("failed to serialize name index: {err}"))?;
    atomic_write(path, NAMES_MAGIC, &payload)
}

pub fn read_names(path: &Path) -> anyhow::Result<NamesBlob> {
    let payload = read_with_header(path, NAMES_MAGIC)?;
    from_bytes(&payload).map_err(|err| anyhow::anyhow!("failed to deserialize name index: {err}"))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(path: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}-{n}", std::process::id()));
    PathBuf::from(name)
}

fn atomic_write(path: &Path, magic: &[u8; 4], payload: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;

    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);

    let tmp = temp_path(path);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_with_header(path: &Path, magic: &[u8; 4]) -> anyhow::Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    if bytes.len() < HEADER_LEN {
        anyhow::bail!("truncated stub cache file: {}", path.display());
    }
    if &bytes[..4] != magic {
        anyhow::bail!("bad magic in stub cache file: {}", path.display());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != CACHE_FORMAT_VERSION {
        anyhow::bail!(
            "unsupported stub cache format version {version} (expected {CACHE_FORMAT_VERSION})"
        );
    }
    let payload_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if bytes.len() != HEADER_LEN + payload_len {
        anyhow::bail!("length mismatch in stub cache file: {}", path.display());
    }
    Ok(bytes[HEADER_LEN..].to_vec())
}

/// Writer for the tier-2 `stubs.caf` file: one length-prefixed postcard
/// record per class/module.
pub struct StubsWriter {
    file: fs::File,
    tmp: PathBuf,
    path: PathBuf,
    offsets: Vec<u64>,
}

impl StubsWriter {
    pub fn create(path: &Path) -> anyhow::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cache path has no parent: {}", path.display()))?;
        fs::create_dir_all(parent)?;

        let tmp = temp_path(path);
        let mut file = fs::File::create(&tmp)?;
        use io::Write as _;
        file.write_all(STUBS_MAGIC)?;
        file.write_all(&CACHE_FORMAT_VERSION.to_le_bytes())?;

        Ok(Self {
            file,
            tmp,
            path: path.to_owned(),
            offsets: Vec::new(),
        })
    }

    pub fn push(&mut self, record: &DiskClassOrModuleRecord) -> anyhow::Result<()> {
        use io::{Seek as _, Write as _};
        let offset = self.file.stream_position()?;
        let payload = to_allocvec(record)
            .map_err(|err| anyhow::anyhow!("failed to serialize stub record: {err}"))?;
        self.file.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.offsets.push(offset);
        Ok(())
    }

    /// Finalizes the file (flush + atomic rename) and returns the record
    /// offsets, absolute in the final file.
    pub fn finish(self) -> anyhow::Result<Vec<u64>> {
        self.file.sync_all().ok();
        drop(self.file);
        fs::rename(&self.tmp, &self.path)?;
        Ok(self.offsets)
    }
}

/// Reads a single length-prefixed record at `offset` (O(1) random access).
pub fn read_record_bytes(path: &Path, offset: u64) -> io::Result<Vec<u8>> {
    use io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(path)?;
    let mut len_bytes = [0u8; 4];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stubs::{
        ClassData, ClassKind, ClassOrModule, DiskClassRecord, DiskModuleRecord, MethodData,
        ModuleData, PrimitiveType, TypeRef,
    };

    type ClassOrModuleU32 = ClassOrModule<u32>;

    #[test]
    fn names_round_trip() {
        let dir = tempfile_dir();
        let blob = NamesBlob {
            strings: vec![
                "java.lang.String".to_string(),
                "java.lang".to_string(),
                "java.lang.Object".to_string(),
            ],
            entries: vec![DiskClassEntry {
                name: 0,
                package: 1,
                kind: ClassKind::Class,
                flags: 0x0021,
                super_class: Some(2),
                interfaces: Vec::new(),
            }],
            modules: Vec::new(),
            offsets: vec![8],
        };

        let path = dir.path().join("test.names");
        write_names(&path, &blob).unwrap();

        let back = read_names(&path).unwrap();
        assert_eq!(back, blob);
    }

    #[test]
    fn stubs_random_access() {
        let dir = tempfile_dir();
        let path = dir.path().join("test.stubs");
        let record_a = ClassOrModuleU32::Class(ClassData {
            fqn: 0,
            name: 1,
            flags: 0x0021,
            kind: ClassKind::Class,
            super_class: None,
            interfaces: Vec::new(),
            type_params: Vec::new(),
            methods: Vec::new(),
            fields: Vec::new(),
            permitted_subclasses: Vec::new(),
            record_components: Vec::new(),
            annotations: Vec::new(),
        });
        let record_b = ClassOrModuleU32::Module(ModuleData {
            name: 2,
            flags: 0x8000,
            version: None,
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        });

        let mut writer = StubsWriter::create(&path).unwrap();
        writer.push(&record_a).unwrap();
        writer.push(&record_b).unwrap();
        let offsets = writer.finish().unwrap();

        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets[0], 8); // header length

        let bytes = read_record_bytes(path.as_path(), offsets[1]).unwrap();
        let decoded: ClassOrModuleU32 = from_bytes(&bytes).unwrap();
        assert_eq!(decoded, record_b);
    }

    #[test]
    fn corrupt_file_is_rejected() {
        let dir = tempfile_dir();
        let path = dir.path().join("test.names");

        std::fs::write(&path, b"BOGUS").unwrap();
        assert!(read_names(&path).is_err());

        std::fs::write(&path, b"CFLS").unwrap();
        assert!(read_names(&path).is_err());
    }

    #[test]
    fn disk_type_serde_round_trip() {
        // DiskClassRecord serializes/deserializes with postcard.
        let record: DiskClassRecord = ClassData {
            fqn: 0,
            name: 1,
            flags: 0x0021,
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![TypeRef::Reference {
                name: 2,
                generic_args: vec![TypeRef::Primitive(PrimitiveType::Int)],
            }],
            type_params: Vec::new(),
            methods: vec![MethodData {
                flags: 0,
                name: 3,
                return_type: TypeRef::Primitive(PrimitiveType::Void),
                type_params: Vec::new(),
                throws_list: Vec::new(),
                params: Vec::new(),
                annotations: Vec::new(),
                default_value: None,
            }],
            fields: Vec::new(),
            permitted_subclasses: Vec::new(),
            record_components: Vec::new(),
            annotations: Vec::new(),
        };
        let bytes = to_allocvec(&record).unwrap();
        let back: DiskClassRecord = from_bytes(&bytes).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn module_serde_round_trip() {
        let record: DiskModuleRecord = ModuleData {
            name: 0,
            flags: 0x8000,
            version: Some(1),
            requires: Vec::new(),
            exports: Vec::new(),
            opens: Vec::new(),
            uses: Vec::new(),
            provides: Vec::new(),
        };
        let bytes = to_allocvec(&record).unwrap();
        let back: DiskModuleRecord = from_bytes(&bytes).unwrap();
        assert_eq!(record, back);
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }
}
