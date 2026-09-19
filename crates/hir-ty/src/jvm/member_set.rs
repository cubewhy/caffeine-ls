//! The JVM-shaped member set: the members a class of any language exposes to a
//! call site (IntelliJ: `JvmClass`'s member enumeration over `ClassFile`).
//!
//! [`member_set`] is the shared enumeration — a name's candidate methods on a
//! receiver and its supertypes, instantiated with the receiver's type
//! arguments, captured per JLS §5.1.10, deduped by overriding signature and
//! filtered by accessibility — plus the declaration-level walks the checks read
//! ([`all_methods`], [`all_methods_raw`], [`abstract_methods`]) and the field
//! side ([`pick_field`]). Which language's *declarations* answer for a class is
//! not this module's business: the per-class enumeration reaches the language's
//! own layer through the registry ([`crate::lang::member_source`]) and the
//! per-language selection rules (the JLS §15.12 applicability and
//! specificity phases) stay in the language that has them.

use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use hir_def::java::item_tree::ItemData;
use hir_def::jvm::access::JvmAccessFlags;
use hir_expand::name::Name;

use crate::{
    java::method::{is_accessible, member_accessible, mode_allows, self_type_param_indexes},
    java::subtyping::supertypes_query,
    java::ty::capture_conversion,
    jvm::db::{ContextKey, ScopeId, ScopeKind, TyDatabase},
    jvm::member::{Access, ClassKey, FieldData, JvmClassKind, MethodData, MethodTypeParam},
    ty::{Ty, TyData, TyKind, TypeVarScope},
};

/// How the method name is qualified: the invocation mode of
/// [JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1):
/// a static invocation (`TypeName.m`, §15.12.1), a super invocation
/// (`super.m` or `TypeName.super.m`), or a virtual invocation (via an
/// expression). The mode restricts which members are candidates
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvocationMode {
    /// `TypeName.m(...)`: only static members are candidates.
    Static,
    /// `T::m` — a *type-qualified* method reference ([JLS §15.13.1]): the
    /// reference resolves a static member (declared in a class *or* an
    /// interface) or an unbound instance member, so no static/instance filter
    /// applies — the §15.12.3 virtual-invocation restriction that excludes
    /// static interface methods is specific to receiver-expression invocations.
    TypeQualified,
    /// `super.m(...)`: only instance members are candidates.
    Super,
    /// `InterfaceName.super.m(...)`: only instance members are candidates.
    Interface,
    /// An unqualified `m(...)` whose receiver is the implicit `this`
    /// ([JLS §15.12.1] MethodName form): the class-or-interface members are
    /// candidates as in a virtual invocation, but a *static* member is also
    /// reachable when it is declared in the class/interface the receiver names
    /// ([§15.12.3]): javac resolves `s()` inside the interface that declares
    /// `static void s()`, yet rejects `expr.s()` where `expr`'s type merely
    /// implements the interface — the static-interface-member exclusion of the
    /// virtual-invocation form applies only when the method is reached through
    /// an expression.
    MethodName,
    /// `expression.m(...)`: all members except static methods declared in an
    /// interface are candidates.
    Virtual,
    /// A member access from a language that admits no static member through a
    /// receiver *expression* — Kotlin resolves `j.stat()`, a static of `j`'s
    /// class, as an unresolved reference and reaches the member through the
    /// class name alone
    /// (<https://kotlinlang.org/docs/java-interop.html#static-methods>). Not a
    /// JLS mode: the JLS's `Virtual` admits a static method of a class
    /// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
    InstanceReceiver,
}

/// The context of a method invocation: how the name is qualified (the
/// invocation mode, JLS §15.12.1/§15.12.3) and the lexical context used for
/// access control ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
///
/// Source call sites obtain a fully constrained context with [`access_context`]
/// and refine the mode per call site with [`InvocationContext::with_mode`];
/// [`InvocationContext::external`] models a library-only probe call site
/// outside the resolved scope.
#[derive(Debug, Clone)]
pub struct InvocationContext {
    /// The invocation mode.
    pub mode: InvocationMode,
    /// The class or interface in which the invocation appears, for `private`
    /// and `protected` access control
    /// ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1),
    /// [§6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)).
    /// A *local* declaration ([JLS §14.3]) is its own class here, which is what
    /// makes its private members accessible to the body that declares it.
    pub enclosing_class: Option<ClassKey>,
    /// The package of the compilation unit in which the invocation appears,
    /// for package and `protected` access control; the unnamed package is `""`.
    pub package: Option<String>,
    /// The class of which the access site is a (possibly anonymous) *subclass*,
    /// for the second half of [§6.6.2]: a protected member declared by that
    /// class is accessible from outside its package to code that is responsible
    /// for the implementation of an object of the subclass. An anonymous class
    /// creation `new C(args) { ... }` is exactly such a subclass — the
    /// anonymous body may invoke C's protected constructor and protected
    /// members even from another package (the Gson `new TypeToken<T>() {}`
    /// idiom) — even though no source item exists for the anonymous class to
    /// name as the enclosing class.
    pub subclass_of: Option<ClassKey>,
}

impl InvocationContext {
    /// The access control of a probe call site that resides outside `scope` —
    /// a library-only caller that is not a member of any of the resolved
    /// classes. It is affected by access control: it is neither a subclass of,
    /// nor in the package of, any `scope` class, so only `public` members are
    /// candidates ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    /// Source call sites use [`access_context`] instead.
    pub fn external(_scope: &hir::ResolutionScope) -> Self {
        Self {
            mode: InvocationMode::Virtual,
            // A fully qualified name that is not a subclass of anything in
            // `scope`, and not a member of any of its classes (§6.6.1).
            enclosing_class: Some(ClassKey::Named(Name::new("library.probe.Caller"))),
            // The unnamed package: package and `protected` members of named
            // packages are not accessible (§6.6.1).
            package: Some(String::new()),
            subclass_of: None,
        }
    }

    /// The context interned as `key` ([`ContextKey`]).
    pub fn from_key(db: &dyn TyDatabase, key: ContextKey) -> InvocationContext {
        InvocationContext {
            mode: *key.mode(db),
            enclosing_class: key.enclosing_class(db).clone(),
            package: key
                .package(db)
                .as_ref()
                .map(|name| name.as_str().to_owned()),
            subclass_of: key.subclass_of(db).clone(),
        }
    }

    /// The access-control context of an import declaration
    /// ([§7.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4)):
    /// an import appears at compilation-unit level, so the site has the unit's
    /// package ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1))
    /// and neither an enclosing class nor a superclass — the unnamed package is
    /// `""`, as in [`InvocationContext::external`]. A static import naming a
    /// package member the unit's own package declares is therefore valid, and
    /// the mode is a static access ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)).
    pub fn for_import(package: Option<&str>) -> Self {
        Self {
            mode: InvocationMode::Static,
            enclosing_class: None,
            package: Some(package.unwrap_or_default().to_owned()),
            subclass_of: None,
        }
    }

