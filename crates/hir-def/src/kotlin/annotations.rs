//! The `kotlin.jvm` annotations the compiler reads when it compiles a
//! declaration to its *JVM* shape.
//!
//! `@JvmName`, `@JvmStatic`, `@JvmField` and `@JvmOverloads` are *library*
//! annotations: kotlin-stdlib declares them, and its jar carries
//! `kotlin/jvm/JvmName.class` and its siblings (verified against
//! kotlin-stdlib 2.2.0, the library this workspace's corpus resolves against).
//! A file reaches them through the `kotlin.jvm` default import
//! (<https://kotlinlang.org/docs/packages.html#default-imports>) exactly as it
//! reaches any other library declaration, so the compiler resolves an
//! application to the annotation before it reads it — and an application names
//! a *type* ([KLS
//! `annotations.html#annotation-declarations`](https://kotlinlang.org/spec/annotations.html#annotation-declarations)).
//!
//! Which annotation an application is must therefore be decided by the written
//! name's *resolution* in the file's scopes, never by comparing the last
//! segment the source wrote:
//!
//! * `@kotlin.jvm.JvmName("x")` is the library's annotation written qualified,
//!   and `@JN("x")` under `import kotlin.jvm.JvmName as JN` is the same
//!   annotation bound to another name — both resolve to
//!   [`JvmAnnotation::Name`];
//! * a `JvmName` that the file's own package, an import or an enclosing
//!   classifier declares *shadows* the library's, and `@JvmName("x")` is then
//!   that annotation — a plain annotation application, not a rename.
//!
//! [`JvmAnnotation`] is the *fully qualified* identity a resolved annotation
//! name is compared against, which is what keeps the two apart. A classpath
//! without kotlin-stdlib resolves none of them, and the compiler's own answer
//! for a file that cannot see the library is the one kotlinc gives: the
//! annotation is not the library's.

/// A `kotlin.jvm` annotation whose presence changes the classfile the compiler
/// emits for the declaration it is applied to
/// (<https://kotlinlang.org/docs/java-interop.html>).
///
/// The JVM view of a declaration reads the annotations it needs through the
/// resolved annotation name ([`JvmAnnotation::is`]), so a same-named annotation
/// of another package is never mistaken for one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JvmAnnotation {
    /// `kotlin.jvm.JvmName`: the JVM name of a function or property accessor —
    /// or of a file's facade class — rather than the Kotlin one
    /// (<https://kotlinlang.org/docs/java-interop.html#handling-signature-clashes-with-jvmname>).
    Name,
    /// `kotlin.jvm.JvmStatic`: a member of an `object` or a `companion object`
    /// is *also* a static of the class the object is reached through
    /// (<https://kotlinlang.org/docs/java-interop.html#static-methods>).
    Static,
    /// `kotlin.jvm.JvmField`: the property is a field of its own, with no
    /// compiler-generated accessors
    /// (<https://kotlinlang.org/docs/java-interop.html#instance-fields>).
    Field,
    /// `kotlin.jvm.JvmOverloads`: one further method per parameter that
    /// declares a default value
    /// (<https://kotlinlang.org/docs/java-interop.html#overloads-generation>).
    Overloads,
}

impl JvmAnnotation {
    /// The annotation's fully qualified name — the identity an application's
    /// *resolved* name is compared against.
    pub const fn fqn(self) -> &'static str {
        match self {
            JvmAnnotation::Name => "kotlin.jvm.JvmName",
            JvmAnnotation::Static => "kotlin.jvm.JvmStatic",
            JvmAnnotation::Field => "kotlin.jvm.JvmField",
            JvmAnnotation::Overloads => "kotlin.jvm.JvmOverloads",
        }
    }

    /// Whether `fqn` — the canonical name an application's written name
    /// resolved to — names this annotation.
    pub fn is(self, fqn: &str) -> bool {
        fqn == self.fqn()
    }
}
