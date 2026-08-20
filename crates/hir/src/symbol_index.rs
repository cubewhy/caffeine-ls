//! Source symbol index.
//!
//! The classpath-scoped source analogue of [`crate::index::NameIndex`]: a
//! salsa-memoized, per-source-set table mapping declaration names to the
//! `(file, item)` they live at. Unlike the library index (which is built once
//! from an immutable archive), the source index is recomputed incrementally —
//! a text edit invalidates exactly the changed file's per-file symbol query,
//! and the per-source-set aggregate re-collects the (unchanged) per-root
//! results.
//!
//! A [`SourceSymbol`] carries the canonical fully qualified name
//! ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
//! of the declaration — the package and enclosing types joined by `.` — its
//! [`ItemId`], its source range and its kind. Types and members are both
//! indexed: class-like declarations
//! ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6),
//! [§8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1),
//! [§8.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9),
//! [§8.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10),
//! [§9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.1),
//! [§9.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6))
//! get their fully qualified name; members
//! ([§8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.2),
//! [§8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3),
//! [§8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4))
//! get `EnclosingFqn.simple` so overloads share one key (the value is a
//! vector). The unnamed package ([§7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
//! yields a bare simple name.
//!
//! Each source set's index is scoped to its *own* roots; name resolution
//! walks the project classpath ([§7.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7.4))
//! in order (see [`crate::db::fqn_resolve`]) and consults one index per
//! internal `ClasspathEntry::SourceSet` entry, so a source set only sees the
//! internal source sets on its classpath.

use rowan::TextRange;
use rustc_hash::FxHashMap;
use vfs::FileId;

use hir_expand::{
    item_tree::{ItemData, ItemId},
    name::Name,
};

/// The kind of a source symbol, mapped from the lowered [`ItemData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceSymbolKind {
    /// A class declaration ([JLS §8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1)).
    Class,
    /// An interface declaration ([JLS §9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.1)).
    Interface,
    /// An enum declaration ([JLS §8.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9)).
    Enum,
    /// A record declaration ([JLS §8.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10)).
    Record,
    /// An annotation type declaration ([JLS §9.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6)).
    Annotation,
    /// A JPMS module declaration ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)).
    Module,
    /// A method or constructor ([JLS §8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4)).
    Method,
    /// A field ([JLS §8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3)).
    Field,
    /// An enum constant ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)).
    EnumConstant,
}

impl SourceSymbolKind {
    /// The symbol kind of a lowered item, or `None` for the nameless
    /// declarations (instance/static initializers) that the index skips.
    pub fn of(data: &ItemData) -> Option<SourceSymbolKind> {
        match data {
            ItemData::Class(_) => Some(SourceSymbolKind::Class),
            ItemData::Interface(_) => Some(SourceSymbolKind::Interface),
            ItemData::Enum(_) => Some(SourceSymbolKind::Enum),
            ItemData::Record(_) => Some(SourceSymbolKind::Record),
            ItemData::Annotation(_) => Some(SourceSymbolKind::Annotation),
            ItemData::Module(_) => Some(SourceSymbolKind::Module),
            ItemData::Method(_) => Some(SourceSymbolKind::Method),
            ItemData::Field(_) => Some(SourceSymbolKind::Field),
            ItemData::EnumConstant(_) => Some(SourceSymbolKind::EnumConstant),
            ItemData::StaticInit(_) | ItemData::InstanceInit(_) => None,
        }
    }

    /// The display label of the kind.
    pub fn label(&self) -> &'static str {
        match self {
            SourceSymbolKind::Class => "class",
            SourceSymbolKind::Interface => "interface",
            SourceSymbolKind::Enum => "enum",
            SourceSymbolKind::Record => "record",
            SourceSymbolKind::Annotation => "@interface",
            SourceSymbolKind::Module => "module",
            SourceSymbolKind::Method => "method",
            SourceSymbolKind::Field => "field",
            SourceSymbolKind::EnumConstant => "constant",
        }
    }
}

/// A single indexed source declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSymbol {
    /// The canonical qualified name: the package and enclosing types joined
    /// by `.` for types ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)),
    /// `EnclosingFqn.simple` for members. The unnamed package
    /// ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
    /// yields a bare simple name.
    pub name: Name,
    /// The lowered item this symbol refers to.
    pub item: ItemId,
    /// The source range of the declaration.
    pub range: TextRange,
    pub kind: SourceSymbolKind,
    /// Whether the declaration is `public` ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    pub public: bool,
}

/// A symbol located in a specific file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSymbolRef {
    pub file: FileId,
    pub symbol: SourceSymbol,
}

/// The source symbol index of one source set: declaration name → every
/// declaration carrying that name. Build once per source set per revision by
/// [`crate::db::source_set_symbol_index_query`]; lookups are O(1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSymbolIndex {
    by_fqn: FxHashMap<Name, Vec<SourceSymbolRef>>,
    /// Lowercased simple name → symbols. The simple name is the last `.`-
    /// separated segment of [`SourceSymbol::name`].
    by_simple: FxHashMap<String, Vec<SourceSymbolRef>>,
}