    /// The invocation context of the same access site with the invocation mode
    /// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
    /// of the call set to `mode`.
    pub fn with_mode(&self, mode: InvocationMode) -> InvocationContext {
        InvocationContext {
            mode,
            enclosing_class: self.enclosing_class.clone(),
            package: self.package.clone(),
            subclass_of: self.subclass_of.clone(),
        }
    }

    /// The invocation context of the same access site, additionally *within an
    /// anonymous class body whose direct superclass (or implemented interface)
    /// is `superclass`* ([JLS §15.9.5], [§6.6.2]): the anonymous body is a
    /// subclass of `superclass`, so protected members the *superclass itself*
    /// declares are accessible to it from any package.
    pub fn with_anonymous_superclass(&self, superclass: Name) -> InvocationContext {
        InvocationContext {
            mode: self.mode,
            enclosing_class: self.enclosing_class.clone(),
            package: self.package.clone(),
            subclass_of: Some(ClassKey::Named(superclass)),
        }
    }
}

/// The member set of a name on a receiver type
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
/// the methods named `name` of the receiver and, transitively, of all its
/// superclasses and superinterfaces, in an unspecified order. The receiver is
/// first captured ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
/// so that wildcard type arguments become fresh type variables, and the
/// candidates are restricted to those allowed by `ctx`'s invocation mode
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
/// and accessible at the invocation site
/// ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// For a type variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. Primitive and array receivers yield only the array
/// supertypes' methods. Memoized per (scope, receiver, name, context).
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    let ctx = ContextKey::from_invocation(db, ctx);
    member_set_query(db, scope, receiver.id, Name::new(name), ctx)
}

/// The methods named `name` on `receiver` and its supertypes *regardless of
/// access control* ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the access-probe companion of [`member_set`]. When the accessible member
/// set is empty but this one is not, a method of the name exists yet is not
/// accessible from the access site — the §6.6 error reported by the body
/// inference layer as `IllegalAccess`. The invocation mode ([§15.12.3]) is
/// still honored, so a static/instance mismatch does not masquerade as an
/// access violation.
pub fn member_set_ignoring_access(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, name, ctx, true, false, false)
}

/// All methods of `ty` across its supertype closure, most-derived first and
/// deduped by overriding signature
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1)):
/// the raw material of the declaration-level checks ([§8.4.8.3],
/// [§9.4.1.3], [`crate::java::decl_check`]). The access-control context is that of
/// the declaring class itself ([§6.6.1]).
///
/// This is a *declaration-level* enumeration ([JLS §8.2], [§9.2]): declared
/// plus inherited members, most-derived first, with the implicit members each
/// declaring class synthesizes ([§8.8.9], [§8.9.3], [§8.10.3/4]) — and with
/// none of the invocation-only transforms of [`member_set_impl`] (the
/// invocation-mode filter and the SELF return-re-pointing). The
/// static-interface-owner rule is *membership*, not an invocation transform
/// ([§9.2], [§9.4.1]), so it applies here as well.
pub fn all_methods(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, "", ctx, true, true, true)
}

/// Every member visible from `receiver` **without** the most-derived dedup
/// ([JLS §8.4.8.1]): the declaration-level checks ([§8.4.8.3], [§9.6.4.4])
/// must see the super declaration an override hides — a deduped member set
/// would report every correct `@Override` as orphaned. A *declaration-level*
/// enumeration like [`all_methods`] (per [§8.2], [§9.2], no invocation-only
/// transforms).
pub fn all_methods_raw(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    member_set_impl(db, scope, receiver, "", ctx, false, true, true)
}

/// The default methods `receiver` inherits, **without** the most-derived
/// dedup of [`member_set_impl`](Self): two unrelated superinterfaces may both
/// declare a matching default without either overriding the other, and that
/// conflict is exactly what the §9.4.1.3 check must see
/// ([JLS §9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3)).
pub(crate) fn inherited_defaults(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture_conversion(db, scope, *receiver);
    let mut stack = vec![receiver];
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, "")
                .into_iter()
                .filter(|method| {
                    method.declaring_interface && !method.is_static && !method.abstract_
                }),
        );
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push(parent);
        }
    }
    out
}

/// Memoized per (scope, receiver, name, context). See [`member_set`]. The
/// receiver and context are interned ids ([`TyData`], [`ContextKey`]), so
/// repeated member sets at the same call site hit the query cache instead of
/// re-walking the class hierarchy.
#[salsa::tracked(returns(clone))]
pub(crate) fn member_set_query(
    db: &dyn TyDatabase,
    scope: ScopeId,
    receiver: TyData,
    name: Name,
    ctx: ContextKey,
) -> Vec<MethodData> {
    member_set_impl(
        db,
        &scope.kind(db).to_scope(),
        &Ty { id: receiver },
        name.as_str(),
        &InvocationContext::from_key(db, ctx),
        true,
        true,
        false,
    )
}

