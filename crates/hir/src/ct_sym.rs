//! The `ct.sym` reader: which platform API a compile *release* provides.
//!
//! `javac --release N` ([JEP 247](https://openjdk.org/jeps/247)) does not
//! resolve the platform against the JDK it runs on but against the release-`N`
//! view of an archive the SDK ships, `lib/ct.sym`. A compilation unit therefore
//! resolves type names against the platform classes in scope *of that release*
//! ([JLS §7.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.3)),
//! and a member reference against the binary form of the declaring class as of
//! that release ([JLS §13.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-13.html#jls-13.1)),
//! whose member identity is name plus descriptor
//! ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)).
//! So `List.of("a")` under `--release 8` is javac's `cannot find symbol`, even
//! though the JDK running javac provides it.
//!
//! This module reads that archive, so the resolver can ask it the same question
//! about an API a reference *did* resolve against the runtime JDK.
//!
//! # Layout
//!
//! `ct.sym` is a plain ZIP. Verified on JDK 25: of its 20670 entries every
//! non-directory one ends in `.sig` (16977 of them, 1031 being
//! `module-info.sig`). An entry is named
//! `<releaseDir>/<module>/<package path>/<SimpleName>.sig`, and the simple name
//! is the last segment of the *binary* name
//! ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2))
//! — `89ABCDEFGHIJKLMNOP/java.base/java/util/Map$Entry.sig` for
//! `java.util.Map.Entry`.
//!
//! A `<releaseDir>` names the set of releases the contained version of the
//! class is effective for, by concatenating those releases' spellings in
//! **base 36, upper case** — the very spelling javac looks a release up by: it
//! matches a directory with `name.contains(Integer.toString(release, 36).toUpperCase())`
//! (`com.sun.tools.javac.platform.JDKPlatformProvider$PlatformDescriptionImpl`,
//! which also skips any name containing `-`). So `8` covers release 8, `9A`
//! covers 9 and 10, `89` covers 8 and 9 (the version of `java.io.Reader` that
//! was current through 9 — `Reader.transferTo` arrived in 10), and
//! `BCDEFGHIJKLMNOP` covers 11 through 25. A class appears in one directory per
//! version it had, so it is provided at release `R` exactly when one of its
//! directories covers `R`: `java.lang.String` sits in
//! `8, 9A, B, C, D, E, F, GHIJK, L, MNOP` — every release from 8 to 25 —
//! while `java.util.SequencedCollection` sits only in `LMNOP` (21..25), which
//! is why `--release 20` rejects it and `--release 21` accepts it.
//!
//! The spellings above 9 are one character each while a release stays below 36,
//! so the archive's overall release bounds ([`CtSymIndex::min_release`],
//! [`CtSymIndex::max_release`]) are exact for every release spelling one
//! character — every release an SDK can currently name. A two-character
//! spelling (release 36 and up) would make a directory name ambiguous between
//! the releases it concatenates; the per-release matching below keeps javac's
//! own `contains` semantics in that case, and only the bounds understate, which
//! abstains instead of misreporting.
//!
//! A `.sig` file is an ordinary class file (`CA FE BA BE`, minor 0) with the
//! `private` members stripped and no `Code` attribute; `protected` members are
//! kept. The attribute names it may carry are a subset of `ConstantValue`,
//! `Deprecated`, `Exceptions`, `InnerClasses` and `Signature`, all of which
//! `rust-asm` already reads.
//!
//! # Scope of the check
//!
//! The check is *additive*: it answers about an API the runtime JDK provides.
//! A class `ct.sym` never tracks at all — `jdk.internal.misc.Unsafe` has no
//! `.sig` anywhere, although the runtime jimage has it — is never reported
//! here: javac rejects such a use through the module system instead
//! ([JLS §7.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.3)).

use std::{fs::File, io::Read as _};

use anyhow::Context as _;
use base_db::salsa;
use camino::{Utf8Path, Utf8PathBuf};
use hir_def::jvm::access::JvmAccessFlags;
use hir_expand::name::Name;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use syntax::stub::ClassOrModuleStub;
use triomphe::Arc;
use zip::ZipArchive;

use crate::db::{HirDatabase, LibraryId, ProjectGraph};
use crate::stubs::ClassRecord;

/// The release bounds and per-class release sets of one SDK's `ct.sym`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtSymIndex {
    /// The lowest release any directory of the archive names.
    min_release: u8,
    /// The highest release any directory of the archive names.
    max_release: u8,
    /// Binary class name (JVMS §4.2: `.` between package and name, `$` for a
    /// nested class) → where the archive keeps it.
    classes: FxHashMap<SmolStr, CtSymClass>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CtSymClass {
    /// One `(directory, module)` pair per directory holding this class.
    parts: Vec<(SmolStr, SmolStr)>,
}

