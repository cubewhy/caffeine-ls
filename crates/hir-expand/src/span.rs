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

/// A source type reference: the lowered [`TypeRef<Name>`] plus the source
/// ranges of the reference names it contains.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedTypeRef {
    pub ty: TypeRef<Name>,
    /// The reference names of `ty`, depth-first, each with its source range.
    pub refs: Vec<NameRef>,
}

impl SpannedTypeRef {
    pub fn new(ty: TypeRef<Name>, refs: Vec<NameRef>) -> Self {
        Self { ty, refs }
    }

    /// A type reference synthesized during lowering, with no source spans.
    pub fn synthetic(ty: TypeRef<Name>) -> Self {
        Self {
            ty,
            refs: Vec::new(),
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
