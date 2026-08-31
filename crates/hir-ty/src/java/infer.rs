//! Expression-level type inference over the lowered body IR
//! ([`hir_expand::body`]).
//!
//! [`body_types`] infers the type of every expression ([JLS §15]) and local
//! variable ([JLS §14.4]) of a method, constructor or initializer body, given
//! the declaration types computed by [`crate::java::db::item_ty_query`] and the
//! body IR of `hir-def`. Names are resolved lexically ([JLS §6.3]); field and
//! method access is resolved by [`crate::java::method::pick_field`] /
//! [`crate::java::method::pick_method`] under the access context of the call site
//! ([JLS §6.6]).
//!
//! The types are computed bottom-up ([§15.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.1)):
//! every expression's type is a function of its operands. Numeric binary
//! expressions follow binary numeric promotion
//! ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2),
//! unboxing a boxed reference operand via [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
//! unary expressions unary numeric promotion
//! ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)),
//! and conditional and array-initializer expressions follow the conditional
//! type of [§15.25](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25)
//! (identity, binary numeric promotion, the null type, then the least upper
//! bound). Method calls are refined by their *target type*
//! ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4))
//! where the context fixes it: a declaration initializer, an assignment
//! right-hand side, a returned expression, or a cast. Lambdas and method
//! references are poly expressions whose type comes from the *target*
//! functional interface ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27)):
//! their standalone type is unknown, so they infer to [`Ty::error`] when no
//! target exists. A nested generic method invocation in an argument position
//! is likewise a poly expression ([JLS §18.5.2.4]): its inference shares the
//! enclosing invocation's table, so `take(emptyList())` resolves the nested
//! `emptyList()` as `List<String>` against `take(List<String>)` instead of the
//! standalone `List<Object>`. The nested invocation's own candidate selection
//! is independent of the enclosing one: each candidate is probed against a
//! fresh bound set ([JLS §18.5.1]), the most specific applicable one
//! ([§15.12.2.5]) wins, and only its constraints are lifted into the
//! enclosing table ([JLS §18.5.2.1/§18.5.2.2]).

use triomphe::Arc;

use hir_def::java::item_tree::{ItemData, ItemId};
use hir_expand::{
    body::{BodyId, BodyTree, ExprId, LocalId},
    name::Name,
    span::SpannedTypeRef,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::TypeRef;
use vfs::FileId;

mod ctx;
mod expr;
mod field;
mod flow;
mod lambda;
mod local;
mod method;
mod operator;
mod overload;
mod poly;
mod stmt;
mod switch;

use self::poly::*;

use crate::{
    java::const_eval::Const,
    java::db::{TyDatabase, type_params_map_query},
    java::diagnostics::{DiagLocation, TypeError},
    java::inference::{Inference, InvocationPhase},
    java::method::{InvocationContext, access_context, member_set},
    java::resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    java::ty::{Ty, TyData, TyKind},
};

/// The inferred types of a method or constructor body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTypes {
    /// The body the types were inferred for.
    pub body: Option<BodyId>,
    /// keyed by its arena id.
    pub exprs: FxHashMap<ExprId, Ty>,
    /// for-loop variables, catch parameters — keyed by its arena id.
    pub locals: FxHashMap<LocalId, Ty>,
    /// The type errors reported while inferring the body, in report order.
    pub diagnostics: Vec<TypeError>,
    /// error.
    pub field_touched: FxHashSet<String>,
}

/// body (a declaration without statements) or is not a body-carrying item.
pub fn body_types(db: &dyn TyDatabase, file: FileId, item: ItemId) -> Option<Arc<BodyTypes>> {
    crate::java::db::body_types_query(db, crate::java::db::ItemKey::new(db, file, item))
}