impl SourceSymbolIndex {
    /// Builds the index from `(file, symbol)` pairs. Order within a name
    /// bucket is preserved from the input (deterministic given the walker's
    /// file/arena order).
    pub fn build(symbols: impl IntoIterator<Item = (FileId, SourceSymbol)>) -> Self {
        let mut by_fqn: FxHashMap<Name, Vec<SourceSymbolRef>> = FxHashMap::default();
        let mut by_simple: FxHashMap<String, Vec<SourceSymbolRef>> = FxHashMap::default();
        for (file, symbol) in symbols {
            let simple = simple_name(&symbol.name);
            by_simple
                .entry(simple.to_lowercase())
                .or_default()
                .push(SourceSymbolRef {
                    file,
                    symbol: symbol.clone(),
                });
            by_fqn
                .entry(symbol.name.clone())
                .or_default()
                .push(SourceSymbolRef { file, symbol });
        }
        Self { by_fqn, by_simple }
    }

    pub fn is_empty(&self) -> bool {
        self.by_fqn.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_fqn.values().map(Vec::len).sum()
    }

    /// Every symbol with the given canonical name. Overloaded members share a
    /// name, so the result is a vector.
    pub fn resolve_fqn(&self, fqn: &Name) -> &[SourceSymbolRef] {
        self.by_fqn.get(fqn).map_or(&[], Vec::as_slice)
    }

    /// The first class-like declaration (type or module) named `fqn`, if any.
    /// Types precede members in the walk order, so this is the declaration
    /// `fqn_resolve` should return.
    pub fn resolve_class_fqn(&self, fqn: &Name) -> Option<&SourceSymbolRef> {
        self.resolve_fqn(fqn).iter().find(|reference| {
            matches!(
                reference.symbol.kind,
                SourceSymbolKind::Class
                    | SourceSymbolKind::Interface
                    | SourceSymbolKind::Enum
                    | SourceSymbolKind::Record
                    | SourceSymbolKind::Annotation
                    | SourceSymbolKind::Module
            )
        })
    }

    /// Symbols whose simple name starts with `query` (case-insensitive),
    /// sorted by (name, file, item) for determinism.
    pub fn lookup_simple(&self, query: &str) -> Vec<SourceSymbolRef> {
        let query = query.to_lowercase();
        let mut out: Vec<SourceSymbolRef> = self
            .by_simple
            .iter()
            .filter(|(simple, _)| simple.starts_with(&query))
            .flat_map(|(_, refs)| refs.iter().cloned())
            .collect();
        out.sort_by_key(|reference| {
            (
                reference.symbol.name.as_str().to_owned(),
                reference.file,
                reference.symbol.item,
            )
        });
        out
    }

    /// Symbols whose simple name contains `query` as a case-insensitive
    /// substring, sorted by (name, file, item) for determinism.
    pub fn lookup_substring(&self, query: &str) -> Vec<SourceSymbolRef> {
        let query = query.to_lowercase();
        let mut out: Vec<SourceSymbolRef> = self
            .by_simple
            .iter()
            .filter(|(simple, _)| simple.contains(&query))
            .flat_map(|(_, refs)| refs.iter().cloned())
            .collect();
        out.sort_by_key(|reference| {
            (
                reference.symbol.name.as_str().to_owned(),
                reference.file,
                reference.symbol.item,
            )
        });
        out
    }

    /// Every symbol, in `by_fqn` order (deterministic only after sorting;
    /// used by tests and workspace-wide search).
    pub fn iter(&self) -> impl Iterator<Item = &SourceSymbolRef> {
        self.by_fqn.values().flatten()
    }
}

/// The last `.`-separated segment of a qualified name.
fn simple_name(name: &Name) -> &str {
    name.as_str()
        .rsplit_once('.')
        .map_or_else(|| name.as_str(), |(_, simple)| simple)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hir_expand::arena::ArenaId;

    fn symbol(name: &str) -> SourceSymbol {
        SourceSymbol {
            name: Name::new(name),
            item: ItemId(ArenaId(0)),
            range: rowan::TextRange::default(),
            kind: SourceSymbolKind::Class,
            public: true,
        }
    }

    #[test]
    fn resolve_fqn_and_simple_lookup() {
        let index = SourceSymbolIndex::build([
            (FileId::from_raw(1), symbol("com.example.Foo")),
            (FileId::from_raw(1), symbol("com.example.Foo.bar")),
            (FileId::from_raw(2), symbol("com.example.Bar")),
        ]);

        assert_eq!(index.resolve_fqn(&Name::new("com.example.Foo")).len(), 1);
        assert_eq!(
            index.resolve_fqn(&Name::new("com.example.Foo.bar")).len(),
            1
        );
        assert_eq!(
            index
                .resolve_class_fqn(&Name::new("com.example.Foo"))
                .map(|r| r.symbol.kind),
            Some(SourceSymbolKind::Class)
        );

        // Case-insensitive simple-name prefix search.
        let foo = index.lookup_simple("foo");
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].symbol.name.as_str(), "com.example.Foo");

        let foos = index.lookup_simple("FOO");
        assert_eq!(foos.len(), 1);

        // Substring search hits members too.
        let bar = index.lookup_substring("bar");
        assert_eq!(bar.len(), 2);
    }

    #[test]
    fn overloads_share_the_name_bucket() {
        let index = SourceSymbolIndex::build([
            (FileId::from_raw(1), symbol("com.example.Foo.m")),
            (FileId::from_raw(1), symbol("com.example.Foo.m")),
        ]);
        assert_eq!(index.resolve_fqn(&Name::new("com.example.Foo.m")).len(), 2);
        assert_eq!(index.len(), 2);
    }
}
