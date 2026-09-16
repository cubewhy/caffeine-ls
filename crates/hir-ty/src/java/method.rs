//! Java method resolution: the applicability and specificity phases of
//! [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2)
//! over the shared JVM member set
//! ([`crate::jvm::member_set`](crate::jvm::member_set)).
//!
//! [`pick_method`] runs the strict
//! ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
//! loose ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3))
//! and variable-arity
//! ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4))
//! applicability phases and chooses the most specific applicable method
//! ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5));
//! the member set it picks from is the JVM layer's, restricted to the
//! candidates the invocation mode allows
//! ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
//! [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
//! and to those accessible at the invocation site
//! ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)) — the
//! [`InvocationContext`] [`access_context`] derives from a call site.
//!
//! A generic method's invocation type ([§15.12.2.6]) is computed by the
//! method invocation type inference of [JLS §18.5.2] ([`crate::java::inference`]):
//! the method's own type parameters become fresh inference variables whose
//! constraints are solved against the actual argument types, so
//! `Collections.identity("s")` is applicable as `identity(String)`, not as the
//! erasure `identity(Object)`. The returned [`MethodData`] is the instantiated
//! invocation: its parameters and return type carry the inferred type
//! arguments. Inference-derived bounds and captured wildcard types
//! ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
//! are modelled; target-type compatibility ([§18.5.2.4]) is incorporated
//! through the `target` argument of [`pick_method`].
//!
//! This module also owns the Java layer's *source* enumerations — the members a
//! source class or a classfile declares ([`source_class_methods`],
//! [`library_class_methods`](crate::jvm::member_set) is the classfile side and
//! lives with the member set) — and the source-side constructors of the shared
//! vocabulary ([`ClassKey::of`], [`ClassKey::of_resolved`]).

use rustc_hash::{FxHashMap, FxHashSet};
use vfs::FileId;

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree, TypeParam};
use hir_expand::name::Name;

use crate::{
    java::db::{access_context_key_query, item_ty_query, method_params_query},
    java::inference::{Constraint, Inference, InvocationPhase},
    java::resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    java::subtyping::is_subtype,
    java::ty::{Ty, TyKind, TypeVarScope, boxed_type},
    jvm::db::{ItemKey, TyDatabase},
    jvm::member::{Access, ClassKey, FieldData, MethodData, MethodTypeParam},
    jvm::member_set::{InvocationContext, InvocationMode},
    jvm::member_set::{member_set, single_abstract_method, source_top_level},
};

/// The access-control context ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of a source call site inside the method or field `item` of `file`: the
/// canonical fully qualified name ([§6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7))
/// of the nearest enclosing class or interface ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1))
/// and the compilation unit's package ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// with the unnamed package ([§7.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.2))
/// as `""`. A virtual invocation
/// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// is assumed; the caller derives the per-call-site mode with
/// [`InvocationContext::with_mode`]. Items outside any class yield `None` for
// The constructors of the shared class key ([`crate::jvm::member::ClassKey`]):
// they resolve a *source* declaration, so they belong to the layer that owns
// source resolution rather than to the vocabulary the type layer hands around.
impl ClassKey {
    /// The key of the class-like declaration `item` of `file`: its canonical
    /// fully qualified name, or the declaration when it has none.
    pub fn of(tree: &ItemTree, file: FileId, item: ItemId) -> ClassKey {
        match crate::java::resolve::canonical_class_fqn(tree, item) {
            Some(fqn) => ClassKey::Named(fqn),
            None => ClassKey::Local(hir::SourceClass { file, item }),
        }
    }

    /// The key of a resolved class: a source class is keyed by its declaration
    /// (which keeps a *local* one identified), a library class by its binary
    /// name ([JVMS §4.2]).
    pub fn of_resolved(db: &dyn TyDatabase, resolved: &hir::Resolved) -> ClassKey {
        match resolved {
            // The name is read from the class's *source*, never from the Java
            // item tree: a Kotlin class has none
            // ([`crate::java::resolve::source_class_fqn_of`]).
            hir::Resolved::Source(class) => {
                match crate::java::resolve::source_class_fqn_of(db, *class) {
                    Some(fqn) => ClassKey::Named(fqn),
                    None => ClassKey::Local(*class),
                }
            }
            // A facade is keyed by the name the compiler gives it.
            hir::Resolved::Facade { fqn, .. } => ClassKey::Named(fqn.clone()),
            hir::Resolved::Library(_) => ClassKey::Named(resolved.fqn(db).as_name().clone()),
        }
    }

    /// The simple name of the class ([§6.7]): the last `.`-separated and
    /// `$`-separated segment of a canonical name, or the declaration's own
    /// name for a local one. This is what javac and the IDE render.
    pub fn simple_name(&self, db: &dyn TyDatabase) -> Name {
        match self {
            ClassKey::Named(fqn) => Name::new(fqn.simple_name()),
            ClassKey::Local(class) => {
                let tree = hir::java_item_tree(db, class.file);
                tree.data(class.item)
                    .name()
                    .cloned()
                    .unwrap_or_else(|| Name::new(""))
            }
        }
    }

    /// The key of the class a reference type denotes: its declaration for a
    /// *local* one ([§6.7]), else the class of its canonical name. The
    /// identity travels on the type itself, so no name lookup is needed.
    pub fn of_ty(db: &dyn TyDatabase, ty: &Ty) -> Option<ClassKey> {
        match ty.kind(db) {
            // A local *declaration* — which has no canonical name ([§6.7]) — is
            // identified by itself; a named one, Kotlin source included, by its
            // canonical name ([`crate::java::resolve::source_class_fqn_of`]).
            TyKind::Reference {
                local: Some(class), ..
            } => Some(
                match crate::java::resolve::source_class_fqn_of(db, *class) {
                    Some(fqn) => ClassKey::Named(fqn),
                    None => ClassKey::Local(*class),
                },
            ),
            TyKind::Reference {
                name, local: None, ..
            } => Some(ClassKey::Named(name.clone())),
            _ => None,
        }
    }

    /// The name a diagnostic about the class renders ([§6.7]): its canonical
    /// name, or the *simple* name of a declaration that has none — a local
    /// declaration, which javac and the IDE render by its simple name.
    pub fn display_name(&self, db: &dyn TyDatabase) -> Name {
        match self {
            ClassKey::Named(fqn) => fqn.clone(),
            ClassKey::Local(_) => self.simple_name(db),
        }
    }

    /// The declaration a *source* class names, `None` for a classpath class.
    pub fn source(&self) -> Option<hir::SourceClass> {
        match self {
            ClassKey::Named(_) => None,
            ClassKey::Local(class) => Some(*class),
        }
    }

    /// The canonical fully qualified name of the class, when it has one.
    pub fn as_fqn(&self) -> Option<&Name> {
        match self {
            ClassKey::Named(fqn) => Some(fqn),
            ClassKey::Local(_) => None,
        }
    }

    /// The class as a type, for a subtype or receiver comparison: a reference
    /// to the named class, or to the local declaration ([§6.7]).
    pub fn as_ty(&self, db: &dyn TyDatabase, args: Vec<Ty>) -> Ty {
        match self {
            ClassKey::Named(fqn) => Ty::reference(db, fqn.as_str(), args),
            ClassKey::Local(class) => Ty::local_reference(db, *class, self.simple_name(db), args),
        }
    }