impl CtSymIndex {
    /// The lowest release the archive can answer for. `min_release() >
    /// max_release()` means it can answer for none — the sentinel an archive
    /// with no release directory at all yields, such as the pre-JDK-9 `ct.sym`
    /// of a JDK 8 install, whose entries live under `META-INF/sym/`.
    pub fn min_release(&self) -> u8 {
        self.min_release
    }

    /// The highest release the archive can answer for. See
    /// [`Self::min_release`] for the empty case.
    pub fn max_release(&self) -> u8 {
        self.max_release
    }

    /// Whether the archive tracks `fqn` (binary name, JVMS §4.2) at all.
    pub fn tracks(&self, fqn: &str) -> bool {
        self.classes.contains_key(fqn)
    }

    /// Whether the archive is within its release bounds — the only releases it
    /// carries a platform view for. An archive with no release directory
    /// answers for none.
    fn covers_release(&self, release: u8) -> bool {
        self.min_release <= self.max_release
            && (self.min_release..=self.max_release).contains(&release)
    }

    /// Whether the platform view of `release` provides `fqn`.
    pub fn provides(&self, fqn: &str, release: u8) -> bool {
        self.part_for(fqn, release).is_some()
    }

    /// The `(directory, module)` whose `.sig` provides `fqn` at `release`.
    ///
    /// A well-formed archive partitions the releases among a class's
    /// directories, so exactly one covers `release`; should two, the
    /// later-starting one wins, being the more recent statement of the class.
    pub fn part_for(&self, fqn: &str, release: u8) -> Option<(&SmolStr, &SmolStr)> {
        let class = self.classes.get(fqn)?;
        class
            .parts
            .iter()
            .filter(|(dir, _)| dir_covers(dir, release))
            .max_by_key(|(dir, _)| lowest_named_release(dir))
            .map(|(dir, module)| (dir, module))
    }
}

/// The base-36 spelling of `release`, upper case — the form a `ct.sym`
/// directory name spells a release in, and the form javac matches it with.
pub fn release_spelling(release: u8) -> SmolStr {
    let mut spelling = String::new();
    let mut value = u32::from(release);
    loop {
        let digit = (value % 36) as u8;
        spelling.push(char::from(if digit < 10 {
            b'0' + digit
        } else {
            b'A' + (digit - 10)
        }));
        value /= 36;
        if value == 0 {
            break;
        }
    }
    spelling.chars().rev().collect()
}

/// The release a base-36 digit stands for.
fn base36_value(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'A'..='Z' => Some(10 + (c as u8 - b'A')),
        _ => None,
    }
}

/// Whether a directory name covers `release`: javac's own rule — the name
/// contains the release's base-36 spelling — plus its exclusion of names
/// carrying a `-` (those describe something other than a release set).
fn dir_covers(dir: &str, release: u8) -> bool {
    !dir.contains('-') && dir.contains(release_spelling(release).as_str())
}

/// The lowest release a directory name mentions. Exact while every release is
/// spelled with one character, which is the case for every release an SDK can
/// name today; used only to order overlapping directories.
fn lowest_named_release(dir: &str) -> u8 {
    dir.chars().filter_map(base36_value).min().unwrap_or(0)
}

/// The `ct.sym` of the SDK `library` belongs to. The archive is looked for
/// beside the platform archive the library was registered from — `<sdk>/lib`
/// for the `lib/modules` jimage and `lib/rt.jar`, `<sdk>/jre/lib` for a JDK 8
/// install's `jre/lib/rt.jar` — and, from either, at the SDK's own
/// `<sdk>/lib/ct.sym` and `<sdk>/jre/lib/ct.sym`. `None` when the SDK ships no
/// symbol file.
fn ct_sym_path(archive: &Utf8Path) -> Option<Utf8PathBuf> {
    // `<sdk>/lib/<archive>` or `<sdk>/jre/lib/<archive>`.
    let lib = archive.parent()?;
    let home = if lib.parent()?.file_name() == Some("jre") {
        lib.parent()?.parent()?
    } else {
        lib.parent()?
    };
    [
        lib.join("ct.sym"),
        home.join("lib").join("ct.sym"),
        home.join("jre").join("lib").join("ct.sym"),
    ]
    .into_iter()
    .find(|candidate| candidate.as_std_path().is_file())
}

