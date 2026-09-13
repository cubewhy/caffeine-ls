//! The Kotlin modifier model.
//!
//! Kotlin's declaration modifiers ([KLS
//! `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility)
//! and the per-declaration `modifiers` rules) are split into the same three
//! axes as Java's ([`crate::java::modifiers`]): a visibility tag, a modality
//! tag and a byte of general flags — compact, with no per-modifier booleans —
//! and lowered to the JVM [`JvmAccessFlags`] at the JVM boundary by
//! [`KotlinModifiers::to_jvm_access_flags`].
//!
//! Two differences from the Java model matter:
//!
//! * a Kotlin declaration is `public` unless it says otherwise
//!   ([`KotlinVisibility::Public`] is the default, where Java's default is the
//!   unnamed package visibility), and
//! * Kotlin's default modality is `final`
//!   ([KotlinModality::Final]), where Java's default is "neither `final` nor
//!   `abstract`".
//!
//! Annotations are *not* stored here: they are declaration attributes, not
//! modifiers, and live in a separate per-item field.

use bitflags::bitflags;

use crate::jvm::access::JvmAccessFlags;

/// The accessibility modifiers of a declaration.
///
/// Kotlin reuses Java's three access modifiers and adds `internal`, which is
/// visible throughout the *module*
/// ([KLS `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum KotlinVisibility {
    /// The implicit visibility of a declaration that names none
    /// ([KLS `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility):
    /// "by default, all the declarations are `public`").
    #[default]
    Public,
    Private,
    Protected,
    /// `internal`: visible inside the module. Not a JVM access level — the
    /// compiler emits it as `public` ([`KotlinModifiers::to_jvm_access_flags`]).
    Internal,
}

impl KotlinVisibility {
    /// The keyword that spells this visibility, if the source wrote one.
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            KotlinVisibility::Public => Some("public"),
            KotlinVisibility::Private => Some("private"),
            KotlinVisibility::Protected => Some("protected"),
            KotlinVisibility::Internal => Some("internal"),
        }
    }

    /// Whether the declaration is `public`.
    pub fn is_public(self) -> bool {
        self == KotlinVisibility::Public
    }
}

/// The inheritance modifiers of a declaration
/// ([KLS `inheritance.html#inheritance`](https://kotlinlang.org/spec/inheritance.html#inheritance)).
///
/// A single tag, because the four are mutually exclusive — and, unlike Java's
/// modality, the default is [`KotlinModality::Final`]: a Kotlin declaration is
/// closed unless it is `open`, `abstract` or `sealed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KotlinModality {
    #[default]
    Final,
    Open,
    Abstract,
    Sealed,
}

impl KotlinModality {
    /// The keyword that spells this modality, if the source wrote one.
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            KotlinModality::Final => Some("final"),
            KotlinModality::Open => Some("open"),
            KotlinModality::Abstract => Some("abstract"),
            KotlinModality::Sealed => Some("sealed"),
        }
    }
}

/// The variance of a type parameter
/// ([KLS `declarations.html#type-parameter-variance`](https://kotlinlang.org/spec/declarations.html#type-parameter-variance)):
/// `out` makes a type parameter covariant, `in` contravariant, and its absence
/// invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KotlinVariance {
    Out,
    In,
}

impl KotlinVariance {
    /// The keyword that spells this variance.
    pub fn keyword(self) -> &'static str {
        match self {
            KotlinVariance::Out => "out",
            KotlinVariance::In => "in",
        }
    }
}

