//! The lowered per-file Kotlin declaration model ("item tree").
//!
//! Lowering turns the Kotlin CST into this flat, arena-based IR: every
//! top-level classifier, member, constructor, property, accessor, enum entry,
//! `init` block and type alias gets a stable [`ItemId`]. The *bodies* of
//! functions, accessors, initializers, property initializers, constructor
//! bodies and enum-entry arguments are lowered into the per-file
//! [`hir_expand::body::BodyTree`], which lives *beside* the item tree
//! ([`crate::item_tree::LoweredFile`]) rather than inside it: keeping the body
//! content out of the memoized item tree lets salsa backdate the
//! signature-level queries across edits that only touch a body.
//!
//! The item tree carries **no source offsets**: every lowered declaration
//! anchors itself to its syntax node with a
//! [`FileAstId`](hir_expand::ast_id_map::FileAstId), and the source ranges of
//! items, names, type references and annotations are resolved on demand from
//! the current syntax tree ([`crate::kotlin::ranges`]). The pointer-based ids
//! are a function of the file's *declaration skeleton* only — body content is
//! pruned from the id map — so a body-only edit leaves the tree's value
//! unchanged and salsa backdates every consumer.
//!
//! The declaration shapes follow the KLS grammar: one `CLASS_DECL` production
//! covers every classifier ([KLS
//! `declarations.html#classifier-declaration`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)),
//! so [`ClassData`] carries a [`KotlinClassKind`] instead of the Java layer's
//! one variant per node kind. The shared, language-neutral twin types of the
//! Java layer ([`ItemTypeRef`], [`ItemAnnotationRef`], [`Param`]) are reused
//! verbatim — they are `syntax::stub`- and `Name`-based and carry no Java
//! concepts — where Kotlin needs an attribute they lack (the variance of a
//! type parameter) it declares its own [`KotlinTypeParam`].

use hir_expand::{
    arena::Arena,
    ast_id_map::{AstIdMap, FileAstId, node_ptr},
    body::{BodyId, ExprId, LocalId},
    name::Name,
};

pub use base_db::LanguageKind;
pub use hir_expand::ids::ItemId;
// The shared declaration-side type and annotation references, and a formal
// parameter, live in the JVM layer: Kotlin lowering constructs them directly
// (Java's `ItemTypeRef::from_spanned` is the Java walker's own constructor)
// and resolves their ranges through [`crate::kotlin::ranges`].
use crate::jvm::decl::{ItemAnnotationRef, ItemTypeRef, Param};

/// A formal parameter of a Kotlin declaration: the shared, language-neutral
/// [`Param`] shape plus the two parameter modifiers Kotlin has and Java does
/// not ([spec: grammar-rule-parameterModifiers], [KLS
/// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)).
///
/// `noinline` marks a function-typed parameter of an `inline` function as
/// never inlined, `crossinline` as inlinable only where it cannot return
/// non-locally; neither changes the parameter's JVM shape, and neither
/// participates in overload resolution, so the type layer reads the shared
/// [`Param`] and ignores them. They are carried because the declaration IR
/// must not drop what the source wrote — the wrapper exists because [`Param`]
/// is shared with Java and a Kotlin-only attribute belongs on a Kotlin-only
/// shape.
#[derive(Debug, Clone, PartialEq)]
pub struct KotlinParam {
    pub param: Param,
    /// Whether the parameter carries `noinline`.
    pub noinline: bool,
    /// Whether the parameter carries `crossinline`.
    pub crossinline: bool,
}

/// An annotation application of a Kotlin declaration ([KLS
/// `annotations.html#annotation-use-site-targets`](https://kotlinlang.org/spec/annotations.html#annotation-use-site-targets)):
/// the shared, language-neutral [`ItemAnnotationRef`] — name, element values,
/// syntax node — plus the *use-site target* Kotlin alone writes, the `get` of
/// `@get:JvmName`, the `field` of `@field:JvmField`, the `file` of
/// `@file:JvmName`.
///
/// A parameter's and a type-use annotation never write a target, so those two
/// keep the shared shape ([`Param::annotations`],
/// [`ItemTypeRef::type_use_annotations`]).
#[derive(Debug, Clone, PartialEq)]
pub struct KotlinAnnotationRef {
    /// The target of `@get:JvmName("x")`, if the application writes one.
    pub target: Option<Name>,
    pub annotation: ItemAnnotationRef,
}

