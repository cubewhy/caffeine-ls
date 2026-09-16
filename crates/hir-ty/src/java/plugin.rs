//! Java as the type layer's registry entry: the JVM view of a Java class
//! ([`JvmMemberSource`]) and the Java type layer's own answers
//! ([`LanguageTypes`]).

use base_db::LanguageKind;
use hir_def::java::item_tree::ItemData;
use vfs::FileId;

use crate::{
    java::{
        method::{access_context, source_class_fields, source_class_methods},
        resolve::item_data,
        subtyping,
        ty::Ty,
    },
    jvm::db::TyDatabase,
    jvm::member::{FieldData, MethodData},
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
        let tree = hir::java_item_tree(db, source.file);
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
}
