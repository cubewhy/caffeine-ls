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
//! probe oracle of kotlinc 2.4.20 (JRE 21.0.11): `listOf(1)` is a
//! `kotlin.collections.List<Int>` with no import (so `List` comes from
//! `kotlin.collections`), and `String`/`Exception`/`StringBuilder` resolve with
//! no import (so `kotlin` and `java.lang` are both default). The list is the
//! compiler's documented one
//! (<https://kotlinlang.org/docs/packages.html#default-imports>) checked
//! against it; a standard-library member that resolves through none of these
//! packages is a missing entry.
//!
//! `kotlin.reflect` is deliberately *not* a default import: `fun f(): KClass<*>`
//! with no import is `unresolved reference 'KClass'.` under kotlinc 2.4.20, and
//! the compiler's documented list has no entry for the package. `String::class`
//! still needs no import — the class literal is an *expression* whose type is
//! `kotlin.reflect.KClass`, and an expression's type needs no name in scope.

use hir::hir_def::kotlin::item_tree::{
    KotlinClassKind, KotlinItemData, KotlinItemTree, KotlinTypeParam,
};
use hir_expand::name::Name;
use vfs::FileId;

use crate::jvm::db::TyDatabase;
use crate::ty::{Ty, TypeVarScope};

/// The `.`-prefixes of a fully qualified name, most specific first: a
/// declaration whose FQN is `fqn` lives in a file whose package is one of
/// these.
fn package_prefixes(fqn: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut end = fqn.len();
    while let Some(dot) = fqn[..end].rfind('.') {
        out.push(fqn[..dot].to_owned());
        end = dot;
    }
    out.push(String::new());
    out
}

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
    /// The declaration the resolved name is written in: what the enclosing
    /// chain — of nested classifiers and of type parameters — is walked from
    /// ([`KotlinResolver::local_declaration`]).
    item: hir_expand::ids::ItemId,
    scope: hir::ResolutionScope,
    /// The type parameters in scope at the resolved item, innermost last.
    type_params: Vec<TypeParamScope>,
    /// The type parameters whose *bounds* are being resolved, so a re-entrant
    /// bound (`T : Comparable<T>`) yields the variable without bounds and
    /// interning terminates — the guard the Java layer keeps in
    /// [`crate::java::resolve`]'s `resolve_type_ref_impl`, here as state
    /// because name resolution is `&self`.
    resolving: std::cell::RefCell<Vec<Name>>,
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
            item,
            scope,
            type_params,
            resolving: std::cell::RefCell::new(Vec::new()),
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
    ///
    /// The variable carries its declared bounds ([KLS
    /// `type-system.html#type-parameters`](https://kotlinlang.org/spec/type-system.html#type-parameters)):
    /// `T : U` makes `T` a subtype of `U`, and the constraint solver reads the
    /// bounds off the type. A parameter without a bound has none recorded — the
    /// implicit `kotlin.Any?` is what an empty bound list means.
    pub fn type_var(&self, item: hir_expand::ids::ItemId, param: &TypeParamScope) -> Ty {
        let scope = self.param_scope(item, &param.name);
        let bounds = self.param_bounds(&param.name, &param.param);
        Ty::type_var(self.db, scope, bounds)
    }

    /// The [`TypeVarScope`] a type parameter of `item` is identified by: a
    /// classifier's parameter is a [`TypeVarScope::Class`], a function's, a
    /// property's and a type alias's a [`TypeVarScope::Method`] ([KLS
    /// `type-system.html#type-parameters`](https://kotlinlang.org/spec/type-system.html#type-parameters)
    /// scopes the variable to its declaring parameter, and the two kinds are
    /// distinct variables).
    fn param_scope(&self, item: hir_expand::ids::ItemId, name: &Name) -> TypeVarScope {
        let scope = |name: &Name| TypeVarScope::Method {
            file: self.file,
            item,
            name: name.clone(),
        };
        match self.tree.data(item) {
            KotlinItemData::Class(_) => TypeVarScope::Class {
                file: self.file,
                item,
                name: name.clone(),
            },
            _ => scope(name),
        }
    }

    /// The resolved bounds of a type parameter, with the recursion guard of
    /// [`Self::resolving`]: a bound that names the parameter itself
    /// (`T : Comparable<T>`) is resolved with an *empty* bound list for the
    /// re-entrant occurrence, so interning terminates.
    fn param_bounds(&self, name: &Name, param: &KotlinTypeParam) -> Vec<Ty> {
        if self
            .resolving
            .borrow()
            .iter()
            .any(|resolving| resolving == name)
        {
            return Vec::new();
        }
        self.resolving.borrow_mut().push(name.clone());
        let bounds = param
            .bounds
            .iter()
            .map(|bound| crate::kotlin::ty::ty_from_type_ref(self.db, self, &bound.ty))
            .collect();
        self.resolving.borrow_mut().pop();
        bounds
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
        if let Some(param) = self.type_param(simple) {
            // A type parameter is not a classifier and takes no type
            // arguments ([KLS
            // `type-system.html#classifier-types`](https://kotlinlang.org/spec/type-system.html#classifier-types)):
            // `T<Int>` is an error, not a classifier lookup of `T`.
            return if args.is_empty() {
                self.type_var(param.declaring, &param)
            } else {
                Ty::error(self.db)
            };
        }
        // A type alias is expanded, not named ([KLS
        // `declarations.html#type-alias`](https://kotlinlang.org/spec/declarations.html#type-alias)):
        // `Handler<Int>` *is* the `(Int) -> Unit` the alias stands for.
        if let Some(alias) = self.type_alias(simple) {
            return self.expand_alias(alias, args);
        }
        // A local classifier, before any *named* one: a local declaration
        // shadows a declaration of the same name from an outer scope ([KLS
        // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
        if let Some(local) = self.local_class_reference(simple, args.clone()) {
            return local;
        }
        match self.class_fqn(text) {
            Some(fqn) => Ty::reference(self.db, fqn, args),
            None => Ty::error(self.db),
        }
    }

    /// The `typealias` declaration this file declares under `simple`, if any.
    fn type_alias(&self, simple: &str) -> Option<hir_expand::ids::ItemId> {
        let item = self.local_declaration(simple)?;
        matches!(self.tree.data(item), KotlinItemData::TypeAlias(_)).then_some(item)
    }

    /// The type of the *local* classifier this file declares under `simple` and
    /// that is in scope at the resolved item, or `None` when the name is not a
    /// local classifier's.
    ///
    /// A local class, a local `object` and an object literal's anonymous class
    /// have no canonical name ([KLS
    /// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)),
    /// so the type is identified by the declaration itself — the identity
    /// [`TyKind::Reference`]'s `local` carries — and never by
    /// [`Self::local_fqn`], which would name a class the compiler does not emit
    /// under that name.
    pub fn local_class_reference(&self, simple: &str, args: Vec<Ty>) -> Option<Ty> {
        let item = self.local_declaration(simple)?;
        if !self.tree.is_local_type(item) {
            return None;
        }
        let name = self.tree.data(item).name()?;
        let class = hir::SourceClass {
            file: self.file,
            item,
        };
        Some(Ty::local_reference(self.db, class, name.clone(), args))
    }

    /// The expansion of a type-alias reference ([KLS
    /// `declarations.html#type-alias`](https://kotlinlang.org/spec/declarations.html#type-alias)):
    /// the aliased type, with the alias's own parameters substituted by the
    /// written arguments — `typealias Handler<T> = (T) -> Unit` used as
    /// `Handler<Int>` is `(Int) -> Unit`. The alias keeps no identity of its
    /// own: the compiler substitutes into its target, which is why a
    /// reference to an alias is never a distinct type.
    fn expand_alias(&self, item: hir_expand::ids::ItemId, args: Vec<Ty>) -> Ty {
        let KotlinItemData::TypeAlias(data) = self.tree.data(item) else {
            return Ty::error(self.db);
        };
        let alias = KotlinResolver::for_item(self.db, self.file, self.tree, item);
        let mut binding = rustc_hash::FxHashMap::default();
        for (param, arg) in data.type_params.iter().zip(args) {
            binding.insert(alias.param_scope(item, &param.name), arg);
        }
        let target = crate::kotlin::ty::ty_from_type_ref(self.db, &alias, &data.target.ty);
        target.substitute(self.db, &binding)
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
        if segments.len() == 1
            && let Some(item) = self.local_declaration(simple)
        {
            // A local declaration has no canonical name — the caller that needs
            // its type asks [`Self::local_class_reference`] — and it shadows any
            // named declaration of the same name.
            if self.tree.is_local_type(item) {
                return None;
            }
            return self.local_fqn(simple);
        }
        for candidate in self.candidates(name) {
            if let Some(fqn) = self.fqn_resolve(&candidate) {
                return Some(fqn);
            }
        }
        None
    }

    /// The *top-level* declaration a written name denotes in this file's scope:
    /// a function, a property or a type alias of the file's own package, of an
    /// explicit import, or of a star import ([KLS
    /// `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
    ///
    /// The workspace's symbol index is keyed by fully qualified name, so the
    /// candidates are the ones a classifier lookup uses; a *library*'s
    /// top-level declarations live in the `*Kt` facade classes of the
    /// classpath, which this does not consult — a recorded gap, and the reason
    /// a standard-library function such as `listOf` is not yet a candidate.
    pub fn source_declaration(&self, name: &Name) -> Option<(FileId, hir_expand::ids::ItemId)> {
        let hir::ResolutionScope::SourceSet(source_set) = &self.scope else {
            return None;
        };
        for candidate in self.candidates(name.as_str()) {
            for package in package_prefixes(&candidate) {
                let symbols = hir::source_set_fqn_symbols(
                    self.db,
                    source_set.clone(),
                    &Name::new(&package),
                    &Name::new(&candidate),
                );
                for reference in symbols.iter() {
                    if matches!(
                        reference.symbol.kind,
                        hir::SourceSymbolKind::Function
                            | hir::SourceSymbolKind::Property
                            | hir::SourceSymbolKind::TypeAlias
                    ) {
                        return Some((reference.file, reference.symbol.item));
                    }
                }
            }
        }
        None
    }

    /// The candidate fully qualified names a written name may denote, in scope
    /// order: a dotted name as written, an import binding, the file's own
    /// package, the star imports, then the default imports.
    fn candidates(&self, name: &str) -> Vec<String> {
        let segments: Vec<&str> = name.split('.').collect();
        let simple = segments[0];
        let mut candidates: Vec<String> = Vec::new();

        if segments.len() == 1 {
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

        candidates
    }

    /// The canonical name of `fqn`, by classpath order: the project's source
    /// symbols first (through [`hir::fqn_resolve`]), which also finds a library
    /// class.
    fn fqn_resolve(&self, fqn: &str) -> Option<Name> {
        let resolved = hir::fqn_resolve(self.db, &self.scope, fqn)?;
        Some(match &resolved {
            hir::Resolved::Source(class) => hir::source_class_fqn(self.db, class.file, class.item)?,
            hir::Resolved::Library(_) => resolved.fqn(self.db).as_name().clone(),
            // A facade is named by the compiler, not by a declaration.
            hir::Resolved::Facade { fqn, .. } => fqn.clone(),
        })
    }

    /// The declaration of this file that `simple` names, if any: the *local*
    /// declarations of the enclosing bodies first, innermost body outwards, then
    /// the *members of the enclosing classifiers*, innermost outwards, then the
    /// file's own declarations ([KLS
    /// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)
    /// scopes a local declaration to the body that declares it, which is why it
    /// shadows a class member of the same name, and
    /// [`#nested-and-inner-classes`](https://kotlinlang.org/spec/declarations.html#nested-and-inner-classes)
    /// scopes a nested classifier to its enclosing declaration, which is why that
    /// shadows a file-level declaration).
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
        // The *local* declarations of the enclosing bodies, innermost body
        // first: a local class, `object`, function or type alias is in scope in
        // the body that declares it, and shadows every outer declaration of the
        // same name ([KLS
        // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
        // The item tree records the declaring body as the parent, so the
        // visibility question is ancestry.
        let mut current = Some(self.item);
        while let Some(id) = current {
            if let Some(found) = self.tree.local_types_of(id).find(|&candidate| {
                self.tree.data(candidate).name().map(|name| name.as_str()) == Some(simple)
            }) {
                return Some(found);
            }
            current = self.tree.parent_of(id);
        }
        // The enclosing classifiers, innermost first: their members are in
        // scope where the name is written.
        let mut current = Some(self.item);
        while let Some(id) = current {
            if let Some(declaration) = self.tree.data(id).body().iter().find(|&&member| {
                self.tree.data(member).name().map(|name| name.as_str()) == Some(simple)
            }) {
                return Some(*declaration);
            }
            current = self.tree.parent_of(id);
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
            let mut out = vec![Ty::reference(self.db, any, Vec::new())];
            // An `enum class` has the implicit supertype `kotlin.Enum<E>`,
            // `E` being the enum class itself ([KLS
            // `built-in-types-and-their-semantics.html#enum-types`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html#enum-types)):
            // it is what gives the entries `name` and `ordinal`, and it is the
            // type a Java `Enum<?>` position accepts.
            if class.kind == KotlinClassKind::Enum {
                let self_ty = match self.local_fqn(class.name.as_str()) {
                    Some(fqn) => Ty::reference(self.db, fqn, Vec::new()),
                    None => Ty::reference(self.db, class.name.clone(), Vec::new()),
                };
                let enumeration = self
                    .class_fqn("Enum")
                    .unwrap_or_else(|| Name::new("kotlin.Enum"));
                out.push(Ty::reference(self.db, enumeration, vec![self_ty]));
            }
            return out;
        }
        class
            .super_types
            .iter()
            .map(|super_type| crate::kotlin::ty::ty_from_type_ref(self.db, self, &super_type.ty.ty))
            .collect()
    }
}
