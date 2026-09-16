//! Java as the file-index layer's registry entry: the symbols, the doc comments
//! and the package a Java file contributes to the workspace.

use base_db::{LanguageKind, parse};
use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::name::Name;

use vfs::FileId;

use hir_expand::ast_id_map::AstIdMap;
use syntax::SourceFile;

use crate::db::HirDatabase;
use crate::db::join_name;
use crate::symbol_index::{SourceSymbol, SourceSymbolKind};
use rowan::TextRange;

pub(crate) struct Java;

pub(crate) static JAVA: Java = Java;

impl crate::lang::LanguageFileIndex for Java {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Java]
    }

    fn file_symbols(&self, db: &dyn HirDatabase, file: FileId) -> Vec<SourceSymbol> {
        let tree = hir_def::java::plugin::tree(db, file);
        collect_symbols(&tree)
    }

    fn file_docs(&self, db: &dyn HirDatabase, file: FileId) -> Vec<(ItemId, TextRange)> {
        let language = LanguageKind::Java;
        let tree = hir_def::java::plugin::tree(db, file);
        let map = hir_def::db::ast_id_map(db, file, language);
        let source = parse(db, file, language).syntax_node(language);
        let mut out = Vec::new();
        for &top in &tree.top {
            collect_docs(map, &source, &tree, top, &mut out);
        }
        out
    }

    fn file_package(&self, db: &dyn HirDatabase, file: FileId) -> Option<Name> {
        hir_def::java::plugin::tree(db, file).package.clone()
    }
}

/// The symbol kind of a lowered Java item, or `None` for the nameless
/// declarations (instance/static initializers) that the index skips.
pub fn symbol_kind(data: &ItemData) -> Option<SourceSymbolKind> {
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

/// The indexed declarations of a Java file's item tree.
///
/// A *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// is deliberately absent: §6.7 gives it neither a fully qualified nor a
/// canonical name, so it can neither be named from another file nor be looked
/// up by name — this index *is* the workspace symbol index, and the IDE
/// surfaces a local declaration from its own file's item tree instead.
fn collect_symbols(tree: &ItemTree) -> Vec<SourceSymbol> {
    fn collect(tree: &ItemTree, id: ItemId, prefix: Option<&Name>, out: &mut Vec<SourceSymbol>) {
        let data = tree.data(id);
        let Some(kind) = symbol_kind(data) else {
            // Initializers have no name and are not indexed.
            return;
        };
        let (simple, public) = match data {
            ItemData::Class(d) | ItemData::Interface(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Enum(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Record(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Annotation(d) => (&d.name, d.modifiers.is_public()),
            // Enum constants are implicitly `public static final`
            // ([JLS §8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)).
            ItemData::EnumConstant(d) => (&d.name, true),
            // A module declaration carries no access modifiers
            // ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)).
            ItemData::Module(d) => (&d.name, false),
            ItemData::Method(d) => (&d.name, d.modifiers.is_public()),
            ItemData::Field(d) => (&d.name, d.modifiers.is_public()),
            ItemData::StaticInit(_) | ItemData::InstanceInit(_) => unreachable!(),
        };
        let name = match prefix {
            Some(prefix) => join_name(prefix, simple.as_str()),
            // The unnamed package
            // ([JLS §7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
            // yields a bare simple name.
            None => match &tree.package {
                Some(package) => join_name(package, simple.as_str()),
                None => simple.clone(),
            },
        };
        out.push(SourceSymbol {
            name: name.clone(),
            item: id,
            kind,
            public,
        });
        if data.body().is_empty() {
            return;
        }
        let child_prefix = match kind {
            // Nested types are indexed under the enclosing FQN
            // ([JLS §8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3));
            // members under `EnclosingFqn.simple`.
            SourceSymbolKind::Class
            | SourceSymbolKind::Interface
            | SourceSymbolKind::Enum
            | SourceSymbolKind::Record
            | SourceSymbolKind::Annotation => Some(&name),
            SourceSymbolKind::Module
            | SourceSymbolKind::Method
            | SourceSymbolKind::Field
            | SourceSymbolKind::EnumConstant
            | SourceSymbolKind::Package
            // The Kotlin kinds never occur for a Java item; a Java declaration
            // that carries one of these cannot exist, so the arm is
            // unreachable but kept total.
            | SourceSymbolKind::Object
            | SourceSymbolKind::Function
            | SourceSymbolKind::Property
            | SourceSymbolKind::Constructor
            | SourceSymbolKind::TypeAlias => prefix,
        };
        for &child in data.body() {
            collect(tree, child, child_prefix, out);
        }
    }

    let mut out = Vec::new();
    for &top in &tree.top {
        collect(tree, top, None, &mut out);
    }
    out
}

/// Collects the doc-comment range of `id` and of every declaration nested in
/// it (its members and its local class-like declarations), mirroring the item
/// walk of the IDE's outline (`ide::nav::java::all_items`).
fn collect_docs(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    id: ItemId,
    out: &mut Vec<(ItemId, TextRange)>,
) {
    if let Some(range) = hir_def::java::ranges::item_doc_range(map, source, tree, id) {
        out.push((id, range));
    }
    for &child in tree.data(id).body() {
        collect_docs(map, source, tree, child, out);
    }
    for local in tree.local_types_of(id) {
        collect_docs(map, source, tree, local, out);
    }
}
