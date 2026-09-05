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

use hir_def::java::item_tree::ItemId;
use hir_expand::{
    body::{BodyId, BodyTree, ExprId, LocalId},
    name::Name,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use vfs::FileId;

mod context;
mod ctx;
mod expr;
mod field;
mod flow;
mod lambda;
mod local;
mod method;
mod new_expr;
mod operator;
mod overload;
mod poly;
mod stmt;
mod switch;

use self::context::*;

use crate::{
    java::const_eval::Const,
    java::db::{TyDatabase, type_params_map_query},
    java::diagnostics::TypeError,
    java::method::{InvocationContext, access_context},
    java::resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    java::ty::Ty,
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

// The `(file, item)` bodies whose `body_types` is currently being computed on
// this thread, innermost last. The blank-final-field seeding of a `this(...)`
// delegation ([`InferCtx::ctor_call`]) resolves the delegation target and runs
// `body_types` on it; a recursive chain (`class A { A() { this(); } }`,
// `Pair() { this(1); }` / `Pair(int) { this(); }`) resolves the target to a
// constructor whose `body_types_query` is still in flight — a salsa
// dependency-graph cycle when another Rayon worker is collecting the same
// body's diagnostics in parallel. The seeding re-entry is skipped when the
// target is already in flight; a *legitimate* chain that bottoms out in
// `super()` seeds every level, because no intermediate target is in flight.
thread_local! {
    static BODY_STACK: RefCell<Vec<(FileId, ItemId)>> = const { RefCell::new(Vec::new()) };
}

/// Whether `body_types` is currently being computed for `(file, item)` on this
/// thread — the in-flight probe of the recursive-`this(...)` guard.
pub(crate) fn body_in_flight(file: FileId, item: ItemId) -> bool {
    BODY_STACK.with(|stack| stack.borrow().iter().any(|&(f, i)| f == file && i == item))
}

/// Scoped push of the current body onto the in-flight-body stack: the
/// `(file, item)` is pushed on construction and popped on drop, so it is
/// exactly the in-flight set while `body_types_impl` runs.
struct BodyScope;

impl BodyScope {
    fn new(file: FileId, item: ItemId) -> Self {
        BODY_STACK.with(|stack| stack.borrow_mut().push((file, item)));
        Self
    }
}

impl Drop for BodyScope {
    fn drop(&mut self) {
        BODY_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

pub(crate) fn body_types_impl(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
) -> Option<BodyTypes> {
    let _scope = BodyScope::new(file, item);
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
        enclosing_class,
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
        for (local, name, expr) in captures {
            if mutated.contains(&local) && reported.insert((name.clone(), expr)) {
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
