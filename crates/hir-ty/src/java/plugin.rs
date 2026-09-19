//! Java as the type layer's registry entry: the JVM view of a Java class
//! ([`JvmMemberSource`]) and the Java type layer's own answers
//! ([`LanguageTypes`]).

use triomphe::Arc;

use base_db::LanguageKind;
use hir_def::java::item_tree::ItemData;
use hir_def::jvm::access::JvmAccessFlags;
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::{
    java::{
        method::{access_context, source_class_fields, source_class_methods},
        resolve::item_data,
        subtyping,
        ty::Ty,
    },
    jvm::db::TyDatabase,
    jvm::member::{FieldData, JvmClassKind, MethodData},
    jvm::member_set::InvocationContext,
    lang::{JvmMemberSource, LanguageTypes},
};

pub(crate) struct Java;

pub(crate) static JAVA: Java = Java;

impl LanguageTypes for Java {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Java]
    }

    fn member_source(&self) -> &'static dyn JvmMemberSource {
        &JAVA
    }

    fn ty_from_jvm(&self, _db: &dyn TyDatabase, ty: Ty) -> Ty {
        // A JVM type *is* a Java type.
        ty
    }

    fn ty_to_jvm(&self, _db: &dyn TyDatabase, ty: Ty) -> Ty {
        ty
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
        access_context(db, file, item)
    }

    fn file_resolved_deps(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<FileId>> {
        crate::java::db::file_resolved_deps(db, file)
    }

    fn file_dependency_refs(&self, db: &dyn TyDatabase, file: FileId) -> Arc<FxHashSet<Name>> {
        crate::java::db::file_dependency_refs(db, file)
    }
}

impl JvmMemberSource for Java {
    fn methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
        name: &str,
    ) -> Vec<MethodData> {
        let hir::Resolved::Source(source) = class else {
            return Vec::new();
        };
        source_class_methods(db, *source, args.to_vec(), name)
    }

    fn fields(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
        name: &str,
    ) -> Vec<FieldData> {
        let hir::Resolved::Source(source) = class else {
            return Vec::new();
        };
        source_class_fields(db, *source, args.to_vec(), name)
    }

    fn abstract_methods(
        &self,
        db: &dyn TyDatabase,
        class: &hir::Resolved,
        args: &[Ty],
    ) -> Vec<MethodData> {
        // §9.4.1: the abstract members of a Java interface are the methods its
        // declaration writes as abstract — an interface's other members are
        // `default` or `static` and are not part of the functional-interface
        // descriptor. A *class* declares no abstract member of its own here:
        // an abstract method it inherits is found by the caller's walk.
        let hir::Resolved::Source(source) = class else {
            return Vec::new();
        };
        let tree = hir_def::java::plugin::tree(db, source.file);
        let Some(ItemData::Interface(class)) = item_data(&tree, source.item) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &item in &class.body {
            let Some(ItemData::Method(method)) = item_data(&tree, item) else {
                continue;
            };
            out.extend(source_class_methods(
                db,
                *source,
                args.to_vec(),
                method.name.as_str(),
            ));
        }
        out
    }

    /// The JVM kind of a Java class, from its classfile flags or its source
    /// modifiers ([JLS §8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1),
    /// [§8.1.1.2], [§8.9], [§9.1]).
    fn kind(&self, db: &dyn TyDatabase, class: &hir::Resolved) -> Option<(JvmClassKind, bool)> {
        match class {
            // A file's facade is a final class ([`crate::kotlin`]'s top-level
            // declarations compile into one).
            hir::Resolved::Facade { .. } => Some((JvmClassKind::Facade, true)),
            hir::Resolved::Library(library) => {
                let record = hir::class_record(db, library)?;
                let syntax::stub::ClassOrModuleStub::Class(class) = record.as_ref() else {
                    return None;
                };
                // JVM access flags ([JVMS §4.1]): an interface carries
                // ACC_INTERFACE (an annotation type both it and
                // ACC_ANNOTATION), an enum ACC_ENUM, a final class ACC_FINAL.
                // A record is implicitly final ([§8.10]).
                let flags = JvmAccessFlags::from_bits_retain(class.flags);
                let kind = if flags.is_interface() {
                    match flags.is_annotation() {
                        true => JvmClassKind::Annotation,
                        false => JvmClassKind::Interface,
                    }
                } else if flags.is_enum() {
                    JvmClassKind::Enum
                } else {
                    JvmClassKind::Class
                };
                Some((kind, flags.is_final() || class.is_record))
            }
            hir::Resolved::Source(source) => {
                let tree = hir_def::java::plugin::tree(db, source.file);
                match item_data(&tree, source.item)? {
                    ItemData::Class(d) => Some((JvmClassKind::Class, d.modifiers.is_final())),
                    ItemData::Record(_) => Some((JvmClassKind::Class, true)),
                    // §8.9: an enum without constant bodies is implicitly final,
                    // but treating every enum as final only ever tightens a cast
                    // check that single inheritance already makes disjoint.
                    ItemData::Enum(_) => Some((JvmClassKind::Enum, true)),
                    ItemData::Interface(_) => Some((JvmClassKind::Interface, false)),
                    // §9.6: an annotation type *is* an interface, which is why
                    // it is a functional interface's candidate too.
                    ItemData::Annotation(_) => Some((JvmClassKind::Annotation, false)),
                    // A resolved *type reference* names one of the five
                    // class-like kinds above; anything else here is not a class
                    // at all, and the interface-like answer is the permissive
                    // one this classifier has always given.
                    _ => Some((JvmClassKind::Interface, false)),
                }
            }
        }
    }
}