    /// The *top-level* class the declaration belongs to ([JLS §6.6.1]: a
    /// private member is accessible throughout the body of its top-level
    /// class, nested and local declarations included): the outermost enclosing
    /// class-like declaration's canonical name. A local declaration's top
    /// level is the class enclosing it.
    pub fn top_level(&self, db: &dyn TyDatabase, package: Option<&str>) -> Option<Name> {
        match self {
            // A canonical name nests with dots, so the top level is the
            // package plus the first type name ([§6.7]).
            ClassKey::Named(fqn) => Some(Name::new(&source_top_level(package, fqn.as_str()))),
            ClassKey::Local(class) => {
                let tree = hir::java_item_tree(db, class.file);
                crate::java::resolve::enclosing_type_chain(&tree, class.item)
                    .last()
                    .cloned()
            }
        }
    }
}

/// the enclosing class.
pub fn access_context(db: &dyn TyDatabase, file: FileId, item: ItemId) -> InvocationContext {
    let key = access_context_key_query(db, ItemKey::new(db, file, item));
    InvocationContext::from_key(db, key)
}

/// §15.9.2.1: whether the class named by `ty` (a resolved reference) declares
/// its own type parameters. `Some(true)`/`Some(false)` when the class
/// resolves; `None` when it does not (the caller then stays silent — the
/// missing type reports itself). A nested class's own parameters are counted
/// (`Outer<String>.Inner` declares none), and a non-generic class of a
/// parameterized outer is not generic either.
pub fn class_declares_type_params(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<bool> {
    let (fqn, _) = ty.as_reference(db)?;
    match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Library(class)) => {
            hir::class_generic_info(db, &hir::Resolved::Library(class))
                .map(|info| !info.type_params.is_empty())
        }
        // A Kotlin file's facade declares none.
        Some(hir::Resolved::Facade { .. }) => Some(false),
        Some(hir::Resolved::Source(source)) => {
            let tree = hir::java_item_tree(db, source.file);
            match crate::java::resolve::item_data(&tree, source.item) {
                Some(hir_def::java::item_tree::ItemData::Class(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Interface(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Record(d)) => {
                    Some(!d.type_params.is_empty())
                }
                Some(hir_def::java::item_tree::ItemData::Enum(_)) => Some(false),
                _ => None,
            }
        }
        None => None,
    }
}

/// The methods of a source class, resolved against the file's own scope and
/// instantiated with `args`. Class type parameters are bound to `args`; method
/// type parameters are kept — [`pick_method`] instantiates them.
pub(crate) fn source_class_methods(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let tree = hir::java_item_tree(db, source.file);
    let Some(class_data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let declared: &[TypeParam] = match class_data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !declared.is_empty();
    // §4.10.2: the receiver's arguments instantiate the declaring class's
    // *own* parameters. The binding is keyed by each parameter's declaring
    // scope ([§4.4], [§6.3]), and a method type parameter is declared by the
    // *method* ([§8.4.4]) — a distinct scope — so a method's own variable can
    // never be captured by the class binding ([§6.4.1], [§4.4]
    // capture-avoidance), with no name-level exclusion needed.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::source_class_binding(source.file, source.item, declared, &args);
    let scope = scope_for_file(db, source.file);
    let resolver = Resolver::for_item(db, source.file, &tree, source.item);
    // §6.7: the receiver's declaration — its canonical name, or the
    // declaration itself when it has none (a local class-like declaration,
    // [JLS §14.3]).
    let class_key = ClassKey::of(&tree, source.file, source.item);
    let simple = class_key.simple_name(db);
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // §6.6.1: a private member's accessibility is scoped by the *top-level*
    // class — the outermost enclosing class-like declaration, which for a
    // local declaration is the class around it.
    let declaring_top_level = class_key
        .top_level(db, package.as_deref())
        .map(|name| name.as_str().to_owned());
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));

    let mut out = Vec::new();
    for item in class_data.body().to_vec() {
        let Some(ItemData::Method(method)) = item_data(&tree, item) else {
            continue;
        };
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::java::decl_check`]); no method can be named "".
        if !name.is_empty() && method.name.as_str() != name {
            continue;
        }
        let method_resolver = Resolver::for_item(db, source.file, &tree, item);
        let type_params = method
            .sig
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                scope: TypeVarScope::Method {
                    file: source.file,
                    item,
                    name: tp.name.clone(),
                },
                bounds: tp
                    .bounds
                    .iter()
                    .map(|bound| resolve_type_ref(db, &scope, &method_resolver, bound))
                    .map(|bound| bound.substitute(db, &binding))
                    .collect(),
            })
            .collect();
        let key = ItemKey::new(db, source.file, item);
        let instantiate = |ty: &Ty| ty.substitute(db, &binding);
        // JLS 4.8: the *instance* members of a raw type have erased
        // signatures. A static member does not depend on the receiver's
        // type arguments at all, so its own generics stay intact.
        let erase = |ty: Ty| {
            if is_raw && !method.modifiers.is_static() {
                ty.erasure(db)
            } else {
                ty
            }
        };
        let is_compact_ctor = method.is_compact_constructor();
        // JLS §4.8 with §5.1.9: an instance member reached through a raw
        // receiver has an *erased* signature, so an invocation of it is
        // unchecked — javac's `unchecked call to … as a member of the raw
        // type …`. The erasure is observable when the declared formal types
        // mention a type variable (the class's or a method's own) or the
        // method declares its own type parameters; a member whose formals are
        // ground (`void m(String)`) stays checked.
        let raw_erased = is_raw
            && !method.modifiers.is_static()
            && (!method.sig.type_params.is_empty()
                || method_params_query(db, key)
                    .iter()
                    .map(instantiate)
                    .any(|param| param.contains_type_var(db)));
        // §8.10.4: a record *compact* constructor (`record R(int x) { R { …
        // } }`) is declared without a formal parameter list — its signature
        // is the record's component list. The canonical constructor is the
        // component list as parameters (same types, in order); a varargs
        // component is the array type ([§8.4.1]). Synthesize that canonical
        // parameter list here so `new R(1)` resolves against the real
        // constructor, the later `declares_canonical` check sees the compact
        // form and suppresses the duplicate implicit canonical, and the
        // `declares_ctor` check counts it (no default constructor either).
        let (varargs, params): (bool, Vec<Ty>) = if is_compact_ctor {
            let mut canonical: Vec<Ty> = Vec::new();
            if let ItemData::Record(record) = class_data {
                for component in &record.components {
                    let mut ty = resolve_type_ref(db, &scope, &method_resolver, &component.ty);
                    if component.varargs {
                        ty = Ty::array(db, ty);
                    }
                    canonical.push(erase(instantiate(&ty)));
                }
            }
            let last_varargs = match class_data {
                ItemData::Record(record) => record.components.last().is_some_and(|c| c.varargs),
                _ => false,
            };
            (last_varargs, canonical)
        } else {
            let varargs = method.sig.params.last().is_some_and(|param| param.varargs);
            let mut params: Vec<Ty> = method_params_query(db, key)
                .iter()
                .map(instantiate)
                .map(erase)
                .collect();
            // A variable-arity parameter `T...` is lowered as the element type
            // `T`; its formal type is the array `T[]` ([JLS §8.4.1]).
            if varargs && let Some(last) = params.last_mut() {
                *last = Ty::array(db, *last);
            }
            (varargs, params)
        };
        let ret = erase(instantiate(&item_ty_query(db, key)));
        // The declared throws clause ([§8.4.6]): resolve and instantiate with
        // the declaring type's type arguments; method type parameters stay as
        // type variables for [`instantiate`] to solve.
        let mut throws: Vec<Ty> = method
            .sig
            .throws
            .iter()
            .map(|ex| resolve_type_ref(db, &scope, &method_resolver, ex))
            .map(|ty| instantiate(&ty))
            .map(erase)
            .collect();
        throws.dedup();
        // §9.4: an interface method without a body and without `static` is
        // implicitly `abstract`, whether or not the keyword is written.
        let abstract_ = if declaring_interface {
            method.body().is_none() && !method.modifiers.is_static()
        } else {
            method.modifiers.is_abstract()
        };
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: method.name.as_str().to_owned(),
            owner: class_key.clone(),
            owner_file: Some(source.file),
            decl_item: Some(item),
            params,
            param_names: Some(
                method
                    .sig
                    .params
                    .iter()
                    .map(|param| param.name.as_str().to_owned())
                    .collect(),
            ),
            ret,
            throws,
            varargs,
            is_static: method.modifiers.is_static(),
            abstract_,
            is_final: method.modifiers.is_final(),
            access: interface_access_of(declaring_interface, &method.modifiers),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
            raw_erased,
            descriptor: None,
        });
    }
    // §8.8.9: a class with no constructor has an implicit *default*
    // constructor — no parameters, same access as the class. Explicit
    // `super(...)` delegation ([§8.8.7.1]) and instance creation expressions
    // resolve against it; without it, a subclass of a constructor-less source
    // base reports `cannot find symbol: method Base()`.
    let declares_ctor = class_data.body().iter().any(|item| {
        item_data(&tree, *item)
            .is_some_and(|data| matches!(data, ItemData::Method(method) if method.is_constructor()))
    });
    if !declares_ctor
        && !declaring_interface
        && matches!(class_data, ItemData::Class(_) | ItemData::Enum(_))
        && (name.is_empty() || name == simple.as_str())
    {
        let modifiers = match class_data {
            ItemData::Class(d) => Some(&d.modifiers),
            ItemData::Enum(d) => Some(&d.modifiers),
            _ => None,
        };
        let access = |public: bool| {
            if public {
                Access::Public
            } else {
                Access::Package
            }
        };
        out.push(MethodData {
            name: simple.as_str().to_owned(),
            owner: class_key.clone(),
            owner_file: Some(source.file),
            decl_item: None,
            params: Vec::new(),
            param_names: None,
            ret: class_key.as_ty(db, Vec::new()),
            throws: Vec::new(),
            varargs: false,
            is_static: false,
            abstract_: false,
            is_final: false,
            // §8.8.9: the default constructor has the same access modifier
            // as the class (package-private when the class has none).
            access: modifiers
                .map(|m| access(m.is_public()))
                .unwrap_or(Access::Package),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface: false,
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
    }
    // §8.9.3: every enum type has two implicit static members —
    // `public static E[] values()` and `public static E valueOf(String name)`
    // — unless the body declares a member of the same name itself.
    if let ItemData::Enum(_) = class_data {
        let declared: FxHashSet<String> = out.iter().map(|m| m.name.to_string()).collect();
        // An enum type is never generic ([JLS §8.9]), so the implicit
        // members' return type is the raw reference itself.
        let self_ty = class_key.as_ty(db, Vec::new());
        if !declared.contains("values") && (name.is_empty() || name == "values") {
            out.push(MethodData {
                name: "values".to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: Vec::new(),
                param_names: None,
                ret: Ty::array(db, self_ty),
                throws: Vec::new(),
                varargs: false,
                is_static: true,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        if !declared.contains("valueOf") && (name.is_empty() || name == "valueOf") {
            out.push(MethodData {
                name: "valueOf".to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: vec![Ty::reference(db, "java.lang.String", Vec::new())],
                param_names: None,
                ret: self_ty,
                throws: Vec::new(),
                varargs: false,
                is_static: true,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
    }
    // §8.10.3: every record component has a public accessor method named
    // after it. An accessor explicitly declared by the record body *replaces*
    // the implicit one only when it has the accessor signature itself (same
    // name, zero parameters); a same-name method with parameters leaves the
    // implicit accessor in place ([§8.10.3]). Its return type is the
    // component type; a varargs component (`String... names`) is carried as
    // the array type `String[]` ([§8.4.1]).
    if let ItemData::Record(record) = class_data {
        for component in &record.components {
            let component_name = component.name.as_str();
            if !name.is_empty() && component_name != name {
                continue;
            }
            if out
                .iter()
                .any(|m| m.name.as_str() == component_name && m.params.is_empty())
            {
                continue;
            }
            let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
            if component.varargs {
                ty = Ty::array(db, ty);
            }
            let ty = ty.substitute(db, &binding);
            // JLS 4.8: a raw record erases its component type.
            let ty = if is_raw { ty.erasure(db) } else { ty };
            out.push(MethodData {
                name: component_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params: Vec::new(),
                param_names: None,
                ret: ty,
                throws: Vec::new(),
                varargs: false,
                is_static: false,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        // §8.10.3: every record implicitly implements `equals`, `hashCode`
        // and `toString`, whose signatures are those mandated by
        // `java.lang.Record` — a concrete `equals(Object): boolean`, a
        // concrete `hashCode(): int` and a concrete `toString(): String`.
        // An explicit declaration of the signature in the record body
        // *replaces* the implicit one ([§8.10.3]), so the implicit members
        // are only added when the record does not declare it itself. Without
        // them the abstract `equals`/`hashCode`/`toString` of
        // `java.lang.Record` look unimplemented ([§8.1.1.1]): `Object`'s
        // concrete members are its *supertypes'* methods, not its subtypes',
        // so they never count as implementations of the record's abstract
        // superclass.
        let string_ty = Ty::reference(db, "java.lang.String", Vec::new());
        let object_ty = Ty::reference(db, "java.lang.Object", Vec::new());
        let implicit_record_members: [(&str, Vec<Ty>, Ty); 3] = [
            (
                "equals",
                vec![object_ty],
                Ty::primitive(db, syntax::stub::PrimitiveType::Boolean),
            ),
            (
                "hashCode",
                Vec::new(),
                Ty::primitive(db, syntax::stub::PrimitiveType::Int),
            ),
            ("toString", Vec::new(), string_ty),
        ];
        for (member_name, params, ret) in implicit_record_members {
            // An empty name is the wildcard of the declaration-level walk
            // ([§9.8], [`crate::java::decl_check`]); a non-empty lookup name
            // filters the implicit members like every other member — a
            // constructor lookup for the record's simple name must not see
            // `equals(Object)` as a candidate, or `new Rec("s")` would
            // resolve against it.
            if !name.is_empty() && member_name != name {
                continue;
            }
            let declared = out.iter().any(|m| {
                m.name.as_str() == member_name
                    && m.params.len() == params.len()
                    && m.params.iter().zip(&params).all(|(x, y)| x == y)
            });
            if declared {
                continue;
            }
            out.push(MethodData {
                name: member_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                decl_item: None,
                params,
                param_names: None,
                ret,
                throws: Vec::new(),
                varargs: false,
                is_static: false,
                abstract_: false,
                is_final: false,
                access: Access::Public,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                declaring_interface: false,
                type_params: Vec::new(),
                raw_erased: false,
                descriptor: None,
            });
        }
        // §8.10.4: a record has a *canonical constructor* whose parameters
        // mirror the component list — same names, types and order — unless
        // the body declares a constructor with that signature itself (a
        // `@Singular`-style or compact form is still the canonical one, but
        // an explicit full-form declaration of matching arity replaces the
        // implicit member). Its access equals the record's own access.
        let simple = simple.as_str();
        if name.is_empty() || name == simple {
            let component_tys: Option<Vec<Ty>> = record
                .components
                .iter()
                .map(|component| {
                    let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
                    // A varargs component's canonical parameter is the array
                    // type ([§8.10.4], [§8.4.1]) — but it stays variable-arity.
                    if component.varargs {
                        ty = Ty::array(db, ty);
                    }
                    Some(ty.substitute(db, &binding))
                })
                .collect();
            if let Some(component_tys) = component_tys {
                let declares_canonical = out.iter().any(|method| {
                    method.name.as_str() == simple
                        && method.params.len() == component_tys.len()
                        && method
                            .params
                            .iter()
                            .zip(&component_tys)
                            .all(|(declared, component)| declared == component)
                });
                if !declares_canonical {
                    let varargs = record.components.last().is_some_and(|c| c.varargs);
                    let mut params = component_tys;
                    // JLS 4.8: a raw record erases its members.
                    if is_raw {
                        for param in &mut params {
                            *param = param.erasure(db);
                        }
                    }
                    out.push(MethodData {
                        name: simple.to_owned(),
                        owner: class_key.clone(),
                        owner_file: Some(source.file),
                        decl_item: None,
                        params,
                        param_names: None,
                        // Constructors carry no return type ([§8.8]).
                        ret: Ty::error(db),
                        throws: Vec::new(),
                        varargs,
                        is_static: false,
                        abstract_: false,
                        is_final: false,
                        access: access_of(&record.modifiers),
                        declaring_package: declaring_package.clone(),
                        declaring_top_level: declaring_top_level.clone(),
                        declaring_interface: false,
                        type_params: Vec::new(),
                        raw_erased: false,
                        descriptor: None,
                    });
                }
            }
        }
    }
    out
}

fn access_of(modifiers: &hir_def::java::modifiers::JavaModifiers) -> Access {
    match modifiers.visibility {
        hir_def::java::modifiers::JavaVisibility::Private => Access::Private,
        hir_def::java::modifiers::JavaVisibility::Protected => Access::Protected,
        hir_def::java::modifiers::JavaVisibility::Public => Access::Public,
        hir_def::java::modifiers::JavaVisibility::Package => Access::Package,
    }
}

/// The access of an interface member: every member of an interface is
/// implicitly `public` ([JLS §9.4], [§9.3]), whether or not the source
/// spells the modifier out.
fn interface_access_of(
    declaring_interface: bool,
    modifiers: &hir_def::java::modifiers::JavaModifiers,
) -> Access {
    match access_of(modifiers) {
        Access::Package if declaring_interface => Access::Public,
        other => other,
    }
}

/// Whether `method` is allowed by the invocation mode of `ctx`
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
pub(crate) fn mode_allows(method: &MethodData, ctx: &InvocationContext) -> bool {
    match ctx.mode {
        // A static invocation selects only static members.
        InvocationMode::Static => method.is_static,
        // §15.13.1: a type-qualified method reference may reference a static or
        // an unbound instance member, so every member of the name is a
        // candidate.
        InvocationMode::TypeQualified => true,
        // Super and interface invocations select only instance members.
        InvocationMode::Super | InvocationMode::Interface => !method.is_static,
        // §15.12.3 MethodName form: the member set is the virtual one plus the
        // static interface methods the searched type itself declares. The
        // static-interface-owner rule (declared-in-receiver-only, never
        // inherited) is applied by [`member_set_impl`], which knows the
        // receiver; no blanket static filter applies here.
        InvocationMode::MethodName => true,
        // A virtual invocation must not select a static method declared in an
        // interface (§15.12.3); a static method of a class may be selected.
        InvocationMode::Virtual => !(method.is_static && method.declaring_interface),
    }
}

/// Whether `method` is accessible to the class in which the invocation appears
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// when accessed through the receiver expression of type `receiver`.
pub(crate) fn is_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> bool {
    member_accessible(
        db,
        scope,
        method.access,
        method.declaring_package.as_deref(),
        &method.owner,
        method.declaring_top_level.as_deref(),
        receiver,
        method.is_static,
        ctx,
    )
}

/// Whether a member with `access` declared in `declaring_package` by the class
/// `owner` (whose top-level class is `declaring_top_level`) is accessible to
/// the class in which the access appears
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// when accessed through the receiver expression of type `receiver`. The two
/// names serve different rules: §6.6.1 scopes *private* access by the
/// top-level class, while §6.6.2 requires a subclass of the *declaring* class.
#[allow(clippy::too_many_arguments)]
pub(crate) fn member_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    access: Access,
    declaring_package: Option<&str>,
    owner: &ClassKey,
    declaring_top_level: Option<&str>,
    receiver: &Ty,
    static_member: bool,
    ctx: &InvocationContext,
) -> bool {
    match access {
        Access::Public => true,
        // §6.6.1: a private member is accessible throughout the top-level
        // class in which it is declared — a local declaration's top level is
        // the class enclosing it.
        Access::Private => match (&ctx.enclosing_class, declaring_top_level) {
            (Some(enclosing), Some(declaring)) => {
                let enclosing = enclosing.top_level(db, ctx.package.as_deref());
                match enclosing {
                    Some(enclosing) => within_top_level(enclosing.as_str(), declaring),
                    None => false,
                }
            }
            _ => false,
        },
        // §6.6.1: a package member is accessible only within its own package;
        // the unnamed package ([§7.4.2]) is `""`.
        Access::Package => match (&ctx.package, declaring_package) {
            (Some(invocation), Some(declaring)) => invocation == declaring,
            _ => false,
        },
        // §6.6.2: a protected member is accessible within the declaring
        // package, or from a class that is a subclass of the declaring class.
        Access::Protected => {
            if let (Some(invocation), Some(declaring)) = (&ctx.package, declaring_package)
                && invocation == declaring
            {
                return true;
            }
            // §6.6.2: code that is "responsible for the implementation of an
            // object of the subclass" — an *anonymous class body* whose direct
            // superclass is the declaring class — may access the protected
            // members the superclass itself declares, from any package (the
            // Gson `new TypeToken<T>() {}` idiom: an anonymous subclass of the
            // generic `TypeToken` invokes its protected no-arg constructor).
            if let Some(subclass_of) = &ctx.subclass_of {
                let subclass_of = subclass_of.as_ty(db, Vec::new());
                let declaring = owner.as_ty(db, Vec::new());
                // The anonymous class subclasses the *created* type exactly;
                // members it inherits from a *grand*-supertype of that type are
                // reachable only when the created type itself is a subclass,
                // which this membership already entails via the transitive
                // subtype check below.
                if is_subtype(db, scope, &subclass_of, &declaring) {
                    return true;
                }
            }
            let declaring = owner.as_ty(db, Vec::new());
            let subclass = ctx.enclosing_class.as_ref().and_then(|enclosing| {
                // §6.6.2: the access may appear in the body of a *nested*
                // class of a subclass — `B.Inner2` inside `B extends A`
                // accessing A's protected members — where the *innermost*
                // enclosing class `app.B.Inner2` is not itself a subclass but
                // its enclosing `app.B` is. Walk the enclosing chain outward
                // and find the subclass `S` whose body contains the access.
                enclosing_class_keys(db, ctx.package.as_deref(), enclosing)
                    .into_iter()
                    .find(|candidate| {
                        let candidate = candidate.as_ty(db, Vec::new());
                        is_subtype(db, scope, &candidate, &declaring)
                    })
            });
            match subclass {
                // §6.6.2: a protected instance member accessed outside the
                // declaring package by a receiver expression requires the type
                // of that expression to be a subtype of the subclass `S` whose
                // body the access is in — a `B.this.value(...)` inside
                // `B.Inner2` accesses through a receiver of type `B` (= S). A
                // `super` invocation accesses the member through the `super`
                // keyword, not an expression, so the rule does not apply
                // ([§15.12.1]).
                Some(_subclass) if static_member || ctx.mode == InvocationMode::Super => true,
                Some(subclass) => {
                    let subclass = subclass.as_ty(db, Vec::new());
                    is_subtype(db, scope, receiver, &subclass)
                }
                None => false,
            }
        }
    }
}

/// Whether some declared type parameter of the class `fqn` has a bound that
/// references the class itself — the *SELF* channel of a fluent API
/// (`SELF extends AbstractStringAssert<SELF>`), as opposed to a plain element
/// parameter (`Enumeration<E>`).
/// The indexes of the declared type parameters of the class `fqn` whose
/// bounds reference the class itself — the *SELF* channel of a fluent API
/// (`SELF extends AbstractStringAssert<SELF>`), as opposed to a plain element
/// parameter (`Enumeration<E>`). Library-only resolution (source classes
/// return empty — their bounds are source references resolved per class, and
/// the fluent-SELF rewriting targets the assertj-style library stubs);
/// `Some`/`None` per resolved class, `None` when the class or its generic
/// info is unavailable.
pub(crate) fn self_type_param_indexes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<Vec<usize>> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    let params = match resolved {
        hir::Resolved::Library(_) => hir::class_generic_info(db, &resolved)?.type_params,
        hir::Resolved::Source(_) | hir::Resolved::Facade { .. } => return Some(Vec::new()),
    };
    fn mention(
        bound: &syntax::stub::TypeRef<hir::Symbol>,
        self_fqn: &str,
        interner: &dyn hir::HirDatabase,
    ) -> bool {
        match bound {
            syntax::stub::TypeRef::Reference { name, generic_args } => {
                interner.hir_state().interner.resolve(name) == self_fqn
                    || generic_args
                        .iter()
                        .any(|arg| mention(arg, self_fqn, interner))
            }
            _ => false,
        }
    }
    Some(
        params
            .iter()
            .enumerate()
            .filter_map(|(i, param)| {
                param
                    .bounds
                    .iter()
                    .any(|bound| mention(bound, fqn, db))
                    .then_some(i)
            })
            .collect(),
    )
}

/// The class an access appears in and each class enclosing it, innermost first
/// ([JLS §6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)):
/// the §6.6.2 protected-access rule looks for the enclosing subclass whose
/// body contains the access, which may be an enclosing class rather than the
/// innermost one. A *local* declaration walks its real declaration chain; a
/// canonical name is trimmed segment by segment, bounded by the package.
fn enclosing_class_keys(
    db: &dyn TyDatabase,
    package: Option<&str>,
    key: &ClassKey,
) -> Vec<ClassKey> {
    let mut out = vec![key.clone()];
    match key {
        ClassKey::Local(class) => {
            let tree = hir::java_item_tree(db, class.file);
            out.extend(
                crate::java::resolve::enclosing_type_chain(&tree, class.item)
                    .into_iter()
                    .map(ClassKey::Named),
            );
        }
        ClassKey::Named(fqn) => {
            let package_prefix = match package {
                Some(package) if !package.is_empty() => format!("{package}."),
                _ => String::new(),
            };
            let mut candidate = fqn.as_str().to_owned();
            while let Some((prefix, _)) = candidate.rsplit_once('.') {
                if !prefix.starts_with(&package_prefix) {
                    break;
                }
                out.push(ClassKey::Named(Name::new(prefix)));
                candidate = prefix.to_owned();
            }
        }
    }
    out
}

/// Whether the class `enclosing` is the top-level class `declaring` or
/// lexically inside it ([JLS §6.6.1]): a private member is accessible
/// throughout the body of its top-level class, including from nested classes.
/// `declaring_top_level` is always the top level itself, so containment is
/// plain dot-prefix matching; the access site's enclosing class is a *source*
/// dotted name (inference only runs on source), so `$` never separates it.
fn within_top_level(enclosing: &str, declaring: &str) -> bool {
    enclosing == declaring || enclosing.starts_with(&format!("{}.", declaring))
}

/// An actual argument of a method invocation: either a concrete type, or a
/// poly expression — a lambda or method reference ([JLS §15.27.3],
/// [§15.13.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.2)) —
/// whose type is the target functional interface of the applicable candidate
/// ([JLS §18.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2)).
/// A poly argument does not constrain the invocation type inference; once the
/// candidate is resolved the caller types it against the resolved formal
/// parameter. The lambda's parameter count ([§15.12.2.2/§15.12.2.3]) is
/// carried along so an overload candidate whose functional interface does not
/// fit the lambda is not applicable.
#[derive(Debug, Clone)]
pub enum PolyArg {
    /// An argument with a concrete standalone type.
    Concrete(Ty),
    /// A poly argument — the lambda or method reference expression. The second
    /// element is the lambda's parameter count; a method reference is not
    /// arity-checkable without resolving the referenced method, so it is `None`.
    Poly(hir_expand::body::ExprId, Option<usize>),
}

impl From<Ty> for PolyArg {
    fn from(ty: Ty) -> Self {
        PolyArg::Concrete(ty)
    }
}

/// Instantiates `method` to its invocation type
/// ([JLS §15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6))
/// for a call with `args` in `phase`, running the method invocation type
/// inference of [JLS §18.5.2] against the actual argument types. `target` is
/// the expected type of the invocation
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)):
/// when present and compatible with the invocation type's return type, the
/// constraint ⟨R → T⟩ joins the constraint set before resolution, so the
/// inference variables are also bounded by the target type. `None` when
/// `method` is not applicable in this phase. `varargs` selects the variable-
/// arity invocation rules of [§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4).
fn instantiate(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[PolyArg],
    phase: InvocationPhase,
    varargs: bool,
    target: Option<Ty>,
) -> Option<MethodData> {
    let mut inference = Inference::new();

    // The method's own type parameters become fresh inference variables; their
    // declared bounds are the initial upper bounds (§18.5.2.2), with the type
    // parameter names substituted by the variables.
    let mut subst: FxHashMap<TypeVarScope, Ty> = FxHashMap::default();
    for tp in &method.type_params {
        let var = inference.fresh_var(db);
        subst.insert(tp.scope.clone(), var);
        let bounds: Vec<Ty> = tp.bounds.iter().map(|b| b.substitute(db, &subst)).collect();
        if bounds.is_empty() {
            inference.add_upper(db, var, Ty::reference(db, "java.lang.Object", Vec::new()));
        } else {
            for bound in bounds {
                inference.add_upper(db, var, bound);
            }
        }
    }
    let formals: Vec<Ty> = method
        .params
        .iter()
        .map(|p| p.substitute(db, &subst))
        .collect();

    // The throws clause ([§8.4.6]) substitutes the same way; a method type
    // parameter that appears in it carries the `throws` α bound (§18.5.2.2,
    // §18.1.3), directing resolution to prefer an unchecked exception type.
    let throws_formals: Vec<Ty> = method
        .throws
        .iter()
        .map(|t| t.substitute(db, &subst))
        .collect();
    for thrown in &throws_formals {
        if thrown.is_infer_var(db) {
            inference.mark_throws(db, *thrown);
        }
    }

    // §15.12.2.2/§15.12.2.3: a lambda is compatible with a function type only
    // when the parameter list has the same arity as the single abstract
    // method ([§15.27.3]). A candidate whose functional interface does not
    // fit a lambda argument is not applicable; a method reference contributes
    // no arity check here (it is resolved against the SAM after selection).
    //
    // For a variable-arity invocation ([§15.12.2.4]) the trailing actuals are
    // compatible with the *i*-th variable arity parameter type — the element
    // `Fn` for `i ≥ n` ([§8.4.1]) — so a trailing lambda's arity must match
    // the element's SAM ([§15.27.3]): the varargs array formal `Fn[]` is not
    // a functional interface, and a mismatch makes the candidate not
    // applicable ([§15.12.2.2]).
    let varargs_element = if varargs {
        formals.last().and_then(|f| f.element(db))
    } else {
        None
    };
    for (i, arg) in args.iter().enumerate() {
        if let PolyArg::Poly(_, Some(arity)) = arg {
            // Beyond the fixed prefix every trailing actual is packed into
            // the varargs array and is checked against the element type; an
            // element whose SAM is not (yet) resolvable — an inference
            // variable awaiting instantiation — skips the check rather than
            // falsely rejecting. (Phase 3 passes `varargs` for every member;
            // a non-varargs candidate with no formals hits `!method.varargs`
            // below, so the empty case must not panic here.)
            let target = if varargs && i >= formals.len().saturating_sub(1) {
                match varargs_element {
                    Some(element) => element,
                    None => continue,
                }
            } else {
                let Some(formal) = formals.get(i) else {
                    continue;
                };
                formal
            };
            if let Some(sam) = single_abstract_method(db, scope, target)
                && sam.params.len() != *arity
            {
                return None;
            }
        }
    }

    // In the loose phase (and for variable-arity invocation, §15.12.2.4)
    // primitive arguments are boxed, so `⟨int → α⟩` yields the boxed lower
    // bound. A poly argument (a lambda or method reference) is not boxed: its
    // type is the target functional interface, and it contributes no
    // constraint (§15.12.2.2/§15.12.2.3, §18.5.2.2).
    let args: Vec<Option<Ty>> = match phase {
        InvocationPhase::Strict => args
            .iter()
            .map(|arg| match arg {
                PolyArg::Concrete(ty) => Some(*ty),
                PolyArg::Poly(_, _) => None,
            })
            .collect(),
        InvocationPhase::Loose => args
            .iter()
            .map(|arg| match arg {
                PolyArg::Concrete(ty) => Some(match ty.kind(db) {
                    TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                    _ => *ty,
                }),
                PolyArg::Poly(_, _) => None,
            })
            .collect(),
    };

    let mut constraints = Vec::new();
    if varargs {
        if !method.varargs || args.len() + 1 < formals.len() {
            return None;
        }
        let (fixed, last) = formals.split_at(formals.len() - 1);
        for (formal, arg) in fixed.iter().zip(&args) {
            if let Some(arg) = arg {
                constraints.push(Constraint::Sub(*arg, *formal));
            }
        }
        let rest = &args[fixed.len()..];
        if !rest.is_empty() {
            if rest.len() == 1 && rest[0].is_some_and(|t| t.is_array(db)) {
                // A single trailing actual of the array type is used as-is.
                if let Some(arg) = rest[0] {
                    constraints.push(Constraint::Sub(arg, last[0]));
                }
            } else {
                // Otherwise the trailing actuals are packed into the array: each
                // is related to the element type.
                let element = last[0].element(db)?;
                for arg in rest.iter().flatten() {
                    constraints.push(Constraint::Sub(*arg, *element));
                }
            }
        }
    } else {
        if formals.len() != args.len() {
            return None;
        }
        for (formal, arg) in formals.iter().zip(&args) {
            if let Some(arg) = arg {
                constraints.push(Constraint::Sub(*arg, *formal));
            }
        }
    }

    // §18.5.2.4 (resolution): when the invocation is a poly expression with an
    // expected type, the constraint ⟨R → T⟩ is incorporated with the
    // argument constraints, so the inference variables are bounded by the
    // target type as well. Only a generic method can have a poly invocation
    // ([JLS §15.12.2.6]): a non-generic method's return type is fixed, so a
    // mismatched target must not reject an otherwise-applicable invocation.
    if let Some(target) = target
        && !method.type_params.is_empty()
    {
        let invocation_ret = method.ret.substitute(db, &subst);
        constraints.push(Constraint::Sub(invocation_ret, target));
    }

    let resolved = inference.solve(db, scope, phase, constraints)?;

    // The invocation type's throws clause ([§18.5.2.3]): the method's throws
    // types with the resolved substitution applied.
    let mut throws: Vec<Ty> = throws_formals
        .iter()
        .map(|t| t.substitute_infer(db, &resolved))
        .collect();
    throws.dedup();

    Some(MethodData {
        name: method.name.clone(),
        owner: method.owner.clone(),
        owner_file: method.owner_file,
        decl_item: method.decl_item,
        params: formals
            .iter()
            .map(|p| p.substitute_infer(db, &resolved))
            .collect(),
        param_names: method.param_names.clone(),
        ret: method
            .ret
            .substitute(db, &subst)
            .substitute_infer(db, &resolved),
        throws,
        varargs: method.varargs,
        is_static: method.is_static,
        abstract_: method.abstract_,
        is_final: method.is_final,
        access: method.access,
        declaring_package: method.declaring_package.clone(),
        declaring_top_level: method.declaring_top_level.clone(),
        declaring_interface: method.declaring_interface,
        type_params: method.type_params.clone(),
        raw_erased: method.raw_erased,
        // The identity of the member does not change with the type arguments
        // the invocation instantiates it at.
        descriptor: method.descriptor.clone(),
    })
}

/// Whether `m1` is more specific than `m2`
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// every formal parameter of `m1` is more specific than the corresponding
/// formal of `m2`. A variable-arity method is treated as a fixed-arity method
/// in the first two applicability phases ([§15.12.2]), so a
/// declared-variable-arity candidate loses to a fixed-arity one only when its
/// array formal is *less* specific — `m(Object)` beats `m(Object...)` for a
/// non-array argument, decided by applicability, not here — and wins when its
/// array formal is more specific: for an array argument `m(Object[])` beats
/// `m(Object)`, as §15.12.2's own note records ("declaring `m(Object...)` in a
/// class which already declares `m(Object)` causes `m(Object)` to no longer be
/// chosen for some invocation expressions (such as `m(null)`), as `m(Object[])`
/// is more specific"). There is no declared-flag tie-break in §15.12.2.5.
/// Only *m2's* genericity gates the comparison: when `m2` is generic,
/// `m1` is more specific under some instantiation of *m2*'s type parameters
/// ([§18.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.4))
/// — approximated here by instantiating them to their declared bounds, so
/// `<T> that(T[])` loses to nothing merely for being generic and beats
/// `that(Object)` for an array argument (`T[] <: Object`) exactly as javac
/// resolves Truth's builder. For two generic methods the
/// type-parameter-relative signatures are compared: `m2`'s type parameters are
/// substituted by `m1`'s (by position), and `m1`'s declared bounds must be at
/// least as restrictive as `m2`'s, so `<T extends String>` is more specific
/// than `<T>`.
pub(crate) fn more_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
    variable_arity: bool,
) -> bool {
    // §15.12.2.5 bullet 3: the variable-arity alignment applies only when
    // both candidates are applicable by *variable arity* invocation — i.e. in
    // the variable-arity phase (§15.12.2.4) and only for candidates declared
    // variable arity.
    // §15.12.2.5 (variable arity): when *both* candidates are applicable by
    // variable arity invocation, the more specific one is decided on their
    // fixed parameter prefixes and varargs element types, aligned at the
    // invocation's argument count. The declared parameter lists may have
    // different lengths — `style(TextColor, Decoration...)` vs
    // `style(StyleBuilderApplicable...)` — so each is normalized to the
    // longer length by repeating its varargs *element* type (the fixed
    // prefix first, then elements), and the position-wise subtype test runs
    // on the aligned lists. `m1` more specific than `m2` iff each of `m1`'s
    // normalized formals is a subtype of the corresponding `m2` formal:
    // `style(TextColor, Decoration...)` beats `style(SBApplicable...)`
    // because `TextColor <: SBApplicable` and `Decoration <: SBApplicable`.
    // Without the alignment both directions report false (different declared
    // lengths) and the invocation is ambiguous.
    if variable_arity && m1.varargs && m2.varargs {
        let norm = |m: &MethodData| -> Vec<Ty> {
            let split = m.params.len().saturating_sub(1);
            let fixed = &m.params[..split];
            let element = m
                .params
                .last()
                .and_then(|last| last.element(db))
                .copied()
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new()));
            let len = m1.params.len().max(m2.params.len());
            let mut out = fixed.to_vec();
            while out.len() < len {
                out.push(element);
            }
            out
        };
        let p1 = norm(m1);
        let mut inference = Inference::new();
        let (m2_formals, _, _) = inference.register_method(db, m2);
        if m2_formals.len() != m2.params.len() {
            return false;
        }
        // Map `m2`'s normalized formals: its own params (with the varargs
        // array) normalized the same way after registering its type params.
        let m2_norm = {
            let split = m2.params.len().saturating_sub(1);
            let mut out = m2_formals[..split].to_vec();
            let element = m2_formals
                .last()
                .and_then(|last| last.element(db))
                .copied()
                .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new()));
            let len = m1.params.len().max(m2.params.len());
            while out.len() < len {
                out.push(element);
            }
            out
        };
        for (a, b) in p1.iter().zip(&m2_norm) {
            inference.add_constraint(Constraint::Sub(*a, *b));
        }
        return inference.check_consistent(db, scope, InvocationPhase::Loose);
    }
    if m1.params.len() != m2.params.len() {
        return false;
    }
    // §15.12.2.5 (functional interface specificity): when the invocation's
    // argument is a lambda, a functional interface type `S` is more specific
    // than a functional interface type `T` when — beyond matching formal
    // parameter lists — `T`'s function type has a `void` return and `S`'s does
    // not, or `S`'s return type is a subtype of `T`'s (the `RS`/`RT`
    // conditions of §15.12.2.5). This is what resolves the
    // `CheckedSupplier<T>`/`CheckedRunnable` pair that the ordinary
    // subsignature test cannot: `Sub(CheckedSupplier<α>, CheckedRunnable)`
    // reduces to false (different erasures), yet a lambda whose body produces
    // a value must select the value-returning overload regardless of the
    // invocation's target (§15.12.2.5) — `return unchecked(() -> getFloat())`
    // selects `<T> T unchecked(CheckedSupplier<T>)`, not the `void`
    // `unchecked(CheckedRunnable)`. Without it the void overload wins the
    // tie-break and the enclosing `return` misreports the selected `void`
    // against the primitive return type.
    if let Some(win) = functional_interface_specificity(db, scope, m1, m2) {
        return win;
    }
    // §15.12.2.5 as javac implements it: `m1` is more specific than `m2` iff
    // `m2` is *applicable* to `m1`'s formal parameter types treated as the
    // invocation arguments (`signatureMoreSpecific`) — `m2`'s type
    // parameters are instantiated from `m1`'s parameter types, not from
    // their declared bounds. This resolves both the mixed-genericity cases
    // javac handles:
    //   `increment(Map<String,Integer>, String)` (non-generic) beats
    //     `<T> increment(Map<T,Integer>, T)` — the generic one instantiates
    //     `T := String` from the non-generic's parameters;
    //   `<T> Subject that(T[])` beats `that(Object)` for an array argument —
    //     the generic method's parameters are `T[]`, and `that(Object)` is
    //     applicable to them, while the reverse is not (`Object` is not an
    //     array). The constraint set is exactly an invocation type inference
    //     table ([JLS §18.5.2]); consistency means `m2` accepts `m1`'s
    //     parameters, i.e. `m1`'s signature is a subsignature of `m2`'s
    //     instantiated one.
    let mut inference = Inference::new();
    let (m2_formals, _, _) = inference.register_method(db, m2);
    if m2_formals.len() != m1.params.len() {
        return false;
    }
    for (param, formal) in m1.params.iter().zip(&m2_formals) {
        inference.add_constraint(Constraint::Sub(*param, *formal));
    }
    inference.check_consistent(db, scope, InvocationPhase::Loose)
}

/// The functional-interface half of the most-specific test
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// whether `m1` is more specific than `m2` because their corresponding
/// parameters are *functional interface* types `S` (m1's) and `T` (m2's)
/// whose function types differ only in their return type — `T` returning
/// `void` while `S` returns a value, or `S`'s return being a subtype of
/// `T`'s. `Some(true)`/`Some(false)` when the rule decides the pair, `None`
/// when the parameters are not a comparable functional-interface pair (the
/// ordinary subsignature test must decide).
///
/// This is the rule javac applies to a lambda argument that is *value
/// compatible* with both overloads: `use(() -> now())` over
/// `void use(V)` / `<T> T use(I<T>)` selects the value-returning `I<T>`
/// overload even in a statement context with no target. The two functional
/// interfaces need not be subtypes of each other (indeed `CheckedSupplier`
/// and `CheckedRunnable` are unrelated), so the generic subsignature test
/// above cannot compare them.
fn functional_interface_specificity(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
) -> Option<bool> {
    if m1.params.len() != m2.params.len() {
        return None;
    }
    let mut s_ret: Option<Ty> = None;
    let mut t_ret: Option<Ty> = None;
    for (s, t) in m1.params.iter().zip(&m2.params) {
        // The rule applies to a *lambda argument*: both formals must be
        // functional interfaces with matching SAM parameter lists.
        let (Some(ssam), Some(tsam)) = (
            single_abstract_method(db, scope, s),
            single_abstract_method(db, scope, t),
        ) else {
            return None;
        };
        if ssam.params != tsam.params {
            return None;
        }
        // The adapt-and-capture nuance of §15.12.2.5 (S's wildcards captured)
        // is approximated by the SAM returns as extracted.
        match (&s_ret, &t_ret) {
            (None, None) => {
                s_ret = Some(ssam.ret);
                t_ret = Some(tsam.ret);
            }
            (Some(a), Some(b)) => {
                if a != &ssam.ret || b != &tsam.ret {
                    return None;
                }
            }
            _ => unreachable!(),
        }
    }
    let (s_ret, t_ret) = (s_ret?, t_ret?);
    // `RT is void` (m2's SAM returns void) makes `S` more specific than `T`
    // for a lambda that is value-compatible with both — the value-returning
    // overload beats the void one. Otherwise `RS <: RT` (m1's return a subtype
    // of m2's) makes `S` more specific.
    if t_ret.is_void_like(db) && !s_ret.is_void_like(db) {
        return Some(true);
    }
    if s_ret.is_void_like(db) && !t_ret.is_void_like(db) {
        return Some(false);
    }
    if !s_ret.is_void_like(db) && !t_ret.is_void_like(db) {
        let s_cap = crate::java::ty::capture_conversion(db, scope, s_ret);
        if crate::java::subtyping::is_subtype(db, scope, &s_cap, &t_ret) {
            return Some(true);
        }
        let t_cap = crate::java::ty::capture_conversion(db, scope, t_ret);
        if crate::java::subtyping::is_subtype(db, scope, &t_cap, &s_ret) {
            return Some(false);
        }
    }
    None
}

/// Whether `param1` is a subtype of `param2` for the most-specific comparison
/// ([JLS §15.12.2.5], [§4.10.1]): reference types by subtyping, primitive
/// types by the primitive supertype order (double ≻ float ≻ long ≻ int ≻
/// {char, short, byte}).
pub(crate) fn choose_most_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: &[(MethodData, MethodData)],
    variable_arity: bool,
) -> Option<MethodData> {
    let mut winners: Vec<usize> = Vec::new();
    for (i, (candidate, _)) in candidates.iter().enumerate() {
        let wins = candidates.iter().all(|(other, _)| {
            other == candidate || more_specific(db, scope, candidate, other, variable_arity)
        });
        if wins {
            winners.push(i);
        }
    }

    // §15.12.2.5/§8.4.8.1: several equally-most-specific candidates whose
    // declared signatures are identical are one method seen through
    // overriding paths (covariant returns defeat exact-signature dedup).
    // Collapse them first, then prefer the most-derived declaring type;
    // genuinely unrelated declarations stay ambiguous.
    let mut unique: Vec<usize> = Vec::new();
    'outer: for &i in &winners {
        for &u in &unique {
            let (a, b) = (&candidates[u].0, &candidates[i].0);
            if a.params == b.params
                && a.ret == b.ret
                && a.is_static == b.is_static
                && a.varargs == b.varargs
            {
                continue 'outer;
            }
        }
        unique.push(i);
    }

    match unique.len() {
        0 => None,
        1 => Some(candidates[unique[0]].1.clone()),
        _ => {
            let chosen = unique.iter().copied().find(|&i| {
                let owner = candidates[i].0.owner.as_ty(db, Vec::new());
                unique.iter().all(|&j| {
                    j == i || {
                        let other = candidates[j].0.owner.as_ty(db, Vec::new());
                        is_subtype(db, scope, &owner, &other)
                    }
                })
            });
            chosen.map(|i| candidates[i].1.clone())
        }
    }
}

