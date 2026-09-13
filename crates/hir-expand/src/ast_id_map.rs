//! A per-file map from syntax nodes to stable ids — the `AstIdMap`, after
//! rust-analyzer's `hir-expand/src/ast_id_map.rs`.
//!
//! Lowering stores a [`FileAstId`] in the item tree instead of a source
//! [`rowan::TextRange`], and resolves ranges on demand against the current
//! syntax tree ([`AstIdMap::try_to_node`]). The map indexes only the
//! *declaration skeleton* of the file: nodes inside method bodies, field and
//! enum-constant initializers, anonymous class bodies and annotation-element
//! defaults are pruned, so a text edit that touches only body content leaves
//! both the indexed-node sequence and every [`FileAstId`] untouched — with one
//! exception, the local class-like declarations of a block ([JLS §14.3]),
//! which *are* declarations and are indexed with their own skeleton. Since
//! the item tree must carry no source offsets, that stability is what lets
//! salsa backdate the signature queries across body-only edits.

use std::marker::PhantomData;

use rustc_hash::FxHashMap;
use syntax::SourceFile;
use syntax::java::SyntaxKind as J;

use crate::arena::{Arena, ArenaId};

/// A pointer to a syntax node: its range and kind. Two distinct nodes never
/// share a pointer (ranges are disjoint), so a pointer can serve as a
/// `HashMap` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxNodePtr {
    pub range: rowan::TextRange,
    pub kind: rowan::SyntaxKind,
}

/// The pointer to `node`.
pub fn node_ptr<L>(node: &rowan::SyntaxNode<L>) -> SyntaxNodePtr
where
    L: rowan::Language,
    L::Kind: Into<rowan::SyntaxKind>,
{
    SyntaxNodePtr {
        range: node.text_range(),
        kind: node.kind().into(),
    }
}

/// A stable id of a syntax node within its owning file's [`AstIdMap`].
///
/// The marker type `N` gives the id a type-safe role (e.g. the AST node of a
/// specific kind of item) without constraining the language: kinds are `u16`
/// and `FileAstId` itself is language-generic. The trait impls are written by
/// hand (and the marker type's parameters are erased behind `fn() -> N`) so
/// the marker type `N` needs no trait impls of its own.
pub struct FileAstId<N> {
    id: ArenaId,
    _marker: PhantomData<fn() -> N>,
}

impl<N> Clone for FileAstId<N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for FileAstId<N> {}

impl<N> PartialEq for FileAstId<N> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<N> Eq for FileAstId<N> {}

impl<N> std::hash::Hash for FileAstId<N> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<N> PartialOrd for FileAstId<N> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<N> Ord for FileAstId<N> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl<N> std::fmt::Debug for FileAstId<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileAstId").field(&self.id).finish()
    }
}

impl<N> FileAstId<N> {
    pub(crate) fn from_raw(id: ArenaId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn into_raw(self) -> ArenaId {
        self.id
    }

    /// A sentinel id for references synthesized during lowering that name no
    /// syntax node (a missing or error type). Resolving such an id must not
    /// happen: every range helper short-circuits on the empty occurrence set
    /// such references carry.
    pub fn placeholder() -> Self {
        Self {
            id: ArenaId(u32::MAX),
            _marker: PhantomData,
        }
    }
}

/// The per-file map from [`SyntaxNodePtr`]s to [`FileAstId`]s.
///
/// Built by a DFS pre-order over the declaration skeleton of the file (see
/// [`AstIdMap::from_source_file`]), so the arena ids are deterministic and —
/// thanks to the body pruning — a function of the *declaration* structure
/// only. `O(N)` to build and `O(1)` to look up: lowering stays linear in the
/// file size.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AstIdMap {
    arena: Arena<SyntaxNodePtr>,
    index: FxHashMap<SyntaxNodePtr, ArenaId>,
}