bitflags! {
    /// The remaining modifiers of a declaration: the `classModifier`,
    /// `memberModifier`, `functionModifier` and `propertyModifier` sets of the
    /// Kotlin grammar other than visibility and modality.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct KotlinModifierFlags: u32 {
        /// `data class` / `data object`
        /// ([KLS `declarations.html#data-class-declaration`](https://kotlinlang.org/spec/declarations.html#data-class-declaration)).
        const DATA = 1 << 0;
        /// `value class`
        /// ([KLS `declarations.html#value-class-declaration`](https://kotlinlang.org/spec/declarations.html#value-class-declaration)).
        const VALUE = 1 << 1;
        const INLINE = 1 << 2;
        /// `suspend fun`
        /// ([KLS `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)).
        const SUSPEND = 1 << 3;
        /// `operator fun`
        /// ([KLS `operator-overloading.html#operator-overloading`](https://kotlinlang.org/spec/operator-overloading.html#operator-overloading)).
        const OPERATOR = 1 << 4;
        /// `infix fun`
        /// ([KLS `declarations.html#infix-functions`](https://kotlinlang.org/spec/declarations.html#infix-functions)).
        const INFIX = 1 << 5;
        /// `const val`
        /// ([KLS `declarations.html#constant-properties`](https://kotlinlang.org/spec/declarations.html#constant-properties)).
        const CONST = 1 << 6;
        /// `lateinit var`
        /// ([KLS `declarations.html#late-initialized-properties`](https://kotlinlang.org/spec/declarations.html#late-initialized-properties)).
        const LATEINIT = 1 << 7;
        /// `tailrec fun`
        /// ([KLS `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)).
        const TAILREC = 1 << 8;
        const EXTERNAL = 1 << 9;
        /// `expect`/`actual` multiplatform declarations
        /// ([KLS `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility)).
        const EXPECT = 1 << 10;
        const ACTUAL = 1 << 11;
        /// The `annotation` of `annotation class`
        /// ([KLS `declarations.html#annotation-class-declaration`](https://kotlinlang.org/spec/declarations.html#annotation-class-declaration)),
        /// which also makes the class kind [`crate::kotlin::item_tree::KotlinClassKind::Annotation`].
        const ANNOTATION = 1 << 12;
        /// The `enum` of `enum class`
        /// ([KLS `declarations.html#enum-class-declaration`](https://kotlinlang.org/spec/declarations.html#enum-class-declaration)),
        /// which also makes the class kind [`crate::kotlin::item_tree::KotlinClassKind::Enum`].
        const ENUM = 1 << 13;
        const INNER = 1 << 14;
        /// `override`
        /// ([KLS `inheritance.html#overriding`](https://kotlinlang.org/spec/inheritance.html#overriding)).
        const OVERRIDE = 1 << 15;
        /// The `vararg` of a parameter
        /// ([KLS `declarations.html#variable-length-parameters`](https://kotlinlang.org/spec/declarations.html#variable-length-parameters)).
        const VARARG = 1 << 16;
        const NOINLINE = 1 << 17;
        const CROSSINLINE = 1 << 18;
        /// `reified` on an inline function's type parameter
        /// ([KLS `declarations.html#reified-type-parameters`](https://kotlinlang.org/spec/declarations.html#reified-type-parameters)).
        const REIFIED = 1 << 19;
        /// The `fun` of `fun interface`
        /// ([KLS `declarations.html#interface-declaration`](https://kotlinlang.org/spec/declarations.html#interface-declaration)),
        /// which appears as a `FUN_KW` token rather than a modifier keyword.
        const FUN_INTERFACE = 1 << 20;
    }
}

impl Default for KotlinModifierFlags {
    fn default() -> Self {
        KotlinModifierFlags::empty()
    }
}

/// The source modifiers of a Kotlin declaration, split into the three axes of
/// the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KotlinModifiers {
    pub visibility: KotlinVisibility,
    pub modality: KotlinModality,
    pub flags: KotlinModifierFlags,
}

impl KotlinModifiers {
    /// The empty modifier set: the implicit `public final` of a declaration
    /// that names no modifier.
    pub fn none() -> KotlinModifiers {
        KotlinModifiers::default()
    }