/// Resolves a method call `receiver.name(args)` by the applicability phases of
/// [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
/// strict invocation ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
/// then loose invocation ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3)),
/// then variable arity ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4));
/// the most specific applicable method
/// ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5))
/// wins. `None` when no method is applicable or the applicable ones are
/// ambiguous. The candidate set is restricted by the invocation mode and
/// access of `ctx` ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// The returned [`MethodData`] is the inferred invocation type
/// ([JLS §18.5.2]), refined by `target` — the expected type of the
/// invocation in its context
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)) —
/// when the call is a poly expression.
pub fn pick_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    args: &[PolyArg],
    ctx: &InvocationContext,
    target: Option<Ty>,
) -> Option<MethodData> {
    let members = member_set(db, scope, receiver, name, ctx);

    // Phase 1: strict invocation (§15.12.2.2) — no boxing or unboxing, fixed
    // arity.
    let strict: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Strict,
                false,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !strict.is_empty() {
        return choose_most_specific(db, scope, &strict, false);
    }

    // Phase 2: loose invocation (§15.12.2.3) — boxing and unboxing allowed.
    let loose: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Loose,
                false,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !loose.is_empty() {
        return choose_most_specific(db, scope, &loose, false);
    }

    // Phase 3: variable arity (§15.12.2.4).
    let varargs: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(
                db,
                scope,
                method,
                args,
                InvocationPhase::Loose,
                true,
                target,
            )
            .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !varargs.is_empty() {
        return choose_most_specific(db, scope, &varargs, true);
    }

    None
}