impl AstIdMap {
    /// Builds the map for `source`, indexing every node whose kind is in the
    /// indexable set (below), in DFS pre-order, and pruning the subtrees that
    /// are *body* content:
    ///
    /// - `BLOCK` — method/constructor/initializer bodies. The body itself is
    ///   pruned, but the local class-like declarations of the block
    ///   ([§14.3]) are declaration skeleton: each is indexed with its own
    ///   modifiers, type parameters, supertypes and members, and each of
    ///   *their* blocks applies this same rule. Everything else in the body
    ///   (statements, local variables, expressions) stays pruned;
    /// - `CLASS_BODY` not directly under a `CLASS_DECL` — anonymous and
    ///   enum-constant class bodies (the members of a *named* class are
    ///   declaration skeleton and are descended into);
    /// - `VARIABLE_DECLARATOR` — the declarator itself is indexed, its
    ///   initializer expression is body-side and re-walked at resolution
    ///   time;
    /// - `ENUM_CONSTANT` — the constant itself is indexed, its arguments and
    ///   constant class body are body-side;
    /// - `ANNOTATION_TYPE_ELEMENT_DECL` — indexed, and only its
    ///   `MODIFIER_LIST` and `TYPE` children are descended into; the default
    ///   value expression is body-side (`default_expr`).
    ///
    /// The indexed-node sequence therefore changes exactly when the declared
    /// structure of the file changes, which is the load-bearing invariant
    /// behind salsa backdating of the item tree.
    pub fn from_source_file(source: &SourceFile) -> Self {
        let mut map = AstIdMap::default();
        match source {
            SourceFile::Java(file) => {
                let mut stack = vec![(file.syntax_node.clone(), None)];
                while let Some((node, parent)) = stack.pop() {
                    let kind = node.kind();
                    if is_indexable(kind) {
                        let ptr = node_ptr(&node);
                        debug_assert!(
                            !map.index.contains_key(&ptr),
                            "two distinct syntax nodes share a node pointer"
                        );
                        map.index.insert(ptr, map.arena.alloc(ptr));
                    }
                    let mut push = |children: Vec<_>| {
                        for child in children.into_iter().rev() {
                            stack.push((child, Some(kind)));
                        }
                    };
                    match kind {
                        // The class-like declarations of a block are the
                        // *local* declarations ([JLS §14.3]), and they are
                        // declaration skeleton: index their outermost ones
                        // and descend into each via the default arm below. A
                        // deeper one is already inside the skeleton of an
                        // outer declaration, and a deeper *block* applies
                        // this same rule when it is reached. Everything else
                        // in a body stays pruned.
                        J::BLOCK => push(
                            node.children()
                                .filter(|child| is_class_like_decl(child.kind()))
                                .collect(),
                        ),
                        // The members of a named class are declarations; the
                        // class body of an anonymous class or an enum
                        // constant is body content.
                        J::CLASS_BODY if parent != Some(J::CLASS_DECL) => {}
                        // The declarator itself is indexed; its name, dims
                        // and initializer are re-walked at resolution time.
                        J::VARIABLE_DECLARATOR | J::ENUM_CONSTANT => {}
                        // The default value expression is body-side; the
                        // element's modifiers and type are declarations.
                        J::ANNOTATION_TYPE_ELEMENT_DECL => push(
                            node.children()
                                .filter(|child| matches!(child.kind(), J::MODIFIER_LIST | J::TYPE))
                                .collect(),
                        ),
                        _ => push(node.children().collect()),
                    }
                }
            }
            SourceFile::Kotlin(_) => {
                // Kotlin is not lowered yet (empty item tree), so nothing is
                // indexed.
            }
        }
        map
    }

    /// The id of the node `ptr` points to, when the node was indexed.
    pub fn ast_id<N>(&self, ptr: &SyntaxNodePtr) -> Option<FileAstId<N>> {
        self.index.get(ptr).map(|id| FileAstId::from_raw(*id))
    }

    /// The node pointer stored at `id`.
    ///
    /// # Panics
    /// If `id` is a [`FileAstId::placeholder`] (synthesized references must
    /// never be resolved).
    pub fn get<N>(&self, id: FileAstId<N>) -> &SyntaxNodePtr {
        self.arena.get(id.into_raw())
    }

    /// The node pointer stored at `id`, or `None` for a placeholder or
    /// out-of-range id (a synthesized reference).
    pub fn try_get<N>(&self, id: FileAstId<N>) -> Option<&SyntaxNodePtr> {
        let raw = id.into_raw();
        (self.arena.len() > raw.0 as usize).then(|| self.arena.get(raw))
    }