/// A supertype specifier ([KLS
/// `declarations.html#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)):
/// the supertype, plus the two things its syntax node carries besides the type.
#[derive(Debug, Clone, PartialEq)]
pub struct KotlinSuperType {
    pub ty: ItemTypeRef,
    /// The constructor arguments of `class C : Base(1, 2)`, in source order.
    /// Empty for a specifier without a call.
    pub args: Vec<ExprId>,
    /// The delegate expression of `interface I by impl`, if written.
    pub delegate: Option<ExprId>,
}

/// The `: this(…)` / `: super(…)` call of a secondary constructor ([KLS
/// `declarations.html#secondary-constructor`](https://kotlinlang.org/spec/declarations.html#secondary-constructor)).
#[derive(Debug, Clone, PartialEq)]
pub struct ConstructorDelegation {
    /// Whether the call delegates to the superclass (`super(…)`) rather than
    /// to another constructor of the same class (`this(…)`).
    pub is_super: bool,
    /// The argument expressions, in source order.
    pub args: Vec<ExprId>,
    pub ast: FileAstId<ConstructorDelegationCallNode>,
}

use crate::kotlin::modifiers::{KotlinModifiers, KotlinVariance};

/// The syntax-node markers of the [`FileAstId`]s stored in the Kotlin item
/// tree. Zero-sized; they type the id's role without constraining the node's
/// language.
///
/// `ClassDeclNode` covers every classifier node (`CLASS_DECL`, `OBJECT_DECL`,
/// `COMPANION_OBJECT`), and `ConstructorDeclNode` both the primary and the
/// secondary constructor — the marker keeps the id's *role* readable while the
/// node kind stays in the pointer ([`FileAstId`] is language- and
/// kind-generic).
pub struct ClassDeclNode;
pub struct FunctionDeclNode;
pub struct AccessorNode;
pub struct ConstructorDeclNode;
pub struct ConstructorDelegationCallNode;
/// A property declaration: a `PROPERTY_DECL` node, or the `CLASS_PARAMETER`
/// node whose `val`/`var` declares one.
pub struct PropertyNode;
pub struct EnumEntryNode;
pub struct TypeAliasNode;
pub struct AnonymousInitializerNode;
pub struct PackageHeaderNode;
pub struct ImportHeaderNode;
pub struct FileAnnotationNode;

/// A Kotlin import ([KLS
/// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KotlinImportItem {
    /// The imported path, without a trailing `.*`
    /// ([`Self::is_asterisk`] records the star).
    pub path: Name,
    /// The name the import binds the path to (`import a.b.C as D`).
    pub alias: Option<Name>,
    /// Whether the import is a star import (`import a.b.*`), which imports
    /// every member of the package or classifier in one declaration
    /// ([KLS `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
    pub is_asterisk: bool,
    /// The `IMPORT_HEADER` syntax node of the import.
    pub ast: FileAstId<ImportHeaderNode>,
}

/// The per-file result of Kotlin lowering.
#[derive(Debug, Clone, PartialEq)]
pub struct KotlinItemTree {
    pub language: LanguageKind,
    /// The declared package ([KLS
    /// `packages-and-imports.html#packages`](https://kotlinlang.org/spec/packages-and-imports.html#packages)),
    /// or the unnamed package.
    pub package: Option<Name>,
    /// The `PACKAGE_HEADER` syntax node, when the file declares a package.
    pub package_header: Option<FileAstId<PackageHeaderNode>>,
    /// The file-level annotations (`@file:JvmName("…")`), in source order,
    /// each with its `file` use-site target and its lowered element values.
    pub file_annotations: Vec<KotlinAnnotationRef>,
    pub imports: Vec<KotlinImportItem>,
    pub top: Vec<ItemId>,
    /// The local declarations of the file — a local class or local function
    /// declared inside a function body ([KLS
    /// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration),
    /// [`#local-function-declaration`](https://kotlinlang.org/spec/declarations.html#local-function-declaration)).
    /// They are not members of any classifier, so they stay out of every
    /// `body()` and are reachable only through this list and through the body
    /// that declares them.
    ///
    /// Empty until the Kotlin body lowering lands.
    pub local_types: Vec<ItemId>,
    /// The declaration each item is nested in: a member's enclosing classifier,
    /// an accessor's property, a primary constructor's classifier. Indexed by
    /// item id — grown in lock-step with `items` by [`KotlinItemTree::alloc`] —
    /// and `None` for a top-level item.
    pub parent: Vec<Option<ItemId>>,
    pub items: Arena<KotlinItemData>,
}