    /// Records a modifier keyword. Returns `false` for a word that is not a
    /// declaration modifier (a soft keyword in another role, e.g. the `by` of
    /// a delegated property, or `companion`, which is a declaration keyword of
    /// its own).
    pub fn push_keyword(&mut self, keyword: &str) -> bool {
        match keyword {
            "public" => self.visibility = KotlinVisibility::Public,
            "protected" => self.visibility = KotlinVisibility::Protected,
            "private" => self.visibility = KotlinVisibility::Private,
            "internal" => self.visibility = KotlinVisibility::Internal,
            "final" => self.modality = KotlinModality::Final,
            "open" => self.modality = KotlinModality::Open,
            "abstract" => self.modality = KotlinModality::Abstract,
            "sealed" => self.modality = KotlinModality::Sealed,
            "data" => self.flags.insert(KotlinModifierFlags::DATA),
            "value" => self.flags.insert(KotlinModifierFlags::VALUE),
            "inline" => self.flags.insert(KotlinModifierFlags::INLINE),
            "suspend" => self.flags.insert(KotlinModifierFlags::SUSPEND),
            "operator" => self.flags.insert(KotlinModifierFlags::OPERATOR),
            "infix" => self.flags.insert(KotlinModifierFlags::INFIX),
            "const" => self.flags.insert(KotlinModifierFlags::CONST),
            "lateinit" => self.flags.insert(KotlinModifierFlags::LATEINIT),
            "tailrec" => self.flags.insert(KotlinModifierFlags::TAILREC),
            "external" => self.flags.insert(KotlinModifierFlags::EXTERNAL),
            "expect" => self.flags.insert(KotlinModifierFlags::EXPECT),
            "actual" => self.flags.insert(KotlinModifierFlags::ACTUAL),
            "annotation" => self.flags.insert(KotlinModifierFlags::ANNOTATION),
            "enum" => self.flags.insert(KotlinModifierFlags::ENUM),
            "inner" => self.flags.insert(KotlinModifierFlags::INNER),
            "override" => self.flags.insert(KotlinModifierFlags::OVERRIDE),
            "vararg" => self.flags.insert(KotlinModifierFlags::VARARG),
            "noinline" => self.flags.insert(KotlinModifierFlags::NOINLINE),
            "crossinline" => self.flags.insert(KotlinModifierFlags::CROSSINLINE),
            "reified" => self.flags.insert(KotlinModifierFlags::REIFIED),
            _ => return false,
        }
        true
    }