    /// Resolves `ptr` back to the live syntax node it points to: the node
    /// covering `ptr.range` (starting from the covering element's node, or
    /// its parent when the covering element is a single-token-width node),
    /// walking up to the kind match. `None` when no ancestor has the pointer's
    /// kind.
    pub fn try_to_node<L>(
        &self,
        ptr: &SyntaxNodePtr,
        root: &rowan::SyntaxNode<L>,
    ) -> Option<rowan::SyntaxNode<L>>
    where
        L: rowan::Language,
        L::Kind: Into<rowan::SyntaxKind> + PartialEq,
    {
        let element = root.covering_element(ptr.range);
        let start = match element {
            rowan::SyntaxElement::Node(node) => node,
            // A single-token-width node (e.g. a narrow `TYPE`): the covering
            // element is the token it wraps.
            rowan::SyntaxElement::Token(token) => token.parent()?,
        };
        let node = start.ancestors().find(|n| n.kind().into() == ptr.kind)?;
        debug_assert_eq!(node.text_range(), ptr.range);
        Some(node)
    }
}

/// A class-like declaration: the four kinds a block may declare
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// and a class body may hold as a member.
fn is_class_like_decl(kind: J) -> bool {
    matches!(
        kind,
        J::CLASS_DECL | J::INTERFACE_DECL | J::ENUM_DECL | J::RECORD_DECL
    )
}