impl Default for KotlinItemTree {
    fn default() -> Self {
        Self {
            language: LanguageKind::Kotlin,
            package: None,
            package_header: None,
            file_annotations: Vec::new(),
            imports: Vec::new(),
            top: Vec::new(),
            local_types: Vec::new(),
            parent: Vec::new(),
            items: Arena::default(),
        }
    }
}

/// The packages every Kotlin file imports implicitly
/// (<https://kotlinlang.org/docs/packages.html#default-imports>).
///
/// KLS does not enumerate the default imports (the *Kotlin/Core* specification
/// has no section for them), so the list below is pinned empirically with the
/// probe oracle of kotlinc 2.4.20 (JRE 21.0.11): `listOf(1)` is a
/// `kotlin.collections.List<Int>` with no import (so `List` comes from
/// `kotlin.collections`), and `String`/`Exception`/`StringBuilder` resolve with
/// no import (so `kotlin` and `java.lang` are both default). The list is the
/// compiler's documented one
/// (<https://kotlinlang.org/docs/packages.html#default-imports>) checked
/// against it; a standard-library member that resolves through none of these
/// packages is a missing entry.
///
/// `kotlin.reflect` is deliberately *not* a default import: `fun f(): KClass<*>`
/// with no import is `unresolved reference 'KClass'.` under kotlinc 2.4.20, and
/// the compiler's documented list has no entry for the package. `String::class`
/// still needs no import — the class literal is an *expression* whose type is
/// `kotlin.reflect.KClass`, and an expression's type needs no name in scope.
pub const DEFAULT_IMPORTS: &[&str] = &[
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "java.lang",
    "kotlin.jvm",
];

impl KotlinItemTree {
    pub fn data(&self, id: ItemId) -> &KotlinItemData {
        self.items.get(id.0)
    }

    /// Allocates an item, keeping [`Self::parent`] aligned with the arena. A
    /// direct `items.alloc` would desynchronize the two; allocate through
    /// here.
    pub fn alloc(&mut self, data: KotlinItemData) -> ItemId {
        let id = ItemId(self.items.alloc(data));
        self.parent.push(None);
        id
    }

    /// The declaration `item` is nested in; `None` for a top-level item.
    pub fn parent_of(&self, item: ItemId) -> Option<ItemId> {
        self.parent.get(item.0.0 as usize).copied().flatten()
    }

    /// The candidate fully qualified names the written name `name` may denote
    /// *in this file*, in KLS's scope order ([KLS
    /// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)):
    ///
    /// 1. an explicit import binding the name — its alias, or its last segment
    ///    ([`KotlinImportItem`]) — or, for a *dotted* name, an alias as its
    ///    head;
    /// 2. a dotted name as written (Kotlin allows a fully qualified
    ///    reference);
    /// 3. a declaration of the file's own package;
    /// 4. a star import's members;
    /// 5. the *default imports*, which is what makes `String`, `Int` and
    ///    `List` resolve in a file that imports nothing.
    ///
    /// A caller that owns a scope resolves the candidates in order — the first
    /// one that names a declaration is the one the compiler picks — while a
    /// caller that has no scope walks them all. The list is the *language's*
    /// rule and needs no database, which is why it lives with the file's
    /// lowered imports rather than with a resolver.
    pub fn candidate_fqns(&self, name: &str) -> Vec<String> {
        let segments: Vec<&str> = name.split('.').collect();
        let simple = segments[0];
        let mut candidates: Vec<String> = Vec::new();

        if segments.len() == 1 {
            // An explicit import (by alias or by its last segment).
            for import in &self.imports {
                if import.is_asterisk {
                    continue;
                }
                let bound = import
                    .alias
                    .as_ref()
                    .map(|alias| alias.as_str().to_owned())
                    .unwrap_or_else(|| import.path.simple_name().to_owned());
                if bound == simple {
                    candidates.push(import.path.as_str().to_owned());
                }
            }
        } else {
            // A dotted name whose head is an imported name.
            for import in &self.imports {
                if import.is_asterisk {
                    continue;
                }
                if import
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias.as_str() == simple)
                {
                    candidates.push(format!("{}.{}", import.path, segments[1..].join(".")));
                }
            }
            candidates.push(name.to_owned());
        }

        // The file's own package, then the star imports, then the defaults.
        let in_package = |fqn: &str| match &self.package {
            Some(package) => format!("{package}.{fqn}"),
            None => fqn.to_owned(),
        };
        candidates.push(in_package(name));
        for import in self.imports.iter().filter(|import| import.is_asterisk) {
            candidates.push(format!("{}.{}", import.path, name));
        }
        for package in DEFAULT_IMPORTS {
            candidates.push(format!("{package}.{name}"));
        }