/// The non-memoized form of [`member_set`]. `dedupe` selects whether the
/// most-derived overriding-signature collapse runs; `strict_access` selects
/// whether the candidates are filtered by accessibility at `ctx`'s access
/// site ([§6.6]) — `false` is the access probe used by the `IllegalAccess`
/// diagnostics of the body inference layer (see
/// [`member_set_ignoring_access`]). `declaration` selects the
/// *declaration-level* enumeration ([`all_methods`], [`all_methods_raw`]):
/// the members are gathered per [§8.2]/[§9.2] (declared + inherited,
/// most-derived first, implicit members synthesized per declaring class) with
/// none of the invocation-only transforms — the invocation-mode filter
/// ([`mode_allows`]) and the SELF return-re-pointing do not apply; the
/// static-interface-owner membership rule and the access filter (the declaring
/// class's own context) still do; the raw-receiver guard (`name == ""` keeps
/// declaration walks un-erased) likewise.
#[allow(clippy::too_many_arguments)]
fn member_set_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
    dedupe: bool,
    strict_access: bool,
    declaration: bool,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    // A *platform* type is what a Kotlin file sees a classfile type as
    // ([`crate::kotlin::ty::ty_from_java`]), and the members it resolves on one
    // are the *lower* half's: `process.onExit()` answers a
    // `CompletableFuture<Process>!`, whose `thenAccept` and `isDone` are that
    // class's own members — the walk below reads a class only from a
    // *reference*, so an unwrapped platform receiver is what reaches them.
    let receiver = capture_conversion(db, scope, *receiver).flexible_lower(db);
    // §9.2/[§9.4.1]/[§8.2]: a static method declared in an interface is a
    // member of *that interface only* — an interface inherits no static
    // methods from its superinterfaces ([§9.2]) and a class inherits none
    // from its superinterfaces either ([§8.2]), so the member set of any
    // other receiver type — a subinterface, an implementing class, or a
    // supertype walk that merely passes through the declaring interface —
    // excludes it. The receiver's own FQN (empty for non-reference
    // receivers, which keeps the rule inert: an array or type-variable
    // receiver inherits interface statics nowhere) is the only type whose
    // *declared* static interface methods are candidates. This is the
    // §15.12.3 static-interface-owner rule, applied uniformly: javac accepts
    // `I.s()` and `I::s` on the declaring interface `I`, and rejects
    // `J.s()`, `C.s()` and `J::s` for `J extends I` and `class C implements
    // I`. (A *static class* method remains inheritable — `MyThread.sleep` is
    // reached through the subclass — because `declaring_interface` is false
    // for it.)
    // §15.12.3: a *static interface* method is reachable by its simple name
    // only through the interface that declares it — not through a subinterface
    // or an implementing class. The declaring class is compared by identity: a
    // local declaration ([JLS §14.3]) is its own class here too.
    let receiver_key = ClassKey::of_ty(db, &receiver);
    let static_interface_owner_ok = |method: &MethodData| {
        let declared_by_receiver = receiver_key
            .as_ref()
            .is_some_and(|key| key == &method.owner);
        !(method.is_static && method.declaring_interface && !declared_by_receiver)
    };

    // §4.4: a type variable's *effective* upper bound is its declared bounds,
    // or `java.lang.Object` when it declares none — the member set of the
    // receiver is the member set of that bound. JLS §4.9: an intersection
    // type's members are those of every conjunct (`T extends MappedEntity &
    // CopyableEntity<T>` finds `copy` through the second bound, and a glb
    // `A & B` value finds members through either side).
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8))
    // gives raw receivers two member rules that this walk implements by
    // erasing *at the supertype edges*, not on the whole member set:
    //
    // * "The superclass types (respectively, superinterface types) of a raw
    //   type are the erasures of the superclass types (superinterface types)
    //   of the named class or interface."
    // * "The type of an inherited instance method or non-static field of a raw
    //   type C, where the member was declared in a class or interface D, is the
    //   type of the member in the supertype of C that names D."
    //
    // So the walk carries an *erasure context* per stack entry: a raw use
    // turns its supertype edges into erasures, and the context is monotone
    // (once raw, always raw), which is what makes a generic ancestor erase
    // behind a non-generic intermediate (`class Sub<T> extends Mid` with
    // `class Mid extends Gen<String>` → the declared `Gen` is reached as the
    // erasure `Gen`, erasing its members) while a member *declared* in a
    // non-generic class keeps its declared type even when reached through
    // generic ancestors (`class Sub<T> extends Mid<T>` with
    // `class Mid<T> extends Base` → the declared `Base` keeps
    // `List<String> items()`).
    //
    // Members *declared* in a raw class are erased per class by the `is_raw`
    // binding in `source_class_methods`/`library_class_methods` ([§4.8]'s
    // first member sentence), and static members keep their generics (they do
    // not depend on the receiver's type arguments).
    //
    // Declaration enumerations (`declaration`, the `all_methods` walks of
    // `decl_check`) keep the declared supertypes: their receiver is the class
    // under its own declaration (`Ty::reference(fqn, Vec::new())`), and
    // [§8.4.8.1]/[§9.4.1.2] override-equivalence compares members as declared,
    // substituted over the class's own type parameters.
    let mut stack: Vec<(Ty, bool)> = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } if bounds.is_empty() => {
            vec![(Ty::reference(db, "java.lang.Object", Vec::new()), false)]
        }
        TyKind::TypeVar { bounds, .. } => bounds.iter().map(|bound| (*bound, false)).collect(),
        TyKind::Intersection(members) => members.iter().map(|member| (*member, false)).collect(),
        _ => vec![(receiver, false)],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some((ty, erased)) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        // JLS §4.9: an intersection conjunct reached through a bound (a
        // `TypeVar` bound that is itself `A & B`) contributes every member.
        if let TyKind::Intersection(members) = ty.kind(db) {
            for member in members.clone() {
                stack.push((member, erased));
            }
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, name)
                .into_iter()
                .filter(|method| {
                    // A declaration-level enumeration has no invocation mode
                    // to honor: the static/instance filter of `mode_allows` is
                    // an invocation rule and applies only to resolution
                    // ([§15.12.3]). The *static-interface-owner* rule is not —
                    // it is membership ([§9.2] "A class inherits from its
                    // direct superclass ... all the non-private member methods
                    // ... other than static methods"; [§9.4.1] "A static
                    // method in an interface ... is not inherited"), so it
                    // applies to every enumeration, declaration-level included:
                    // a subinterface or implementing class has no inherited
                    // copy of a superinterface's `static` method, and
                    // [§8.4.8.1]'s override/clash comparison must not see one.
                    // The access filter stays (the caller passes the declaring
                    // class's own context, so private/package supertype
                    // members are excluded from what a subtype "inherits").
                    (declaration || mode_allows(method, ctx))
                        && (!strict_access || is_accessible(db, scope, method, &receiver, ctx))
                        && static_interface_owner_ok(method)
                }),
        );
        let raws = !declaration && (erased || is_raw_use(db, scope, &ty));
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    // §8.4.8: a member whose result type is the class's own *SELF* type
    // parameter — a parameter whose declared bound renames the class itself,
    // the assertj idiom `SELF extends AbstractStringAssert<SELF>` resolved on a
    // receiver parameterized by a *captured wildcard* — returns the receiver's
    // captured type, not a bare capture variable. Re-pointing the result at
    // the full receiver keeps assertion chains
    // (`assertThat(s).contains(a).contains(b)`) member-resolution-capable: the
    // bare capture's recursively-referencing bound degrades every subsequent
    // chained call. A *plain* type-parameter return (`Enumeration<? extends
    // ZipEntry>.nextElement()` returning `E`) is not a SELF channel and keeps
    // the captured element type, which must stay assignable to `ZipEntry`.
    // Invocation-only: a declaration-level enumeration reports the declared
    // returns unchanged.
    //
    // The re-pointing is keyed to the *self parameter* — the declared type
    // parameter whose bound mentions the class — instantiated at the
    // receiver's argument: `Chain<E, SELF extends Chain<E, SELF>>.self()` on
    // `Chain<?, ?>` returns the receiver's second argument (a capture
    // variable whose interned id is exactly the substituted `SELF` handle),
    // not every capture that happens to flow through the method's return. An
    // ordinary captured element return (`Chain<E, SELF...>.element()` on
    // `Chain<?, ?>` → the capture of `E`) is left untouched: it is not a
    // SELF channel and must stay assignable to the element type. The capture
    // of a wildcard-arg self parameter is a `CAP#` type variable whose
    // interned id equals the substituted return — `Ty` is interned, so `==`
    // is identity of interning.
    let self_args: Vec<Ty> = if declaration {
        Vec::new()
    } else {
        match receiver.kind(db) {
            // The declared type parameters of the receiver's class whose
            // bounds mention the class, each mapped to the receiver's actual
            // argument at that index ([§5.1.10] captures make a wildcard arg
            // a `CAP#` variable; a concrete arg is itself).
            TyKind::Reference { name, args, .. } => {
                self_type_param_indexes(db, scope, name.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|i| args.get(i).copied())
                    .collect()
            }
            _ => Vec::new(),
        }
    };
    if !self_args.is_empty() {
        for method in &mut out {
            // The member's return type was instantiated with the receiver's
            // arguments by `class_methods`, so a SELF-typed return is exactly
            // one of the self-parameter arguments (`Ty` is interned, so `==`
            // is identity of interning). Re-point only those; an ordinary
            // captured return of a non-self parameter is a different
            // `CAP#`/concrete handle and stays.
            if self_args.contains(&method.ret) {
                method.ret = receiver;
            }
        }
    }
    // §8.4.8/§4.4: a member whose result type is a *type variable of the
    // declaring class* resolved on a type-variable receiver keeps the
    // receiver's own type variable, not the recursion-guarded bare handle that
    // named it. `interface G<T extends G<T>> { T noarg(); }` with a receiver
    // `v: T` (`T extends G<T>`, the method's own type parameter) looks its
    // members up through the bound `G<T>`, whose type argument is the *inner*
    // recursion-guarded `T` (no further bounds, [§4.4] interning). Instantiating
    // `noarg`'s return with that argument yields a bound-less `T` whose member
    // set is empty — the next chained call (`v.noarg().noarg()`) reports
    // `no-such-method`. The declared bound means the receiver's variable
    // itself, so re-point the result at it: the receiver `T` carries the full
    // `G<T>` bound and the chain stays member-resolution-capable.
    // Invocation-only, like the SELF channel above.
    if !declaration && let TyKind::TypeVar { scope, .. } = receiver.kind(db) {
        let receiver_scope = scope.clone();
        for method in &mut out {
            // §4.4/§6.3: the result is re-pointed only when it is *the
            // receiver's own* type variable — same declaring parameter, not
            // merely the same name (a same-named variable of another
            // declaration, [§6.4.1], is a different type).
            if let TyKind::TypeVar { scope, .. } = method.ret.kind(db)
                && *scope == receiver_scope
            {
                method.ret = receiver;
            }
        }
    }
    // §10.7: every array type has a public `clone` method with no parameters
    // and no checked exceptions, whose return type is the array type itself.
    // It overrides `Object.clone` (which is `protected`) as public, so it is
    // invokable from anywhere the array is visible.
    if name == "clone" && matches!(receiver.kind(db), TyKind::Array(_)) {
        out.push(MethodData {
            name: "clone".to_owned(),
            owner: ClassKey::Named(Name::new("java.lang.Object")),
            owner_file: None,
            decl_item: None,
            params: Vec::new(),
            param_names: None,
            // The return type is the array type itself ([§10.7]).
            ret: receiver,
            throws: Vec::new(),
            varargs: false,
            is_static: false,
            abstract_: false,
            is_final: false,
            access: Access::Public,
            declaring_package: Some("java.lang".to_owned()),
            declaring_top_level: Some("Object".to_owned()),
            declaring_interface: false,
            type_params: Vec::new(),
            raw_erased: false,
            descriptor: None,
        });
    }
    // §8.4.8.1: an overriding method replaces the overridden one in the
    // member set — a subtype's declaration of a method with the same signature
    // shadows the supertype's — so only the most-derived declaration of each
    // signature survives. The walk is derived-first, so the first occurrence
    // *tends* to be the most-derived — but the walk is a LIFO stack whose
    // parents are pushed superclass-first, so at a class/interface join the
    // interface's declaration pops first. Without this, `List.iterator()`
    // (overriding `Collection.iterator()`/`Iterable.iterator()`) would surface
    // three identical candidates that the most-specific tie-break reports as
    // ambiguous.
    //
    // [§8.4.8.1]/[§15.12.3.1]: the declaration that survives is the *most
    // specific* one — the declaration whose declaring type is a subtype of the
    // other's, which for a class declaration and an inherited abstract
    // interface declaration is the class's ([§8.4.8.1] "the method declared in
    // the class"), and when the owners are unrelated, the *class* declaration
    // (a superinterface's abstract member never displaces a superclass's). The
    // comparison is on the declaring types, not on walk order: with
    // `interface I { void m(); } class B implements I { public void m() {} }
    // class M extends B implements I {}` the LIFO walk yields `I.m()` first,
    // and keeping it made `super.m()` from a subclass a §15.12.3
    // `AbstractSuperAccess` error even though the selected member is `B.m()`.
    //
    // Return covariance ([§8.4.8.3]) is the *last* tie-break, for owners that
    // neither relate by subtyping nor differ in class-vs-interface: a covariant
    // override narrows the return (`B toBuilder()` overriding
    // `ComponentBuilder<?,?> toBuilder()`), and a diamond walk can surface the
    // wider-return declaration first (`TranslatableComponent` via
    // `ScopedComponent → Component` (wildcard) before `BuildableComponent`
    // (`Builder`)). Keeping the wider return would pin `toBuilder()` to
    // `ComponentBuilder<?,?>` and reject the `TranslatableComponent.Builder`
    // target. Equal returns keep the first (walk order).
    if !dedupe {
        return out;
    }
    let mut deduped: Vec<MethodData> = Vec::with_capacity(out.len());
    for method in out {
        let mut replaced = false;
        for seen in deduped.iter_mut() {
            if !same_overriding_signature(seen, &method) {
                continue;
            }
            let owner = |m: &MethodData| m.owner.as_ty(db, Vec::new());
            let (new_owner, seen_owner) = (owner(&method), owner(seen));
            let new_derives =
                crate::java::subtyping::is_subtype(db, scope, &new_owner, &seen_owner);
            let seen_derives =
                crate::java::subtyping::is_subtype(db, scope, &seen_owner, &new_owner);
            let new_wins = if new_derives != seen_derives {
                new_derives
            } else if seen.declaring_interface != method.declaring_interface {
                // Unrelated owners of different kinds at this join: the class
                // declaration is the inherited member ([§8.4.8.1]); only a
                // class's superinterfaces are searched for a member the class
                // chain does not provide.
                !method.declaring_interface
            } else {
                // Same declaring kind and unrelated owners (or the same
                // owner): fall back to return covariance, then walk order.
                crate::java::subtyping::is_subtype(db, scope, &method.ret, &seen.ret)
                    && !crate::java::subtyping::is_subtype(db, scope, &seen.ret, &method.ret)
            };
            if new_wins {
                *seen = method.clone();
            }
            replaced = true;
            break;
        }
        if !replaced {
            deduped.push(method);
        }
    }
    deduped
}

