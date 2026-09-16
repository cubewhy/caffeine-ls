//! Kotlin as the type layer's registry entry: the JVM view of a Kotlin class
//! ([`JvmMemberSource`], the `FooKt` facade included) and the Kotlin type
//! layer's own answers ([`LanguageTypes`]).

use triomphe::Arc;

use base_db::LanguageKind;
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::{
    jvm::db::TyDatabase,
    jvm::member::{FieldData, MethodData},
    jvm::member_set::InvocationContext,
    kotlin::{
        jvm_view::{file_facade_fields, file_facade_members, java_view_fields, java_view_members},
        method::access_context_for_kotlin,
        subtyping,
        ty::{ty_from_java, ty_from_kotlin},
    },
    lang::{JvmMemberSource, LanguageTypes},
    ty::Ty,
};

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl LanguageTypes for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn member_source(&self) -> &'static dyn JvmMemberSource {
        &KOTLIN
    }

    fn ty_from_jvm(&self, db: &dyn TyDatabase, ty: Ty) -> Ty {
        // A Java or classfile type is a *platform* type in Kotlin
        // ([KLS `java-interop.html#null-safety-and-platform-types`]).
        ty_from_java(db, ty)
    }

    fn ty_to_jvm(&self, db: &dyn TyDatabase, ty: Ty) -> Ty {
        ty_from_kotlin(db, ty)
    }

    fn supertypes(&self, db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: Ty) -> Vec<Ty> {
        subtyping::supertypes(db, scope, &ty)
    }

    fn access_context(
        &self,
        db: &dyn TyDatabase,
        file: FileId,
        item: hir_expand::ids::ItemId,
    ) -> InvocationContext {
        access_context_for_kotlin(db, file, item)
    }

    /// Kotlin's own dependency index is not built yet: the file depends on
    /// nothing it does not declare, which is the answer the Java index gave
    /// over a Kotlin file's (empty) Java model — a recorded gap, not a
    /// regression.
    fn file_resolved_deps(&self, _db: &dyn TyDatabase, _file: FileId) -> Arc<FxHashSet<FileId>> {
        Arc::new(FxHashSet::default())
    }

    /// See [`Kotlin::file_resolved_deps`](Self::file_resolved_deps).
    fn file_dependency_refs(&self, _db: &dyn TyDatabase, _file: FileId) -> Arc<FxHashSet<Name>> {
        Arc::new(FxHashSet::default())
    }
}

impl JvmMemberSource for Kotlin {
    fn methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        _args: &[Ty],
        name: &str,
    ) -> Vec<MethodData> {
        match class {
            hir::Resolved::Source(source) => java_view_members(db, *source, name),
            // A file's facade carries its top-level functions.
            hir::Resolved::Facade { file, .. } => file_facade_members(db, *file, name),
            hir::Resolved::Library(_) => Vec::new(),
        }
    }

    fn fields(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        _args: &[Ty],
        name: &str,
    ) -> Vec<FieldData> {
        match class {
            hir::Resolved::Source(source) => java_view_fields(db, *source, name),
            // A file's facade carries its top-level `const val`s.
            hir::Resolved::Facade { file, .. } => file_facade_fields(db, *file, name),
            hir::Resolved::Library(_) => Vec::new(),
        }
    }
}