        candidates
    }

    /// The local declarations nested in `owner`, in source order.
    pub fn local_types_of(&self, owner: ItemId) -> impl Iterator<Item = ItemId> + '_ {
        self.local_types
            .iter()
            .copied()
            .filter(move |item| self.parent_of(*item) == Some(owner))
    }

    /// Whether `item` is a local declaration
    /// ([KLS `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
    pub fn is_local_type(&self, item: ItemId) -> bool {
        self.local_types.contains(&item)
    }

    /// The id viewed as a classifier id, if the item is a class-like
    /// declaration.
    pub fn as_class(&self, id: ItemId) -> Option<ClassId> {
        self.data(id).is_class().then_some(ClassId(id))
    }

    /// The id viewed as a callable id, if the item is a function, a
    /// constructor or a property accessor.
    pub fn as_callable(&self, id: ItemId) -> Option<CallableId> {
        self.data(id).is_callable().then_some(CallableId(id))
    }

    /// The id viewed as a property id, if the item is a property.
    pub fn as_property(&self, id: ItemId) -> Option<PropertyId> {
        self.data(id).is_property().then_some(PropertyId(id))
    }

    /// The declaration data of a classifier item.
    ///
    /// # Panics
    /// If `id` was not produced by [`Self::as_class`].
    pub fn class(&self, id: ClassId) -> &ClassData {
        match self.data(id.0) {
            KotlinItemData::Class(data) => data,
            _ => panic!("ClassId for non-class item: {id:?}"),
        }
    }

    /// The declaration data of a property item.
    ///
    /// # Panics
    /// If `id` was not produced by [`Self::as_property`].
    pub fn property(&self, id: PropertyId) -> &PropertyData {
        match self.data(id.0) {
            KotlinItemData::Property(data) => data,
            _ => panic!("PropertyId for non-property item: {id:?}"),
        }
    }
}

/// The id of a classifier item within its owning [`KotlinItemTree`]. A typed
/// view of an [`ItemId`]; the bare id is recoverable through the tuple field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub ItemId);

/// The id of a callable item (a function, a constructor or an accessor).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallableId(pub ItemId);

/// The id of a property item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PropertyId(pub ItemId);

/// A lowered Kotlin declaration or member.
#[derive(Debug, Clone, PartialEq)]
pub enum KotlinItemData {
    Class(ClassData),
    Constructor(ConstructorData),
    Function(FunctionData),
    /// A declared property accessor: the `get()`/`set()` of a property
    /// ([KLS `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters)).
    /// Its own item rather than a [`FunctionData`] because it declares no name
    /// — the JVM name (`getX`/`setX`) is synthesized from the property
    /// ([`AccessorData::is_setter`]) — and its type comes from the property.
    Accessor(AccessorData),
    Property(PropertyData),
    AnonymousInitializer(InitData),
    EnumEntry(EnumEntryData),
    TypeAlias(TypeAliasData),
}

/// The kind of a Kotlin classifier ([KLS
/// `declarations.html#classifier-declaration`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)).
/// One variant per declaration form the grammar names; `object` and
/// `companion object` are distinct because a companion is reached through its
/// enclosing classifier rather than through an expression.
///
/// The shapes the compiler emits for each kind (observed with kotlinc 2.4.20 /
/// `javap -p`), which the member-set and JVM-flag layers follow:
///
/// | declaration | classfile |
/// |---|---|
/// | `object Util` | `public final class Util` with `public static final Util INSTANCE`, a `private Util()` and `public final int f()` |
/// | `class Point(val x: Int)` | `private final int x`, `public Point(int)`, `public final int getX()` — a plain parameter (`class Point(x: Int)`) emits no accessor |
/// | `enum class E { A, B }` | `public final class E extends java.lang.Enum<E>` with `public static final E A` |
/// | `annotation class Ann(val name: String)` | `public interface Ann extends java.lang.annotation.Annotation` with `public abstract String name()` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KotlinClassKind {
    Class,
    Interface,
    Enum,
    Annotation,
    Object,
    CompanionObject,
}

impl KotlinClassKind {
    /// The keyword that introduces this kind, as the source spells it.
    pub fn keyword(self) -> &'static str {
        match self {
            KotlinClassKind::Class => "class",
            KotlinClassKind::Interface => "interface",
            KotlinClassKind::Enum => "enum class",
            KotlinClassKind::Annotation => "annotation class",
            KotlinClassKind::Object => "object",
            KotlinClassKind::CompanionObject => "companion object",
        }
    }
}