/// Whether `receiver` is a *raw* use of a generic class
/// ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
/// a reference type with no type arguments whose class declares type
/// parameters (`CheckContainer` with `CheckContainer<T>`, `ListBinaryTag.Builder`
/// with `Builder<T>`). A non-generic class (`String`) or a parameterized use
/// (`List<String>`) is not raw, even with empty args in the latter's case the
/// args are present.
fn is_raw_use(db: &dyn TyDatabase, scope: &hir::ResolutionScope, receiver: &Ty) -> bool {
    let crate::java::ty::TyKind::Reference { name, args, .. } = receiver.kind(db) else {
        return false;
    };
    if !args.is_empty() {
        return false;
    }
    let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) else {
        return false;
    };
    match resolved {
        // A Kotlin file's facade declares no type parameters.
        hir::Resolved::Facade { .. } => false,
        hir::Resolved::Library(library) => {
            hir::class_generic_info(db, &hir::Resolved::Library(library))
                .is_some_and(|info| !info.type_params.is_empty())
        }
        hir::Resolved::Source(source) => {
            let tree = hir_def::java::plugin::tree(db, source.file);
            let Some(data) = crate::java::resolve::item_data(&tree, source.item) else {
                return false;
            };
            match data {
                ItemData::Class(d) | ItemData::Interface(d) => !d.type_params.is_empty(),
                ItemData::Record(d) => !d.type_params.is_empty(),
                _ => false,
            }
        }
    }
}