/// The fields of a source class, resolved against the file's own scope and
/// instantiated with `args`.
pub(crate) fn source_class_fields(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<FieldData> {
    let tree = hir::java_item_tree(db, source.file);
    let Some(class_data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let declared: &[TypeParam] = match class_data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !declared.is_empty();
    // §4.10.2 with [§4.4]/[§6.3]: the fields' declared types are instantiated
    // with the receiver's arguments, keyed by the declaring parameters'
    // scopes so only the class's *own* variables are replaced.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::source_class_binding(source.file, source.item, declared, &args);
    let resolver = Resolver::for_item(db, source.file, &tree, source.item);
    let scope = scope_for_file(db, source.file);
    // §6.7: the receiver's declaration — its canonical name, or the
    // declaration itself when it has none (a local class-like declaration,
    // [JLS §14.3]).
    let class_key = ClassKey::of(&tree, source.file, source.item);
    let package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_package = Some(package.clone().unwrap_or_default());
    // §6.6.1: a private member's accessibility is scoped by the *top-level*
    // class — the outermost enclosing class-like declaration, which for a
    // local declaration is the class around it.
    let declaring_top_level = class_key
        .top_level(db, package.as_deref())
        .map(|name| name.as_str().to_owned());
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));
    let declaring_enum = matches!(class_data, ItemData::Enum(_));

    let mut out = Vec::new();
    for &item in class_data.body() {
        match item_data(&tree, item) {
            Some(ItemData::Field(field)) => {
                if field.name.as_str() != name {
                    continue;
                }
                let key = ItemKey::new(db, source.file, item);
                // §9.3/[§9.6]: every field of an interface or annotation type
                // is implicitly `public static final`, whether or not the
                // source spells the modifiers out. A static context may read
                // such a field by simple name ([§8.1.3]) and a qualified
                // access (`I.F`) resolves it as static; treating it as an
                // instance field would report a false
                // non-static-cannot-be-referenced.
                let (is_static, is_final) = if declaring_interface {
                    (true, true)
                } else {
                    (field.modifiers.is_static(), field.modifiers.is_final())
                };
                // JLS 4.8: the *instance* fields of a raw type have erased
                // types. A static field does not depend on the receiver's type
                // arguments at all, so its declared type stays intact — the
                // companion of the raw-source-method rule in
                // [`source_class_methods`]. Erasing `NBTType.STRING` to raw
                // `NBTType` would discard the `NBTType<NBTString>` argument
                // javac reads from the constant's declaration.
                let ty = {
                    let ty = item_ty_query(db, key).substitute_incl_bounds(db, &binding);
                    if is_raw && !is_static {
                        ty.erasure(db)
                    } else {
                        ty
                    }
                };
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: class_key.clone(),
                    owner_file: Some(source.file),
                    decl_item: Some(item),
                    ty,
                    is_static,
                    access: interface_access_of(declaring_interface, &field.modifiers),
                    is_final,
                    declaring_package: declaring_package.clone(),
                    declaring_top_level: declaring_top_level.clone(),
                    descriptor: None,
                });
            }
            // §8.9.2: each enum constant is an implicitly `public static
            // final` field of the enum type, typed as the enum itself — a
            // static import (`import static E.CONSTANT`) and qualified reads
            // resolve through it.
            Some(ItemData::EnumConstant(constant))
                if declaring_enum && constant.name.as_str() == name =>
            {
                out.push(FieldData {
                    name: name.to_owned(),
                    owner: class_key.clone(),
                    owner_file: Some(source.file),
                    decl_item: Some(item),
                    ty: class_key.as_ty(db, binding.values().copied().collect()),
                    is_static: true,
                    access: Access::Public,
                    is_final: true,
                    declaring_package: declaring_package.clone(),
                    declaring_top_level: declaring_top_level.clone(),
                    descriptor: None,
                });
            }
            _ => {}
        }
    }
    // §8.10.3: each record component declares a private instance field of the
    // component type; a varargs component (`String... names`) is carried as
    // the array type `String[]` ([§8.4.1]). Reads inside the record body and
    // accessor synthesis resolve through it.
    if let ItemData::Record(record) = class_data {
        for component in &record.components {
            let component_name = component.name.as_str();
            if !name.is_empty() && component_name != name {
                continue;
            }
            let mut ty = resolve_type_ref(db, &scope, &resolver, &component.ty);
            if component.varargs {
                ty = Ty::array(db, ty);
            }
            out.push(FieldData {
                name: component_name.to_owned(),
                owner: class_key.clone(),
                owner_file: Some(source.file),
                // A record component is not an `ItemId`, so the synthesized
                // field has no declaration anchor; a `@Deprecated` on a
                // component is a javac *warning* about the annotation having
                // no effect here, not a deprecation of the component.
                decl_item: None,
                ty: ty.substitute(db, &binding),
                is_static: false,
                access: Access::Private,
                is_final: true,
                declaring_package: declaring_package.clone(),
                declaring_top_level: declaring_top_level.clone(),
                descriptor: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
}