/// A classifier declaration: `class`, `interface`, `enum class`,
/// `annotation class`, `object` and `companion object` all lower to this one
/// shape, differing in [`ClassData::kind`].
#[derive(Debug, Clone, PartialEq)]
pub struct ClassData {
    pub name: Name,
    pub kind: KotlinClassKind,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    /// The declared type parameters, with their variance ([KLS
    /// `declarations.html#type-parameter-variance`](https://kotlinlang.org/spec/declarations.html#type-parameter-variance)).
    pub type_params: Vec<KotlinTypeParam>,
    /// The supertypes of the delegation specifier list ([KLS
    /// `declarations.html#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)),
    /// in source order, each with its constructor arguments and its delegate
    /// expression. A classifier with none is a subtype of `kotlin.Any`; that
    /// implicit supertype is *not* recorded here (the type layer adds it).
    pub super_types: Vec<KotlinSuperType>,
    /// The declared primary constructor ([KLS
    /// `declarations.html#primary-constructor`](https://kotlinlang.org/spec/declarations.html#primary-constructor)),
    /// if the classifier header has a parameter list. A classifier that
    /// declares no constructor has no implicit one recorded: the compiler
    /// synthesizes it, but the source declares none.
    pub primary_constructor: Option<ItemId>,
    /// The members declared in the class body / enum class body, in source
    /// order. A primary constructor's `val`/`var` class parameters are
    /// members too ([KLS
    /// `declarations.html#primary-constructor`](https://kotlinlang.org/spec/declarations.html#primary-constructor):
    /// they declare properties) and appear here.
    pub body: Vec<ItemId>,
    /// The classifier's syntax node (`CLASS_DECL`, `OBJECT_DECL` or
    /// `COMPANION_OBJECT`).
    pub ast: FileAstId<ClassDeclNode>,
}

/// A constructor declaration: the primary constructor of a classifier header
/// or a secondary `constructor(…)` in the class body.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstructorData {
    pub params: Vec<KotlinParam>,
    /// The same parameters as body *locals*, one per entry of `params`: a
    /// primary constructor's parameter scope is in scope in every initializer
    /// and `init` block of its classifier ("the primary constructor parameter
    /// scope is downward-linked to the classifier initialization scope", [KLS
    /// `declarations.html#constructor-declaration-scopes`](https://kotlinlang.org/spec/declarations.html#constructor-declaration-scopes)),
    /// so the declaration has to be a local of the file's arena for those bodies
    /// to reach it. A secondary constructor's parameters are the parameters of
    /// its own body and are bound there instead, so this is empty for it.
    pub param_locals: Vec<LocalId>,
    /// One entry per parameter, in parameter order: the lowered `= expr` a
    /// parameter declares, or `None` ([KLS
    /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
    pub defaults: Vec<Option<ExprId>>,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    /// The `CONSTRUCTOR_DELEGATION_CALL` of a secondary constructor
    /// (`: this(…)` / `: super(…)`), if it declares one, with its arguments.
    pub delegation: Option<ConstructorDelegation>,
    /// The lowered body of a secondary constructor ([KLS
    /// `declarations.html#secondary-constructor`](https://kotlinlang.org/spec/declarations.html#secondary-constructor)):
    /// a `BLOCK`, an expression body, or nothing. Always `None` for a primary
    /// constructor, whose body is the class body's `init` blocks.
    pub body: Option<BodyId>,
    /// The `PRIMARY_CONSTRUCTOR` or `SECONDARY_CONSTRUCTOR` syntax node.
    pub ast: FileAstId<ConstructorDeclNode>,
}

/// A function declaration ([KLS
/// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)),
/// including an extension function ([`Self::receiver`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionData {
    pub name: Name,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    pub type_params: Vec<KotlinTypeParam>,
    /// The extension receiver type (`fun String.toURI(): URI`), if the
    /// declaration is an extension.
    pub receiver: Option<ItemTypeRef>,
    pub params: Vec<KotlinParam>,
    /// One entry per parameter, in parameter order: the lowered `= expr` a
    /// parameter declares, or `None` ([KLS
    /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
    /// A call may omit every argument whose parameter has a default.
    pub defaults: Vec<Option<ExprId>>,
    /// The declared return type, if the declaration writes one (`fun f(): Int`
    /// or `fun f() = expr`). A block-bodied function without one returns
    /// `Unit` ([KLS
    /// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration));
    /// the type layer applies that default.
    pub ret: Option<ItemTypeRef>,
    /// The lowered body: the block, the expression body, or nothing for a
    /// declaration without one (`abstract`, an interface member, `expect`,
    /// `external`).
    pub body: Option<BodyId>,
    /// Whether the body is the *expression* form (`fun f() = expr`) rather than
    /// a block. A declaration that writes no return type is typed by that
    /// expression ([KLS
    /// `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)),
    /// while a block-bodied one returns `Unit` whatever its block's last
    /// expression is — which is why the shape is recorded here and not derived
    /// from the statements, whose list is a single expression in both forms.
    pub expression_body: bool,
    /// The `FUNCTION_DECL` syntax node.
    pub ast: FileAstId<FunctionDeclNode>,
}