/// Whether two methods declare the same overriding signature
/// ([JLS §8.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.2),
/// [§8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1),
/// [§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)):
/// the same name, identical parameter types, and both static or neither. The
/// signature — and with it override-equivalence — never includes
/// variable-arity-ness: a method's parameter types are already in the
/// array-lowered form (`String...` is `String[]`, [§8.4.1]), so a
/// `void m(String...)` and a `void m(String[])` declare the same signature
/// ([§8.4.2]) and one may override the other ([§8.4.8.1], [§9.4.1.2]).
///
/// The *name* is part of it: two methods of one type that differ only in name
/// (`void a()` and `void b()`) are unrelated members, so the wildcard
/// enumeration ([`all_methods`]) must keep both — only same-name candidates
/// can shadow one another. Two members with the same signature differing only
/// in declaring type are the same method inherited and overridden down the
/// hierarchy; the return type is deliberately *not* compared, since an
/// override may narrow it (covariant returns, [§8.4.8.3]).
fn same_overriding_signature(a: &MethodData, b: &MethodData) -> bool {
    a.name == b.name && a.params == b.params && a.is_static == b.is_static
}

/// The abstract methods of the interface `ty` and its superinterfaces
/// ([JLS §9.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.2),
/// [§9.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.8)):
/// used to find the single abstract method that a lambda or method reference
/// is a value of the functional interface for. Memoized per (scope, type).
pub fn abstract_methods(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<MethodData> {
    let scope = ScopeId::new(db, ScopeKind::from_scope(scope));
    abstract_methods_query(db, scope, ty.id)
}

/// Memoized per (scope, type). See [`abstract_methods`].
#[salsa::tracked(returns(clone))]
pub(crate) fn abstract_methods_query(
    db: &dyn TyDatabase,
    scope: ScopeId,
    ty: TyData,
) -> Vec<MethodData> {
    abstract_methods_impl(db, &scope.kind(db).to_scope(), &Ty { id: ty })
}

/// The non-memoized form of [`abstract_methods`].
fn abstract_methods_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let ty = capture_conversion(db, scope, *ty);
    // §9.8/[§9.4]: the abstract members of a functional interface are the
    // *inherited* abstract methods not implemented by a `default` (or other
    // concrete) declaration anywhere closer to `ty`. Both kinds are gathered
    // across the supertype closure — an abstract in a distant superinterface
    // is discharged by a default declared in `ty` or an intermediate
    // interface — and a signature is abstract exactly when the gathered set
    // holds no concrete implementation of it.
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
    // a raw receiver's superinterface types are the *erasures* of the declared
    // ones, and an inherited member's type is its type in the erased supertype
    // that names it. The walk therefore carries the monotone erasure context
    // as in `member_set_impl`: the SAM of a raw functional interface is the
    // erased descriptor (§4.6), so a raw `Sub<T> extends F<T>` finds the
    // `F`-inherited `void accept(T)` as `void accept(Bound)` — the erasure of
    // the type variable's first bound. This descriptor is what
    // [§9.8]/[§15.27.3] match the lambda parameters against.
    let mut stack: Vec<(Ty, bool)> = vec![(ty, false)];
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut declarations: Vec<MethodData> = Vec::new();
    while let Some((t, erased)) = stack.pop() {
        if !seen.insert(t.id) {
            continue;
        }
        let TyKind::Reference { args, .. } = t.kind(db) else {
            continue;
        };
        // §6.7: the closure is seeded with each class's *declaration* — a
        // local functional interface ([JLS §14.3]) is its own item.
        let Some(resolved) = crate::java::resolve::reference_class(db, scope, &t) else {
            continue;
        };
        let args = args.clone();
        match resolved {
            // A Kotlin file's facade is no functional interface.
            hir::Resolved::Facade { .. } => continue,
            hir::Resolved::Library(class) => {
                let Some(record) = hir::class_record(db, &class) else {
                    continue;
                };
                let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
                    continue;
                };
                let interner = &db.hir_state().interner;
                for method in &stub.methods {
                    let flags = JvmAccessFlags::from_bits_retain(method.flags);
                    if !flags.is_abstract() {
                        continue;
                    }
                    let name = interner.resolve(&method.name);
                    declarations.extend(library_class_methods(
                        db,
                        class.clone(),
                        args.clone(),
                        name,
                    ));
                }
            }
            // A source class answers through the JVM view of the language
            // that declares it: an interface's declared abstract members are
            // that language's own ([`crate::lang::JvmMemberSource`]).
            class @ hir::Resolved::Source(_) => {
                if let Some(source) = crate::lang::member_source(db, &class) {
                    declarations.extend(source.abstract_methods(db, &class, &args));
                }
            }
        }
        for parent in supertypes_query(db, scope_id, t.id) {
            let raws = erased || is_raw_use(db, scope, &t);
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    // A default (concrete, non-static) method's signature discharges every
    // abstract of the same signature gathered from *other* interfaces; the
    // default itself is not an abstract member ([JLS §9.8], [§9.4.1.2]).
    // Override-equivalence never includes variable-arity-ness ([§8.4.2],
    // [§9.4.1.2]): the lowered parameter lists already compare `String...`
    // as `String[]`, so a default `void m(String...)` discharges an abstract
    // `void m(String[])` and vice versa.
    let has_default = |method: &MethodData| {
        declarations.iter().any(|candidate| {
            !candidate.abstract_
                && !candidate.is_static
                && candidate.name == method.name
                && candidate.params == method.params
                && candidate.type_params.len() == method.type_params.len()
        })
    };
    // §9.4.1.2/[§9.9]: abstract methods that override-equivalent one another
    // (same signature, possibly redeclared down a superinterface chain —
    // `Closeable.close` redeclaring `AutoCloseable.close`) are ONE abstract
    // method. Keep the first (most-derived) declaration of each signature.
    let mut deduped: Vec<MethodData> = Vec::with_capacity(declarations.len());
    for method in &declarations {
        if !method.abstract_ || has_default(method) {
            continue;
        }
        if !deduped.iter().any(|seen| {
            seen.name == method.name
                && seen.params == method.params
                && seen.type_params.len() == method.type_params.len()
        }) {
            deduped.push(method.clone());
        }
    }
    deduped
}

/// The single abstract method of the functional interface `ty`
/// ([JLS §9.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.8)):
/// the unique abstract method of the interface, disregarding those that
/// override `Object` members (`equals`, `hashCode`, `toString`). `None` when
/// `ty` is not a functional interface.
///
/// §9.8 makes a functional interface an *interface* with just one abstract
/// method: a **class** member with one abstract method — an abstract class
/// implementing `Runnable`, say — is not one, so a lambda or method reference
/// is not compatible with it ([§15.27.3], [§15.13.2]) and an overload taking
/// it is not applicable to such an argument. This is what keeps
/// `runTaskAsynchronously(Plugin, Runnable)` selected over the deprecated
/// `runTaskAsynchronously(Plugin, BukkitRunnable)` for `() -> …`, as javac has
/// it.
pub fn single_abstract_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<MethodData> {
    // §9.8: only an interface — or an annotation type, which is one
    // ([§9.6]) — can be a functional interface. A type this layer cannot
    // classify is not resolved any further either, so the answer is the same
    // `None` either way.
    if !class_kind(db, scope, ty)?.0.is_interface() {
        return None;
    }
    let mut methods = abstract_methods(db, scope, ty);
    // §9.8/[§9.4.1.2]: the abstract members of a functional interface are
    // those *not* matching a `public` member of `java.lang.Object` — the
    // class implementing the interface always provides those through
    // `Object`, so they are not abstract obligations
    // ([§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
    // A *user-declared* abstract method with an Object signature (an
    // interface restating `String toString()`) is equally discharged by
    // `Object`; only an abstract whose signature differs (`void go()`) is an
    // obligation.
    methods.retain(|m| !object_member_signature(db, scope, m));
    if methods.len() == 1 {
        let method = methods.pop().expect("one");
        // §9.8: the single abstract method of a functional interface must not
        // be generic — a generic method's parameter types depend on its own
        // type variables, so a lambda cannot provide an implementation for
        // every instantiation. javac: `invalid functional descriptor for
        // lambda expression … method (T)T in interface I is generic`.
        if !method.type_params.is_empty() {
            return None;
        }
        Some(method)
    } else {
        None
    }
}

/// The JVM kind of the reference type `ty` and whether it is `final` — the
/// classifier the member set's functional-interface test and Java's
/// provably-distinct-cast rule ([JLS §5.5.1]) ask of a type.
///
/// The answer is the *declaring* language's, through the registry: a Kotlin
/// `interface` is an interface in the classfile, and only the Kotlin layer can
/// read that from its declaration
/// ([`crate::lang::JvmMemberSource::kind`]). A classfile declares no source
/// language, so the classfile entry answers for it
/// ([`crate::lang::classfile`]).
///
/// `None` when `ty` is not a reference, its name does not resolve, or the
/// declaration it names is one no layer can classify — the permissive answer
/// the callers of this helper expect.
pub fn class_kind(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<(JvmClassKind, bool)> {
    let resolved = crate::java::resolve::reference_class(db, scope, ty)?;
    match &resolved {
        hir::Resolved::Library(_) => crate::lang::classfile().member_source().kind(db, &resolved),
        _ => crate::lang::member_source(db, &resolved)?.kind(db, &resolved),
    }
}

/// §9.8/[§9.4.1.2]: whether `method`'s signature matches a `public` member
/// of `java.lang.Object` — the interface need not (indeed cannot) require a
/// lambda for it, because the implementing class inherits `Object`'s
/// concrete implementation. The public Object members ([§8.4.8.1] lists the
/// canonical set for the override rule; [§9.4.1.2] applies it to interface
/// members) discharge abstract interface redeclarations of the same
/// signature: the JDK `Object` classfile (or the test fixture's stub) is
/// consulted by name + parameter-erasure. `clone()`/`finalize()` are
/// `protected`, so an interface can never override them through `Object`
/// (the interface is in a different package) and they do not discharge;
/// `getClass()`, `hashCode()`, `equals(Object)`, `toString()`, `notify()`,
/// `notifyAll()` and the `wait` overloads are `public` and do.
fn object_member_signature(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
) -> bool {
    let object = Ty::reference(db, "java.lang.Object", Vec::new());
    // The invocation context of a library-only caller: Object's public
    // members are all visible to it, so the enumeration is the full public
    // Object surface.
    let ctx = InvocationContext::external(scope);
    member_set(db, scope, &object, &method.name, &ctx)
        .iter()
        .any(|object_method| {
            if object_method.access != Access::Public {
                return false;
            }
            object_method.params.len() == method.params.len()
                && object_method
                    .params
                    .iter()
                    .zip(&method.params)
                    .all(|(a, b)| a == b || a.erasure(db) == b.erasure(db))
        })
}

/// The methods of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_methods(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<MethodData> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return Vec::new();
    };
    // §6.7: the receiver's declaration — a *local* class's own item, or the
    // class its canonical name resolves to.
    let Some(resolved) =
        crate::java::resolve::reference_class(db, &scope_id.kind(db).to_scope(), ty)
    else {
        return Vec::new();
    };
    let args = args.clone();
    match resolved {
        // A classfile's members are its record's.
        hir::Resolved::Library(class) => library_class_methods(db, class, args, name),
        // A source class — a facade included — answers through the JVM view of
        // the language that declares it (a Kotlin class's JVM-visible members
        // are the ones its compiler emits; a facade's are the file's top-level
        // declarations).
        class => crate::lang::member_source(db, &class)
            .map_or_else(Vec::new, |source| source.methods(db, &class, &args, name)),
    }
}