pub(crate) fn body_types_impl(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
) -> Option<BodyTypes> {
    let tree = hir::file_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);
    let scope = scope_for_file(db, file);
    let type_params = type_params_map_query(db, db.file_text(file));
    let resolver = Resolver::new(&tree, type_params, item);
    let access = access_context(db, file, item);
    let enclosing_class = enclosing_self_ty(db, file, &tree, item, &scope, &resolver);
    let mut ctx = InferCtx {
        db,
        scope,
        tree: bodies.clone(),
        resolver,
        access,
        enclosing_class: enclosing_class.clone(),
        enclosing_chain: {
            // The chain of enclosing class-like declarations of `item`,
            // innermost first, as raw types ([§6.3], [§8.1.3]).
            crate::java::resolve::enclosing_type_chain(&tree, item)
                .into_iter()
                .map(|name| Ty::reference(db, name.as_str(), Vec::new()))
                .collect()
        },
        enclosing_ret: None,
        enclosing_throws: Vec::new(),
        thrown: Vec::new(),
        forward_names: Vec::new(),
        static_context: static_context_of(&tree, item),
        before_super: false,
        flow: Flow::default(),
        exited: false,
        writing: false,
        mutating: false,
        in_constructor: false,
        init_ctx: None,
        blank_finals: FxHashSet::default(),
        lambda_depth: 0,
        types: FxHashMap::default(),
        locals: FxHashMap::default(),
        diagnostics: Vec::new(),
        scopes: vec![FxHashMap::default()],
        lambda_params: Vec::new(),
        lambda_returns: Vec::new(),
        target: None,
        switch_targets: Vec::new(),
        const_locals: FxHashMap::default(),
        case_values: Vec::new(),
        loop_depth: 0,
        switch_depth: 0,
        labels: Vec::new(),
        loop_breaks: Vec::new(),
        pending_loop_label: None,
        probing: false,
        bool_outcomes: None,
        rethrow_sets: FxHashMap::default(),
    };
    // §8.3.1.2/[§16]: seed the body's already-assigned set with the blank
    // `final` fields that *earlier* initializer bodies of the same class —
    // which run before this one — may have assigned ([§8.3.2], [§8.6],
    // [§8.7], [§8.8]): a static field initializer / static initializer runs
    // after the earlier static ones; an instance field initializer /
    // instance initializer runs after the earlier instance ones; and a
    // constructor runs after *every* instance initializer and instance field
    // initializer ([§8.8.7.1] — the implicit `super()` precedes the whole
    // body, so the instance initializers have already run). A second write to
    // such a field is then the already-assigned error.
    ctx.flow.field_touched = prior_initializer_writes(db, file, &tree, item);
    let mut body = None;
    // §6.5.5.1: the expression forests of body-less items (field
    // initializers, enum constant arguments, annotation element defaults),
    // walked for their type references by [`crate::java::name_check`].
    let mut ctx_orphan_exprs = Vec::new();
    match item_data(&tree, item)? {
        // A method or constructor body ([§8.4]); the return type is the
        // target of a `return` ([§14.17], [§18.5.2.4]).
        hir_def::java::item_tree::ItemData::Method(method) => {
            ctx.enclosing_ret = method
                .sig
                .ret
                .as_ref()
                .map(|ret| resolve_type_ref(db, &ctx.scope, &ctx.resolver, ret));
            // §11.2: the declared throws clause discharges the method's
            // checked-exception liability.
            ctx.enclosing_throws = method
                .sig
                .throws
                .iter()
                .map(|ex| resolve_type_ref(db, &ctx.scope, &ctx.resolver, ex))
                .collect();
            match method.body() {
                Some(body_id) => {
                    body = Some(body_id);
                    let stmts = bodies.body(body_id).stmts.clone();
                    ctx.in_constructor = method.is_constructor();
                    for &param in &bodies.body(body_id).params {
                        ctx.declare_param(param);
                    }
                    // §8.8.7.1: a constructor body's explicit
                    // `this(...)`/`super(...)` invocation bounds the
                    // before-super window — its *arguments* (and any earlier
                    // statements) are evaluated while the supertype constructor
                    // has not run, so `this`/instance references there are an
                    // error ([§15] evaluation order); an invocation that is not
                    // the first statement is itself an error. When there is no
                    // explicit invocation (the implicit `super()` precedes the
                    // whole body) the window is empty.
                    if method.is_constructor() {
                        let first_call = stmts.iter().position(|&stmt| {
                            matches!(
                                bodies.stmt(stmt),
                                hir_expand::body::StmtData::Expr(expr)
                                    if matches!(
                                        bodies.expr(*expr),
                                        hir_expand::body::ExprData::CtorCall { .. }
                                    )
                            )
                        });
                        ctx.before_super = first_call.is_some();
                        if let Some(index) = first_call
                            && index > 0
                            && let hir_expand::body::StmtData::Expr(expr) =
                                bodies.stmt(stmts[index])
                        {
                            ctx.report(TypeError::ConstructorCallNotFirst { expr: *expr });
                        }
                    }
                    ctx.infer_block_statements(&stmts);
                    // §11.2: the body must discharge its checked exceptions.
                    ctx.check_thrown_liability();
                    // §8.4.7: a method whose return type is neither `void`
                    // nor an inferred type variable must not be able to
                    // complete normally — every execution path ends in a
                    // `return` (or `throw`). Constructors and `void` methods
                    // may complete normally.
                    if !method.is_constructor()
                        && ctx
                            .enclosing_ret
                            .as_ref()
                            .is_some_and(|ret| !ret.is_void_like(db) && !ret.is_error(db))
                        && !ctx.exited
                    {
                        // javac's caret sits on the method's closing
                        // brace.
                        ctx.report(TypeError::MissingReturnValue {
                            range: Some(rowan::TextRange::empty(method.range.end())),
                        });
                    }
                }
                // An annotation type element default ([JLS §9.6.2]): a poly
                // expression whose target is the element's return type.
                None => {
                    let default = method.default_expr()?;
                    let _ = ctx.with_target(ctx.enclosing_ret, |this| this.infer_expr(default));
                    ctx_orphan_exprs.push(default);
                }
            }
        }
        hir_def::java::item_tree::ItemData::StaticInit(init) => {
            // §8.7: a static initializer may assign the class's blank
            // `static final` fields once ([§8.3.1.2], [§16]).
            ctx.init_ctx = Some(InitCtx::Static);
            let body_id = init.body?;
            body = Some(body_id);
            for &param in &bodies.body(body_id).params {
                ctx.declare_param(param);
            }
            ctx.infer_block_statements(&bodies.body(body_id).stmts);
        }
        hir_def::java::item_tree::ItemData::InstanceInit(init) => {
            // §8.6: an instance initializer may assign the class's blank
            // `final` instance fields once ([§8.3.1.2], [§16]).
            ctx.init_ctx = Some(InitCtx::Instance);
            let body_id = init.body?;
            body = Some(body_id);
            for &param in &bodies.body(body_id).params {
                ctx.declare_param(param);
            }
            ctx.infer_block_statements(&bodies.body(body_id).stmts);
            // §11.2.2: an initializer cannot declare throws, so every
            // remaining checked exception is unreported.
            ctx.check_thrown_liability();
        }
        // A field initializer ([§8.3.3]): a poly expression whose target is
        // the field's declared type.
        hir_def::java::item_tree::ItemData::Field(field) => {
            // §8.3.1.2/[§8.3.2]: a field initializer runs in the context of
            // its own static/instance kind — a static field initializer may
            // assign a blank `static final` and an instance field initializer
            // a blank `final` instance field, once ([§16]).
            ctx.init_ctx = Some(if field.modifiers.is_static() {
                InitCtx::Static
            } else {
                InitCtx::Instance
            });
            let initializer = field.initializer_expr?;
            let target = resolve_type_ref(db, &ctx.scope, &ctx.resolver, &field.ty);
            // §8.3.3: the names this initializer may not read by simple name
            // — same-class fields of the same static/instance kind declared
            // textually after it.
            ctx.forward_names = forward_field_names(&tree, item, field.modifiers.is_static());
            let _ = ctx.with_target(Some(target), |this| this.infer_expr(initializer));
            ctx_orphan_exprs.push(initializer);
        }
        // Enum constant arguments ([§8.9.1]) — inferred standalone (the
        // constructor resolution is out of scope here).
        hir_def::java::item_tree::ItemData::EnumConstant(constant) => {
            if constant.argument_exprs.is_empty() {
                return None;
            }
            for &arg in &constant.argument_exprs {
                let _ = ctx.infer_expr(arg);
            }
            ctx_orphan_exprs.extend(constant.argument_exprs.iter().copied());
        }
        _ => return None,
    }
    // §6.5.5.1/[§7.5.2]: the unknown-name and on-demand-ambiguity reports of
    // the body's *own* type references (locals it declares, patterns and
    // expression type references; the signature types are covered by the
    // declaration pass). Body-owned references resolve against the same
    // resolver the inference used.
    let mut resolved_diags = Vec::new();
    let body_refs: Vec<(
        crate::java::diagnostics::DiagLocation,
        hir_expand::span::SpannedTypeRef,
    )> = match body {
        Some(body) => crate::java::name_check::body_type_refs(&ctx.tree, body),
        // A field initializer, enum constant arguments or an annotation
        // element default carry their type references as expression
        // forests rather than a [`Body`].
        None => crate::java::name_check::expr_forest_type_refs(&ctx.tree, &ctx_orphan_exprs),
    };
    for (location, spanned) in body_refs {
        let mut issues = Vec::new();
        crate::java::name_check::check_spanned(
            db,
            &ctx.scope,
            &ctx.resolver,
            &spanned,
            &mut issues,
        );
        for issue in issues {
            match issue {
                crate::java::name_check::TypeRefDiag::CannotResolve { name, range } => {
                    resolved_diags.push(TypeError::CannotResolveType {
                        location: location.clone(),
                        name,
                        range,
                    });
                }
                crate::java::name_check::TypeRefDiag::Ambiguous { name, range } => {
                    resolved_diags.push(TypeError::AmbiguousName {
                        location: location.clone(),
                        name,
                        range,
                    });
                }
                crate::java::name_check::TypeRefDiag::ModuleNotAccessible { name, range } => {
                    resolved_diags.push(TypeError::ModuleNotAccessible {
                        location: location.clone(),
                        name,
                        range,
                    });
                }
            }
        }
    }
    ctx.diagnostics.extend(resolved_diags);
    // §6.5.6.1/[§15.27.2]: a local variable captured by a lambda expression —
    // one referenced inside a lambda body — that is *mutated* anywhere in its
    // scope is not effectively final, and every capture of it is an error.
    // The scan is syntactic (independent of the inference it runs beside), so
    // this covers a local reassigned after the capturing lambda too, and only
    // the *final* inference pass's body-tree participates (each `body_types`
    // invocation re-runs it).
    if let Some(body_id) = body {
        let (mutated, captures) = flow::effective_final_scan(&ctx.tree, body_id);
        let mut reported: FxHashSet<(Name, ExprId)> = FxHashSet::default();
        for (name, expr) in captures {
            if mutated.contains(&name) && reported.insert((name.clone(), expr)) {
                ctx.report(TypeError::VariableMustBeEffectivelyFinal { expr, name });
            }
        }
    }
    Some(BodyTypes {
        body,
        exprs: ctx.types,
        locals: ctx.locals,
        diagnostics: ctx.diagnostics,
        field_touched: ctx.flow.field_touched,
    })
}