/// A declared property accessor.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessorData {
    /// Whether the accessor is the setter. Its declared parameters are empty
    /// for a getter, and hold the declared parameter of a setter that names one
    /// (`set(value)`); a setter without one has the implicit `value`
    /// parameter, which the type layer synthesizes.
    pub is_setter: bool,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    pub params: Vec<KotlinParam>,
    /// The lowered body: the block, the expression body (`get() = …`), or
    /// nothing for a declaration without one.
    pub body: Option<BodyId>,
    /// Whether the body is the *expression* form (`get() = expr`), as on
    /// [`FunctionData::expression_body`].
    pub expression_body: bool,
    /// The `GETTER` or `SETTER` syntax node.
    pub ast: FileAstId<AccessorNode>,
}

/// A property declaration ([KLS
/// `declarations.html#property-declaration`](https://kotlinlang.org/spec/declarations.html#property-declaration)),
/// read-only (`val`) or mutable (`var`), including an extension property
/// ([`Self::receiver`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyData {
    pub name: Name,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    pub type_params: Vec<KotlinTypeParam>,
    /// The extension receiver type (`val Response.string: String`), if the
    /// declaration is an extension property.
    pub receiver: Option<ItemTypeRef>,
    /// The declared type, if the declaration writes one. A property without
    /// one infers it from its initializer ([KLS
    /// `declarations.html#property-declaration`](https://kotlinlang.org/spec/declarations.html#property-declaration)).
    pub ty: Option<ItemTypeRef>,
    /// Whether the declaration is a `var` (mutable).
    pub is_var: bool,
    /// The lowered initializer expression, if the declaration writes `= expr`.
    pub initializer_expr: Option<ExprId>,
    /// The lowered delegated-property expression, if the declaration writes
    /// `by expr` ([KLS
    /// `declarations.html#delegated-property-declaration`](https://kotlinlang.org/spec/declarations.html#delegated-property-declaration)).
    pub delegate_expr: Option<ExprId>,
    /// The accessors the declaration spells out (`get() = …`,
    /// `private set`), in source order. A property without any uses the
    /// synthesized accessors the type layer adds.
    pub accessors: Vec<ItemId>,
    /// The `PROPERTY_DECL` syntax node, or the `CLASS_PARAMETER` node of a
    /// primary constructor parameter that declares the property.
    pub ast: FileAstId<PropertyNode>,
}

/// An `init { … }` block of a class body ([KLS
/// `declarations.html#classifier-initialization`](https://kotlinlang.org/spec/declarations.html#classifier-initialization)).
#[derive(Debug, Clone, PartialEq)]
pub struct InitData {
    /// The lowered body; `None` until the body lowering lands. An `init` block
    /// always has one.
    pub body: Option<BodyId>,
    /// The `ANONYMOUS_INITIALIZER` syntax node.
    pub ast: FileAstId<AnonymousInitializerNode>,
}

/// An enum entry declaration ([KLS
/// `declarations.html#enum-class-declaration`](https://kotlinlang.org/spec/declarations.html#enum-class-declaration)).
#[derive(Debug, Clone, PartialEq)]
pub struct EnumEntryData {
    pub name: Name,
    pub annotations: Vec<KotlinAnnotationRef>,
    /// The lowered constructor arguments, in source order. Empty for an entry
    /// without an argument list.
    pub argument_exprs: Vec<ExprId>,
    /// The members of the entry's class body, in source order: an entry with a
    /// body denotes an anonymous subclass of the enum, whose members are
    /// declared inside it.
    pub body: Vec<ItemId>,
    /// The `ENUM_ENTRY` syntax node.
    pub ast: FileAstId<EnumEntryNode>,
}

