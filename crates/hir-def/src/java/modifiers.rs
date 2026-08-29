//! The Java *syntax* modifier model, faithful to the source grammar.
//!
//! Java modifiers split into three axes ([JLS §8.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1)
//! classes, [§8.3.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3.1)
//! fields, [§8.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3)
//! methods):
//!
//! * [`JavaVisibility`] — `public` / `protected` / package-private / `private`;
//! * [`JavaModality`] — the type-classification modifiers `abstract`,
//!   `final`, `sealed`, `non-sealed`;
//! * [`JavaModifierFlags`] — everything else (`static`, `synchronized`,
//!   `native`, `strictfp`, `transient`, `volatile`, `default`).
//!
//! This is a pure *source* structure: it faithfully records what the
//! declaration spelled, without the synthetic defaults the JLS applies
//! (interface members are implicitly `public abstract`, enum constants are
//! implicitly `public static final`, ...). The defaults are applied at the
//! JVM boundary by [`JavaModifiers::to_jvm_access_flags`], which lowers this
//! syntax view into the [`crate::jvm::access::JvmAccessFlags`] bitflags of the
//! compiled artifact.
//!
//! Annotations are *not* stored here: they are declaration attributes, not
//! modifiers, and live in a separate per-item field so they never bloat the
//! modifier representation.

use bitflags::bitflags;

use crate::jvm::access::JvmAccessFlags;

/// The accessibility modifiers of a declaration ([JLS §6.6.1]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JavaVisibility {
    Public,
    Protected,
    /// The unnamed accessibility: no access modifier at all.
    Package,
    Private,
}

impl Default for JavaVisibility {
    fn default() -> Self {
        JavaVisibility::Package
    }
}

impl JavaVisibility {
    /// The keyword that spells this visibility, if the source wrote one.
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            JavaVisibility::Public => Some("public"),
            JavaVisibility::Protected => Some("protected"),
            JavaVisibility::Private => Some("private"),
            JavaVisibility::Package => None,
        }
    }

    /// Whether the declaration is `public`.
    pub fn is_public(self) -> bool {
        self == JavaVisibility::Public
    }
}

bitflags! {
    /// The type-classification modifiers of a declaration: `abstract`,
    /// `final`, `sealed` and `non-sealed`. `sealed` and `non-sealed` are
    /// mutually exclusive with each other but both may co-occur with
    /// `abstract`, so a flat bitflags is the faithful shape.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct JavaModality: u8 {
        const ABSTRACT = 0b0000_0001;
        const FINAL = 0b0000_0010;
        const SEALED = 0b0000_0100;
        const NON_SEALED = 0b0000_1000;
    }
}

impl Default for JavaModality {
    fn default() -> Self {
        JavaModality::empty()
    }
}

bitflags! {
    /// The remaining modifiers of a declaration ([JLS §8.3.1], [§8.4.3]):
    /// `static`, `synchronized`, `native`, `strictfp`, `transient`,
    /// `volatile` and `default`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct JavaModifierFlags: u8 {
        const STATIC = 0b0000_0001;
        const SYNCHRONIZED = 0b0000_0010;
        const NATIVE = 0b0000_0100;
        const STRICTFP = 0b0000_1000;
        const TRANSIENT = 0b0001_0000;
        const VOLATILE = 0b0010_0000;
        const DEFAULT = 0b0100_0000;
    }
}

impl Default for JavaModifierFlags {
    fn default() -> Self {
        JavaModifierFlags::empty()
    }
}

/// The source modifiers of a Java declaration, split into the three axes of
/// the grammar. Compact — a `JavaVisibility` tag, a byte of modality flags
/// and a byte of general flags — with no per-modifier booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct JavaModifiers {
    pub visibility: JavaVisibility,
    pub modality: JavaModality,
    pub flags: JavaModifierFlags,
}

/// The kind of declaration whose modifiers are being lowered, used to apply
/// the JLS default access flags ([JLS §8.9], [§9.1.1], [§9.3], [§9.4], [§9.6]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierTarget {
    Class,
    Interface,
    AnnotationType,
    Enum,
    Record,
    /// An enum constant ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)).
    EnumConstant,
    Field,
    Method,
    Constructor,
    /// An annotation element declaration
    /// ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1)).
    AnnotationElement,
}