/// Reads the archive's entry names into a [`CtSymIndex`]. Directories (names
/// ending in `/`), non-`.sig` entries, `module-info.sig` and the `.sig` files
/// of the surrogate `package-info` are skipped, as are directories whose name
/// is no release set ([`dir_covers`]).
fn build_index(path: &Utf8Path) -> anyhow::Result<CtSymIndex> {
    let file = File::open(path).with_context(|| format!("failed to open {path}"))?;
    let archive =
        ZipArchive::new(file).with_context(|| format!("invalid symbol archive {path}"))?;

    let mut classes: FxHashMap<SmolStr, CtSymClass> = FxHashMap::default();
    // Inverted on purpose: an archive that names no release keeps
    // `min > max`, which [`CtSymIndex::covers_release`] reads as "answers for
    // no release". The pre-JDK-9 `ct.sym` of a JDK 8 install is such an archive
    // — its entries are jars under `META-INF/sym/rt.jar/`, not release
    // directories.
    let mut min_release = u8::MAX;
    let mut max_release = 0u8;

    // Only the entry *names* are needed, and those come from the central
    // directory — the ~11 MB of signatures are never decompressed.
    for index in 0..archive.len() {
        let Some(name) = archive.name_for_index(index) else {
            continue;
        };
        let Some((dir, rest)) = name.split_once('/') else {
            continue;
        };
        // The directory must be a release set the archive can answer for.
        if dir.contains('-') {
            continue;
        }
        let mut named = dir.chars().filter_map(base36_value).peekable();
        if named.peek().is_none() {
            continue;
        }
        let (low, high) = named.fold((u8::MAX, 0u8), |(low, high), release| {
            (low.min(release), high.max(release))
        });
        min_release = min_release.min(low);
        max_release = max_release.max(high);

        // `<module>/<package path>/<SimpleName>.sig`; a module's own
        // `module-info.sig` sits directly under `<module>`.
        let Some((module, in_module)) = rest.split_once('/') else {
            continue;
        };
        let Some(file_name) = in_module.rsplit('/').next() else {
            continue;
        };
        let Some(simple) = file_name.strip_suffix(".sig") else {
            continue;
        };
        // `module-info` describes the module, and `package-info` is the
        // surrogate a module's package annotations live in; neither is a type a
        // compilation unit can name.
        if simple.is_empty() || simple == "module-info" || simple == "package-info" {
            continue;
        }
        let pkg_path = &in_module[..in_module.len() - file_name.len()];
        let pkg = pkg_path.trim_end_matches('/').replace('/', ".");
        let fqn = if pkg.is_empty() {
            simple.to_owned()
        } else {
            format!("{pkg}.{simple}")
        };

        let class = classes
            .entry(SmolStr::from(fqn))
            .or_insert_with(|| CtSymClass { parts: Vec::new() });
        let part = (SmolStr::from(dir), SmolStr::from(module));
        if !class.parts.contains(&part) {
            class.parts.push(part);
        }
    }

    Ok(CtSymIndex {
        min_release,
        max_release,
        classes,
    })
}

/// The release index of a library's SDK, `None` when the library is
/// unregistered, its SDK ships no `ct.sym`, or the archive cannot be read.
#[salsa::tracked(returns(ref))]
fn ct_sym_index_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
    library: LibraryId,
) -> Option<Arc<CtSymIndex>> {
    let archive = crate::db::library_archive(db, library)?;
    let path = ct_sym_path(&archive)?;
    match build_index(&path) {
        Ok(index) => Some(Arc::new(index)),
        Err(err) => {
            tracing::warn!(path = %path, "failed to read the symbol archive: {err:#}");
            None
        }
    }
}

/// The release index of a library's SDK, when the workspace is loaded and the
/// SDK ships a readable `ct.sym`.
pub fn ct_sym_index(db: &dyn HirDatabase, library: LibraryId) -> Option<Arc<CtSymIndex>> {
    let project_graph = ProjectGraph::try_get(db)?;
    ct_sym_index_query(db, project_graph, library).clone()
}

/// The release index of a library's SDK, when the workspace is loaded and the
/// SDK ships a readable `ct.sym`.
fn ct_sym_index_ref(db: &dyn HirDatabase, library: LibraryId) -> Option<&CtSymIndex> {
    let project_graph = ProjectGraph::try_get(db)?;
    ct_sym_index_query(db, project_graph, library).as_deref()
}