/// A type alias ([KLS
/// `declarations.html#type-alias`](https://kotlinlang.org/spec/declarations.html#type-alias)).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeAliasData {
    pub name: Name,
    pub modifiers: KotlinModifiers,
    pub annotations: Vec<KotlinAnnotationRef>,
    pub type_params: Vec<KotlinTypeParam>,
    /// The aliased type.
    pub target: ItemTypeRef,
    /// The `TYPE_ALIAS` syntax node.
    pub ast: FileAstId<TypeAliasNode>,
}

/// A declared type parameter of a classifier, function or property.
///
/// Kotlin's declaration-site variance has no analogue in the Java
/// [`TypeParam`](crate::java::item_tree::TypeParam) (Java's `? extends T` is
/// use-site only), so the Kotlin layer carries the variance itself.
#[derive(Debug, Clone, PartialEq)]
pub struct KotlinTypeParam {
    pub name: Name,
    /// The declared variance ([KLS
    /// `declarations.html#type-parameter-variance`](https://kotlinlang.org/spec/declarations.html#type-parameter-variance));
    /// `None` is invariance.
    pub variance: Option<KotlinVariance>,
    /// Whether the parameter is `reified` ([KLS
    /// `declarations.html#reified-type-parameters`](https://kotlinlang.org/spec/declarations.html#reified-type-parameters)):
    /// legal only on the `inline` function that declares it, and usable in a
    /// type test.
    pub reified: bool,
    /// The declared bounds. Kotlin writes an upper bound with `:` (`<T : Any>`)
    /// and several with `where`; a type parameter with none is bounded by
    /// `kotlin.Any?`, which the type layer adds.
    pub bounds: Vec<ItemTypeRef>,
    pub annotations: Vec<KotlinAnnotationRef>,
}

/// The kind of a lowered item, as the classifier kinds name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KotlinItemKind {
    ClassLike(KotlinClassKind),
    Constructor,
    Function,
    Accessor,
    Property,
    AnonymousInitializer,
    EnumEntry,
    TypeAlias,
}

impl KotlinItemData {
    /// The kind of the item.
    pub fn kind(&self) -> KotlinItemKind {
        match self {
            KotlinItemData::Class(data) => KotlinItemKind::ClassLike(data.kind),
            KotlinItemData::Constructor(_) => KotlinItemKind::Constructor,
            KotlinItemData::Function(_) => KotlinItemKind::Function,
            KotlinItemData::Accessor(_) => KotlinItemKind::Accessor,
            KotlinItemData::Property(_) => KotlinItemKind::Property,
            KotlinItemData::AnonymousInitializer(_) => KotlinItemKind::AnonymousInitializer,
            KotlinItemData::EnumEntry(_) => KotlinItemKind::EnumEntry,
            KotlinItemData::TypeAlias(_) => KotlinItemKind::TypeAlias,
        }
    }

    /// Whether the item is a class-like declaration.
    pub fn is_class(&self) -> bool {
        matches!(self, KotlinItemData::Class(_))
    }

    /// Whether the item is callable: a function, a constructor or an accessor.
    pub fn is_callable(&self) -> bool {
        matches!(
            self,
            KotlinItemData::Function(_)
                | KotlinItemData::Constructor(_)
                | KotlinItemData::Accessor(_)
        )
    }

    /// Whether the item is a function declaration (not a constructor or an
    /// accessor).
    pub fn is_function(&self) -> bool {
        matches!(self, KotlinItemData::Function(_))
    }

    /// Whether the item is a property.
    pub fn is_property(&self) -> bool {
        matches!(self, KotlinItemData::Property(_))
    }

    /// The declared name of the item, if it has one. An accessor and an `init`
    /// block declare no name; an enum entry, a property and a function do.
    pub fn name(&self) -> Option<&Name> {
        match self {
            KotlinItemData::Class(data) => Some(&data.name),
            KotlinItemData::Constructor(_) => None,
            KotlinItemData::Function(data) => Some(&data.name),
            KotlinItemData::Accessor(_) => None,
            KotlinItemData::Property(data) => Some(&data.name),
            KotlinItemData::AnonymousInitializer(_) => None,
            KotlinItemData::EnumEntry(data) => Some(&data.name),
            KotlinItemData::TypeAlias(data) => Some(&data.name),
        }
    }

