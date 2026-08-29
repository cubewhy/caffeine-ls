//! Light-class views: a small, JVM-level abstraction over a class — whether
//! it comes from a workspace source file or a compiled library — without any
//! Java (or Kotlin) syntax concepts.
//!
//! This is the `jvm`-namespace hook prepared for class resolution and the
//! light-class synthetic view: [`LightClassId`] names a class in a workspace
//! source file ([`ItemId`]) or in a library ([`project_model::LibraryId`]),
//! and [`LightClass`] is the JVM-level face of it (its name, kind, access
//! flags and supertypes). The concrete construction is the language layer's
//! job: `hir` builds it from the stub index for libraries and from the Java
//! item tree for sources. Nothing here references a Java AST.

use vfs::FileId;

use hir_expand::name::Name;

use crate::jvm::access::JvmAccessFlags;
use crate::jvm::fqn::FqName;
use crate::jvm::ids::ItemId;
use crate::jvm::stubs::ClassKind;

/// Identifies a class for the light-class view: a workspace source item or a
/// library entry. The `jvm`-namespace analog of `hir::Resolved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LightClassId {
    /// A class declared by a workspace source file.
    Source { file: FileId, item: ItemId },
    /// A class read from a compiled library.
    Library(project_model::LibraryId),
}

/// The JVM-level face of a class, independent of whether it is defined in
/// source or in bytecode. Consumed by IDE features that only need the
/// declaration-level facts of a class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightClass {
    /// The fully qualified name of the class.
    pub name: FqName,
    /// Whether the class is a class, interface, enum, record or annotation
    /// type ([JVMS §4.1]).
    pub kind: ClassKind,
    /// The JVM access flags of the class ([JVMS §4.1]).
    pub access: JvmAccessFlags,
    /// The direct superclass, if any.
    pub super_class: Option<FqName>,
    /// The direct superinterfaces.
    pub interfaces: Vec<FqName>,
    /// The declared type parameters, in order.
    pub type_params: Vec<Name>,
}