/// The package of a fully qualified class name, or `None` for the unnamed
/// package.
fn package_of(fqn: &str) -> Option<String> {
    fqn.rfind('.').map(|i| fqn[..i].to_owned())
}

/// The fully qualified name of the top-level class of a *library* binary
/// name: the name up to the first `$`. Library nested classes are named
/// `Outer$Inner` ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2));
/// source names nest with dots and must use [`source_top_level`] instead —
/// `$` inside them is an ordinary identifier character ([JLS §3.8]).
pub(crate) fn top_level_of(fqn: &str) -> String {
    match fqn.find('$') {
        Some(i) => fqn[..i].to_owned(),
        None => fqn.to_owned(),
    }
}

/// The fully qualified name of the top-level class of a *source* `fqn`: the
/// known package plus the first enclosing type — `com.example.Outer.Inner` is
/// `com.example.Outer`, an unnamed-package `Outer.Inner` is `Outer`
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
/// Source names never separate nesting with `$`, so none is split off.
pub(crate) fn source_top_level(package: Option<&str>, fqn: &str) -> String {
    let rest = match package {
        Some(pkg) => fqn
            .strip_prefix(pkg)
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(fqn),
        None => fqn,
    };
    let top = rest.split('.').next().unwrap_or(rest);
    match package {
        Some(pkg) if !pkg.is_empty() => format!("{pkg}.{top}"),
        _ => top.to_owned(),
    }
}

