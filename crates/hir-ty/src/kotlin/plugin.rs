//! Kotlin as the type layer's registry entry: the JVM view of a Kotlin class
//! ([`JvmMemberSource`], the `FooKt` facade included) and the Kotlin type
//! layer's own answers ([`LanguageTypes`]).

use triomphe::Arc;

use base_db::LanguageKind;
use hir::hir_def::kotlin::item_tree::{KotlinClassKind, KotlinItemData};
use hir::hir_def::kotlin::modifiers::KotlinModality;
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::{
    jvm::db::TyDatabase,
    jvm::member::{FieldData, JvmClassKind, MethodData},
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

    /// The workspace source files whose declarations `file`'s type outputs
    /// resolve against — its imports, the type references of its declaration
    /// model and bodies, and the declarations its bodies resolved their names
    /// to, closed over source supertypes. See
    /// [`crate::kotlin::dep_index::file_resolved_deps`].
    fn file_resolved_deps(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<FileId>> {
        crate::kotlin::dep_index::file_resolved_deps(db, file)
    }

    /// The resolution-relevant *names* of `file`, the sound name-level fallback
    /// of the cross-file dependency index. See
    /// [`crate::kotlin::dep_index::file_dependency_refs`].
    fn file_dependency_refs(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<Name>> {
        crate::kotlin::dep_index::file_dependency_refs(db, file)
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

    /// KLS gives an interface's body-less member no implementation
    /// ([KLS `declarations.html#classifier-declaration`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)):
    /// the classfile carries it `abstract`, and the JVM view is where that flag
    /// is derived ([`Shapes::member_flags`], which marks an interface member
    /// with no body abstract and one with a body the `default` method). A
    /// *class* declares no abstract member of its own here — an abstract member
    /// it inherits is found by the caller's walk — and neither does a
    /// classfile-backed Kotlin declaration.
    fn abstract_methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        _args: &[Ty],
    ) -> Vec<MethodData> {
        let hir::Resolved::Source(source) = class else {
            return Vec::new();
        };
        java_view_members(db, *source, "")
            .into_iter()
            .filter(|method| method.abstract_ && !method.is_static)
            .collect()
    }

    /// The JVM kind of a Kotlin classifier, from the kind its declaration
    /// writes and the modality it compiles with
    /// (<https://kotlinlang.org/docs/java-interop.html#classes-and-interfaces>):
    /// an `interface` and an `annotation class` are interfaces in the
    /// classfile, an `enum class` is an enum, and `class`/`object`/
    /// `companion object` are classes.
    ///
    /// A `class` is `final` unless it writes `open`, `abstract` or `sealed`
    /// ([KLS
    /// `declarations.html#classifier-declaration`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)),
    /// and so are an `object` and a `companion object`, which cannot be
    /// subclassed at all. Every other member of the tuple is `false`: an
    /// interface is never final, and neither is an enum whose entries may write
    /// bodies — the classfile still marks the `final` the compiler emits, and
    /// nothing here needs to claim it.
    fn kind(&self, db: &dyn TyDatabase, class: &hir::Resolved) -> Option<(JvmClassKind, bool)> {
        match class {
            // A file's facade is a final class.
            hir::Resolved::Facade { .. } => Some((JvmClassKind::Facade, true)),
            hir::Resolved::Library(_) => None,
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(db, source.file);
                let tree = hir_def::kotlin::plugin::model(&tree)?;
                let KotlinItemData::Class(declaration) = tree.data(source.item) else {
                    return None;
                };
                let kind = match declaration.kind {
                    KotlinClassKind::Interface => JvmClassKind::Interface,
                    KotlinClassKind::Annotation => JvmClassKind::Annotation,
                    KotlinClassKind::Enum => JvmClassKind::Enum,
                    KotlinClassKind::Class
                    | KotlinClassKind::Object
                    | KotlinClassKind::CompanionObject => JvmClassKind::Class,
                };
                let final_ = declaration.kind != KotlinClassKind::Interface
                    && declaration.kind != KotlinClassKind::Annotation
                    && declaration.modifiers.modality == KotlinModality::Final;
                Some((kind, final_))
            }
        }
    }
}
