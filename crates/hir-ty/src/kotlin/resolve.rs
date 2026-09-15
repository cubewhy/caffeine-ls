//! Kotlin name resolution for the type layer.
//!
//! A Kotlin type name is resolved against the scopes the language defines
//! ([KLS
//! `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers),
//! [KLS `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)):
//!
//! 1. a type parameter declared by the declaration the name is written in, or
//!    by an enclosing classifier — the innermost declaration wins;
//! 2. a classifier the *same file* declares (a top-level or nested one) and the
//!    `typealias`es of the file;
//! 3. an explicit import of the file, by its alias or its last segment, then a
//!    classifier of the file's own package;
//! 4. a star import's members;
//! 5. the *default imports* (`kotlin.*`, `kotlin.collections.*`, ... and the
//!    JVM ones), which are what make `String`, `Int` and `List` resolve in a
//!    file that imports nothing.
//!
//! Classpath lookup goes through [`hir::fqn_resolve`], so a candidate that is a
//! library class resolves to it in classpath order exactly as it does for Java
//! — there is no second classpath walk here.
//!
//! # The default-import list
//!
//! KLS does not enumerate the default imports (the *Kotlin/Core* specification
//! has no section for them), so the list below is pinned empirically with the
//! probe oracle of kotlinc 2.4.20 (JRE 25): `listOf(1)` is a
//! `kotlin.collections.List<Int>` with no import (so `List` comes from
//! `kotlin.collections`), `String`/`Exception`/`StringBuilder` resolve with no
//! import (so `kotlin` and `java.lang` are both default), and `String::class`
//! needs no import for `kotlin.reflect`. The list is the compiler's documented
//! one (<https://kotlinlang.org/docs/packages.html#default-imports>) checked
//! against it; a standard-library member that resolves through none of these
//! packages is a missing entry.

use hir::hir_def::kotlin::item_tree::{
    KotlinClassKind, KotlinItemData, KotlinItemTree, KotlinTypeParam,
};
use hir_expand::name::Name;
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::ty::{Ty, TypeVarScope};

/// The packages every Kotlin file imports implicitly
/// (<https://kotlinlang.org/docs/packages.html#default-imports>).
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

/// The Kotlin file scope a type name is resolved in.
pub struct KotlinResolver<'a> {
    db: &'a dyn TyDatabase,
    file: FileId,
    tree: &'a KotlinItemTree,
    scope: hir::ResolutionScope,
    /// The type parameters in scope at the resolved item, innermost last.
    type_params: Vec<TypeParamScope>,
}

/// One declared type parameter, with the declaration that declares it.
#[derive(Clone)]
pub struct TypeParamScope {
    /// The declaration id, so two declarations' same-named parameters are
    /// distinct type variables ([KLS
    /// `declarations.html#declarations-with-type-parameters`](https://kotlinlang.org/spec/declarations.html#declarations-with-type-parameters)).
    /// The declaration the parameter belongs to; two same-named parameters of
    /// different declarations are distinct type variables.
    pub declaring: hir_expand::ids::ItemId,
    pub name: Name,
    pub param: KotlinTypeParam,
}