/// JEP 247: `Some((found, added))` when the platform class `fqn` — binary name,
/// [JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2) —
/// is provided by the *runtime* JDK of `library` but not by the platform API of
/// `release`, where `added` is the earliest release whose view provides it.
/// `None` when the release view provides the class, when the archive cannot
/// answer for `release` (outside its release bounds), or when `ct.sym` never
/// tracks the class at all (an internal `jdk.internal.*` class, whose use javac
/// reports through the module system instead —
/// [JLS §7.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.3)).
///
/// `found` is the requested `release`, returned so callers need not re-read it.
pub fn ct_sym_class_not_in_release(
    db: &dyn HirDatabase,
    library: LibraryId,
    release: u8,
    fqn: &str,
) -> Option<(u8, u8)> {
    let index = ct_sym_index_ref(db, library)?;
    if !index.covers_release(release) {
        return None;
    }
    let added = earliest_release_providing(index, fqn, release)?;
    Some((release, added))
}

/// The earliest release above `release` whose view provides `fqn`, or `None`
/// when the archive provides it at `release` already (or never does).
fn earliest_release_providing(index: &CtSymIndex, fqn: &str, release: u8) -> Option<u8> {
    if index.provides(fqn, release) {
        return None;
    }
    (release + 1..=index.max_release()).find(|added| index.provides(fqn, *added))
}

/// JEP 247 for one member of a platform class. `name`/`descriptor` are the
/// classfile identity of the member
/// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6) for
/// methods and constructors — `<init>` for a constructor — and
/// [§4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5) for fields,
/// where the name alone is already unique); `descriptor: None` matches a member
/// of that name of either kind (the form a static single import needs,
/// [JLS §7.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4)),
/// and with it the *synthesized* members of a classfile — a bridge or synthetic
/// method — are ignored, exactly as source member resolution ignores them.
///
/// `Some((found, added))` when the release's platform view does not declare the
/// member but a later release does. `None` when the release view declares it,
/// when the archive cannot answer for `release` (outside its release bounds),
/// when the class itself is absent from the release's view (the class-level
/// report covers that case, and the two must not both fire), or when no later
/// release declares the member.
pub fn ct_sym_member_not_in_release(
    db: &dyn HirDatabase,
    library: LibraryId,
    release: u8,
    fqn: &str,
    name: &str,
    descriptor: Option<&str>,
) -> Option<(u8, u8)> {
    let index = ct_sym_index_ref(db, library)?;
    if !index.covers_release(release) {
        return None;
    }
    // The check answers for a member of a class the release view provides; a
    // class the view lacks is [`ct_sym_class_not_in_release`]'s report.
    if !index.provides(fqn, release) {
        return None;
    }
    let declares = |release: u8| {
        ct_sym_class_view(db, library, release, fqn)
            .is_some_and(|class| declares_member(db, &class, name, descriptor))
    };
    if declares(release) {
        return None;
    }
    (release + 1..=index.max_release())
        .find(|added| declares(*added))
        .map(|added| (release, added))
}

/// Whether a class declaration as of one release declares a member of the
/// given classfile identity ([JVMS §4.5], [§4.6]).
fn declares_member(
    db: &dyn HirDatabase,
    class: &ClassRecord,
    name: &str,
    descriptor: Option<&str>,
) -> bool {
    let interner = &db.hir_state().interner;
    if let Some(descriptor) = descriptor {
        if class.methods.iter().any(|method| {
            interner.resolve(&method.name) == name
                && interner.resolve(&method.descriptor) == descriptor
        }) {
            return true;
        }
        return class.fields.iter().any(|field| {
            interner.resolve(&field.name) == name
                && interner.resolve(&field.descriptor) == descriptor
        });
    }
    // A name-only match ignores the members a classfile synthesizes, as source
    // member resolution does ([JVMS §4.6], the `ACC_BRIDGE`/`ACC_SYNTHETIC`
    // filter of `library_class_methods`).
    class.methods.iter().any(|method| {
        interner.resolve(&method.name) == name
            && !JvmAccessFlags::from_bits_retain(method.flags)
                .intersects(JvmAccessFlags::BRIDGE | JvmAccessFlags::SYNTHETIC)
    }) || class
        .fields
        .iter()
        .any(|field| interner.resolve(&field.name) == name)
}

/// The declaration of platform class `fqn` as of `release`, parsed from the
/// `.sig` the archive's view of that release holds (memoized per library,
/// release and name). `None` when the release's view does not hold the class,
/// when its entry cannot be read, or when the entry does not parse as a class.
fn ct_sym_class_view(
    db: &dyn HirDatabase,
    library: LibraryId,
    release: u8,
    fqn: &str,
) -> Option<Arc<ClassRecord>> {
    let project_graph = ProjectGraph::try_get(db)?;
    ct_sym_class_view_query(db, project_graph, library, release, Name::new(fqn)).clone()
}