impl JavaModifiers {
    /// The empty modifier set.
    pub fn none() -> JavaModifiers {
        JavaModifiers::default()
    }

    /// Records a modifier keyword. Returns `false` for unrecognized modifiers
    /// (e.g. `transitive`/`open` module modifiers, which are lowered
    /// elsewhere).
    pub fn push(&mut self, keyword: &str) -> bool {
        match keyword {
            "public" => self.visibility = JavaVisibility::Public,
            "protected" => self.visibility = JavaVisibility::Protected,
            "private" => self.visibility = JavaVisibility::Private,
            "abstract" => self.modality.insert(JavaModality::ABSTRACT),
            "final" => self.modality.insert(JavaModality::FINAL),
            "sealed" => self.modality.insert(JavaModality::SEALED),
            "non-sealed" => self.modality.insert(JavaModality::NON_SEALED),
            "static" => self.flags.insert(JavaModifierFlags::STATIC),
            "synchronized" => self.flags.insert(JavaModifierFlags::SYNCHRONIZED),
            "native" => self.flags.insert(JavaModifierFlags::NATIVE),
            "strictfp" => self.flags.insert(JavaModifierFlags::STRICTFP),
            "transient" => self.flags.insert(JavaModifierFlags::TRANSIENT),
            "volatile" => self.flags.insert(JavaModifierFlags::VOLATILE),
            "default" => self.flags.insert(JavaModifierFlags::DEFAULT),
            _ => return false,
        }
        true
    }