    /// The recognized modifier names, in display order (stable across the
    /// snapshots rendered by `hir-def`'s pretty printer).
    ///
    /// Only the modifiers the declaration *departs* from the defaults with are
    /// listed: the implicit [`KotlinVisibility::Public`] and
    /// [`KotlinModality::Final`] of a declaration that names neither are
    /// omitted rather than spelled out, so a snapshot shows at a glance what
    /// the source wrote (an explicitly written `public`/`final` is
    /// indistinguishable from the default here, and renders the same either
    /// way).
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        let flag = |flag: KotlinModifierFlags, name: &'static str| {
            self.flags.contains(flag).then_some(name)
        };
        [
            (self.visibility != KotlinVisibility::Public)
                .then(|| self.visibility.keyword())
                .flatten(),
            (self.modality != KotlinModality::Final)
                .then(|| self.modality.keyword())
                .flatten(),
            flag(KotlinModifierFlags::DATA, "data"),
            flag(KotlinModifierFlags::VALUE, "value"),
            flag(KotlinModifierFlags::ENUM, "enum"),
            flag(KotlinModifierFlags::ANNOTATION, "annotation"),
            flag(KotlinModifierFlags::INLINE, "inline"),
            flag(KotlinModifierFlags::SUSPEND, "suspend"),
            flag(KotlinModifierFlags::OPERATOR, "operator"),
            flag(KotlinModifierFlags::INFIX, "infix"),
            flag(KotlinModifierFlags::CONST, "const"),
            flag(KotlinModifierFlags::LATEINIT, "lateinit"),
            flag(KotlinModifierFlags::TAILREC, "tailrec"),
            flag(KotlinModifierFlags::EXTERNAL, "external"),
            flag(KotlinModifierFlags::EXPECT, "expect"),
            flag(KotlinModifierFlags::ACTUAL, "actual"),
            flag(KotlinModifierFlags::INNER, "inner"),
            flag(KotlinModifierFlags::OVERRIDE, "override"),
            flag(KotlinModifierFlags::VARARG, "vararg"),
            flag(KotlinModifierFlags::NOINLINE, "noinline"),
            flag(KotlinModifierFlags::CROSSINLINE, "crossinline"),
            flag(KotlinModifierFlags::REIFIED, "reified"),
            flag(KotlinModifierFlags::FUN_INTERFACE, "fun"),
        ]
        .into_iter()
        .flatten()
    }

    /// Lowers the source modifiers to the JVM access flags of the compiled
    /// declaration.
    ///
    /// The JVM has no `internal`, and kotlinc emits no access flag for it: an
    /// `internal class` compiles to a `public final class` and an
    /// `internal fun` to a `public` method whose *name* is mangled
    /// (`g$<moduleName>`, observed with kotlinc 2.4.20), so `internal` maps to
    /// `ACC_PUBLIC` here. The name mangling is a JVM-tier concern the item tree
    /// does not model: the item keeps its source name.
    ///
    /// The `sealed` modifier sets no access flag; a sealed class is `abstract`
    /// in bytecode, which this returns as no `ACC_FINAL` (the `abstract` flag
    /// itself is derived from the members, exactly as for Java's
    /// [`crate::java::modifiers::JavaModifierFlags`]).
    pub fn to_jvm_access_flags(&self) -> JvmAccessFlags {
        let mut flags = JvmAccessFlags::empty();
        match self.visibility {
            KotlinVisibility::Public | KotlinVisibility::Internal => {
                flags |= JvmAccessFlags::PUBLIC;
            }
            KotlinVisibility::Protected => flags |= JvmAccessFlags::PROTECTED,
            KotlinVisibility::Private => flags |= JvmAccessFlags::PRIVATE,
        }
        if self.modality == KotlinModality::Final {
            flags |= JvmAccessFlags::FINAL;
        }
        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_visibility_and_modality() {
        let modifiers = KotlinModifiers::none();
        assert!(modifiers.visibility.is_public());
        assert_eq!(modifiers.modality, KotlinModality::Final);
        // The implicit `public final` of a declaration naming no modifier is
        // not spelled out.
        assert_eq!(modifiers.names().collect::<Vec<_>>(), Vec::<&str>::new());
    }

    #[test]
    fn modifiers_lower_to_jvm_access_flags() {
        let mut modifiers = KotlinModifiers::none();
        assert!(modifiers.push_keyword("private"));
        assert_eq!(
            modifiers.to_jvm_access_flags(),
            JvmAccessFlags::PRIVATE | JvmAccessFlags::FINAL
        );

        // `internal` is not a JVM access level: kotlinc emits `public`.
        let mut modifiers = KotlinModifiers::none();
        assert!(modifiers.push_keyword("internal"));
        assert_eq!(
            modifiers.to_jvm_access_flags(),
            JvmAccessFlags::PUBLIC | JvmAccessFlags::FINAL
        );

        // `open` is not `final`; both are `public`.
        let mut modifiers = KotlinModifiers::none();
        assert!(modifiers.push_keyword("open"));
        assert_eq!(modifiers.to_jvm_access_flags(), JvmAccessFlags::PUBLIC);

        let mut modifiers = KotlinModifiers::none();
        assert!(modifiers.push_keyword("protected"));
        assert!(modifiers.push_keyword("data"));
        assert_eq!(
            modifiers.to_jvm_access_flags(),
            JvmAccessFlags::PROTECTED | JvmAccessFlags::FINAL
        );
        assert_eq!(
            modifiers.names().collect::<Vec<_>>(),
            vec!["protected", "data"]
        );
    }

    #[test]
    fn non_modifier_keywords_are_rejected() {
        let mut modifiers = KotlinModifiers::none();
        for keyword in ["by", "companion", "init", "constructor", "get", "set"] {
            assert!(!modifiers.push_keyword(keyword), "{keyword}");
        }
        assert_eq!(modifiers, KotlinModifiers::none());
    }
}