#[salsa::tracked(returns(ref))]
fn ct_sym_class_view_query(
    db: &dyn HirDatabase,
    _project_graph: ProjectGraph,
    library: LibraryId,
    release: u8,
    fqn: Name,
) -> Option<Arc<ClassRecord>> {
    let index = ct_sym_index_ref(db, library)?;
    let fqn = fqn.as_str();
    let (dir, module) = index.part_for(fqn, release)?;
    let archive = crate::db::library_archive(db, library)?;
    let path = ct_sym_path(&archive)?;
    let (package, simple) = match fqn.rsplit_once('.') {
        Some((package, simple)) => (package.replace('.', "/"), simple),
        None => (String::new(), fqn),
    };
    let entry = format!("{dir}/{module}/{package}/{simple}.sig");
    let interner = &db.hir_state().interner;
    let bytes = read_entry(&path, &entry)?;
    match syntax::class_parser::ClassParser::new(interner).parse_cafebabe(&bytes) {
        Ok(ClassOrModuleStub::Class(class)) => Some(Arc::new(class)),
        Ok(ClassOrModuleStub::Module(_)) => None,
        Err(err) => {
            tracing::warn!(path = %path, entry = %entry, "failed to parse a symbol file: {err:#}");
            None
        }
    }
}

/// The bytes of one archive entry, `None` when the archive or the entry cannot
/// be read.
fn read_entry(path: &Utf8Path, entry: &str) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut archive = ZipArchive::new(file).ok()?;
    let mut file = archive.by_name(entry).ok()?;
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_spell_in_base_36() {
        assert_eq!(release_spelling(8), "8");
        assert_eq!(release_spelling(9), "9");
        assert_eq!(release_spelling(10), "A");
        assert_eq!(release_spelling(25), "P");
        assert_eq!(release_spelling(35), "Z");
        assert_eq!(release_spelling(36), "10");
        assert_eq!(release_spelling(46), "1A");
    }

    #[test]
    fn directories_cover_the_releases_they_name() {
        // The directory names of a real archive (JDK 25).
        assert!(dir_covers("8", 8));
        assert!(!dir_covers("8", 9));
        assert!(dir_covers("89", 8) && dir_covers("89", 9));
        assert!(!dir_covers("89", 10));
        assert!(dir_covers("9A", 9) && dir_covers("9A", 10));
        assert!(!dir_covers("9A", 11));
        assert!(dir_covers("BCDEFGHIJKLMNOP", 11));
        assert!(dir_covers("BCDEFGHIJKLMNOP", 25));
        assert!(!dir_covers("BCDEFGHIJKLMNOP", 10));
        assert!(!dir_covers("LMNOP", 20));
        assert!(dir_covers("LMNOP", 21));
        // A name carrying `-` is not a release set at all (javac skips it).
        assert!(!dir_covers("8-mr1", 8));
    }

    /// The archive is found from every layout an SDK keeps its platform classes
    /// in: the modular `<sdk>/lib/modules`, the legacy `<sdk>/lib/rt.jar` and a
    /// JDK 8 install's `<sdk>/jre/lib/rt.jar`.
    #[test]
    fn symbol_archive_is_found_beside_the_platform_archive() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = Utf8PathBuf::from_path_buf(dir.path().join("jdk")).unwrap();
        for archive in ["lib/modules", "lib/rt.jar", "jre/lib/rt.jar"] {
            let archive = home.join(archive);
            std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
            std::fs::write(&archive, b"").unwrap();
        }
        let bare = home.join("lib").join("modules");
        assert_eq!(ct_sym_path(&bare), None);

        // A JDK 9+ install keeps it under `lib/`.
        let lib = home.join("lib").join("ct.sym");
        std::fs::write(&lib, b"").unwrap();
        assert_eq!(ct_sym_path(&bare), Some(lib.clone()));
        assert_eq!(ct_sym_path(&home.join("lib").join("rt.jar")), Some(lib));

        // A JDK 8 install keeps it under `jre/lib/` instead.
        std::fs::remove_file(&home.join("lib").join("ct.sym")).unwrap();
        let legacy = home.join("jre").join("lib").join("ct.sym");
        std::fs::write(&legacy, b"").unwrap();
        assert_eq!(
            ct_sym_path(&home.join("jre").join("lib").join("rt.jar")),
            Some(legacy.clone())
        );
        assert_eq!(ct_sym_path(&bare), Some(legacy));
    }
}