/// The methods of a library class, whose `Signature` attribute
/// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1))
/// may declare type parameters. The class's parameters are bound to `args`;
/// the method's own parameters (generic methods) keep their type parameters —
/// [`pick_method`] instantiates them by invocation type inference
/// ([JLS §18.5.2]).
fn library_class_methods(
    db: &dyn TyDatabase,
    class: hir::ResolvedClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let Some(record) = hir::class_record(db, &class) else {
        return Vec::new();
    };
    let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !class.type_params.is_empty();
    let fqn = interner.resolve(&class.fqn).to_owned();
    let owner = Name::new(&fqn);
    // §4.10.2: the receiver's arguments instantiate the declaring class's
    // *own* parameters — the binding is keyed by each parameter's declaring
    // scope ([§4.4], [§6.3]), so a same-named type variable appearing in a
    // member's signature but declared by another declaration is not captured
    // ([§6.4.1]).
    let binding: FxHashMap<TypeVarScope, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        class
            .type_params
            .iter()
            .zip(args.iter().copied())
            .map(|(tp, arg)| {
                (
                    TypeVarScope::LibraryClass {
                        owner: owner.clone(),
                        name: Name::new(interner.resolve(&tp.name)),
                    },
                    arg,
                )
            })
            .collect()
    };
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let declaring_interface = matches!(
        hir::ClassKind::from_flags(class.flags, class.is_record),
        hir::ClassKind::Interface | hir::ClassKind::Annotation
    );

    let mut out = Vec::new();
    for method in &class.methods {
        // An empty name is the wildcard of the declaration-level walk
        // ([§9.8], [`crate::java::decl_check`]); no method can be named "".
        if !name.is_empty() && interner.resolve(&method.name) != name {
            continue;
        }
        // [JVMS §4.6]: a method flagged `ACC_BRIDGE` or `ACC_SYNTHETIC` is
        // invisible to source-code member resolution. javac hides every
        // synthetic member of a classfile from name lookup — the covariant
        // bridge `Player[] getOnlinePlayers()` javac emits next to the real
        // `Collection<? extends Player> getOnlinePlayers()` ([§8.4.8.3]) is
        // ACC_BRIDGE|ACC_SYNTHETIC, so an invocation of `getOnlinePlayers()`
        // must resolve the real declaration and *not* report an ambiguity or
        // prefer the array overload. Only classfile members carry these flags
        // (source lowering never synthesizes a bridge), so filtering here
        // cannot hide a user declaration.
        let flags = JvmAccessFlags::from_bits_retain(method.flags);
        if flags.contains(JvmAccessFlags::BRIDGE) || flags.contains(JvmAccessFlags::SYNTHETIC) {
            continue;
        }
        // [§8.4.4]/[§6.4.1]: the method's own type parameters are declared by
        // *this* method, so their scope is the method's — distinct from a
        // same-named class parameter of the declaring class, which the class
        // binding cannot therefore capture ([§4.4] capture-avoidance).
        let method_names: Vec<Name> = method
            .type_params
            .iter()
            .map(|tp| Name::new(interner.resolve(&tp.name)))
            .collect();
        let method_name = Name::new(interner.resolve(&method.name));
        let signature = crate::java::resolve::LibrarySignature {
            owner: &owner,
            method: Some((&method_name, &method_names)),
        };
        let member_lower = |tyref: &hir::TypeRef<hir::Symbol>| {
            crate::java::resolve::ty_from_library_signature(db, tyref, &signature)
                .substitute(db, &binding)
        };
        // JLS §4.8 with §5.1.9: an instance member reached through a raw
        // receiver has an *erased* signature, so an invocation of it cannot be
        // statically checked — javac's `unchecked call to … as a member of the
        // raw type …`. The erasure is observable exactly when the declared
        // formal types mention a type variable (the class's or the method's
        // own) or the method declares its own type parameters; a member whose
        // formals are ground (`void m(String)`) stays checked.
        let raw_erased = is_raw
            && !flags.is_static()
            && (!method.type_params.is_empty()
                || method
                    .params
                    .iter()
                    .any(|param| member_lower(&param.param_type).contains_type_var(db)));
        let type_params = method
            .type_params
            .iter()
            .zip(method_names.iter())
            .map(|(tp, tp_name)| MethodTypeParam {
                scope: TypeVarScope::LibraryMethod {
                    owner: owner.clone(),
                    method: method_name.clone(),
                    name: tp_name.clone(),
                },
                // The bound is instantiated with the declaring class's type
                // arguments: a `<U extends T>` bound on a generic class
                // references the class type parameter `T`, which resolves to
                // the receiver's actual argument here (§18.5.2.2). A bound
                // over the method's own type parameters is untouched by the
                // class binding and stays a bare type variable.
                bounds: tp.bounds.iter().map(member_lower).collect(),
            })
            .collect();
        // JLS 4.8: the *instance* members of a raw type have erased
        // signatures. A static member does not depend on the receiver's
        // type arguments at all, so its own generics stay intact.
        let is_static_member = flags.is_static();
        let erase = |ty: Ty| {
            if is_raw && !is_static_member {
                ty.erasure(db)
            } else {
                ty
            }
        };
        out.push(MethodData {
            // The method's own name — not the lookup filter, which is the
            // empty wildcard in the declaration-level walk.
            name: interner.resolve(&method.name).to_owned(),
            owner: ClassKey::Named(Name::new(&fqn)),
            owner_file: None,
            decl_item: None,
            descriptor: Some(SmolStr::from(interner.resolve(&method.descriptor))),
            params: method
                .params
                .iter()
                .map(|param| erase(member_lower(&param.param_type)))
                .collect(),
            param_names: None,
            ret: erase(member_lower(&method.return_type)),
            throws: method
                .throws_list
                .iter()
                .map(&member_lower)
                .map(erase)
                .collect(),
            varargs: JvmAccessFlags::from_bits_retain(method.flags).is_varargs(),
            is_static: JvmAccessFlags::from_bits_retain(method.flags).is_static(),
            abstract_: JvmAccessFlags::from_bits_retain(method.flags).is_abstract(),
            is_final: JvmAccessFlags::from_bits_retain(method.flags).is_final(),
            access: Access::from_flags(method.flags),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
            raw_erased,
        });
    }
    out
}