/// argument lists, where no blank `final` field write is legal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitCtx {
    /// A static initializer or a static field initializer ([JLS §8.7]).
    Static,
    /// constructor ([JLS §8.6], [§8.8]).
    Instance,
}

/// or any other illegal `final`-field write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalFieldWrite {
    /// The write is the legal blank-final initialization.
    Legal,
    /// `variable {f} might already have been assigned`.
    AlreadyAssigned,
    /// to {final|static final} variable {f}`.
    CannotAssign,
}

/// lockstep — so they are bundled here rather than as two loose fields.
#[derive(Clone, Default)]
struct Flow {
    /// The locals definitely assigned at the current position ([§16]).
    definite: FxHashSet<LocalId>,
    /// so far ([§8.3.1.2], [§16]).
    field_touched: FxHashSet<String>,
}

impl Flow {
    /// longer be assigned, so it is the *union*.
    fn join_definite(&mut self, other: &Flow) {
        self.definite.retain(|local| other.definite.contains(local));
        for touched in &other.field_touched {
            self.field_touched.insert(touched.clone());
        }
    }

    /// path's set and the pre-branch set.
    fn union_touched(&mut self, other: &Flow) {
        for touched in &other.field_touched {
            self.field_touched.insert(touched.clone());
        }
    }
}