/// The kinds that may be indexed: every declaration node an item or a
/// resolved range can target, plus the narrower nodes ranges are derived from
/// (`TYPE`, `QUALIFIED_NAME`, `ANNOTATION`/`MARKER_ANNOTATION`,
/// `FORMAL_PARAMETER`/`SPREAD_PARAMETER` for record components and the module
/// directives).
fn is_indexable(kind: J) -> bool {
    matches!(
        kind,
        J::PACKAGE_DECL
            | J::IMPORT_DECL
            | J::CLASS_DECL
            | J::INTERFACE_DECL
            | J::ENUM_DECL
            | J::RECORD_DECL
            | J::ANNOTATION_TYPE_DECL
            | J::MODULE_DECL
            | J::METHOD_DECL
            | J::CONSTRUCTOR_DECL
            | J::COMPACT_CONSTRUCTOR_DECL
            | J::ANNOTATION_TYPE_ELEMENT_DECL
            | J::FIELD_DECL
            | J::VARIABLE_DECLARATOR
            | J::ENUM_CONSTANT
            | J::STATIC_INITIALIZER
            | J::INSTANCE_INITIALIZER
            | J::FORMAL_PARAMETER
            | J::SPREAD_PARAMETER
            | J::TYPE
            | J::ANNOTATION
            | J::MARKER_ANNOTATION
            | J::QUALIFIED_NAME
            | J::REQUIRES_DIRECTIVE
            | J::EXPORTS_DIRECTIVE
            | J::OPENS_DIRECTIVE
            | J::USES_DIRECTIVE
            | J::PROVIDES_DIRECTIVE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::LanguageKind;

    fn sample() -> (SourceFile, AstIdMap) {
        let text = "\
package com.example;

class Foo {
    int f = 1;

    void m() {
        int local = 1;
    }
}
";
        let parse = syntax::SourceFile::parse(LanguageKind::Java, text);
        let file = parse.syntax_node(LanguageKind::Java);
        let map = AstIdMap::from_source_file(&file);
        (file, map)
    }

    #[test]
    fn round_trip() {
        let (file, map) = sample();
        let SourceFile::Java(file) = &file else {
            panic!("expected a Java source file");
        };
        let root = &file.syntax_node;

        // Resolve every indexed pointer and check the id round-trips.
        for (_, ptr) in map.arena.iter() {
            let node = map
                .try_to_node(ptr, root)
                .unwrap_or_else(|| panic!("unresolvable pointer {ptr:?}"));
            assert_eq!(node.text_range(), ptr.range);
            let id: Option<FileAstId<()>> = map.ast_id(&node_ptr(&node));
            assert!(id.is_some(), "indexed pointer missing from the map");
            assert_eq!(map.get(id.unwrap()).range, ptr.range);
        }
    }

    #[test]
    fn body_edits_do_not_change_the_map() {
        let text = "\
class Foo {
    void m() {
        int local = 1;
    }
}
";
        let unrelated_body = "\
class Foo {
    void m() {
        int local = 2;
        int extra = 7;
    }
}
";
        // The map *values* carry ranges, so body edits shift them; what must
        // be stable is the sequence of (kind, id) pairs — the FileAstIds the
        // item tree stores. That is the invariant salsa backdating relies on.
        fn id_sequence(text: &str) -> Vec<(u16, u32)> {
            let parse = syntax::SourceFile::parse(LanguageKind::Java, text);
            let map = AstIdMap::from_source_file(&parse.syntax_node(LanguageKind::Java));
            map.arena
                .iter()
                .map(|(id, ptr)| (ptr.kind.0, id.0))
                .collect()
        }
        assert_eq!(id_sequence(text), id_sequence(unrelated_body));
    }

    /// A local class declaration ([§14.3]) is declaration skeleton: the
    /// declaration, its modifiers, name, supertypes, members and *their*
    /// bodies' local declarations are all anchored.
    #[test]
    fn local_declarations_are_indexed() {
        let text = "\
class Foo {
    void m() {
        int local = 1;
        class Local extends Base {
            int f;
            void n() {
                interface Nested {}
            }
        }
    }
}
";
        let parse = syntax::SourceFile::parse(LanguageKind::Java, text);
        let SourceFile::Java(file) = &parse.syntax_node(LanguageKind::Java) else {
            panic!("expected a Java source file");
        };
        let root = &file.syntax_node;
        let map = AstIdMap::from_source_file(&parse.syntax_node(LanguageKind::Java));

        let indexed: Vec<_> = map
            .arena
            .iter()
            .map(|(_, ptr)| {
                let node = map.try_to_node(ptr, root).expect("resolvable pointer");
                (node.kind(), ptr.range)
            })
            .collect();
        let text_of = |range: rowan::TextRange| &text[range.start().into()..range.end().into()];

        // The file's skeleton: the class, its method, the method's local
        // class with its members, and the local class's own nested local
        // interface.
        let kinds: Vec<_> = indexed.iter().map(|(kind, _)| *kind).collect();
        assert!(kinds.contains(&J::CLASS_DECL), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|k| **k == J::CLASS_DECL).count(),
            2,
            "{kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == J::INTERFACE_DECL).count(),
            1,
            "{kinds:?}"
        );
        assert!(kinds.contains(&J::FIELD_DECL), "{kinds:?}");

        // The declarations are anchored at their own ranges, not at the
        // block's.
        let declared: Vec<_> = indexed
            .iter()
            .filter(|(kind, _)| *kind == J::CLASS_DECL || *kind == J::INTERFACE_DECL)
            .map(|(_, range)| text_of(*range))
            .collect();
        assert_eq!(
            declared,
            vec![
                "class Foo {\n    void m() {\n        int local = 1;\n        class Local extends Base {\n            int f;\n            void n() {\n                interface Nested {}\n            }\n        }\n    }\n}",
                "class Local extends Base {\n            int f;\n            void n() {\n                interface Nested {}\n            }\n        }",
                "interface Nested {}",
            ]
        );

        // A body edit that leaves the declarations alone leaves the ids
        // untouched; declaring one more local class is a structural edit.
        fn id_sequence(text: &str) -> Vec<(u16, u32)> {
            let parse = syntax::SourceFile::parse(LanguageKind::Java, text);
            let map = AstIdMap::from_source_file(&parse.syntax_node(LanguageKind::Java));
            map.arena
                .iter()
                .map(|(id, ptr)| (ptr.kind.0, id.0))
                .collect()
        }
        let body_edit = text.replace("int local = 1;", "int local = 2;");
        assert_eq!(id_sequence(text), id_sequence(&body_edit));
        let added = text.replace("int local = 1;", "int local = 1;\n        class Other {}");
        assert_ne!(id_sequence(text), id_sequence(&added));
    }
}
