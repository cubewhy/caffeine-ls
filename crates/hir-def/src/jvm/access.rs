//! JVM access semantics: the canonical [`JvmAccessFlags`] bitflags and the
//! semantic visibility model derived from a classfile's access flags.
//!
//! The bitflags definition itself lives in [`rust_asm::constants`] — the
//! single source of truth for the JVM ClassFile access-flag set ([JVMS §4.1]
//! classes, [§4.5] fields, [§4.6] methods) — and is re-exported here so the
//! semantic layer never re-states the bit values.
//!
//! [JVMS §4.1]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1
//! [§4.5]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5
//! [§4.6]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6

use syntax::stub::ClassKind;

pub use rust_asm::constants::JvmAccessFlags;

/// The visibility of a member or type, ordered from most to least accessible.
///
/// A declaration's visibility is derived from its JVM access flags; the
/// collapse order follows [JLS §6.6.1]: `private` wins over `protected`,
/// which wins over `public`, and the absence of all three yields the
/// package-private default.
///
/// [JLS §6.6.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum JvmVisibility {
    Public,
    Protected,
    #[default]
    Package,
    Private,
}

impl JvmVisibility {
    /// The visibility of the JVM access flags ([JLS §6.6.1]).
    pub fn from_access_flags(flags: JvmAccessFlags) -> JvmVisibility {
        if flags.contains(JvmAccessFlags::PRIVATE) {
            JvmVisibility::Private
        } else if flags.contains(JvmAccessFlags::PROTECTED) {
            JvmVisibility::Protected
        } else if flags.contains(JvmAccessFlags::PUBLIC) {
            JvmVisibility::Public
        } else {
            JvmVisibility::Package
        }
    }

    /// Whether the visibility is `public`.
    pub fn is_public(self) -> bool {
        self == JvmVisibility::Public
    }

    /// Whether the visibility is `protected` or `public`.
    pub fn is_protected_or_public(self) -> bool {
        matches!(self, JvmVisibility::Protected | JvmVisibility::Public)
    }
}

/// Classifies a type by its JVM access flags and the presence of a `Record`
/// attribute. Records carry no distinguishing access flag ([JVMS §4.1]), so
/// `is_record` disambiguates them from plain classes. Mirrors
/// [`syntax::stub::ClassKind::from_flags`], which stays authoritative for the
/// classfile reader.
pub fn class_kind_from_flags(flags: JvmAccessFlags, is_record: bool) -> ClassKind {
    if flags.contains(JvmAccessFlags::INTERFACE) {
        if flags.contains(JvmAccessFlags::ANNOTATION) {
            ClassKind::Annotation
        } else {
            ClassKind::Interface
        }
    } else if flags.contains(JvmAccessFlags::ENUM) {
        ClassKind::Enum
    } else if is_record {
        ClassKind::Record
    } else {
        ClassKind::Class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_collapse_order() {
        assert_eq!(
            JvmVisibility::from_access_flags(JvmAccessFlags::PUBLIC | JvmAccessFlags::PRIVATE),
            JvmVisibility::Private
        );
        assert_eq!(
            JvmVisibility::from_access_flags(JvmAccessFlags::PUBLIC | JvmAccessFlags::PROTECTED),
            JvmVisibility::Protected
        );
        assert_eq!(
            JvmVisibility::from_access_flags(JvmAccessFlags::PUBLIC),
            JvmVisibility::Public
        );
        assert_eq!(
            JvmVisibility::from_access_flags(JvmAccessFlags::STATIC),
            JvmVisibility::Package
        );
    }

    #[test]
    fn class_kind_classification() {
        assert_eq!(
            class_kind_from_flags(JvmAccessFlags::PUBLIC, false),
            ClassKind::Class
        );
        assert_eq!(
            class_kind_from_flags(
                JvmAccessFlags::PUBLIC | JvmAccessFlags::INTERFACE | JvmAccessFlags::ABSTRACT,
                false
            ),
            ClassKind::Interface
        );
        assert_eq!(
            class_kind_from_flags(
                JvmAccessFlags::PUBLIC | JvmAccessFlags::INTERFACE | JvmAccessFlags::ANNOTATION,
                false
            ),
            ClassKind::Annotation
        );
        assert_eq!(
            class_kind_from_flags(JvmAccessFlags::PUBLIC | JvmAccessFlags::ENUM, false),
            ClassKind::Enum
        );
        assert_eq!(
            class_kind_from_flags(JvmAccessFlags::PUBLIC, true),
            ClassKind::Record
        );
    }
}
