//! The declaration-side IR every JVM language lowers into (IntelliJ: the
//! declaration model of `JvmElement`).
//!
//! A type reference with the names it mentions, a formal parameter and an
//! annotation application are what the Java and Kotlin declaration models have
//! in common: `syntax::stub`- and `Name`-based shapes carrying no language
//! concept, which each language's lowering constructs from its own CST —
//! Kotlin's directly, Java's through the source-spanned conversions that live
//! with the Java walker ([`ItemTypeRef::from_spanned`]).
//! Keeping them here is what lets a language-agnostic consumer read a
//! declaration of any language without knowing which language lowered it.

use hir_expand::{ast_id_map::FileAstId, name::Name};
use syntax::stub::TypeRef;

/// The syntax-node id of a type reference ([`ItemTypeRef::node`]).
pub struct TypeNode;
/// The syntax-node id of an annotation application ([`ItemAnnotationRef::node`]).
pub struct AnnotationNode;

/// A declaration-side source type reference: the lowered [`TypeRef<Name>`]
/// plus the reference names it contains (depth-first, in the same order the
/// source-spanned form keeps) and the syntax node of the type itself, from
/// which the per-name source ranges are re-derived on demand
/// ([`crate::java::ranges::type_ref_occurrences`] for a Java file,
/// [`crate::kotlin::ranges`] for a Kotlin one). The item tree stores no
/// source offsets; `node` is a [`FileAstId`] into the file's
/// [`AstIdMap`](hir_expand::ast_id_map::AstIdMap).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTypeRef {
    pub ty: TypeRef<Name>,
    /// The reference names of `ty`, depth-first, names only. The source
    /// ranges are computed from `node` by [`crate::java::ranges`].
    pub refs: Vec<Name>,
    /// The type-use annotations of the type
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
    /// `int @Nullable []`, `List<@NonNull T>`), in the same (flattened,
    /// depth-first) order the ranged [`hir_expand::span::SpannedTypeRef`] keeps. The annotation
    /// names also appear in [`ItemTypeRef::refs`], so they resolve like any
    /// type name.
    pub type_use_annotations: Vec<ItemAnnotationRef>,
    /// The `TYPE` syntax node of the type (or the `QUALIFIED_NAME` node for a
    /// module directive's service / implementation reference).
    pub node: FileAstId<TypeNode>,
}

impl std::ops::Deref for ItemTypeRef {
    type Target = TypeRef<Name>;

    fn deref(&self) -> &TypeRef<Name> {
        &self.ty
    }
}

impl ItemTypeRef {
    /// A type reference synthesized during lowering (a missing or error
    /// type), naming no syntax node. Its occurrence list is empty, and range
    /// resolution of its placeholder id never happens.
    pub fn synthetic(ty: TypeRef<Name>) -> Self {
        Self {
            ty,
            refs: Vec::new(),
            type_use_annotations: Vec::new(),
            node: FileAstId::placeholder(),
        }
    }
}

/// A declaration-side annotation with its element-value arguments
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
/// [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)) —
/// the range-free twin of [`hir_expand::span::AnnotationRef`]. The (possibly qualified)
/// annotation name is [`ItemAnnotationRef::name`] and the annotation's syntax
/// node is [`ItemAnnotationRef::node`], from which the name's source range is
/// re-derived on demand.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemAnnotationRef {
    pub name: Name,
    /// The element-value pairs of the argument list ([§9.7.1]), in source
    /// order. Empty for a marker annotation (`@Foo`).
    pub args: Vec<ItemAnnotationArg>,
    /// The `ANNOTATION`/`MARKER_ANNOTATION` syntax node of the annotation.
    pub node: FileAstId<AnnotationNode>,
}

/// One element-value pair `name = value` of an annotation
/// ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1),
/// [§9.7.1]) — the range-free twin of [`hir_expand::span::AnnotationArg`].
/// The value's source range is re-derived on demand from the owning
/// annotation's syntax node ([`crate::java::ranges::annotation_arg_value_range`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemAnnotationArg {
    /// The element name of the pair; `value` for the implicit single-argument
    /// form ([§9.7.1]).
    pub name: Name,
    pub value: ItemAnnotationValue,
}

/// The value of an annotation element ([JLS §9.7.1]) — the range-free twin of
/// [`hir_expand::span::AnnotationValue`].
#[derive(Debug, Clone, PartialEq)]
pub enum ItemAnnotationValue {
    /// A constant literal ([JLS §15.28]) — a primitive, string or text-block
    /// literal.
    Literal(hir_expand::body::Literal),
    /// An enum constant ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)):
    /// `Type.CONSTANT` with its qualifier, or a bare `CONSTANT`, whose
    /// declaring type is inferred from the element's type ([§9.7.1]).
    EnumConstant {
        qualifier: Option<Name>,
        member: Name,
    },
    /// A class literal `Foo.class` ([§15.8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2)).
    ClassLit(Box<ItemTypeRef>),
    /// A nested annotation ([§9.7.1]).
    Annotation(Box<ItemAnnotationRef>),
    /// An array initializer `{ v1, v2 }` ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6)).
    Array(Vec<ItemAnnotationValue>),
    /// An element value that is not one of the literal forms above — a unary,
    /// binary, conditional, parenthesized, cast or `null` expression — as an
    /// expression of the file's body tree ([JLS §9.7.1]: the value is a
    /// `ConditionalExpression`).
    Expr(hir_expand::body::ExprId),
    /// An element value whose expression arena is unavailable, kept as its raw
    /// source text — the annotation of a *written type* ([§9.7.4]), which has
    /// no owning declaration to lower the value against.
    Unresolved { text: String },
}

/// A formal parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Name,
    pub ty: ItemTypeRef,
    pub varargs: bool,
    /// The annotation modifiers of the parameter declaration
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
    /// `void m(@A int p)`, `void m(@A String... p)`), in source order. The
    /// type annotations of `ty` are separate: they annotate the *type*
    /// ([§9.7.4]).
    pub annotations: Vec<ItemAnnotationRef>,
}
