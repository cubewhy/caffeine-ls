//! Kotlin as the file-index layer's registry entry: the symbols, the doc
//! comments and the facade class a Kotlin file contributes to the workspace.

use base_db::{LanguageKind, file_language_kind, parse};
use hir_def::jvm::decl::ItemAnnotationValue;
use hir_def::kotlin::annotations::JvmAnnotation;
use hir_def::kotlin::item_tree::{KotlinClassKind, KotlinItemData, KotlinItemTree};
use hir_expand::body::Literal;
use hir_expand::ids::ItemId;
use hir_expand::name::Name;

use vfs::FileId;

use hir_expand::ast_id_map::AstIdMap;
use syntax::SourceFile;

use crate::db::HirDatabase;
use crate::db::join_name;
use crate::symbol_index::{SourceSymbol, SourceSymbolKind};
use rowan::TextRange;

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl crate::lang::LanguageFileIndex for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn file_symbols(&self, db: &dyn HirDatabase, file: FileId) -> Vec<SourceSymbol> {
        let Some(tree) = hir_def::kotlin::plugin::tree(db, file) else {
            return Vec::new();
        };
        collect_symbols(&tree)
    }

    fn file_docs(&self, db: &dyn HirDatabase, file: FileId) -> Vec<(ItemId, TextRange)> {
        let Some(tree) = hir_def::kotlin::plugin::tree(db, file) else {
            return Vec::new();
        };
        // The *file's* kind, not the language: the id map and the parse are
        // keyed by `(file, kind)` together (`crate::db::item_tree_query`), and
        // the tree's anchors were resolved against the entry of the file's own
        // kind — for a `.kts` script the `script` production
        // ([spec: grammar-rule-script]), for a `.kt` file the `kotlinFile` one.
        let language = file_language_kind(db, file).unwrap_or(LanguageKind::Kotlin);
        let map = hir_def::db::ast_id_map(db, file, language);
        let source = parse(db, file, language).syntax_node(language);
        let mut out = Vec::new();
        for &top in &tree.top {
            collect_docs(map, &source, &tree, top, &mut out);
        }
        out
    }

    /// The package the file declares ([KLS
    /// `packages-and-imports.html#packages`](https://kotlinlang.org/spec/packages-and-imports.html#packages)),
    /// `None` for the unnamed package — the namespace the file's declarations
    /// are indexed under, and the one the compiler's synthesized facade class
    /// lives in.
    fn file_package(&self, db: &dyn HirDatabase, file: FileId) -> Option<Name> {
        hir_def::kotlin::plugin::tree(db, file)?.package.clone()
    }

    fn file_facade_class(&self, db: &dyn HirDatabase, file: FileId) -> Option<Name> {
        // The compiler's name for the facade (`FooKt`, or the `@file:JvmName`
        // the file writes).
        let tree = hir_def::kotlin::plugin::tree(db, file)?;
        facade_class(db, file, &tree)
    }
}