/// the assignments made before the exits, not the whole body.
#[derive(Clone)]
struct BreakFrame {
    /// `break label` records on exactly this frame. A `switch` has no label.
    label: Option<Name>,
    /// The flows at each `break` targeting this frame reached so far.
    flows: Vec<Flow>,
}

impl BreakFrame {
    /// A fresh frame for the breakable statement about to be inferred.
    fn new(label: Option<Name>) -> Self {
        BreakFrame {
            label,
            flows: Vec::new(),
        }
    }

    /// cannot complete normally.
    fn joined(&self) -> Option<Flow> {
        let mut iter = self.flows.iter();
        let mut joined = iter.next()?.clone();
        for flow in iter {
            joined.join_definite(flow);
        }
        Some(joined)
    }
}

struct InferCtx<'a> {
    db: &'a dyn TyDatabase,
    scope: hir::ResolutionScope,
    tree: Arc<BodyTree>,
    resolver: Resolver,
    access: InvocationContext,
    enclosing_class: Option<Ty>,
    /// class ([§6.5.5.1], [§8.1.3]).
    enclosing_chain: Vec<Ty>,
    /// type ([JLS §18.5.2.4]) of the expressions it returns.
    enclosing_ret: Option<Ty>,
    /// check ([§11.2]).
    enclosing_throws: Vec<Ty>,
    /// `catch` clause handles their type.
    thrown: Vec<(Ty, ExprId)>,
    /// illegal forward reference.
    forward_names: Vec<Name>,
    /// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
    static_context: bool,
    /// error ([`TypeError::CannotReferenceBeforeSuper`]).
    before_super: bool,
    /// locals and the blank-`final` fields already touched (see [`Flow`]).
    flow: Flow,
    /// this point are not definite-assignment errors.
    exited: bool,
    /// [§16]).
    writing: bool,
    /// variables and fields.
    mutating: bool,
    /// its own class, as its initialization ([§8.3.1.2], [§16]).
    in_constructor: bool,
    /// constant argument lists, where no blank `final` field write is legal.
    init_ctx: Option<InitCtx>,
    /// assignment — so the `CannotAssignToFinalVariable` check exempts it.
    blank_finals: FxHashSet<LocalId>,
    /// blank-final-field write inside a lambda is always an error.
    lambda_depth: usize,
    types: FxHashMap<ExprId, Ty>,
    locals: FxHashMap<LocalId, Ty>,
    /// [`Ty::error`]; the diagnostics layer collects them per file.
    diagnostics: Vec<TypeError>,
    /// The lexical scope stack ([JLS §6.3]): innermost first.
    scopes: Vec<FxHashMap<Name, LocalId>>,
    /// [`LocalId`]s, so these are tracked separately from [`Self::scopes`].
    lambda_params: Vec<FxHashMap<Name, Ty>>,
    /// body's type is inferred during overload probing.
    lambda_returns: Vec<Vec<Ty>>,
    /// assignment right-hand side, or a return statement.
    target: Option<Ty>,
    /// as target, not the enclosing method's return type.
    switch_targets: Vec<Option<Ty>>,
    /// of it are constant expressions ([§15.28]).
    const_locals: FxHashMap<LocalId, Const>,
    /// reported as duplicate.
    case_values: Vec<FxHashMap<String, ()>>,
    /// loop or a `switch` ([§14.15]).
    loop_depth: usize,
    /// unlabeled `break` may target the nearest enclosing switch.
    switch_depth: usize,
    /// loop ([§14.16]).
    labels: Vec<(Name, bool)>,
    /// normal-completing arm paths ([§16.2.9]). See [`BreakFrame`].
    loop_breaks: Vec<BreakFrame>,
    /// records on that loop's frame.
    pending_loop_label: Option<Name>,
    /// total-failure path), not once per probed candidate.
    probing: bool,
    /// been inferred.
    bool_outcomes: Option<(Flow, Flow)>,
    /// assignment to it drops the entry ([§14.20]).
    rethrow_sets: FxHashMap<LocalId, Vec<Ty>>,
}

