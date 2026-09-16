//! The language registry of the type layer: one registration per language for
//! the questions the type layer asks of *another* language, plus the JVM view a
//! language's classes expose to the shared member set (IntelliJ: a language's
//! `JvmPsiElement` view plus its own element typing).
//!
//! A language reaches another only through this module: a lookup is keyed by
//! the *target's* file language ([`for_file`] / [`for_class`]), so the layer
//! that owns a class answers for it, and the codec at the boundary
//! ([`LanguageTypes::ty_from_jvm`] / [`LanguageTypes::ty_to_jvm`]) is the target
//! language's own — a Java caller reads a Kotlin class's supertypes in Java's
//! shapes, and never by naming the Kotlin layer.

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::jvm::db::TyDatabase;
use crate::jvm::member::{FieldData, MethodData};
use crate::jvm::member_set::InvocationContext;
use crate::ty::Ty;

/// The JVM-visible members a class of one language declares (IntelliJ:
/// `JvmClass`/`ClassFile`). The *raw* declaration enumeration: the shared
/// call-site member set ([`crate::jvm::member_set`]) applies its own filters on
/// top, and the classfile shape of the members is this trait's contract — a
/// language whose source has no direct classfile equivalent reports what its
/// compiler emits.
pub trait JvmMemberSource: Sync {
    /// The methods named `name` (`""` = all) the class declares, instantiated
    /// with the receiver's type arguments.
    fn methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
        name: &str,
    ) -> Vec<MethodData>;

    /// The fields named `name` (`""` = all) the class declares, instantiated
    /// with the receiver's type arguments.
    fn fields(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
        name: &str,
    ) -> Vec<FieldData>;

    /// The *declared* abstract methods of the class, for the functional-
    /// interface test of JLS §9.8: the members the class itself writes as
    /// abstract, not the inherited closure the caller computes.
    fn abstract_methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
    ) -> Vec<MethodData> {
        let _ = (db, class, args);
        Vec::new()
    }
}

/// The type layer of one language (IntelliJ: the language's `JvmPsiElement`
/// view plus its own element typing).
pub trait LanguageTypes: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];

    /// The JVM view of this language's classes: what the shared member set
    /// ([`crate::jvm::member_set`]) enumerates a class of this language
    /// through.
    fn member_source(&self) -> &'static dyn JvmMemberSource;

    /// The `Ty` a JVM type denotes in this language: the identity for Java,
    /// Kotlin's platform types for a Java type
    /// ([`crate::kotlin::ty::ty_from_java`]).
    fn ty_from_jvm(&self, db: &dyn TyDatabase, ty: Ty) -> Ty;

    /// The JVM's view of a type of this language — the inverse codec, so a
    /// caller of another language reads this language's types in its own
    /// shapes ([`crate::kotlin::ty::ty_from_kotlin`] for a Kotlin type). The
    /// identity for Java.
    fn ty_to_jvm(&self, db: &dyn TyDatabase, ty: Ty) -> Ty;

    /// The declared supertypes of `ty` in `scope`, in this language's types.
    fn supertypes(&self, db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: Ty) -> Vec<Ty>;

    /// The access-control context of the call sites of `file` — derived from
    /// this language's own declarations.
    fn access_context(
        &self,
        db: &dyn TyDatabase,
        file: FileId,
        item: hir_expand::ids::ItemId,
    ) -> InvocationContext;

    /// The workspace files the file's type outputs resolve against, for the
    /// cross-file dependency index.
    fn file_resolved_deps(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<FileId>>;

    /// The resolution-relevant names of the file, the sound name-level fallback
    /// of the cross-file dependency index.
    fn file_dependency_refs(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<Name>>;
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LanguageTypes] =
    &[&crate::java::plugin::JAVA, &crate::kotlin::plugin::KOTLIN];

/// The type layer answering for a file of `kind`.
pub fn types(kind: LanguageKind) -> Option<&'static dyn LanguageTypes> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The type layer of the language declaring `file`.
pub fn for_file(db: &dyn TyDatabase, file: FileId) -> Option<&'static dyn LanguageTypes> {
    types(hir_def::lang::declarations(db, file).language())
}

/// The type layer of the language declaring `class` — the reference for a
/// cross-language lookup.
pub fn for_class(db: &dyn TyDatabase, class: &hir::Resolved) -> Option<&'static dyn LanguageTypes> {
    match class {
        hir::Resolved::Source(source) => for_file(db, source.file),
        hir::Resolved::Facade { file, .. } => for_file(db, *file),
        // A classfile declares no source language ([`classfile`]).
        hir::Resolved::Library(_) => None,
    }
}

/// The JVM view of the class `class` declares, `None` for a classfile (whose
/// members the shared member set enumerates from its record itself).
pub fn member_source(
    db: &dyn TyDatabase,
    class: &hir::Resolved,
) -> Option<&'static dyn JvmMemberSource> {
    for_class(db, class).map(|language| language.member_source())
}

/// The type layer that answers for a *classfile* class — and for a file whose
/// declarations no language lowered: a classfile declares no source language,
/// so the entry that drives the classfile readers answers for it, in the shared
/// `Ty` model. Every language reads the result in its own types through its own
/// codec.
pub fn classfile() -> &'static dyn LanguageTypes {
    &crate::java::plugin::JAVA
}

/// The file dependencies of `file`, answered by the language that declares it:
/// the workspace files its type outputs resolve against, and the names it
/// mentions.
pub fn file_resolved_deps(db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<FileId>> {
    for_file(db, file).map_or_else(
        || Arc::new(FxHashSet::default()),
        |language| language.file_resolved_deps(db, file),
    )
}

/// The resolution-relevant names of `file` (see [`file_resolved_deps`]).
pub fn file_dependency_refs(db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<Name>> {
    for_file(db, file).map_or_else(
        || Arc::new(FxHashSet::default()),
        |language| language.file_dependency_refs(db, file),
    )
}

/// Whether the Java language declares `file` — the Java layer's own question at
/// a site that must read a Java item tree by item id: a class of another
/// language has no Java declaration, and its id indexes another model's arena.
pub fn is_java_file(db: &dyn TyDatabase, file: FileId) -> bool {
    hir_def::java::plugin::declares_file(db, file)
}
