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
    body::{BodyId, BodyTree, ExprData, ExprId, LambdaBody, LocalId},
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
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
mod poly;
mod stmt;
mod switch;

use self::poly::*;

use crate::{
    java::const_eval::Const,
    java::db::{TyDatabase, type_params_map_query},
    java::diagnostics::{DiagLocation, TypeError},
    java::inference::{Constraint, Inference, InvocationPhase},
    java::method::{
        InvocationContext, MethodData, access_context, member_set, single_abstract_method,
    },
    java::resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    java::ty::{Ty, TyData, TyKind, boxed_type},
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
    /// argument, inferred standalone.
    fn arg_kinds(&mut self, args: &[ExprId]) -> Vec<ArgInfo> {
        args.iter().map(|arg| self.arg_info(*arg)).collect()
    }

    fn arg_info(&mut self, arg: ExprId) -> ArgInfo {
        let leaves = poly_leaves(&self.tree, arg);
        if leaves.is_empty() {
            // §18.5.2.2: a concrete argument is not a poly expression — its
            // type is its standalone type ([§15.12.2.6]). The enclosing
            // invocation's own target type must not reach it, or a nested
            // generic call inside the argument would be constrained by an
            // unrelated expectation (`new Ctor(..., l.toArray(new T[0]))`
            // would demand `toArray`'s return to be `Ctor`). Only the *poly*
            // arguments are typed by the resolved formal parameter.
            ArgInfo {
                id: arg,
                poly: false,
                leaves: vec![ArgKind::Concrete(
                    self.with_target(None, |this| this.infer_expr(arg)),
                )],
            }
        } else {
            ArgInfo {
                id: arg,
                poly: true,
                leaves: leaves
                    .iter()
                    .map(|leaf| match self.tree.expr(*leaf).clone() {
                        ExprData::Lambda { .. } | ExprData::MethodRef { .. } => ArgKind::Lambda {
                            id: *leaf,
                            arity: poly_arity(&self.tree, *leaf),
                        },
                        ExprData::MethodCall { .. } => ArgKind::Invocation { id: *leaf },
                        ExprData::New { diamond: true, .. } => ArgKind::DiamondNew { id: *leaf },
                        _ => unreachable!(
                            "a poly leaf is a lambda, method reference, call or diamond new"
                        ),
                    })
                    .collect(),
            }
        }
    }

    /// applicable ones are ambiguous.
    fn resolve_call(
        &mut self,
        receiver_ty: &Ty,
        name: &Name,
        arg_kinds: &[ArgInfo],
        target: Option<Ty>,
        ctx: &InvocationContext,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let members = member_set(self.db, &self.scope, receiver_ty, name.as_str(), ctx);
        for phase in [InvocationPhase::Strict, InvocationPhase::Loose] {
            if let Some(chosen) = self.choose_candidate(
                &members,
                arg_kinds,
                phase,
                false,
                target,
                explicit_type_args.clone(),
            ) {
                return Some(chosen);
            }
        }
        self.choose_candidate(
            &members,
            arg_kinds,
            InvocationPhase::Loose,
            true,
            target,
            explicit_type_args,
        )
    }

    /// candidate is probed in its own fresh inference table.
    #[allow(clippy::too_many_arguments)]
    fn choose_candidate(
        &mut self,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        target: Option<Ty>,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let mut applicable: Vec<ApplicableCandidate> = Vec::new();
        for member in members {
            let mut inference = Inference::new();
            let mut deferred = Vec::new();
            // The probe is speculative: diagnostics inside the argument
            // expressions are discarded, matching javac's overload resolution.
            if let Some(invocation) = self.with_probing(|this| {
                this.try_candidate(
                    &mut inference,
                    member,
                    arg_kinds,
                    phase,
                    varargs,
                    target,
                    explicit_type_args.as_deref(),
                    &mut deferred,
                    true,
                )
            }) {
                applicable.push((member.clone(), invocation, deferred));
            }
        }
        if applicable.is_empty() {
            if members.iter().any(|m| m.name == "allOf") {}
            return None;
        }
        // The most specific applicable candidate ([§15.12.2.5]); identical
        // signatures seen through overriding paths collapse to their
        // most-derived declaration (see [`crate::java::method::choose_most_specific`]).
        let pairs: Vec<(MethodData, MethodData)> = applicable
            .iter()
            .map(|(candidate, invocation, _)| (candidate.clone(), invocation.clone()))
            .collect();
        let chosen = crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)?;
        let index = applicable
            .iter()
            .position(|(_, invocation, _)| *invocation == chosen)?;
        let (_, invocation, deferred) = applicable.remove(index);
        Some((invocation, deferred))
    }

    /// resolved formals are collected in `deferred`.
    #[allow(clippy::too_many_arguments)]
    fn try_candidate(
        &mut self,
        inference: &mut Inference,
        method: &MethodData,
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        target: Option<Ty>,
        explicit_type_args: Option<&[Ty]>,
        deferred: &mut Vec<(ExprId, usize)>,
        resolve: bool,
    ) -> Option<MethodData> {
        // §15.12.2.2: explicit type arguments (`obj.<String>m(...)`) bind the
        // method's type parameters directly — the formals, return type and
        // throws clause are substituted with the written arguments and nothing
        // is left to inference.
        let (formals, ret, throws_formals) = match explicit_type_args {
            Some(explicit) => {
                let subst: FxHashMap<Name, Ty> = method
                    .type_params
                    .iter()
                    .zip(explicit.iter().copied())
                    .map(|(tp, ty)| (tp.name.clone(), ty))
                    .collect();
                let formals: Vec<Ty> = method
                    .params
                    .iter()
                    .map(|p| p.substitute(self.db, &subst))
                    .collect();
                let ret = method.ret.substitute(self.db, &subst);
                let throws: Vec<Ty> = method
                    .throws
                    .iter()
                    .map(|t| t.substitute(self.db, &subst))
                    .collect();
                (formals, ret, throws)
            }
            None => inference.register_method(self.db, method),
        };
        // §15.12.2.2/§15.12.2.3/§15.13.2: a lambda or method reference is a
        // poly expression whose type *is* the target functional interface —
        // be compatible with a formal parameter only when that formal is a
        // functional interface ([§9.8]). Otherwise the candidate is not
        // applicable at all: `Collection.toArray(T[])` must not appear
        // applicable to the argument `T[]::new`, or the overload resolution
        // would report an ambiguity javac resolves in favour of
        // `toArray(IntFunction<T[]>)`. A lambda whose parameter list has a
        // different arity than the SAM's is likewise inapplicable ([§15.27.3]).
        for (i, info) in arg_kinds.iter().enumerate() {
            let Some(formal) = formals.get(i).copied() else {
                break;
            };
            for kind in &info.leaves {
                let ArgKind::Lambda { id, arity } = kind else {
                    continue;
                };
                // §15.12.2.2/§15.12.2.3/§15.13.2: a lambda or method reference
                // (a method reference has `arity == None`) is compatible with
                // a formal parameter only when that formal is a functional
                // interface ([§9.8]) — *or* when it is still an inference
                // variable that the enclosing invocation may unify with one
                // (`<T> T id(T)` probed against `Function<String,Integer>`
                // for the argument `id(s -> s.length())`). A proper
                // non-interface type — `Collection.toArray(T[])` for
                // `T[]::new` — makes the candidate inapplicable, or the
                // overload resolution would report an ambiguity javac
                // resolves in favour of `toArray(IntFunction<T[]>)`.
                let bare_var = matches!(
                    formal.kind(self.db),
                    TyKind::InferenceVar(_) | TyKind::TypeVar { .. }
                );
                let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                    if bare_var {
                        continue;
                    }
                    return None;
                };
                if let Some(arity) = arity {
                    if sam.params.len() != *arity {
                        return None;
                    }
                }
                // §15.13.2: a method reference has no syntactic parameter
                // list to arity-check, so its congruence with the SAM — the
                // referenced method set's arity (after the unbound-instance
                // receiver, §15.13.3) and per-parameter compatibility — is
                // checked against the referenced name itself. Without it the
                // `thenComparing(Comparator)` overload of the JDK `Comparator`
                // stays applicable to a zero-argument `Foo::length` argument
                // — its `compare` SAM takes one more parameter than the
                // reference's function descriptor — and [§15.12.2.5]
                // picks nothing: the chained
                // `comparing(Foo::length).thenComparing(Foo::width)` sorts of
                // the wMatcher diff engine would go *ambiguous* rather than
                // resolve the `thenComparing(Function)` overload javac picks.
                if arity.is_none()
                    && let ExprData::MethodRef {
                        qualifier,
                        type_name,
                        name,
                    } = self.tree.expr(*id).clone()
                    && !self.method_ref_congruent(qualifier, type_name.as_ref(), &name, &sam.params)
                {
                    return None;
                }
                // §15.27.3: a block lambda that is not value-compatible — no
                // `return` statement carries a value — is congruent only with
                // a void function result. Without this check a void body stays
                // applicable to a value-returning target
                // (`assertDoesNotThrow(exe, msg)` would go ambiguous against
                // the `ThrowingSupplier` overload).
                if arity.is_some()
                    && let ExprData::Lambda { body, .. } = self.tree.expr(*id).clone()
                    && !sam.ret.is_void_like(self.db)
                    && matches!(body, LambdaBody::Block(_))
                    && !self.lambda_block_has_value(&body)
                {
                    return None;
                }
            }
        }

        // §18.5.2.2: in the loose phase (and for variable-arity invocation,
        // §15.12.2.4) primitive arguments are boxed, so `⟨int → α⟩` yields the
        // boxed lower bound. A poly argument is not boxed: its type is the
        // target functional interface, and it contributes no constraint
        // (§15.12.2.2/§15.12.2.3, §18.5.2.2).
        if varargs {
            if !method.varargs || arg_kinds.len() + 1 < formals.len() {
                return None;
            }
            let (fixed, last) = formals.split_at(formals.len() - 1);
            for (i, info) in arg_kinds.iter().enumerate().take(fixed.len()) {
                let formal = fixed[i];
                if info.poly {
                    deferred.push((info.id, i));
                }
                for kind in &info.leaves {
                    if !self.contribute_leaf(inference, kind, formal, phase) {
                        return None;
                    }
                }
            }
            // §15.12.2.4: a single trailing actual of the array type is used
            // as-is; otherwise the trailing actuals are packed into the array,
            // each related to the element type.
            let rest: Vec<&ArgKind> = arg_kinds
                .iter()
                .skip(fixed.len())
                .flat_map(|info| info.leaves.iter())
                .collect();
            if rest.len() == 1 {
                match rest[0] {
                    ArgKind::Concrete(ty) if ty.is_array(self.db) => {
                        if !self.contribute_leaf(inference, rest[0], last[0], phase) {
                            return None;
                        }
                    }
                    // §15.12.2.4: a single trailing *poly* leaf that is itself
                    // an array — a nested call (`allOf(futures.toArray(...))`)
                    // — is used as-is against the varargs array type, not
                    // packed against the element. `toArray` cannot convert a
                    // `CompletableFuture[]` result to a single
                    // `CompletableFuture<?>` element. A nested invocation is
                    // probed against the array type first (it fails to
                    // resolve there when it is not array-typed) and packed
                    // against the element otherwise.
                    ArgKind::Invocation { .. } | ArgKind::DiamondNew { .. } => {
                        if !self.contribute_leaf(inference, rest[0], last[0], phase) {
                            let element = last[0].element(self.db).copied()?;
                            if !self.contribute_leaf(inference, rest[0], element, phase) {
                                return None;
                            }
                        }
                    }
                    _ => {
                        let element = last[0].element(self.db).copied()?;
                        for kind in rest {
                            if !self.contribute_leaf(inference, kind, element, phase) {
                                return None;
                            }
                        }
                    }
                }
            } else {
                let element = last[0].element(self.db).copied()?;
                for kind in rest {
                    if !self.contribute_leaf(inference, kind, element, phase) {
                        return None;
                    }
                }
            }
        } else {
            if formals.len() != arg_kinds.len() {
                return None;
            }
            for (i, info) in arg_kinds.iter().enumerate() {
                let formal = formals[i];
                if info.poly {
                    deferred.push((info.id, i));
                }
                for kind in &info.leaves {
                    if !self.contribute_leaf(inference, kind, formal, phase) {
                        return None;
                    }
                }
            }
        }

        // §18.5.2.4 (resolution): when the invocation is a poly expression with
        // an expected type, the constraint ⟨R → T⟩ joins the constraint set,
        // so the inference variables are bounded by the target type as well.
        // A *generic* method's invocation type is a poly expression wherever
        // it appears ([JLS §15.12.2.6]); a non-generic one is only a poly
        // expression as a nested *argument* (the `resolve == false` probe of
        // [`Self::choose_nested_candidate`]), where its fixed return type must
        // still be compatible with the enclosing formal.
        let constrains_target = target.is_some() && (!method.type_params.is_empty() || !resolve);
        if let (Some(target), true) = (target, constrains_target) {
            inference.add_constraint(Constraint::Sub(ret, target));
        }

        let build = |resolved: &FxHashMap<u64, Ty>| MethodData {
            name: method.name.clone(),
            owner: method.owner.clone(),
            owner_file: method.owner_file,
            params: formals
                .iter()
                .map(|p| p.substitute_infer(self.db, resolved))
                .collect(),
            ret: ret.substitute_infer(self.db, resolved),
            throws: throws_formals
                .iter()
                .map(|t| t.substitute_infer(self.db, resolved))
                .collect(),
            varargs: method.varargs,
            is_static: method.is_static,
            abstract_: method.abstract_,
            is_final: method.is_final,
            access: method.access,
            declaring_package: method.declaring_package.clone(),
            declaring_top_level: method.declaring_top_level.clone(),
            declaring_interface: method.declaring_interface,
            type_params: method.type_params.clone(),
        };
        if resolve {
            let resolved = match inference.solve_after(self.db, &self.scope, phase) {
                Some(r) => r,
                None => {
                    if method.name == "unmodifiableMap" {}
                    return None;
                }
            };
            // §18.5.4: the resolved invocation type must still satisfy the
            // target — element-level bounds alone do not guarantee array- or
            // parameterized-level compatibility after substitution
            // (`copyOf(T[],int)` with `α=int` yields `int[]`, which does not
            // convert to a `long[]` target).
            let invocation = build(&resolved);
            // A target still carrying inference variables or capture
            // variables (a captured formal of an *enclosing* invocation) is
            // not yet checkable — its own resolution validates it later.
            let target_proper = target.is_some_and(|t| {
                !t.contains_infer_var(self.db) && !t.contains_type_var_named_capture(self.db)
            });
            if let (Some(target), true) = (target, constrains_target && target_proper) {
                let ok = invocation.ret == target
                    || match phase {
                        InvocationPhase::Strict => crate::java::subtyping::strict_conversion(
                            self.db,
                            &self.scope,
                            &invocation.ret,
                            &target,
                        ),
                        InvocationPhase::Loose => crate::java::subtyping::is_assignable(
                            self.db,
                            &self.scope,
                            &invocation.ret,
                            &target,
                        ),
                    };
                if !ok {
                    // A residual mismatch between two parameterizations of
                    // the *same* generic type is wildcard/capture
                    // representation noise left by joint inference — the
                    // bound set has already constrained those arguments
                    // ([§18.2.3]). Only genuinely different types (`int[]`
                    // for a `long[]` target) reject the candidate.
                    let lenient = match (invocation.ret.kind(self.db), target.kind(self.db)) {
                        (
                            TyKind::Reference { name: rn, .. },
                            TyKind::Reference { name: tn, .. },
                        ) => rn == tn,
                        _ => false,
                    };
                    if !lenient {
                        return None;
                    }
                }
            }
            Some(invocation)
        } else if inference.check_consistent(self.db, &self.scope, phase) {
            Some(build(&FxHashMap::default()))
        } else {
            None
        }
    }

    /// `false` when the nested invocation has no applicable method.
    fn contribute_leaf(
        &mut self,
        inference: &mut Inference,
        kind: &ArgKind,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        match kind {
            ArgKind::Concrete(ty) => {
                // §15.12.2.3: loose invocation boxes primitive arguments.
                let ty = match (phase, ty.kind(self.db)) {
                    (InvocationPhase::Loose, TyKind::Primitive(p)) => {
                        Ty::reference(self.db, boxed_type(*p), Vec::new())
                    }
                    _ => *ty,
                };
                inference.add_constraint(Constraint::Sub(ty, formal));
                true
            }
            ArgKind::Lambda { id, .. } => {
                // §15.13.3/§18.5.2.2: a method reference constrains the
                // target functional interface's return type by the referenced
                // method's return — `stream.map(this::f)` infers its element
                // type from `f`'s return, exactly like a lambda body would.
                if let ExprData::MethodRef {
                    qualifier,
                    type_name,
                    name,
                } = self.tree.expr(*id).clone()
                {
                    let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                        return true;
                    };
                    let ref_ret =
                        self.method_ref_return(qualifier, type_name.as_ref(), &name, &sam.params);
                    // §15.13.2: a *void-compatible* reference constrains
                    // nothing — any result the referenced method produces is
                    // discarded (`attributeNode::getDepth` is compatible with
                    // an `Executable` regardless of its return type).
                    if !ref_ret.is_error(self.db)
                        && !ref_ret.is_void_like(self.db)
                        && !sam.ret.is_void_like(self.db)
                    {
                        // §5.1.10: constrain the wildcard bounds the SAM's
                        // captured return stands for (see the lambda case).
                        let target = self.decapture(&sam.ret);
                        // §5.1.7: a primitive result boxes against a
                        // reference-typed bound (`String::length` for a
                        // `Function<String,Integer>`).
                        let boxed = match ref_ret.kind(self.db) {
                            TyKind::Primitive(p) => {
                                Ty::reference(self.db, boxed_type(*p), Vec::new())
                            }
                            _ => ref_ret,
                        };
                        inference.add_constraint(Constraint::Sub(boxed, target));
                    }
                    return true;
                }
                // §18.5.2.2/§15.27.3: a lambda body is inferred against the
                // functional interface's return type; its result constrains
                // that type's instantiation — `map(s -> s + "!")` makes the
                // stream element `String`, not `Object`.
                if let ExprData::Lambda { params, body } = self.tree.expr(*id).clone() {
                    let Some(sam) = single_abstract_method(self.db, &self.scope, &formal) else {
                        return true;
                    };
                    let Some(body_ty) = self.infer_lambda_body_result(*id, &params, body, &sam)
                    else {
                        return true;
                    };
                    // An error-typed body (a speculative probe whose
                    // parameters are still uninstantiated inference variables)
                    // constrains nothing — it must not reject the candidate.
                    if !body_ty.is_error(self.db) {
                        // §5.1.10/§18.5.2.2: the SAM was extracted from a
                        // *captured* formal, so its return carries capture
                        // variables standing for the formal's wildcards; the
                        // body's result constrains the underlying wildcard
                        // bounds, not the captures themselves.
                        let target = self.decapture(&sam.ret);
                        inference.add_constraint(Constraint::Sub(body_ty, target));
                    }
                    // §18.5.2.1/§15.27.3: a lambda with *declared* parameter
                    // types constrains the target functional interface's
                    // type variables — `⟨(P1..Pn) λ -> F⟩` reduces to the
                    // per-parameter constraints `⟨Pᵢ -> Uᵢ⟩`, not to silence.
                    // `Comparator.comparingInt((FieldPair p) -> ...)` binds
                    // the `? super T` of the comparator through `p`'s type;
                    // without it `T` would resolve to `Object` and every
                    // later `thenComparing` in the chain would fail. The SAM
                    // `Uᵢ` was extracted from the *captured* formal, so its
                    // wildcards carry capture variables
                    // ([§5.1.10]); [`Self::decapture`] recovers the bound the
                    // declared type must conform to, exactly as for the return
                    // side above.
                    if sam.params.len() == params.len() {
                        for ((_, declared, _), formal_param) in params.iter().zip(&sam.params) {
                            let Some(tyref) = declared else {
                                continue;
                            };
                            let declared_ty =
                                resolve_type_ref(self.db, &self.scope, &self.resolver, tyref);
                            if !declared_ty.is_error(self.db) {
                                let target = self.decapture(formal_param);
                                inference.add_constraint(Constraint::Sub(declared_ty, target));
                            }
                        }
                    }
                }
                true
            }
            ArgKind::Invocation { id } => self.contribute_invocation(inference, *id, formal, phase),
            // §15.9.3: a diamond class instance creation in an invocation
            // context contributes the created class — its type variables
            // registered in the shared table — constrained by the formal:
            // `synchronizedList(new ArrayList<>())` contributes
            // `ArrayList<α> <: List<T>`, so the target `List<String>` reaches
            // the element type.
            ArgKind::DiamondNew { id } => {
                self.contribute_diamond_new(inference, *id, formal, phase)
            }
        }
    }

    /// `synchronizedList(new ArrayList<>())` against `List<String>`.
    fn contribute_diamond_new(
        &mut self,
        inference: &mut Inference,
        id: ExprId,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        let ExprData::New { ty, args, .. } = self.tree.expr(id).clone() else {
            return true;
        };
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return true;
        };
        let type_params = self.class_type_param_bounds(&name);
        if type_params.is_empty() {
            return true;
        }
        let subst = inference.register_class_type_params(self.db, &type_params);
        // The created type with its type variables as fresh inference vars:
        // `ArrayList<α>`.
        let created = Ty::reference(
            self.db,
            name.clone(),
            type_params
                .iter()
                .map(|tp| {
                    Ty::type_var(self.db, tp.name.clone(), tp.bounds.clone())
                        .substitute(self.db, &subst)
                })
                .collect(),
        );
        // The constructor arguments constrain the variables too
        // (`new Analyzer<>(new BasicInterpreter())`); relate them against the
        // parameterized constructor's formals. Each candidate constructor is
        // probed against a snapshot of the shared table — `LinkedHashMap`
        // declares several single-parameter constructors (`LinkedHashMap(int)`,
        // `LinkedHashMap(Map)`), and only the one whose formals are compatible
        // with the actual arguments may constrain the variables.
        let bare: Vec<Ty> = type_params
            .iter()
            .map(|tp| Ty::type_var(self.db, tp.name.clone(), tp.bounds.clone()))
            .collect();
        let param_class = Ty::reference(self.db, name.clone(), bare.clone());
        let access = self.access.clone();
        let ctor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        let members = member_set(self.db, &self.scope, &param_class, &ctor_name, &access);
        let arg_kinds = self.arg_kinds(&args);
        for member in members {
            if member.params.len() != arg_kinds.len() {
                continue;
            }
            let base = inference.snapshot();
            let formals: Vec<Ty> = member
                .params
                .iter()
                .map(|p| p.substitute(self.db, &subst))
                .collect();
            let mut ok = true;
            for (info, formal) in arg_kinds.iter().zip(&formals) {
                for leaf in &info.leaves {
                    if !self.contribute_leaf(inference, leaf, *formal, phase) {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            if ok && inference.check_consistent(self.db, &self.scope, phase) {
                break;
            }
            inference.restore(base);
        }
        // §15.9.3: the created class is compatible with the formal.
        inference.add_constraint(Constraint::Sub(created, formal));
        true
    }

    /// instead of dead-ending on the captures themselves.
    fn decapture(&self, ty: &Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::TypeVar {
                name,
                bounds,
                lower,
                ..
            } if name.as_str().starts_with("CAP#") => match lower {
                Some(lower) => self.decapture(lower),
                None => bounds
                    .first()
                    .map(|bound| self.decapture(bound))
                    .unwrap_or(*ty),
            },
            TyKind::Reference { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| match arg.kind(self.db) {
                        TyKind::Wildcard(Some(bound)) => {
                            let decaptured = self.decapture(&bound.ty);
                            let kind = Box::new(crate::java::ty::WildcardBound {
                                kind: bound.kind,
                                ty: decaptured,
                            });
                            Ty::wildcard(self.db, Some(kind))
                        }
                        _ => self.decapture(arg),
                    })
                    .collect();
                Ty::reference(self.db, name.clone(), args)
            }
            TyKind::Array(element) => Ty::array(self.db, self.decapture(element)),
            _ => *ty,
        }
    }

    /// resolved formal overwrites them.
    fn infer_lambda_body_result(
        &mut self,
        expr: ExprId,
        params: &[(Name, Option<SpannedTypeRef>, TextRange)],
        body: LambdaBody,
        sam: &MethodData,
    ) -> Option<Ty> {
        self.lambda_params.push(FxHashMap::default());
        for ((name, declared, range), formal) in params.iter().zip(&sam.params) {
            let ty = match declared {
                Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                None => match formal.kind(self.db) {
                    TyKind::TypeVar {
                        lower: Some(lower), ..
                    } => *lower,
                    // §15.27.3: a wildcard SAM parameter specializes the
                    // lambda parameter to the wildcard's bound — `? super X`
                    // to `X` (its lower bound, so members are visible),
                    // `? extends X` likewise to `X`, `?` to `Object`.
                    TyKind::Wildcard(Some(bound)) => bound.ty,
                    TyKind::Wildcard(None) => {
                        Ty::reference(self.db, "java.lang.Object", Vec::new())
                    }
                    _ => *formal,
                },
            };
            self.check_lambda_param_duplicate(expr, name, *range);
            self.lambda_params
                .last_mut()
                .expect("lambda param scope pushed")
                .insert(name.clone(), ty);
        }
        let saved_ret = self.enclosing_ret.replace(self.decapture(&sam.ret));
        // The speculative body inference is its own flow context, exactly
        // like [`Self::lambda_type`]: nothing it does may leak into the
        // enclosing statement's state.
        let saved_throws_ctx = std::mem::replace(&mut self.enclosing_throws, sam.throws.clone());
        let saved_exited = self.exited;
        let saved_flow = self.flow.clone();
        // §15.27/[§8.3.1.2]: a lambda body is never the once-only blank-final
        // field initialization (the lambda may execute any number of times).
        self.lambda_depth += 1;
        // The speculative body inference must leave no liability behind: its
        // entries reuse the *same* expression ids the final re-inference will
        // add, so they are drained here — otherwise
        // [`Self::settle_lambda_thrown`]'s diff would mistake the final pass's
        // entries for pre-existing ones.
        let thrown_before: FxHashSet<ExprId> = self.thrown.iter().map(|(_, e)| *e).collect();
        // §15.27.3: a *void*-compatible target (`Runnable`, `Executable`, …)
        // gives the expression body no target — a statement expression may
        // produce a value that is simply discarded, and constraining it
        // against `void` would wrongly reject the candidate.
        let ret_is_void = sam.ret.is_void_like(self.db);
        // §5.1.10: the SAM was extracted from a *captured* formal, so its
        // return type may be a bare capture variable standing for the
        // formal's wildcard. The body infers against the wildcard bound the
        // capture stands for (`Seq<? extends R>`), not against `CAP#n` — a
        // nested generic invocation constrained by ⟨R → CAP#n⟩ cannot reduce
        // (a parameterized type never converts to an unrelated type variable)
        // and would wrongly reject every applicable candidate. This is the
        // same substitution [`Self::contribute_leaf`] applies to the body's
        // *result* constraint; applying it to the body's *target* keeps both
        // ends of the constraint in wildcard terms.
        let body_target = self.decapture(&sam.ret);
        let result = match body {
            // §15.27.2: an expression lambda's body is a poly expression
            // whose target is the SAM's return type.
            LambdaBody::Expr(expr) if !ret_is_void => {
                Some(self.with_target(Some(body_target), |this| this.infer_expr(expr)))
            }
            LambdaBody::Expr(expr) => {
                self.with_target(None, |this| this.infer_expr(expr));
                None
            }
            LambdaBody::Block(stmt) => {
                // The block's valued `return` expressions are recorded while
                // inferring ([§15.27.3]); their least upper bound is the
                // body's type and constrains the SAM return exactly like an
                // expression body's result — without this, a block lambda
                // would leave the SAM's variables unconstrained (`U :=
                // Object` for `map(v -> { return v == x; })`) and every
                // downstream conversion against the instantiated `Optional`
                // would fail.
                self.lambda_returns.push(Vec::new());
                self.with_target(Some(body_target), |this| this.infer_stmt(stmt));
                let returns = self.lambda_returns.pop().unwrap_or_default();
                match returns.as_slice() {
                    [] => None,
                    [single] => Some(*single),
                    many => Some(crate::java::inference::least_upper_bound(
                        self.db,
                        &self.scope,
                        many,
                    )),
                }
            }
        };
        self.thrown.retain(|(_, expr)| thrown_before.contains(expr));
        self.enclosing_throws = saved_throws_ctx;
        self.exited = saved_exited;
        self.flow = saved_flow;
        self.lambda_depth -= 1;
        self.enclosing_ret = saved_ret;
        self.lambda_params.pop();
        result
    }

    /// `false` when no candidate is applicable against the formal.
    fn contribute_invocation(
        &mut self,
        inference: &mut Inference,
        id: ExprId,
        formal: Ty,
        phase: InvocationPhase,
    ) -> bool {
        let ExprData::MethodCall {
            receiver,
            name,
            type_args,
            args,
        } = self.tree.expr(id).clone()
        else {
            return true;
        };
        let (receiver_ty, mode, _) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        let arg_kinds = self.arg_kinds(&args);
        let explicit = if type_args.is_empty() {
            None
        } else {
            Some(
                type_args
                    .iter()
                    .map(|t| resolve_type_ref(self.db, &self.scope, &self.resolver, t))
                    .collect::<Vec<Ty>>(),
            )
        };
        // The nested invocation is resolved in the *same* phase as its
        // enclosing invocation ([§15.12.2.2], [§15.12.2.3]): a strictly
        // probed member must not admit a loosely resolved argument, or
        // boxed-formal overloads would appear strictly applicable.
        if self.choose_nested_candidate(
            inference,
            &members,
            &arg_kinds,
            phase,
            false,
            &formal,
            explicit.clone(),
        ) {
            return true;
        }
        self.choose_nested_candidate(
            inference, &members, &arg_kinds, phase, true, &formal, explicit,
        )
    }

    /// this phase or the applicable ones are ambiguous ([JLS §15.12.2.5]).
    #[allow(clippy::too_many_arguments)]
    fn choose_nested_candidate(
        &mut self,
        inference: &mut Inference,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        formal: &Ty,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> bool {
        let base = inference.snapshot();
        let mut applicable: Vec<MethodData> = Vec::new();
        for member in members {
            inference.restore(base.clone());
            let mut deferred = Vec::new();
            // Speculative probe: no diagnostics from the argument expressions
            // (see [`Self::with_probing`]).
            if self
                .with_probing(|this| {
                    this.try_candidate(
                        inference,
                        member,
                        arg_kinds,
                        phase,
                        varargs,
                        Some(*formal),
                        explicit_type_args.as_deref(),
                        &mut deferred,
                        false,
                    )
                })
                .is_some()
            {
                applicable.push(member.clone());
            }
        }
        // The probes are speculative: a failed consistency check leaves its
        // partially reduced constraints in the shared worklist, so the base
        // snapshot is *always* reinstalled before anything is lifted.
        if applicable.is_empty() {
            inference.restore(base);
            return false;
        }
        // The most specific applicable member ([§15.12.2.5]); identical
        // signatures seen through overriding paths collapse to their
        // most-derived declaration (see [`crate::java::method::choose_most_specific`]).
        let pairs: Vec<(MethodData, MethodData)> =
            applicable.iter().map(|m| (m.clone(), m.clone())).collect();
        let Some(winner) = crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)
        else {
            inference.restore(base);
            return false;
        };
        inference.restore(base);
        // §18.5.2.1: lift the winner's constraints from the base snapshot —
        // the losing candidates are discarded with it, and only the winner's
        // argument/target constraints join the enclosing bound set (B3).
        let mut deferred = Vec::new();
        let _ = self.with_probing(|this| {
            this.try_candidate(
                inference,
                &winner,
                arg_kinds,
                phase,
                varargs,
                Some(*formal),
                explicit_type_args.as_deref(),
                &mut deferred,
                false,
            )
        });
        true
    }

    /// its expression tree records the target-dependent types.
    fn reinfer_deferred(&mut self, method: &MethodData, deferred: &[(ExprId, usize)]) {
        for (arg, index) in deferred {
            if let Some(formal) = method.params.get(*index) {
                let _ = self.with_target(Some(*formal), |this| this.infer_expr(*arg));
            }
        }
    }

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