impl<'a> InferCtx<'a> {
    /// candidate is probed in its own fresh inference table.
    #[allow(clippy::too_many_arguments)]

    /// resolved formals are collected in `deferred`.
    #[allow(clippy::too_many_arguments)]

    /// this phase or the applicable ones are ambiguous ([JLS §15.12.2.5]).
    #[allow(clippy::too_many_arguments)]

    /// class, library constructors are `<init>`.
    fn new_expr(
        &mut self,
        expr: ExprId,
        ty: SpannedTypeRef,
        diamond: bool,
        args: &[ExprId],
        target: Option<Ty>,
        anonymous_body: bool,
    ) -> Ty {
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
            let raw = matches!(
                from_target.kind(self.db),
                TyKind::Reference { args, .. } if args.is_empty()
            );
            if raw {
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
    fn check_instantiable(&mut self, expr: ExprId, class_ty: Ty) -> bool {
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
    fn diamond_instantiation(&self, class_ty: Ty, target: Option<Ty>) -> Ty {
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
    fn class_type_var_names(&self, fqn: &Name) -> Vec<Ty> {
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
    fn class_type_param_bounds(&self, fqn: &Name) -> Vec<crate::java::method::MethodTypeParam> {
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
    fn diamond_instantiation_from_ctor_args(
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

/// later static, or vice versa) is legal.
fn forward_field_names(
    tree: &hir_def::java::item_tree::ItemTree,
    field: hir_def::java::item_tree::ItemId,
    static_field: bool,
) -> Vec<Name> {
    // The class-like declaration owning `field`.
    fn owner_of(
        tree: &hir_def::java::item_tree::ItemTree,
        id: hir_def::java::item_tree::ItemId,
        target: hir_def::java::item_tree::ItemId,
    ) -> Option<hir_def::java::item_tree::ItemId> {
        let data = tree.data(id);
        let class_like = data.is_type();
        for &child in data.body() {
            if child == target {
                return class_like.then_some(id);
            }
            if let Some(found) = owner_of(tree, child, target) {
                return Some(found);
            }
        }
        None
    }
    for top in &tree.top {
        if let Some(class_item) = owner_of(tree, *top, field) {
            return tree
                .data(class_item)
                .body()
                .iter()
                .filter(|&&item| item > field)
                .filter_map(|&item| match tree.data(item) {
                    ItemData::Field(later)
                        if later.modifiers.is_static() == static_field && item != field =>
                    {
                        Some(later.name.clone())
                    }
                    _ => None,
                })
                .collect();
        }
    }
    Vec::new()
}

/// implicitly static field, so its argument expressions are a static context.
fn static_context_of(tree: &hir_def::java::item_tree::ItemTree, item: ItemId) -> bool {
    match tree.data(item) {
        ItemData::Method(method) => method.modifiers.is_static(),
        ItemData::Field(field) => field.modifiers.is_static(),
        ItemData::StaticInit(_) | ItemData::EnumConstant(_) => true,
        // Instance methods, constructors, instance initializers and instance
        // fields: `this` is available, so unqualified instance invocations are
        // legal ([§15.12.3]).
        _ => false,
    }
}

/// outside any class-like declaration.
fn enclosing_self_ty(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: ItemId,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
) -> Option<Ty> {
    // Parent links, one walk (the same shape as
    // [`crate::java::resolve::enclosing_type_chain`]).
    fn parents(tree: &hir_def::java::item_tree::ItemTree, map: &mut FxHashMap<ItemId, ItemId>) {
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: ItemId,
            parents: &mut FxHashMap<ItemId, ItemId>,
        ) {
            for &child in tree.data(id).body() {
                parents.insert(child, id);
                walk(tree, child, parents);
            }
        }
        for &top in &tree.top {
            walk(tree, top, map);
        }
    }
    let mut links: FxHashMap<ItemId, ItemId> = FxHashMap::default();
    parents(tree, &mut links);

    let mut current = links.get(&item).copied();
    while let Some(id) = current {
        // Enums and annotations cannot declare type parameters ([§8.9],
        // [§9.6]); their self-type is always raw.
        let declared: Option<&[hir_def::java::item_tree::TypeParam]> = match tree.data(id) {
            hir_def::java::item_tree::ItemData::Class(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Interface(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Record(d) => Some(&d.type_params),
            hir_def::java::item_tree::ItemData::Enum(_)
            | hir_def::java::item_tree::ItemData::Annotation(_) => Some(&[]),
            _ => None,
        };
        if let Some(declared) = declared {
            let fqn = hir::source_class_fqn(db, file, id)?;
            // The declared type variables are in scope as types ([§8.1.2]);
            // their bounds are resolved against the file like any other type.
            let args = declared
                .iter()
                .map(|tp| {
                    let bounds = tp
                        .bounds
                        .iter()
                        .map(|b| resolve_type_ref(db, scope, resolver, b))
                        .collect();
                    Ty::type_var(db, tp.name.clone(), bounds)
                })
                .collect();
            return Some(Ty::reference(db, fqn.as_str(), args));
        }
        current = links.get(&id).copied();
    }
    None
}

/// later write to a seeded blank final is the already-assigned error.
fn prior_initializer_writes(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: ItemId,
) -> FxHashSet<String> {
    use hir_def::java::item_tree::ItemData as I;
    // The item's parent links (the same shape as
    // [`enclosing_self_ty`]'s `parents`).
    fn parents(tree: &hir_def::java::item_tree::ItemTree, map: &mut FxHashMap<ItemId, ItemId>) {
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: ItemId,
            parents: &mut FxHashMap<ItemId, ItemId>,
        ) {
            for &child in tree.data(id).body() {
                parents.insert(child, id);
                walk(tree, child, parents);
            }
        }
        for &top in &tree.top {
            walk(tree, top, map);
        }
    }
    let mut links: FxHashMap<ItemId, ItemId> = FxHashMap::default();
    parents(tree, &mut links);
    // The innermost class-like declaration owning `item`; the sibling
    // initializers live in its body.
    let mut class_item = None;
    let mut current = links.get(&item).copied();
    while let Some(id) = current {
        if tree.data(id).is_type() {
            class_item = Some(id);
            break;
        }
        current = links.get(&id).copied();
    }
    let Some(class_item) = class_item else {
        return FxHashSet::default();
    };

    // The kind of sibling bodies that run *before* `item`: whether they are
    // the static or the instance initializers, and whether `item` is itself a
    // constructor (which runs after every instance initializer).
    let (sibling_is_static, all_prior) = match tree.data(item) {
        I::Field(field) => (field.modifiers.is_static(), false),
        I::StaticInit(_) => (true, false),
        I::InstanceInit(_) => (false, false),
        I::Method(method) => (false, method.is_constructor()),
        _ => return FxHashSet::default(),
    };

    let mut seeded = FxHashSet::default();
    for &child in tree.data(class_item).body() {
        // For a constructor, every instance field initializer and instance
        // initializer precedes the body ([§8.8.7.1]); for a field initializer
        // or initializer, only its earlier same-kind siblings do.
        let is_prior = match tree.data(child) {
            I::Field(f) if all_prior => !f.modifiers.is_static(),
            I::InstanceInit(_) if all_prior => true,
            I::Field(f) => {
                !all_prior && f.modifiers.is_static() == sibling_is_static && child < item
            }
            I::StaticInit(_) => !all_prior && sibling_is_static && child < item,
            I::InstanceInit(_) => !all_prior && !sibling_is_static && child < item,
            _ => false,
        };
        if !is_prior {
            continue;
        }
        // A body-less field has no initializer to run; every body-carrying
        // sibling contributes its already-touched set.
        if let Some(types) = body_types(db, file, child) {
            for touched in &types.field_touched {
                seeded.insert(touched.clone());
            }
        }
    }
    seeded
}

/// purposes of the blank-final-field delegation tracking ([§8.8.7.1]).
fn find_method_item(
    db: &dyn TyDatabase,
    file: FileId,
    method: &crate::java::method::MethodData,
) -> Option<ItemId> {
    let tree = hir::file_item_tree(db, file);
    for top in &tree.top {
        if let Some(found) = find_method_rec(&tree, *top, method) {
            return Some(found);
        }
    }
    None
}

fn find_method_rec(
    tree: &hir_def::java::item_tree::ItemTree,
    id: ItemId,
    method: &crate::java::method::MethodData,
) -> Option<ItemId> {
    use hir_def::java::item_tree::ItemData as I;
    match tree.data(id) {
        I::Method(m)
            if m.name.as_str() == method.name && m.sig.params.len() == method.params.len() =>
        {
            return Some(id);
        }
        _ => {}
    }
    for &child in tree.data(id).body() {
        if let Some(found) = find_method_rec(tree, child, method) {
            return Some(found);
        }
    }
    None
}
