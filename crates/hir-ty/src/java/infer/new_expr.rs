//! Class instance creation inference ([JLS §15.9], [§15.9.3]): the
//! constructor resolution of `new`, the wildcard/instantiation checks, and
//! the diamond type-argument inference from the target or constructor
//! arguments.

use hir_expand::{body::ExprId, name::Name, span::SpannedTypeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::TypeRef;

use crate::java::{
    diagnostics::{DiagLocation, TypeError},
    inference::{Inference, InvocationPhase},
    method::member_set,
    resolve::resolve_type_ref,
    ty::{Ty, TyData, TyKind},
};

use super::{InferCtx, poly::ArgInfo};

impl InferCtx<'_> {
    /// class, library constructors are `<init>`.
    pub(super) fn new_expr(
        &mut self,
        expr: ExprId,
        ty: SpannedTypeRef,
        diamond: bool,
        args: &[ExprId],
        target: Option<Ty>,
        anonymous_body: bool,
        receiver_ty: Option<Ty>,
    ) -> Ty {
        // §15.9: the created class of a *qualified* class instance creation
        // (`primary.new Inner(...)`) is the member class `Inner` of the
        // receiver's compile-time type. Its type reference is a bare name in
        // the syntax ([JLS §15.9.1]) that the lexical scope never declares,
        // so it is rewritten to the receiver-qualified FQN before resolution
        // — `a.new B()` with `a: A` resolves `A.B`.
        let ty = match &receiver_ty {
            Some(receiver_ty) => {
                match crate::java::resolve::qualify_member_type_of(
                    self.db,
                    &self.scope,
                    receiver_ty,
                    &ty.ty,
                ) {
                    // The receiver's member class exists on the classpath:
                    // resolve against it. Fall back to the lexical name when
                    // it does not (the receiver may be a type parameter whose
                    // member is reached another way, or the member class is
                    // genuinely missing and the lexical name reports it).
                    Some(qualified) => SpannedTypeRef::new(qualified, ty.refs),
                    None => ty,
                }
            }
            None => ty,
        };
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        // §15.9/[§4.5.1]: a wildcard type argument names no concrete type, so
        // `new ArrayList<?>()` creates nothing — and a type argument not within
        // its bounds is rejected ([§4.5.1]). The wildcard check reads the
        // *written* type reference: a diamond `new ArrayList<>()` has no
        // written arguments, and the type inferred for it (from the target)
        // may legitimately carry wildcards.
        if let TypeRef::Reference { generic_args, .. } = &ty.ty
            && generic_args
                .iter()
                .any(|a| matches!(a, TypeRef::Wildcard { .. }))
        {
            self.types.insert(expr, self.error());
            self.report(TypeError::CannotInstantiateWildcard { expr, ty: class_ty });
            return self.error();
        }
        self.check_type_argument_bounds(DiagLocation::Expr(expr), &ty);
        // §15.9: instantiating a type variable, an interface, an abstract
        // class or an enum with `new` is a compile-time error — unless an
        // anonymous class body implements the interface or extends the
        // abstract class ([§15.9.5]).
        let non_instantiable = if anonymous_body {
            false
        } else {
            self.check_instantiable(expr, class_ty)
        };
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let constructor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        let arg_kinds = self.arg_kinds(args);
        // §15.9.2: `new Foo<>()` — the created class's type arguments are
        // inferred from the target type ([§15.9.2.2]); when the target fixes
        // none, they are inferred from the constructor's formal parameters —
        // `new Analyzer<>(new BasicInterpreter())` infers
        // `Analyzer<BasicValue>` even without a target.
        let class_ty = if diamond {
            let from_target = self.diamond_instantiation(class_ty, target);
            // §15.9.2.2: a diamond's type arguments are fixed by the target
            // only when the target names them concretely. A target that
            // leaves them all as wildcards (`ValueSnapshot<?, ?>` for
            // `new ValueSnapshot<>(…)`) fixes nothing — the type variables
            // are inferred from the constructor arguments instead, exactly as
            // if there were no target at all. So a *raw* diamond (no target
            // args) and an all-wildcard diamond both fall through to the
            // constructor-argument inference.
            let raw = matches!(
                from_target.kind(self.db),
                TyKind::Reference { args, .. } if args.is_empty()
            );
            let all_wildcards = matches!(
                from_target.kind(self.db),
                TyKind::Reference { args, .. }
                    if !args.is_empty() && args.iter().all(|arg| arg.is_wildcard(self.db))
            );
            if raw || all_wildcards {
                self.diamond_instantiation_from_ctor_args(
                    from_target,
                    &arg_kinds,
                    &constructor_name,
                )
            } else {
                from_target
            }
        } else {
            class_ty
        };
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let access = self.access.clone();
        // §15.9.5/[§6.6.2]: an anonymous class body (`new C(...) { ... }`) is
        // a subclass of `C`, so its constructor invocation may reach C's
        // *protected* constructors from any package — `new TypeToken<T>() {}`
        // invoking the protected `TypeToken()` no-arg constructor from another
        // package. The access context is widened to the anonymous subclass.
        let access = if anonymous_body {
            access.with_anonymous_superclass(name.clone())
        } else {
            access
        };
        if let Some((method, deferred)) = self.resolve_call(
            &class_ty,
            &Name::new(&constructor_name),
            &arg_kinds,
            None,
            &access,
            None,
        ) {
            self.reinfer_deferred(&method, &deferred);
            // §11.2.1: a class instance creation throws the checked
            // exceptions of the chosen constructor — they join the
            // enclosing liability exactly like a method invocation's.
            for thrown in &method.throws {
                if self.is_checked(thrown) {
                    self.thrown.push((thrown.clone(), expr));
                }
            }
        } else {
            for arg in args {
                let _ = self.infer_expr(*arg);
            }
            // §15.9/[§15.12.1]/[§15.12.2]: members of the name exist but none
            // is applicable — a `cant.apply.symbol` at the `new` — or the
            // class declares no constructor of the name at all. Non-
            // instantiable types (interface/abstract/enum/type-var) already
            // reported their own error; a failed/unknown class type reported
            // the unknown type.
            if !anonymous_body && !non_instantiable && !class_ty.is_error(self.db) {
                let ctor = Name::new(&constructor_name);
                let owner = Name::new(name.simple_name());
                let members = member_set(self.db, &self.scope, &class_ty, ctor.as_str(), &access);
                if members.is_empty() {
                    // §8.8.9: a source class that declares *no* constructors
                    // synthesizes its implicit no-arg default into the member
                    // set ([`source_class_methods`]), so an empty set here
                    // means the class declares constructors that are hidden
                    // from this caller (a private constructor, [§6.6.1]) and
                    // no implicit default applies — `cannot find symbol:
                    // constructor {Foo}()` ([§15.9]). Library classes are
                    // skipped: their member records now always surface `<init>`
                    // when loaded ([`LibraryIndex`]), so an empty set there
                    // means an incomplete fixture, not a missing constructor.
                    if let Some(hir::Resolved::Source(_)) =
                        hir::fqn_resolve(self.db, &self.scope, name.as_str())
                    {
                        self.report(TypeError::NoSuchConstructor { expr, name: ctor });
                    }
                } else {
                    let found = args.len();
                    self.report_wrong_arity(expr, ctor, Some(owner), &members, &arg_kinds, found);
                }
            }
        }
        class_ty
    }

    /// top of the instantiation error.
    pub(super) fn check_instantiable(&mut self, expr: ExprId, class_ty: Ty) -> bool {
        let (TyKind::TypeVar { .. } | TyKind::Reference { .. }) = class_ty.kind(self.db) else {
            return false;
        };
        let non_instantiable = match class_ty.kind(self.db) {
            TyKind::TypeVar { .. } => true,
            TyKind::Reference { name, .. } => {
                let Some(hir::Resolved::Source(source)) =
                    hir::fqn_resolve(self.db, &self.scope, name.as_str())
                else {
                    return false;
                };
                let tree = hir::file_item_tree(self.db, source.file);
                let Some(data) = crate::java::resolve::item_data(&tree, source.item) else {
                    return false;
                };
                let non_instantiable = match data {
                    hir_def::java::item_tree::ItemData::Interface(_)
                    | hir_def::java::item_tree::ItemData::Enum(_)
                    | hir_def::java::item_tree::ItemData::Annotation(_) => true,
                    hir_def::java::item_tree::ItemData::Class(d) => d.modifiers.is_abstract(),
                    _ => false,
                };
                if !non_instantiable {
                    return false;
                }
                true
            }
            _ => return false,
        };
        if non_instantiable {
            self.types.insert(expr, self.error());
            self.report(TypeError::CannotInstantiateTypeVar { expr, ty: class_ty });
        }
        non_instantiable
    }

    /// is created raw ([§15.9.2.2]).
    pub(super) fn diamond_instantiation(&self, class_ty: Ty, target: Option<Ty>) -> Ty {
        let Some(target) = target else {
            return class_ty;
        };
        let TyKind::Reference {
            name: target_name,
            args: target_args,
        } = target.kind(self.db)
        else {
            return class_ty;
        };
        let TyKind::Reference {
            name: class_name,
            args: _,
        } = class_ty.kind(self.db)
        else {
            return class_ty;
        };

        // §15.9.2.1: same erasure — take the target's arguments directly.
        if target_name.as_str() == class_name.as_str() {
            if !target_args.is_empty() {
                return Ty::reference(self.db, class_name.as_str(), target_args.clone());
            }
            return class_ty;
        }
        // A parameterized supertype of the created class matching the
        // target's erasure: its type arguments witness the created class's
        // own type variables ([§15.9.2]). The walk is *transitive* — the
        // supertype may be several levels up (`new LinkedHashMap<>()` to a
        // `Map<K,V>` target through `HashMap<K,V>` / `AbstractMap<K,V>`) —
        // and runs against the *parameterized* self-type: a raw receiver
        // would erase its supertypes ([§4.8]) and lose the witness.
        let declared_params = self.class_type_var_names(class_name);
        let probe = if declared_params.is_empty() {
            class_ty
        } else {
            Ty::reference(self.db, class_name.clone(), declared_params.clone())
        };
        let mut stack = vec![probe];
        let mut visited: FxHashSet<TyData> = FxHashSet::default();
        while let Some(current) = stack.pop() {
            if !visited.insert(current.id) {
                continue;
            }
            for parent in crate::java::subtyping::supertypes_impl(self.db, &self.scope, &current) {
                let TyKind::Reference {
                    name: parent_name,
                    args: parent_args,
                } = parent.kind(self.db)
                else {
                    continue;
                };
                if parent_name.as_str() != target_name.as_str()
                    || parent_args.len() != target_args.len()
                {
                    stack.push(parent);
                    continue;
                }
                let mut binding: FxHashMap<Name, Ty> = FxHashMap::default();
                for (parent_arg, target_arg) in parent_args.iter().zip(target_args.iter()) {
                    if let TyKind::TypeVar { name, .. } = parent_arg.kind(self.db) {
                        // §15.9.2.2: a *wildcard* target argument bounds the
                        // created class's type variable — `? extends X` gives
                        // it the upper bound X, `? super X` the lower bound X
                        // — and the diamond instantiates to X in both cases.
                        // Binding the bare wildcard would create a nested
                        // wildcard (`LinkedHashMap<? extends ? extends X>`)
                        // that no later constructor application can use.
                        let instantiation = match target_arg.kind(self.db) {
                            TyKind::Wildcard(Some(bound))
                                if !bound.ty.contains_infer_var(self.db) =>
                            {
                                bound.ty
                            }
                            _ if !target_arg.contains_infer_var(self.db) => *target_arg,
                            _ => continue,
                        };
                        binding.insert(name.clone(), instantiation);
                    }
                }
                if !binding.is_empty() {
                    return probe.substitute(self.db, &binding);
                }
                stack.push(parent);
            }
        }
        class_ty
    }

    /// and unresolvable names.
    pub(super) fn class_type_var_names(&self, fqn: &Name) -> Vec<Ty> {
        let Some(resolved) = hir::fqn_resolve(self.db, &self.scope, fqn.as_str()) else {
            return Vec::new();
        };
        let names: Vec<Name> = match resolved {
            hir::Resolved::Library(library) => {
                let Some(info) = hir::class_generic_info(self.db, &hir::Resolved::Library(library))
                else {
                    return Vec::new();
                };
                let interner = &self.db.hir_state().interner;
                info.type_params
                    .iter()
                    .map(|tp| Name::new(interner.resolve(&tp.name)))
                    .collect()
            }
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(self.db, source.file);
                let declared = match crate::java::resolve::item_data(&tree, source.item) {
                    Some(hir_def::java::item_tree::ItemData::Class(d)) => Some(&d.type_params),
                    Some(hir_def::java::item_tree::ItemData::Interface(d)) => Some(&d.type_params),
                    Some(hir_def::java::item_tree::ItemData::Record(d)) => Some(&d.type_params),
                    _ => None,
                };
                match declared {
                    Some(declared) => declared.iter().map(|tp| tp.name.clone()).collect(),
                    None => Vec::new(),
                }
            }
        };
        names
            .into_iter()
            .map(|name| Ty::type_var(self.db, name, Vec::new()))
            .collect()
    }

    /// inference variables.
    pub(super) fn class_type_param_bounds(
        &self,
        fqn: &Name,
    ) -> Vec<crate::java::method::MethodTypeParam> {
        let Some(resolved) = hir::fqn_resolve(self.db, &self.scope, fqn.as_str()) else {
            return Vec::new();
        };
        match resolved {
            hir::Resolved::Library(library) => {
                let Some(info) = hir::class_generic_info(self.db, &hir::Resolved::Library(library))
                else {
                    return Vec::new();
                };
                let interner = &self.db.hir_state().interner;
                info.type_params
                    .iter()
                    .map(|tp| crate::java::method::MethodTypeParam {
                        name: Name::new(interner.resolve(&tp.name)),
                        bounds: tp
                            .bounds
                            .iter()
                            .map(|bound| crate::java::resolve::ty_from_library(self.db, bound))
                            .collect(),
                    })
                    .collect()
            }
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(self.db, source.file);
                let declared = match crate::java::resolve::item_data(&tree, source.item) {
                    Some(hir_def::java::item_tree::ItemData::Class(d)) => Some(&d.type_params),
                    Some(hir_def::java::item_tree::ItemData::Interface(d)) => Some(&d.type_params),
                    Some(hir_def::java::item_tree::ItemData::Record(d)) => Some(&d.type_params),
                    _ => None,
                };
                match declared {
                    Some(declared) => declared
                        .iter()
                        .map(|tp| crate::java::method::MethodTypeParam {
                            name: tp.name.clone(),
                            bounds: tp
                                .bounds
                                .iter()
                                .map(|b| resolve_type_ref(self.db, &self.scope, &self.resolver, b))
                                .collect(),
                        })
                        .collect(),
                    None => Vec::new(),
                }
            }
        }
    }

    /// to the raw `class_ty` when no constructor decides it.
    pub(super) fn diamond_instantiation_from_ctor_args(
        &mut self,
        class_ty: Ty,
        arg_kinds: &[ArgInfo],
        ctor_name: &str,
    ) -> Ty {
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let type_params = self.class_type_param_bounds(&name);
        if type_params.is_empty() {
            return class_ty;
        }
        let bare: Vec<Ty> = type_params
            .iter()
            .map(|tp| Ty::type_var(self.db, tp.name.clone(), tp.bounds.clone()))
            .collect();
        // Resolve the constructors against the *parameterized* class so the
        // formal parameter types keep the class's type variables ([§15.9.2.2]).
        let param_class = Ty::reference(self.db, name.clone(), bare.clone());
        let access = self.access.clone();
        let members = member_set(self.db, &self.scope, &param_class, ctor_name, &access);
        for member in members {
            if member.params.len() != arg_kinds.len() {
                continue;
            }
            let mut inference = Inference::new();
            let subst = inference.register_class_type_params(self.db, &type_params);
            let formals: Vec<Ty> = member
                .params
                .iter()
                .map(|p| p.substitute(self.db, &subst))
                .collect();
            let mut ok = true;
            for (info, formal) in arg_kinds.iter().zip(&formals) {
                for leaf in &info.leaves {
                    if !self.contribute_leaf(&mut inference, leaf, *formal, InvocationPhase::Loose)
                    {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            if !ok {
                continue;
            }
            if let Some(resolved) =
                inference.solve_after(self.db, &self.scope, InvocationPhase::Loose)
            {
                let mut binding: FxHashMap<Name, Ty> = FxHashMap::default();
                for (tp_name, var) in &subst {
                    if let Some(var_id) = var.as_infer_var(self.db)
                        && let Some(inst) = resolved.get(&var_id)
                    {
                        binding.insert(tp_name.clone(), *inst);
                    }
                }
                if binding.len() == type_params.len() {
                    let args = bare
                        .iter()
                        .map(|t| t.substitute(self.db, &binding))
                        .collect();
                    return Ty::reference(self.db, name.clone(), args);
                }
            }
        }
        class_ty
    }
}