/// Resolves a field access `receiver.name`
/// ([JLS §15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)):
/// the field named `name` of the receiver type, or of the closest of its
/// superclasses and superinterfaces (the member set of
/// [§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)
/// — field hiding is resolved in favour of the most derived declaration).
/// The receiver is first captured ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
/// so wildcard type arguments become fresh type variables, and only fields
/// accessible at the access site ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// are returned. For a type variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. `None` when no field is found.
pub fn pick_field(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Option<FieldData> {
    pick_field_impl(db, scope, receiver, name, ctx, true)
}

/// The most-derived field named `name` on the receiver *regardless of access
/// control* ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the access-probe companion of [`pick_field`]. When the accessible
/// [`pick_field`] misses but this hits, a field of the name exists yet is not
/// accessible from the access site — the §6.6 error reported by the body
/// inference layer as `IllegalAccess`.
pub fn pick_field_ignoring_access(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Option<FieldData> {
    pick_field_impl(db, scope, receiver, name, ctx, false)
}

/// The non-memoized form of [`pick_field`] and
/// [`pick_field_ignoring_access`]: the most-derived declaration of `name` on
/// the receiver, filtered by accessibility only when `strict_access` is set.
fn pick_field_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
    strict_access: bool,
) -> Option<FieldData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    // A *platform* type's fields are its lower half's, exactly as its methods
    // are ([`member_set_impl`]): `container.preferredSize.width` reads the
    // `int` field of the `Dimension` a Java getter answered.
    let receiver = capture_conversion(db, scope, *receiver).flexible_lower(db);
    // §4.4: an unbounded type variable's effective upper bound is
    // `java.lang.Object`, so its fields are the fields of `Object` (none).
    //
    // JLS §4.8 ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)):
    // "... the superclass types (respectively, superinterface types) of a raw
    // type are the erasures of the superclass types (superinterface types) of
    // the named class or interface", and "the type of an inherited instance
    // method or non-static field of a raw type C, where the member was
    // declared in a class or interface D, is the type of the member in the
    // supertype of C that names D." As in `member_set_impl`, the walk carries a
    // monotone erasure context per stack entry: a raw receiver erases the
    // supertype edges, so a field *declared* in a generic ancestor erases to
    // its §4.6 erasure (`Gen<T>` with `List<String> items` reached raw gives
    // `List`), while a field declared in a non-generic class keeps its
    // declared type even when reached through generic ancestors.
    let mut stack: Vec<(Ty, bool)> = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } if bounds.is_empty() => {
            vec![(Ty::reference(db, "java.lang.Object", Vec::new()), false)]
        }
        TyKind::TypeVar { bounds, .. } => bounds.iter().map(|bound| (*bound, false)).collect(),
        _ => vec![(receiver, false)],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    while let Some((ty, erased)) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        for field in class_fields(db, &scope_id, &ty, name) {
            if !strict_access
                || member_accessible(
                    db,
                    scope,
                    field.access,
                    field.declaring_package.as_deref(),
                    &field.owner,
                    field.declaring_top_level.as_deref(),
                    &receiver,
                    field.is_static,
                    ctx,
                )
            {
                return Some(field);
            }
        }
        let raws = erased || is_raw_use(db, scope, &ty);
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push((if raws { parent.erasure(db) } else { parent }, raws));
        }
    }
    None
}

/// The fields of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_fields(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<FieldData> {
    let TyKind::Reference { args, .. } = ty.kind(db) else {
        return Vec::new();
    };
    // §6.7: the receiver's declaration — a *local* class's own item, or the
    // class its canonical name resolves to.
    let Some(resolved) =
        crate::java::resolve::reference_class(db, &scope_id.kind(db).to_scope(), ty)
    else {
        return Vec::new();
    };
    let args = args.clone();
    match resolved {
        // A classfile's fields are its record's.
        hir::Resolved::Library(class) => library_class_fields(db, class, args, name),
        // A source class answers through the JVM view of its own language (a
        // facade's fields are the file's top-level `const val`s).
        class => crate::lang::member_source(db, &class)
            .map_or_else(Vec::new, |source| source.fields(db, &class, &args, name)),
    }
}

/// The fields of a library class, instantiated with the class's type
/// parameters bound to `args`.
fn library_class_fields(
    db: &dyn TyDatabase,
    class: hir::ResolvedClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<FieldData> {
    let Some(record) = hir::class_record(db, &class) else {
        return Vec::new();
    };
    let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    // JLS 4.8: a *raw* use of a generic class erases its members'
    // signatures; the erasure is applied to each constructed member below.
    let is_raw = args.is_empty() && !class.type_params.is_empty();
    let fqn = interner.resolve(&class.fqn).to_owned();
    let owner = Name::new(&fqn);
    let class_names: Vec<Name> = class
        .type_params
        .iter()
        .map(|tp| Name::new(interner.resolve(&tp.name)))
        .collect();
    // §4.10.2 with [§4.4]/[§6.3]: the field's declared type is instantiated
    // with the receiver's arguments, keyed by the classfile parameters'
    // declaring scopes.
    let binding: FxHashMap<TypeVarScope, Ty> =
        crate::java::resolve::library_class_binding(&owner, &class_names, &args);
    let class_ctx = crate::java::resolve::LibrarySignature::class(&owner);
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let mut out = Vec::new();
    for field in &class.fields {
        if interner.resolve(&field.name) != name {
            continue;
        }
        let is_static = JvmAccessFlags::from_bits_retain(field.flags).is_static();
        // JLS 4.8: the *instance* fields of a raw type have erased types. A
        // static field does not depend on the receiver's type arguments, so
        // its declared type stays intact.
        let ty = {
            let ty =
                crate::java::resolve::ty_from_library_signature(db, &field.field_type, &class_ctx)
                    .substitute(db, &binding);
            if is_raw && !is_static {
                ty.erasure(db)
            } else {
                ty
            }
        };
        out.push(FieldData {
            name: name.to_owned(),
            owner: ClassKey::Named(Name::new(&fqn)),
            owner_file: None,
            decl_item: None,
            ty,
            descriptor: Some(SmolStr::from(interner.resolve(&field.descriptor))),
            is_static,
            access: Access::from_flags(field.flags),
            is_final: JvmAccessFlags::from_bits_retain(field.flags).is_final(),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_top_level_splits_first_dollar() {
        assert_eq!(top_level_of("java.util.Map$Entry"), "java.util.Map");
        assert_eq!(top_level_of("com.example.Foo"), "com.example.Foo");
    }

    #[test]
    fn source_top_level_keeps_dollar_identifiers() {
        // §3.8: `$` is part of the identifier; the top level of `A$B` is `A$B`.
        assert_eq!(
            source_top_level(Some("com.example"), "com.example.A$B"),
            "com.example.A$B"
        );
        // §6.6.1: the top level of a nested class is its first enclosing type.
        assert_eq!(
            source_top_level(Some("com.example"), "com.example.Outer.Inner"),
            "com.example.Outer"
        );
        // The unnamed package ([§7.4.2]) has no prefix to keep.
        assert_eq!(source_top_level(None, "Outer.Inner"), "Outer");
        assert_eq!(source_top_level(None, "A$B"), "A$B");
    }
}
