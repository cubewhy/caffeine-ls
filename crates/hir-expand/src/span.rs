//! Source spans of lowered type references.
//!
//! The shared [`TypeRef`] stub ([`syntax::stub`]) also backs library class
//! files, which have no source positions, so it cannot carry ranges. On the
//! *source* side — where a type reference was lowered from a syntactic
//! `TYPE` node — we keep the [`TypeRef<Name>`] together with the source range
//! of every *reference name* it contains, in depth-first order. Resolution
//! keeps using the spanned value (it derefs to the plain [`TypeRef<Name>`]);
//! the spans let diagnostics ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5))
//! point at the exact occurrence of an unknown type.

use rowan::TextRange;
use syntax::stub::TypeRef;

use crate::body::Literal;
use crate::name::Name;

/// A reference type name occurring within a [`SpannedTypeRef`], with its
/// source range. `None` for constructs synthesized during lowering (e.g. a
/// missing name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRef {
    pub name: Name,
    pub range: Option<TextRange>,
}

impl NameRef {
    pub fn new(name: Name, range: TextRange) -> Self {
        Self {
            name,
            range: Some(range),
        }
    }
}

/// An annotation with its element-value arguments
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7),
/// [§9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)):
/// the (possibly qualified) annotation name with its source range, plus the
/// element-value pairs of the argument list in source order. The implicit
/// single-argument form (`@Foo(v)` → element `value`) is lowered like an
/// explicit `value = v` pair.
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationRef {
    pub name: NameRef,
    /// The element-value pairs of the argument list ([§9.7.1]), in source
    /// order. Empty for a marker annotation (`@Foo`).
    pub args: Vec<AnnotationArg>,
}

/// One element-value pair `name = value` of an annotation
/// ([JLS §9.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.1),
/// [§9.7.1]).
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationArg {
    /// The element name of the pair; `value` for the implicit single-argument
    /// form ([§9.7.1]).
    pub name: Name,
    pub value: AnnotationValue,
    /// The source range of the value expression.
    pub range: TextRange,
}

/// The value of an annotation element ([JLS §9.7.1]).
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotationValue {
    /// A constant literal ([JLS §15.28]) — a primitive, string or text-block
    /// literal.
    Literal(Literal),
    /// An enum constant ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)):
    /// `Type.CONSTANT` with its qualifier, or a bare `CONSTANT`, whose
    /// declaring type is inferred from the element's type ([§9.7.1]).
    EnumConstant {
        qualifier: Option<Name>,
        member: Name,
    },
    /// A class literal `Foo.class` ([§15.8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2)).
    ClassLit(SpannedTypeRef),
    /// A nested annotation ([§9.7.1]).
    Annotation(Box<AnnotationRef>),
    /// An array initializer `{ v1, v2 }` ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6)).
    Array(Vec<AnnotationValue>),
    /// An element value that is not a constant literal — a unary or binary
    /// expression, a conditional, a parenthesized expression. Kept as its raw
    /// source text.
    Unresolved { text: String },
}

/// A source type reference: the lowered [`TypeRef<Name>`] plus the source
/// ranges of the reference names it contains.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedTypeRef {
    pub ty: TypeRef<Name>,
    /// The reference names of `ty`, depth-first, each with its source range.
    pub refs: Vec<NameRef>,
    /// The type-use annotations of the type
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4),
    /// `int @Nullable []`, `List<@NonNull T>`), each with its parsed
    /// arguments. The annotation names also appear in
    /// [`SpannedTypeRef::refs`], so they resolve like any type name.
    pub type_use_annotations: Vec<AnnotationRef>,
}

impl SpannedTypeRef {
    pub fn new(ty: TypeRef<Name>, refs: Vec<NameRef>) -> Self {
        Self {
            ty,
            refs,
            type_use_annotations: Vec::new(),
        }
    }

    /// A type reference synthesized during lowering, with no source spans.
    pub fn synthetic(ty: TypeRef<Name>) -> Self {
        Self {
            ty,
            refs: Vec::new(),
            type_use_annotations: Vec::new(),
        }
    }

    /// The first (leftmost) reference name of the type, if any.
    pub fn first_ref(&self) -> Option<&NameRef> {
        self.refs.first()
    }
}

impl std::ops::Deref for SpannedTypeRef {
    type Target = TypeRef<Name>;

    fn deref(&self) -> &TypeRef<Name> {
        &self.ty
    }
}
