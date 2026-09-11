//! The `ct.sym` check against the *real* archive of the JDK at `JAVA_HOME`.
//!
//! Every other release-API test runs on a hand-built archive, so this is the
//! only proof that the reader copes with the archive an SDK actually ships:
//! 20 000 entries, ~167 release directories, nested classes spelled with `$`,
//! and `.sig` files that `rust-asm` must parse.
//!
//! The test skips — with a notice, never an assertion — when `JAVA_HOME` is
//! unset, when the SDK ships no `lib/ct.sym`, or when a checked release lies
//! above the archive's own last one (so an older JDK still passes).

mod common;

use camino::Utf8PathBuf;
use common::TestDatabase;
use hir::{LibraryId, LibraryKind};
use vfs::AbsPathBuf;

/// A database with the JDK at `JAVA_HOME` registered as its single library.
/// `None` with a notice when the JDK or its symbol archive is unavailable.
fn real_jdk() -> Option<(TestDatabase, LibraryId)> {
    let Ok(java_home) = std::env::var("JAVA_HOME") else {
        eprintln!("skipping: JAVA_HOME is not set");
        return None;
    };
    let home = Utf8PathBuf::from(java_home);
    let ct_sym = home.join("lib").join("ct.sym");
    if !ct_sym.as_std_path().is_file() {
        eprintln!("skipping: {ct_sym} does not exist");
        return None;
    }
    let archive = home.join("lib").join("modules");
    if !archive.as_std_path().is_file() {
        eprintln!("skipping: {archive} does not exist");
        return None;
    }
    let lib = LibraryId::from_file_path(archive.as_std_path()).unwrap();

    let mut db = TestDatabase::new();
    let mut data = hir::ProjectGraphData::default();
    data.libraries.insert(
        lib,
        hir::LibraryInfo::new(
            LibraryKind::Jimage,
            AbsPathBuf::assert_utf8(archive.into_std_path_buf()),
        ),
    );
    data.jdk_libraries.push(lib);
    hir::set_project_graph(&mut db, data);
    Some((db, lib))
}

#[test]
fn real_ct_sym_indexes_and_reports() {
    let Some((db, lib)) = real_jdk() else {
        return;
    };
    let index = hir::ct_sym_index(&db, lib).expect("the SDK ships a readable ct.sym");
    let (min, max) = (index.min_release(), index.max_release());
    eprintln!("ct.sym covers releases {min}..={max}");
    assert!(max - min > 5, "the archive should span many releases");
    // A class the archive tracks at all is tracked at the archive's last
    // release: `ct.sym` is generated from the very JDK that ships it.
    assert!(index.tracks("java.lang.Object"));

    // A class that first appeared in release 21 (`--release 20` rejects it and
    // `--release 21` accepts it) is exactly what `--release 8` must flag.
    assert_eq!(
        hir::ct_sym_class_not_in_release(&db, lib, 8, "java.util.SequencedCollection"),
        Some((8, 21))
    );
    assert_eq!(
        hir::ct_sym_class_not_in_release(&db, lib, 21, "java.util.SequencedCollection"),
        None
    );

    // A class that has been there all along is never reported — under any
    // release the archive can answer for.
    for release in min..=max {
        assert_eq!(
            hir::ct_sym_class_not_in_release(&db, lib, release, "java.util.List"),
            None,
            "java.util.List exists at release {release}"
        );
    }

    // A class the archive never tracks (an internal `jdk.internal.*` class —
    // the runtime jimage has it, `ct.sym` does not) is never reported, because
    // javac rejects its use through the module system instead.
    assert!(!index.tracks("jdk.internal.misc.Unsafe"));
    assert_eq!(
        hir::ct_sym_class_not_in_release(&db, lib, 8, "jdk.internal.misc.Unsafe"),
        None
    );

    // A release above the archive's last one cannot be answered for, so the
    // check abstains rather than reporting every platform class.
    let beyond = max.saturating_add(1);
    if beyond > 19 {
        assert_eq!(
            hir::ct_sym_class_not_in_release(&db, lib, beyond, "java.util.SequencedCollection"),
            None
        );
    }
}