/// The JVM facade class the compiler synthesizes for `file`'s top-level
/// declarations, using the compiler's filename normalization, or the resolved
/// `@file:JvmName("Y")` override.
/// <https://kotlinlang.org/docs/java-to-kotlin-interop.html#package-level-functions>
///
/// `@file:JvmName` is the `kotlin.jvm.JvmName` *library* annotation
/// ([`JvmAnnotation::Name`]), so the application is read through the name it
/// resolves to in the file's scopes
/// ([`KotlinItemTree::candidate_fqns`]): `@file:JvmName("Y")` is the compiler's
/// annotation — as is the qualified `@file:kotlin.jvm.JvmName("Y")` — while a
/// `JvmName` that the file's own package or an import binds is the file's own
/// annotation, and renames nothing. The file's *name* is what decides the
/// default, `Foo.kt` compiling to `FooKt`.
///
/// A `.kts` script has **no** facade: it compiles to a class of its own — the
/// file's name, `hello.kts` to `Hello` and `build.gradle.kts` to
/// `Build_gradle`, which `@file:JvmName` does not rename — whose *members are
/// the script's instance members*, not statics: `fun helper(): Int = 1` in a
/// script is `public final int helper()`, while the same declaration in a `.kt`
/// file is `public static final int helper()` on `HelperKt` (`javap -p` on
/// kotlinc 2.4.20's output for both). A Java caller has no class of statics to
/// name, so the answer is `None` rather than the script class: naming the
/// script class here would resolve a *facade* lookup against members that are
/// not statics.
fn facade_class(db: &dyn HirDatabase, file: FileId, tree: &KotlinItemTree) -> Option<Name> {
    if tree.language == LanguageKind::KotlinScript {
        return None;
    }
    if let Some(name) = file_jvm_name(db, file, tree) {
        return Some(Name::new(&name));
    }
    let name = crate::file_name(db, file)?;
    let stem = name
        .strip_suffix(".kt")
        .or_else(|| name.strip_suffix(".kts"))?;
    // NameUtils.getPackagePartClassNamePrefix (kotlinc 2.4.20): Unicode
    // Letter categories and ASCII digits survive. The initial-character test
    // uses a UTF-16 code unit, so supplementary letters require an underscore.
    // https://kotlinlang.org/docs/java-to-kotlin-interop.html#package-level-functions
    use unicode_general_category::{GeneralCategory, get_general_category};
    let mut facade = String::with_capacity(stem.len() + 3);
    for (index, ch) in stem.chars().enumerate() {
        let letter = matches!(
            get_general_category(ch),
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::ModifierLetter
                | GeneralCategory::OtherLetter
        );
        let ch = if letter || ch.is_ascii_digit() {
            ch
        } else {
            '_'
        };
        if index == 0 {
            if ch.is_ascii_digit() || ch.len_utf16() == 2 {
                facade.push('_');
                facade.push(ch);
            } else {
                facade.extend(ch.to_uppercase());
            }
        } else {
            facade.push(ch);
        }
    }
    if facade.is_empty() {
        facade.push('_');
    }
    facade.push_str("Kt");
    Some(Name::new(&facade))
}

/// The value of the `@file:JvmName("…")` the file writes, when the application
/// is the standard library's `kotlin.jvm.JvmName`.
///
/// The element values are the ones the lowering carries, so a renamed facade
/// whose name is a *constant* (`@file:JvmName(FACADE)`) is not read yet — a
/// recorded gap, not a different rule.
fn file_jvm_name(db: &dyn HirDatabase, file: FileId, tree: &KotlinItemTree) -> Option<String> {
    let source_set = crate::source_set_for_file(db, file)?;
    for application in &tree.file_annotations {
        // The lowering records `@file:` applications only, so the target is
        // the file's; a *multi*-annotation (`@file:[A B]`) writes none and is
        // applied to the file as well.
        if application
            .target
            .as_ref()
            .is_some_and(|target| target.as_str() != "file")
        {
            continue;
        }
        // The first candidate that resolves is the declaration the compiler
        // picks, so a `JvmName` the file's own package declares — which
        // shadowed the library's — is the one an application names
        // ([`KotlinItemTree::candidate_fqns`] is in scope order). The lookup is
        // the facade-free one ([`crate::db::declaration_resolve`]): this
        // function *is* the facade rule, and the full resolution would ask it
        // for its own answer.
        let resolved = tree
            .candidate_fqns(application.annotation.name.as_str())
            .into_iter()
            .find(|candidate| crate::db::declaration_resolve(db, &source_set, candidate).is_some());
        if !resolved.is_some_and(|fqn| JvmAnnotation::Name.is(&fqn)) {
            continue;
        }
        for arg in &application.annotation.args {
            if let ItemAnnotationValue::Literal(Literal::Str(value)) = &arg.value {
                return Some(value.clone());
            }
        }
    }
    None
}

/// The symbol kind of a lowered Kotlin item, or `None` for the nameless
/// declarations (`init` blocks, property accessors) the index skips.
///
/// A property's accessors are skipped deliberately: the source declares no name
/// for them (the JVM name is synthesized from the property), so they are not
/// nameable from another file. They stay reachable through
/// [`hir_def::kotlin::item_tree::PropertyData::accessors`].
fn kind_of(data: &KotlinItemData) -> Option<SourceSymbolKind> {
    match data {
        KotlinItemData::Class(data) => Some(match data.kind {
            KotlinClassKind::Class => SourceSymbolKind::Class,
            KotlinClassKind::Interface => SourceSymbolKind::Interface,
            KotlinClassKind::Enum => SourceSymbolKind::Enum,
            KotlinClassKind::Annotation => SourceSymbolKind::Annotation,
            KotlinClassKind::Object | KotlinClassKind::CompanionObject => SourceSymbolKind::Object,
        }),
        KotlinItemData::Constructor(_) => Some(SourceSymbolKind::Constructor),
        KotlinItemData::Function(_) => Some(SourceSymbolKind::Function),
        KotlinItemData::Property(_) => Some(SourceSymbolKind::Property),
        KotlinItemData::EnumEntry(_) => Some(SourceSymbolKind::EnumConstant),
        KotlinItemData::TypeAlias(_) => Some(SourceSymbolKind::TypeAlias),
        KotlinItemData::Accessor(_) | KotlinItemData::AnonymousInitializer(_) => None,
    }
}