    /// The declaration modifiers of the item, if it has any.
    pub fn modifiers(&self) -> Option<&KotlinModifiers> {
        match self {
            KotlinItemData::Class(data) => Some(&data.modifiers),
            KotlinItemData::Constructor(data) => Some(&data.modifiers),
            KotlinItemData::Function(data) => Some(&data.modifiers),
            KotlinItemData::Accessor(data) => Some(&data.modifiers),
            KotlinItemData::Property(data) => Some(&data.modifiers),
            KotlinItemData::TypeAlias(data) => Some(&data.modifiers),
            KotlinItemData::AnonymousInitializer(_) | KotlinItemData::EnumEntry(_) => None,
        }
    }

    /// The nested member items of the declaration: a classifier's body
    /// (including an enum entry's, whose anonymous subclass declares them).
    /// A property's accessors are *not* members in this sense — they are
    /// reachable through [`PropertyData::accessors`].
    pub fn body(&self) -> &[ItemId] {
        match self {
            KotlinItemData::Class(data) => &data.body,
            KotlinItemData::EnumEntry(data) => &data.body,
            _ => &[],
        }
    }

    /// The lowered body of the declaration, if it has one.
    pub fn body_id(&self) -> Option<BodyId> {
        match self {
            KotlinItemData::Constructor(data) => data.body,
            KotlinItemData::Function(data) => data.body,
            KotlinItemData::Accessor(data) => data.body,
            KotlinItemData::AnonymousInitializer(data) => data.body,
            _ => None,
        }
    }

    /// A display label used by [`crate::kotlin::pretty::pretty_print`].
    pub fn label(&self) -> &'static str {
        match self {
            KotlinItemData::Class(data) => data.kind.keyword(),
            KotlinItemData::Constructor(_) => "constructor",
            KotlinItemData::Function(_) => "fun",
            KotlinItemData::Accessor(data) if data.is_setter => "set",
            KotlinItemData::Accessor(_) => "get",
            KotlinItemData::Property(data) if data.is_var => "var",
            KotlinItemData::Property(_) => "val",
            KotlinItemData::AnonymousInitializer(_) => "init",
            KotlinItemData::EnumEntry(_) => "entry",
            KotlinItemData::TypeAlias(_) => "typealias",
        }
    }

    /// The syntax node the item anchored, as a raw node pointer.
    ///
    /// The item tree stores no offsets; this is how a range is resolved
    /// ([`crate::kotlin::ranges`]). `None` for an item whose anchor is a
    /// placeholder (never lowered from source).
    pub fn ast_id<'a>(
        &self,
        map: &'a AstIdMap,
    ) -> Option<&'a hir_expand::ast_id_map::SyntaxNodePtr> {
        macro_rules! id {
            ($id:expr) => {
                map.try_get($id)
            };
        }
        match self {
            KotlinItemData::Class(data) => id!(data.ast),
            KotlinItemData::Constructor(data) => id!(data.ast),
            KotlinItemData::Function(data) => id!(data.ast),
            KotlinItemData::Accessor(data) => id!(data.ast),
            KotlinItemData::Property(data) => id!(data.ast),
            KotlinItemData::AnonymousInitializer(data) => id!(data.ast),
            KotlinItemData::EnumEntry(data) => id!(data.ast),
            KotlinItemData::TypeAlias(data) => id!(data.ast),
        }
    }
}

/// The syntax-node pointer of `node` in `map`.
///
/// # Panics
/// If `node` is not part of the file's declaration skeleton — every
/// declaration and declaration part the lowering anchors is indexed by
/// [`crate::kotlin::lower`]'s contract with
/// [`hir_expand::ast_id_map::AstIdMap`].
pub(crate) fn ast_id_of<N, L>(map: &AstIdMap, node: &rowan::SyntaxNode<L>) -> FileAstId<N>
where
    L: rowan::Language,
    L::Kind: Into<rowan::SyntaxKind>,
{
    map.ast_id(&node_ptr(node))
        .unwrap_or_else(|| panic!("the lowering anchors only indexed nodes: {:?}", node.kind()))
}

/// The syntax-node id of `node` in `map`, or a placeholder for a node the id
/// map does not index.
///
/// A *part* of a declaration — a type reference, an annotation — may be
/// unindexed on an erroneous tree, and a placeholder id simply resolves to no
/// range instead of failing the lowering; an item's own anchor goes through
/// [`ast_id_of`] instead, where an unindexed node is a lowering bug.
pub(crate) fn ast_id_or_placeholder<N, L>(
    map: &AstIdMap,
    node: &rowan::SyntaxNode<L>,
) -> FileAstId<N>
where
    L: rowan::Language,
    L::Kind: Into<rowan::SyntaxKind>,
{
    map.ast_id(&node_ptr(node))
        .unwrap_or_else(FileAstId::placeholder)
}
