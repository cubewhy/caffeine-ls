//! The deprecation of a Kotlin declaration, as a *Java* caller of it sees it
//! ([JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)).
//!
//! A Kotlin declaration is deprecated by the `kotlin.Deprecated` annotation its
//! declaration applies. kotlinc 2.4.20 compiles that annotation to *both* the
//! classfile's `Deprecated` attribute
//! ([JVMS §4.7.15](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.15))
//! and the runtime-visible `kotlin.Deprecated` annotation — `javap -v -p` on
//! `class D { @Deprecated("use g") fun f(): Int = 1 }` prints
//! `Deprecated: true` and `kotlin.Deprecated(message="use g")`, at every
//! `level` — so a Java caller reads a deprecated Kotlin declaration exactly as
//! it reads a deprecated Java one
//! (<https://kotlinlang.org/docs/java-interop.html> names the compiler's
//! attribute).
//!
//! The annotation's identity is the canonical name its application *resolves*
//! to, never the last segment the source wrote: `kotlin.Deprecated`, its
//! qualified spelling and an alias of it are the standard library's, while a
//! `Deprecated` the file's own package or an enclosing classifier declares is
//! another annotation — the rule [`crate::kotlin::jvm_view`] applies to the
//! `kotlin.jvm` annotations.

use hir_def::jvm::decl::ItemAnnotationValue;
use hir_def::kotlin::item_tree::{KotlinAnnotationRef, KotlinItemData};
use hir_expand::ids::ItemId;
use vfs::FileId;

use crate::java::deprecation::Deprecation;
use crate::jvm::db::TyDatabase;
use crate::kotlin::jvm_view::annotation_fqn;
use crate::kotlin::resolve::KotlinResolver;

/// The canonical name of the standard library's deprecation annotation, whose
/// applications are the ones the compiler turns into the classfile's
/// `Deprecated` attribute (kotlin-stdlib's `kotlin.Deprecated`).
const DEPRECATED: &str = "kotlin.Deprecated";

/// The level a `@Deprecated` argument list declares, as javac's two kinds.
///
/// Kotlin's `DeprecationLevel` has three values
/// (<https://kotlinlang.org/docs/annotations.html#deprecated>). `WARNING` — and
/// an absent argument — is the ordinary deprecation, the one an enclosing
/// `@Deprecated` exempts from the warning ([JLS §9.6.4.6]'s "the use is within
/// an entity that is itself annotated with `@Deprecated`"). `ERROR` and
/// `HIDDEN` are [`Deprecation::Terminal`]: the classfile carries the same
/// boolean `Deprecated` attribute for every level (kotlinc 2.4.20), so the
/// level is this layer's own reading of `kotlin.Deprecated`'s `level` element,
/// and a level the compiler refuses to use quietly is one a Java caller should
/// not be invited to use without a warning that survives a deprecated
/// enclosing declaration.
///
/// A level written as an *expression* the lowering could not carry
/// ([`ItemAnnotationValue::Unresolved`]) is not read, and the deprecation is
/// the ordinary one.
pub(crate) fn level_of(args: &[hir_def::jvm::decl::ItemAnnotationArg]) -> Deprecation {
    for arg in args {
        if arg.name.as_str() != "level" {
            continue;
        }
        let ItemAnnotationValue::EnumConstant { member, .. } = &arg.value else {
            continue;
        };
        if matches!(member.as_str(), "ERROR" | "HIDDEN") {
            return Deprecation::Terminal;
        }
    }
    Deprecation::Ordinary
}

/// The deprecation of the Kotlin declaration `item` of `file`, or `None` when
/// the declaration applies no `kotlin.Deprecated`.
pub(crate) fn item_deprecation(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
) -> Option<Deprecation> {
    let outer = hir::file_item_tree(db, file);
    let tree = hir_def::kotlin::plugin::model(&outer)?;
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    for application in annotations_of(tree.data(item)) {
        if annotation_fqn(&resolver, application).is_some_and(|fqn| fqn.as_str() == DEPRECATED) {
            return Some(level_of(&application.annotation.args));
        }
    }
    None
}

/// The annotations the declaration of `data` applies, in source order — the
/// accessor of every declaration form that can carry one, since a Kotlin
/// `@Deprecated` is applicable to a class, a function, a property, a
/// constructor, an enum entry and a type alias alike
/// (<https://kotlinlang.org/docs/annotations.html#annotation-targets>).
fn annotations_of(data: &KotlinItemData) -> &[KotlinAnnotationRef] {
    match data {
        KotlinItemData::Class(d) => &d.annotations,
        KotlinItemData::Constructor(d) => &d.annotations,
        KotlinItemData::Function(d) => &d.annotations,
        KotlinItemData::Property(d) => &d.annotations,
        KotlinItemData::EnumEntry(d) => &d.annotations,
        KotlinItemData::TypeAlias(d) => &d.annotations,
        // An accessor's own annotations are `@get:`/`@set:`-targeted, and a
        // `kotlin.Deprecated` on an *accessor* is a compiler error ("this
        // annotation is not applicable to target 'member property'", kotlinc
        // 2.4.20); an `init` block carries none.
        KotlinItemData::Accessor(_) | KotlinItemData::AnonymousInitializer(_) => &[],
    }
}