/// The indexed declarations of a Kotlin file's item tree.
///
/// Kotlin's own name rules ([KLS
/// `declarations.html#classifier-declaration-scopes`](https://kotlinlang.org/spec/declarations.html#classifier-declaration-scopes),
/// [KLS `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)):
/// a nested classifier and a member are both reached as `Enclosing.simple`, a
/// type alias and a top-level declaration as `package.simple`, a constructor
/// as its class's name (the JVM name the compiler synthesizes), and a
/// `companion object` as a member of its enclosing classifier.
///
/// A declaration is `public` unless it is `private`; `internal` is visible
/// inside the workspace, which is the granularity a workspace index has
/// ([KLS `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility)).
fn collect_symbols(tree: &KotlinItemTree) -> Vec<SourceSymbol> {
    fn collect(
        tree: &KotlinItemTree,
        id: ItemId,
        prefix: Option<&Name>,
        out: &mut Vec<SourceSymbol>,
    ) {
        let data = tree.data(id);
        let Some(kind) = kind_of(data) else {
            // An `init` block and a property accessor declare no name.
            return;
        };
        // A constructor has no declared name: it is indexed under its
        // classifier's qualified name.
        let simple = match data {
            KotlinItemData::Class(data) => data.name.as_str(),
            KotlinItemData::Function(data) => data.name.as_str(),
            KotlinItemData::Property(data) => data.name.as_str(),
            KotlinItemData::EnumEntry(data) => data.name.as_str(),
            KotlinItemData::TypeAlias(data) => data.name.as_str(),
            KotlinItemData::Constructor(_) => {
                let Some(prefix) = prefix else {
                    // A constructor is always nested in a classifier.
                    return;
                };
                out.push(SourceSymbol {
                    name: prefix.clone(),
                    item: id,
                    kind,
                    public: data
                        .modifiers()
                        .is_none_or(|modifiers| !modifiers.visibility.is_private()),
                });
                return;
            }
            KotlinItemData::Accessor(_) | KotlinItemData::AnonymousInitializer(_) => return,
        };
        let name = match prefix {
            Some(prefix) => join_name(prefix, simple),
            None => match &tree.package {
                Some(package) => join_name(package, simple),
                None => Name::new(simple),
            },
        };
        out.push(SourceSymbol {
            name: name.clone(),
            item: id,
            kind,
            // An enum entry declares no visibility and is `public`; every
            // other indexed item is `public` unless it says `private`.
            public: data
                .modifiers()
                .is_none_or(|modifiers| !modifiers.visibility.is_private()),
        });
        if let KotlinItemData::Class(class) = data {
            if let Some(constructor) = class.primary_constructor {
                collect(tree, constructor, Some(&name), out);
            }
        }
        let child_prefix = match kind {
            // Nested classifiers and members are both `Enclosing.simple`; an
            // enum entry's members belong to the enum's anonymous subclass.
            SourceSymbolKind::Class
            | SourceSymbolKind::Interface
            | SourceSymbolKind::Enum
            | SourceSymbolKind::Annotation
            | SourceSymbolKind::Object
            | SourceSymbolKind::EnumConstant => Some(&name),
            _ => prefix,
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

/// Collects the KDoc range of `id` and of every declaration nested in it — its
/// members and a property's accessors — mirroring the item walk the IDE's
/// outline uses.
fn collect_docs(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &KotlinItemTree,
    id: ItemId,
    out: &mut Vec<(ItemId, TextRange)>,
) {
    if let Some(range) = hir_def::kotlin::ranges::item_doc_range(map, source, tree, id) {
        out.push((id, range));
    }
    if let KotlinItemData::Property(property) = tree.data(id) {
        for &accessor in &property.accessors {
            collect_docs(map, source, tree, accessor, out);
        }
    }
    for &child in tree.data(id).body() {
        collect_docs(map, source, tree, child, out);
    }
}