impl<'a> KotlinResolver<'a> {
    /// The resolver of `item` in `file`: the file's imports, package and
    /// default imports plus the type parameters the item can see (its own, and
    /// those of every enclosing classifier).
    pub fn for_item(
        db: &'a dyn TyDatabase,
        file: FileId,
        tree: &'a KotlinItemTree,
        item: hir_expand::ids::ItemId,
    ) -> KotlinResolver<'a> {
        let scope = match hir::source_set_for_file(db, file) {
            Some(source_set) => hir::ResolutionScope::SourceSet(source_set),
            None => hir::ResolutionScope::JdkBuiltins,
        };
        let mut type_params = Vec::new();
        // The item's own parameters, then the enclosing classifiers' — the
        // caller of `type_param` looks innermost-*last*, so the chain is built
        // outermost first.
        let mut chain = vec![item];
        let mut current = item;
        while let Some(parent) = tree.parent_of(current) {
            chain.push(parent);
            current = parent;
        }
        for &id in chain.iter().rev() {
            type_params.extend(Self::declared_params(tree, id));
        }
        KotlinResolver {
            db,
            file,
            tree,
            scope,
            type_params,
        }
    }

    /// The file's scope, for a caller that needs the raw [`hir::ResolutionScope`].
    pub fn scope(&self) -> &hir::ResolutionScope {
        &self.scope
    }

    /// The type parameters a declaration declares, as scope entries keyed by
    /// the declaration itself ([`TypeVarScope::Class`] for a classifier,
    /// [`TypeVarScope::Method`] for a function, property or type alias — the
    /// scope the Java layer's `T#1`/`T#2` distinction uses).
    fn declared_params(
        tree: &KotlinItemTree,
        item: hir_expand::ids::ItemId,
    ) -> Vec<TypeParamScope> {
        let params = match tree.data(item) {
            KotlinItemData::Class(data) => &data.type_params,
            KotlinItemData::Function(data) => &data.type_params,
            KotlinItemData::Property(data) => &data.type_params,
            KotlinItemData::TypeAlias(data) => &data.type_params,
            _ => return Vec::new(),
        };
        params
            .iter()
            .map(|param| TypeParamScope {
                declaring: item,
                name: param.name.clone(),
                param: param.clone(),
            })
            .collect()
    }

    /// The type parameters the resolved item can see, in scope order
    /// (outermost first).
    pub fn declared_type_params(&self) -> Vec<TypeParamScope> {
        self.type_params.clone()
    }

    /// The type parameter `name` names in scope — the innermost declaration's
    /// — as a type variable.
    pub fn type_param(&self, name: &str) -> Option<TypeParamScope> {
        self.type_params
            .iter()
            .rev()
            .find(|param| param.name.as_str() == name)
            .cloned()
    }

    /// The type variable of a type parameter — declared by `item` itself, so
    /// the scope carries both the file and the declaring item.
    pub fn type_var(&self, item: hir_expand::ids::ItemId, param: &TypeParamScope) -> Ty {
        let scope = TypeVarScope::Class {
            file: self.file,
            item,
            name: param.name.clone(),
        };
        Ty::type_var(self.db, scope, Vec::new())
    }

    /// The type the written reference `name` denotes, or [`Ty::error`] when it
    /// resolves to nothing.
    ///
    /// A *type parameter* wins over every declaration of the same name; a
    /// classifier resolves through the file's scopes, which end in the default
    /// imports.
    pub fn resolve_reference(&self, name: &Name, args: Vec<Ty>) -> Ty {
        let text = name.as_str();
        let simple = text.rsplit('.').next().unwrap_or(text);
        if args.is_empty()
            && let Some(param) = self.type_param(simple)
        {
            return self.type_var(param.declaring, &param);
        }
        match self.class_fqn(text) {
            Some(fqn) => Ty::reference(self.db, fqn, args),
            None => Ty::error(self.db),
        }
    }

    /// The canonical fully qualified name the written reference `name` denotes.
    ///
    /// A dotted name is tried as written first — Kotlin allows a fully
    /// qualified reference (`kotlin.collections.List`) — then with its first
    /// segment read as an import binding or as a package the file's own
    /// package is a prefix of.
    pub fn class_fqn(&self, name: &str) -> Option<Name> {
        let segments: Vec<&str> = name.split('.').collect();
        let simple = segments[0];
        let mut candidates: Vec<String> = Vec::new();

        if segments.len() == 1 {
            // A `typealias` or a top-level/nested declaration of this file.
            if self.local_declaration(simple).is_some() {
                return self.local_fqn(simple);
            }
            // An explicit import (by alias or by its last segment).
            for import in &self.tree.imports {
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
            for import in &self.tree.imports {
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
        let in_package = |fqn: &str| match &self.tree.package {
            Some(package) => format!("{package}.{fqn}"),
            None => fqn.to_owned(),
        };
        candidates.push(in_package(name));
        for import in self.tree.imports.iter().filter(|import| import.is_asterisk) {
            candidates.push(format!("{}.{}", import.path, name));
        }
        for package in DEFAULT_IMPORTS {
            candidates.push(format!("{package}.{name}"));
        }

        for candidate in candidates {
            if let Some(fqn) = self.fqn_resolve(&candidate) {
                return Some(fqn);
            }
        }
        None
    }

    /// The canonical name of `fqn`, by classpath order: the project's source
    /// symbols first (through [`hir::fqn_resolve`]), which also finds a library
    /// class.
    fn fqn_resolve(&self, fqn: &str) -> Option<Name> {
        let resolved = hir::fqn_resolve(self.db, &self.scope, fqn)?;
        Some(match &resolved {
            hir::Resolved::Source(class) => hir::source_class_fqn(self.db, class.file, class.item)?,
            hir::Resolved::Library(_) => resolved.fqn(self.db).as_name().clone(),
        })
    }

    /// The declaration of this file that `simple` names, if any.
    fn local_declaration(&self, simple: &str) -> Option<hir_expand::ids::ItemId> {
        fn walk(
            tree: &KotlinItemTree,
            item: hir_expand::ids::ItemId,
            simple: &str,
        ) -> Option<hir_expand::ids::ItemId> {
            if tree.data(item).name().map(|name| name.as_str()) == Some(simple) {
                return Some(item);
            }
            for &child in tree.data(item).body() {
                if let Some(found) = walk(tree, child, simple) {
                    return Some(found);
                }
            }
            None
        }
        for &top in &self.tree.top {
            if let Some(found) = walk(self.tree, top, simple) {
                return Some(found);
            }
        }
        None
    }

    /// The canonical name of a declaration of this file: its package, then the
    /// chain of enclosing classifiers, then its own name.
    fn local_fqn(&self, simple: &str) -> Option<Name> {
        let item = self.local_declaration(simple)?;
        let mut path = vec![simple.to_owned()];
        let mut current = item;
        while let Some(parent) = self.tree.parent_of(current) {
            if let Some(name) = self.tree.data(parent).name() {
                path.push(name.as_str().to_owned());
            }
            current = parent;
        }
        path.reverse();
        let qualified = path.join(".");
        Some(match &self.tree.package {
            Some(package) => Name::new(&format!("{package}.{qualified}")),
            None => Name::new(&qualified),
        })
    }

    /// The declared supertypes of a classifier: its `super_types` resolved, or
    /// `kotlin.Any` when it declares none ([KLS
    /// `declarations.html#class-declaration`](https://kotlinlang.org/spec/declarations.html#class-declaration):
    /// a class without a supertype list extends `Any`).
    pub fn super_types(&self, class: &hir::hir_def::kotlin::item_tree::ClassData) -> Vec<Ty> {
        if class.super_types.is_empty() {
            let any = self
                .class_fqn("Any")
                .unwrap_or_else(|| Name::new("kotlin.Any"));
            return vec![Ty::reference(self.db, any, Vec::new())];
        }
        class
            .super_types
            .iter()
            .map(|super_type| crate::kotlin::ty::ty_from_type_ref(self.db, self, &super_type.ty.ty))
            .collect()
    }

    /// Whether the classifier is an interface (or an annotation class), whose
    /// supertypes are all interfaces.
    pub fn is_interface(class: &hir::hir_def::kotlin::item_tree::ClassData) -> bool {
        matches!(
            class.kind,
            KotlinClassKind::Interface | KotlinClassKind::Annotation
        )
    }
}