    /// The recognized modifier names, in display order (stable across the
    /// snapshots rendered by `hir-def`'s pretty printer).
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        let visibility = match self.visibility {
            JavaVisibility::Public => Some("public"),
            JavaVisibility::Protected => Some("protected"),
            JavaVisibility::Private => Some("private"),
            JavaVisibility::Package => None,
        };
        [
            visibility,
            self.flags
                .contains(JavaModifierFlags::STATIC)
                .then_some("static"),
            self.modality
                .contains(JavaModality::FINAL)
                .then_some("final"),
            self.modality
                .contains(JavaModality::ABSTRACT)
                .then_some("abstract"),
            self.modality
                .contains(JavaModality::SEALED)
                .then_some("sealed"),
            self.modality
                .contains(JavaModality::NON_SEALED)
                .then_some("non-sealed"),
            self.flags
                .contains(JavaModifierFlags::STRICTFP)
                .then_some("strictfp"),
            self.flags
                .contains(JavaModifierFlags::DEFAULT)
                .then_some("default"),
            self.flags
                .contains(JavaModifierFlags::NATIVE)
                .then_some("native"),
            self.flags
                .contains(JavaModifierFlags::SYNCHRONIZED)
                .then_some("synchronized"),
            self.flags
                .contains(JavaModifierFlags::TRANSIENT)
                .then_some("transient"),
            self.flags
                .contains(JavaModifierFlags::VOLATILE)
                .then_some("volatile"),
        ]
        .into_iter()
        .flatten()
    }

    /// Lowers the source modifiers to the JVM access flags of the compiled
    /// declaration, applying the JLS defaults for the declaration `target`.
    /// `in_interface` says whether a *member* declaration (`field`, `method`,
    /// `annotation element`) appears in an interface or annotation type body,
    /// where the JLS applies the implicit `public`/`abstract`/`static`/`final`
    /// defaults.
    ///
    /// The JLS defaults:
    ///
    /// * interfaces and annotation types are implicitly `ACC_INTERFACE |
    ///   ACC_ABSTRACT` ([JLS §9.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.1.1),
    ///   [§9.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6));
    /// * interface fields are implicitly `ACC_PUBLIC | ACC_STATIC | ACC_FINAL`
    ///   ([JLS §9.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.3));
    /// * interface and annotation-element methods are implicitly
    ///   `ACC_PUBLIC | ACC_ABSTRACT` ([JLS §9.4],
    ///   [§9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1))
    ///   unless they are `static`, `default` or `private`;
    /// * enum constants are implicitly `ACC_PUBLIC | ACC_STATIC | ACC_FINAL |
    ///   ACC_ENUM` ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1));
    /// * enums are implicitly `ACC_ENUM | ACC_FINAL` unless they declare an
    ///   abstract member, which the caller (with the whole body in view) can
    ///   refine to `ACC_ABSTRACT` ([JLS §8.9]).
    ///
    /// `sealed`/`non-sealed` set no JVM access flag (the JVMS encodes them via
    /// the `PermittedSubclasses` attribute).
    pub fn to_jvm_access_flags(
        &self,
        target: ModifierTarget,
        in_interface: bool,
    ) -> JvmAccessFlags {
        let mut flags = JvmAccessFlags::empty();
        match self.visibility {
            JavaVisibility::Public => flags |= JvmAccessFlags::PUBLIC,
            JavaVisibility::Protected => flags |= JvmAccessFlags::PROTECTED,
            JavaVisibility::Private => flags |= JvmAccessFlags::PRIVATE,
            JavaVisibility::Package => {}
        }
        if self.flags.contains(JavaModifierFlags::STATIC) {
            flags |= JvmAccessFlags::STATIC;
        }
        if self.modality.contains(JavaModality::FINAL) {
            flags |= JvmAccessFlags::FINAL;
        }
        if self.modality.contains(JavaModality::ABSTRACT) {
            flags |= JvmAccessFlags::ABSTRACT;
        }
        if self.flags.contains(JavaModifierFlags::STRICTFP) {
            flags |= JvmAccessFlags::STRICT;
        }
        if self.flags.contains(JavaModifierFlags::NATIVE) {
            flags |= JvmAccessFlags::NATIVE;
        }
        if self.flags.contains(JavaModifierFlags::SYNCHRONIZED) {
            flags |= JvmAccessFlags::SYNCHRONIZED;
        }
        if self.flags.contains(JavaModifierFlags::TRANSIENT) {
            flags |= JvmAccessFlags::TRANSIENT;
        }
        if self.flags.contains(JavaModifierFlags::VOLATILE) {
            flags |= JvmAccessFlags::VOLATILE;
        }

        match target {
            ModifierTarget::Interface => {
                flags |= JvmAccessFlags::INTERFACE | JvmAccessFlags::ABSTRACT;
            }
            ModifierTarget::AnnotationType => {
                flags |= JvmAccessFlags::INTERFACE
                    | JvmAccessFlags::ABSTRACT
                    | JvmAccessFlags::ANNOTATION;
            }
            ModifierTarget::Enum => {
                flags |= JvmAccessFlags::ENUM;
                if !self.modality.contains(JavaModality::ABSTRACT) {
                    flags |= JvmAccessFlags::FINAL;
                }
            }
            ModifierTarget::EnumConstant => {
                flags |= JvmAccessFlags::PUBLIC
                    | JvmAccessFlags::STATIC
                    | JvmAccessFlags::FINAL
                    | JvmAccessFlags::ENUM;
            }
            ModifierTarget::Field if in_interface => {
                // Implicit public static final of an interface field.
                flags |= JvmAccessFlags::PUBLIC | JvmAccessFlags::STATIC | JvmAccessFlags::FINAL;
            }
            ModifierTarget::Method | ModifierTarget::AnnotationElement
                if in_interface
                    && !flags.contains(JvmAccessFlags::STATIC)
                    && !self.flags.contains(JavaModifierFlags::DEFAULT)
                    && self.visibility != JavaVisibility::Private =>
            {
                // Implicit public abstract of an interface / annotation-element
                // method.
                flags |= JvmAccessFlags::PUBLIC | JvmAccessFlags::ABSTRACT;
            }
            _ => {}
        }
        flags
    }

    /// The size of the compact representation in bytes (visibility tag + two
    /// flag bytes).
    pub const SIZE: usize = 3;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(mods: &[&str]) -> JavaModifiers {
        let mut out = JavaModifiers::none();
        for &m in mods {
            assert!(out.push(m), "unknown modifier {m}");
        }
        out
    }

    #[test]
    fn names_match_source_order() {
        let mods = parse(&[
            "public",
            "static",
            "final",
            "abstract",
            "sealed",
            "non-sealed",
            "strictfp",
            "default",
            "native",
            "synchronized",
            "transient",
            "volatile",
        ]);
        let names: Vec<_> = mods.names().collect();
        assert_eq!(
            names,
            vec![
                "public",
                "static",
                "final",
                "abstract",
                "sealed",
                "non-sealed",
                "strictfp",
                "default",
                "native",
                "synchronized",
                "transient",
                "volatile",
            ]
        );
    }

    #[test]
    fn compact_representation() {
        assert_eq!(JavaModifiers::SIZE, 3);
        assert_eq!(std::mem::size_of::<JavaModifiers>(), 3);
    }

    #[test]
    fn class_defaults() {
        assert!(
            !JavaModifiers::none()
                .to_jvm_access_flags(ModifierTarget::Class, false)
                .contains(JvmAccessFlags::PUBLIC)
        );
        let flags = parse(&["public"]).to_jvm_access_flags(ModifierTarget::Class, false);
        assert!(flags.contains(JvmAccessFlags::PUBLIC));
    }

    #[test]
    fn interface_defaults() {
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::Interface, false);
        assert!(flags.contains(JvmAccessFlags::INTERFACE | JvmAccessFlags::ABSTRACT));

        let flags = parse(&["public"]).to_jvm_access_flags(ModifierTarget::AnnotationType, false);
        assert!(flags.contains(JvmAccessFlags::INTERFACE | JvmAccessFlags::ANNOTATION));
    }

    #[test]
    fn interface_method_defaults() {
        let mods = parse(&[]);
        let flags = mods.to_jvm_access_flags(ModifierTarget::Method, true);
        assert!(flags.contains(JvmAccessFlags::PUBLIC | JvmAccessFlags::ABSTRACT));

        let static_mods = parse(&["static"]);
        let flags = static_mods.to_jvm_access_flags(ModifierTarget::Method, true);
        assert!(flags.contains(JvmAccessFlags::STATIC));
        assert!(!flags.contains(JvmAccessFlags::ABSTRACT));

        let default_mods = parse(&["default"]);
        let flags = default_mods.to_jvm_access_flags(ModifierTarget::Method, true);
        assert!(!flags.contains(JvmAccessFlags::ABSTRACT));

        // A class method is not abstract by default.
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::Method, false);
        assert!(!flags.contains(JvmAccessFlags::ABSTRACT));
    }

    #[test]
    fn interface_field_defaults() {
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::Field, true);
        assert!(
            flags.contains(JvmAccessFlags::PUBLIC | JvmAccessFlags::STATIC | JvmAccessFlags::FINAL)
        );
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::Field, false);
        assert!(flags.is_empty());
    }

    #[test]
    fn enum_defaults() {
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::Enum, false);
        assert!(flags.contains(JvmAccessFlags::ENUM | JvmAccessFlags::FINAL));

        let flags = parse(&["abstract"]).to_jvm_access_flags(ModifierTarget::Enum, false);
        assert!(flags.contains(JvmAccessFlags::ENUM | JvmAccessFlags::ABSTRACT));
        assert!(!flags.contains(JvmAccessFlags::FINAL));
    }

    #[test]
    fn enum_constant_defaults() {
        let flags = JavaModifiers::none().to_jvm_access_flags(ModifierTarget::EnumConstant, false);
        assert!(flags.contains(
            JvmAccessFlags::PUBLIC
                | JvmAccessFlags::STATIC
                | JvmAccessFlags::FINAL
                | JvmAccessFlags::ENUM
        ));
    }

    #[test]
    fn sealed_sets_no_jvm_flag() {
        let flags = parse(&["public", "sealed"]).to_jvm_access_flags(ModifierTarget::Class, false);
        assert!(flags.contains(JvmAccessFlags::PUBLIC));
        assert!(!flags.contains(JvmAccessFlags::ABSTRACT));
    }
}
