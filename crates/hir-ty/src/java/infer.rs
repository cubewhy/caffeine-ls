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
    body::{
        AssignOp, BinaryOp, BodyId, BodyTree, ExprData, ExprId, LambdaBody, Literal, LocalId,
        PatternData, PatternId, StmtData, StmtId, SwitchArm, SwitchLabel, UnaryOp,
    },
    name::Name,
    span::SpannedTypeRef,
};
use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::{PrimitiveType, TypeRef};
use vfs::FileId;

use crate::{
    java::const_eval::{Const, ConstEnv},
    java::db::{TyDatabase, type_params_map_query},
    java::diagnostics::{DiagLocation, IllegalAccessKind, NonStaticThisKind, TypeError},
    java::inference::{Constraint, Inference, InvocationPhase, least_upper_bound},
    java::method::{
        Access, FieldData, InvocationContext, InvocationMode, MethodData, access_context,
        member_set, member_set_ignoring_access, pick_field, pick_field_ignoring_access,
        pick_method, single_abstract_method,
    },
    java::resolve::{
        Resolver, item_data, resolve_type_ref, scope_for_file, type_argument_bound_violation,
    },
    java::subtyping::supertypes_impl,
    java::ty::{
        Ty, TyData, TyKind, boxed_type, capture_conversion, numeric_promotion, unboxed_primitive,
    },
};

/// The inferred types of a method or constructor body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTypes {
    /// The body the types were inferred for.
    pub body: Option<BodyId>,
    /// The type of every expression reachable from the body's statements,
    /// keyed by its arena id.
    pub exprs: FxHashMap<ExprId, Ty>,
    /// The type of every local of the body — parameters, declared locals,
    /// for-loop variables, catch parameters — keyed by its arena id.
    pub locals: FxHashMap<LocalId, Ty>,
    /// The type errors reported while inferring the body, in report order.
    pub diagnostics: Vec<TypeError>,
    /// The blank `final` fields of the enclosing class that the body's
    /// execution *touches* on some path ([§8.3.1.2], [§16]): every name
    /// assigned by a plain `name = …`/`this.name = …` reachable through the
    /// body's flow. A later sibling initializer body (or a constructor after
    /// the instance initializers) seeds its already-assigned set with these,
    /// so a second write to the same blank final is the already-assigned
    /// error.
    pub field_touched: FxHashSet<String>,
}

/// Infers the types of the body of `item` in `file`, memoized per (file,
/// item) by the tracked query in [`crate::java::db`]. `None` when the item has no
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
        let (mutated, captures) = effective_final_scan(&ctx.tree, body_id);
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

/// The kind of *initializer* body being inferred — the only contexts in which
/// a blank `final` field may be assigned once ([JLS §8.3.1.2], [§16]). A
/// static field initializer and a static initializer are **static** contexts;
/// an instance field initializer, an instance initializer and a constructor
/// are **instance** contexts. `None` for method bodies and enum constant
/// argument lists, where no blank `final` field write is legal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitCtx {
    /// A static initializer or a static field initializer ([JLS §8.7]).
    Static,
    /// An instance initializer, an instance field initializer or a
    /// constructor ([JLS §8.6], [§8.8]).
    Instance,
}

/// The verdict on a write to a `final` field ([JLS §8.3.1.2], [§16]): the
/// once-only blank-`final` initialization, a second (already-assigned) write,
/// or any other illegal `final`-field write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalFieldWrite {
    /// The write is the legal blank-final initialization.
    Legal,
    /// The write targets a blank final already assigned on some path — javac:
    /// `variable {f} might already have been assigned`.
    AlreadyAssigned,
    /// Any other illegal `final`-field write — javac: `cannot assign a value
    /// to {final|static final} variable {f}`.
    CannotAssign,
}

/// The definite-assignment flow state of the body ([JLS §16]): which locals
/// are definitely assigned, and which blank `final` fields of the enclosing
/// class are *touched* (assigned on some path) so a later write to one is the
/// already-assigned error ([§8.3.1.2]). The two sets are threaded through the
/// branch machinery together — a branch save/restore/join must move both in
/// lockstep — so they are bundled here rather than as two loose fields.
#[derive(Clone, Default)]
struct Flow {
    /// The locals definitely assigned at the current position ([§16]).
    definite: FxHashSet<LocalId>,
    /// The blank `final` fields of the enclosing class assigned on *some* path
    /// so far ([§8.3.1.2], [§16]).
    field_touched: FxHashSet<String>,
}

impl Flow {
    /// §16.1: the *definite-assignment* join of two paths — a local (or blank
    /// final field) is definitely assigned after a join only when it was
    /// definitely assigned on *every* incoming path, so the definite set is
    /// the intersection. The field-touched set, by contrast, records "may
    /// already be assigned": a blank final that either path touched may no
    /// longer be assigned, so it is the *union*.
    fn join_definite(&mut self, other: &Flow) {
        self.definite.retain(|local| other.definite.contains(local));
        for touched in &other.field_touched {
            self.field_touched.insert(touched.clone());
        }
    }

    /// The field-touched union used where an abrupt-completing branch
    /// contributes no *definite* assignments but may still have touched a
    /// blank final ([§16]): the surviving field is the union of the surviving
    /// path's set and the pre-branch set.
    fn union_touched(&mut self, other: &Flow) {
        for touched in &other.field_touched {
            self.field_touched.insert(touched.clone());
        }
    }
}

/// §16.2.10/[§16.2.12]: the definite-assignment flows captured at the `break`
/// statements that target one enclosing loop — the only paths by which a
/// constant-`true` loop ([§16.1.1]) completes normally (JLS Example 16-1).
/// Each `break` records the [`Flow`] at the break, so the after-loop join
/// collects exactly the assignments made before the exits, not the whole body.
#[derive(Clone)]
struct LoopBreakFrame {
    /// The label naming this loop ([§14.14]), when it has one — a labeled
    /// `break label` records on exactly this frame.
    label: Option<Name>,
    /// The flows at each `break` targeting this loop reached so far.
    flows: Vec<Flow>,
}

impl LoopBreakFrame {
    /// A fresh frame for the loop about to be inferred.
    fn new(label: Option<Name>) -> Self {
        LoopBreakFrame {
            label,
            flows: Vec::new(),
        }
    }

    /// The join of every recorded break flow ([§16.1]): a local is definitely
    /// assigned after the loop only when *every* break path assigned it. `None`
    /// when no break was recorded — the loop cannot complete normally.
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
    /// Every enclosing class-like declaration, innermost first ([§6.3]):
    /// their static members are in scope by simple name inside a nested
    /// class ([§6.5.5.1], [§8.1.3]).
    enclosing_chain: Vec<Ty>,
    /// The return type of the enclosing method or constructor: the target
    /// type ([JLS §18.5.2.4]) of the expressions it returns.
    enclosing_ret: Option<Ty>,
    /// The declared `throws` clause of the enclosing method or constructor
    /// ([§8.4.6]): the discharge targets of the checked-exception liability
    /// check ([§11.2]).
    enclosing_throws: Vec<Ty>,
    /// Checked exceptions thrown so far at the current position and not yet
    /// discharged by a catch clause, with the expression that threw them
    /// ([§11.2]): entries are appended by invocations of throwing methods
    /// ([§11.2.1]) and `throw` statements ([§14.18]), and removed when a
    /// `catch` clause handles their type.
    thrown: Vec<(Ty, ExprId)>,
    /// The names of same-class fields declared *textually after* the field
    /// whose initializer is currently being inferred and of the same
    /// static/instance kind ([§8.3.3]): reading one by simple name is an
    /// illegal forward reference.
    forward_names: Vec<Name>,
    /// Whether the body is in a static context
    /// ([JLS §8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)):
    /// the body of a static method, a static field initializer or a static
    /// initializer, where `this` is unavailable. An unqualified invocation of
    /// an instance method from such a body is a compile-time error
    /// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
    static_context: bool,
    /// Whether the body of the *constructor* currently being inferred is
    /// before its supertype constructor has been called
    /// ([JLS §8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
    /// the explicit `this(...)`/`super(...)` invocation has not been reached
    /// yet (or the body has none, in which case the implicit `super()` runs
    /// before the first statement and the window is empty). While set, a
    /// reference to `this`, `super` or an instance member is a compile-time
    /// error ([`TypeError::CannotReferenceBeforeSuper`]).
    before_super: bool,
    /// The definite-assignment flow state ([JLS §16]): the definitely-assigned
    /// locals and the blank-`final` fields already touched (see [`Flow`]).
    flow: Flow,
    /// Whether control flow has exited (return/break/continue/throw/yield)
    /// on every path to the current position ([§14.22] in effect): reads past
    /// this point are not definite-assignment errors.
    exited: bool,
    /// Whether the expression currently being inferred is the left-hand side
    /// of a simple assignment — a write, not a value read ([§15.26.1],
    /// [§16]).
    writing: bool,
    /// Whether the expression currently being inferred is the target of a
    /// write — an assignment (simple or compound) or an increment/decrement
    /// ([§15.26], [§15.14], [§15.15.1]). Unlike [`Self::writing`] (which only
    /// marks the *simple*-assignment left-hand side, for the §16 definite-
    /// assignment rule), this marks every mutating target, so the
    /// `CannotAssignToFinalVariable` check can reject writes to `final`
    /// variables and fields.
    mutating: bool,
    /// Whether the body being inferred is a constructor
    /// ([JLS §8.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8)):
    /// a **blank** `final` field may be assigned once, in a constructor of
    /// its own class, as its initialization ([§8.3.1.2], [§16]).
    in_constructor: bool,
    /// The *initializer* context of the body, when it is one
    /// ([JLS §8.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.6),
    /// [§8.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.7)):
    /// a static initializer or static field initializer is a **static**
    /// context in which a blank `static final` field may be assigned once; an
    /// instance initializer, instance field initializer or constructor is an
    /// **instance** context in which a blank `final` instance field may be
    /// assigned once ([§8.3.1.2], [§16]). `None` for method bodies and enum
    /// constant argument lists, where no blank `final` field write is legal.
    init_ctx: Option<InitCtx>,
    /// The **blank** `final` locals of the body ([§4.12.4], [§16]): declared
    /// `final` with no initializer. Unlike every other `final` variable
    /// (parameters, catch/foreach/resource variables, finals with an
    /// initializer), a blank one may still be assigned — once, by definite
    /// assignment — so the `CannotAssignToFinalVariable` check exempts it.
    blank_finals: FxHashSet<LocalId>,
    /// The nesting depth of enclosing lambda bodies ([JLS §15.27]): a lambda
    /// body can never be the once-only blank-`final`-field initialization —
    /// the lambda may execute any number of times ([§8.3.1.2], [§16]) — so a
    /// blank-final-field write inside a lambda is always an error.
    lambda_depth: usize,
    types: FxHashMap<ExprId, Ty>,
    locals: FxHashMap<LocalId, Ty>,
    /// The type errors reported so far, in report order ([§14.4.1], [§8.3.3]).
    /// Every entry corresponds to a source construct the compiler degrades to
    /// [`Ty::error`]; the diagnostics layer collects them per file.
    diagnostics: Vec<TypeError>,
    /// The lexical scope stack ([JLS §6.3]): innermost first.
    scopes: Vec<FxHashMap<Name, LocalId>>,
    /// The lambda parameter scopes in effect ([JLS §6.3], [§15.27.2]): a
    /// lambda's parameters are in scope throughout its body, shadowed by any
    /// locals declared inside. The lambda expression itself carries no
    /// [`LocalId`]s, so these are tracked separately from [`Self::scopes`].
    lambda_params: Vec<FxHashMap<Name, Ty>>,
    /// The types of the valued `return` expressions seen so far in each
    /// enclosing *block* lambda body, innermost frame last ([§15.27.3]):
    /// the frame stack mirrors [`Self::lambda_params`]' nesting, and a
    /// frame's contents are the result expressions from which the block
    /// body's type is inferred during overload probing.
    lambda_returns: Vec<Vec<Ty>>,
    /// The expected type of the expression currently being inferred — set
    /// where the context fixes the type: a declaration initializer, an
    /// assignment right-hand side, or a return statement.
    target: Option<Ty>,
    /// The target types of the enclosing switch expressions, innermost last
    /// ([JLS §14.21]): a `yield` value has the type of its switch expression
    /// as target, not the enclosing method's return type.
    switch_targets: Vec<Option<Ty>>,
    /// The constant variables in scope ([JLS §4.12.4]): a `final` local whose
    /// initializer was itself a constant expression, with its value — reads
    /// of it are constant expressions ([§15.28]).
    const_locals: FxHashMap<LocalId, Const>,
    /// The constant values of the case labels seen in the enclosing switch,
    /// innermost last ([§14.11.1]): a label repeating an earlier value is
    /// reported as duplicate.
    case_values: Vec<FxHashMap<String, ()>>,
    /// The nesting depth of enclosing loops — `while`, `do`, `for` and
    /// enhanced `for` ([§14.12]-[§14.14]): an unlabeled `continue` is legal
    /// only inside a loop ([§14.16]) and an unlabeled `break` only inside a
    /// loop or a `switch` ([§14.15]).
    loop_depth: usize,
    /// The nesting depth of enclosing `switch` statements ([§14.11]): an
    /// unlabeled `break` may target the nearest enclosing switch.
    switch_depth: usize,
    /// The labels in scope, innermost last: `(name, is_loop)` — `break label`
    /// may target any labeled statement, a labeled `continue` only a labeled
    /// loop ([§14.16]).
    labels: Vec<(Name, bool)>,
    /// §16.2.10: the definite-assignment flows at the `break` statements that
    /// target each enclosing loop — one frame per loop, innermost first. A
    /// constant-condition loop ([§16.1.1]) completes normally only through
    /// those breaks, so their flows join into the after-loop state (JLS
    /// Example 16-1). See [`LoopBreakFrame`].
    loop_breaks: Vec<LoopBreakFrame>,
    /// Whether the current inference is *speculative* — the applicability
    /// probe of an overload candidate ([§15.12.2]): like javac, diagnostics
    /// from speculatively attributed arguments are discarded, so a nested
    /// resolution failure is reported once (by the final re-inference or the
    /// total-failure path), not once per probed candidate.
    probing: bool,
    /// §16.1.2–[§16.1.5]: the definite-assignment flows after the
    /// most-recently-inferred boolean expression when it evaluates to `true`
    /// and to `false`. Every expression but the conditional boolean forms
    /// (`&&`, `||`, `!`, `?:`) leaves both outcomes equal to the
    /// after-expression flow ([§16.1.7]); the conditional forms split them so
    /// the enclosing statement feeds each branch its own flow. Consumed by
    /// [`Self::check_condition`]'s callers; `None` before an expression has
    /// been inferred.
    bool_outcomes: Option<(Flow, Flow)>,
    /// The precise rethrow set of each catch parameter in scope
    /// ([JLS §11.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-11.html#jls-11.2.2)):
    /// the checked exceptions the try block can throw that are assignable to
    /// the parameter's type and were not caught by an earlier clause. A
    /// `throw e` of such a parameter throws these types — not the parameter's
    /// declared type — provided the parameter stays effectively final; an
    /// assignment to it drops the entry ([§14.20]).
    rethrow_sets: FxHashMap<LocalId, Vec<Ty>>,
}

impl<'a> InferCtx<'a> {
    fn error(&self) -> Ty {
        Ty::error(self.db)
    }

    /// Records a type error. The construct it reports degrades to
    /// [`Ty::error`] so that downstream resolution of the body keeps working;
    /// the diagnostics layer collects these per file via
    /// [`BodyTypes::diagnostics`]. Suppressed while speculatively probing an
    /// overload candidate ([`Self::probing`]): javac also discards the
    /// diagnostics of speculatively attributed arguments, so a nested failure
    /// is reported once — by the chosen candidate's re-inference or by the
    /// total-failure path — not once per probed overload.
    fn report(&mut self, diagnostic: TypeError) {
        if self.probing {
            return;
        }
        self.diagnostics.push(diagnostic);
    }

    /// Infers `expr` under the expected type `target`, restoring the previous
    /// target afterwards. The target participates in method invocation type
    /// inference ([JLS §18.5.2.4]).
    fn with_target<T>(&mut self, target: Option<Ty>, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.target;
        self.target = target;
        let result = f(self);
        self.target = saved;
        result
    }

    /// Consumes and returns the true/false outcome flows of the most recently
    /// inferred expression ([§16.1.2]–[§16.1.5]). Every expression records
    /// them (a non-boolean form sets both to the after-flow, [§16.1.7]); falls
    /// back to the current flow for a bare expression that skipped the record
    /// (e.g. a condition degraded to an error).
    fn take_bool_outcomes(&mut self) -> (Flow, Flow) {
        self.bool_outcomes
            .take()
            .unwrap_or_else(|| (self.flow.clone(), self.flow.clone()))
    }

    /// Whether the boolean expression `id` is a constant expression
    /// ([JLS §15.29], [§16.1.1]) whose value the flow analysis may rely on:
    /// a literal `true`/`false`, or a `&&`/`||`/`!`/`?:`/comparison folding of
    /// constant operands. The constant-variable environment
    /// ([§4.12.4]) is captured at the current position.
    fn const_bool(&self, id: ExprId) -> Option<bool> {
        match self.const_value(id) {
            Some(Const::Bool(b)) => Some(b),
            _ => None,
        }
    }

    /// Whether the boolean expression `id` is one whose *conditional* flow
    /// analysis the analysis specializes: `&&`, `||`, `!` or `?:` — the forms
    /// of [§16.1.2]–[§16.1.5], whose operands run under the true/false flows of
    /// their left context. Any other boolean expression is analyzed under both
    /// outcomes identically ([§16.1.7]). Parentheses are transparent
    /// ([§16.1.7]: `(a && b)` behaves like `a && b`).
    fn is_bool_flow_expr(&self, id: ExprId) -> bool {
        match self.tree.expr(id).clone() {
            ExprData::Paren(inner) => self.is_bool_flow_expr(inner),
            ExprData::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                ..
            } => true,
            ExprData::Unary {
                op: UnaryOp::Not, ..
            } => true,
            ExprData::Conditional { .. } => true,
            _ => false,
        }
    }

    /// Runs `f` in *speculative* mode: diagnostics reported inside are
    /// discarded ([`Self::probing`]). Used for the applicability probes of
    /// overload resolution ([§15.12.2]).
    fn with_probing<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.probing, true);
        let result = f(self);
        self.probing = saved;
        result
    }

    fn primitive(&self, p: PrimitiveType) -> Ty {
        Ty::primitive(self.db, p)
    }

    /// The boxed form of a primitive type ([§5.1.7]): `int` → `Integer`.
    fn box_primitive(&self, ty: Ty) -> Ty {
        let TyKind::Primitive(p) = ty.kind(self.db) else {
            return ty;
        };
        Ty::reference(self.db, boxed_type(*p), Vec::new())
    }

    fn string(&self) -> Ty {
        Ty::reference(self.db, "java.lang.String", Vec::new())
    }

    fn is_string(&self, ty: Ty) -> bool {
        matches!(ty.kind(self.db), TyKind::Reference { name, .. } if name.as_str() == "java.lang.String")
    }

    /// Whether `ty` is `boolean` in a condition position ([JLS §14.9], [§15.25.1]):
    /// a primitive `boolean`, or a boxed `Boolean` after unboxing ([§5.1.8]).
    fn is_boolean(&self, ty: Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(PrimitiveType::Boolean) => true,
            TyKind::Reference { name, .. } => {
                unboxed_primitive(name.as_str()) == Some(PrimitiveType::Boolean)
            }
            _ => false,
        }
    }

    /// Whether `ty` is numeric for a unary/binary numeric or shift operator
    /// ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1),
    /// [§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2)):
    /// a primitive other than `boolean`, or a boxed primitive after unboxing.
    fn is_numeric_operand(&self, ty: Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(p) => !matches!(p, PrimitiveType::Boolean),
            TyKind::Reference { name, .. } => match unboxed_primitive(name.as_str()) {
                Some(p) => !matches!(p, PrimitiveType::Boolean),
                None => false,
            },
            _ => false,
        }
    }

    /// Whether `ty` is a reference type in the loose sense used by the
    /// comparison and castability checks: a class, interface, array, type
    /// variable or intersection type ([JLS §4.3]).
    fn is_reference_like(&self, ty: Ty) -> bool {
        matches!(
            ty.kind(self.db),
            TyKind::Reference { .. }
                | TyKind::Array(_)
                | TyKind::TypeVar { .. }
                | TyKind::Intersection(_)
                | TyKind::Null
        )
    }

    /// Infers the condition expression of an `if`/`while`/`do`/`for`/`assert`
    /// ([§14.9], [§14.11], [§14.16]), the scrutinee-free condition of a
    /// conditional expression ([§15.25.1]) and the boolean operand positions
    /// of `!`, `&&`/`||`. A condition that is not `boolean` degrades to the
    /// error type and is reported.
    fn check_condition(&mut self, cond: ExprId) {
        let ty = self.infer_expr(cond);
        // A condition that already failed to type (an unresolved name, a
        // failed call) has reported its own error — do not cascade.
        if ty.is_error(self.db) {
            return;
        }
        if !self.is_boolean(ty) {
            self.types.insert(cond, self.error());
            self.report(TypeError::NonBooleanCondition {
                expr: cond,
                found: ty,
            });
        }
    }

    /// Whether the two operand types of an equality or relational operator are
    /// comparable ([§15.20], [§15.21]): both numeric (after boxing/unboxing),
    /// both boolean-like, or reference types that could be related. The
    /// provably-unrelated reference case (`String == Integer`) is rejected by
    /// demanding a subtype link when exactly one operand unboxes.
    fn comparable(&self, a: Ty, b: Ty) -> bool {
        let boolean_like = |t: Ty| match t.kind(self.db) {
            TyKind::Primitive(PrimitiveType::Boolean) => true,
            TyKind::Reference { name, .. } => {
                unboxed_primitive(name.as_str()) == Some(PrimitiveType::Boolean)
            }
            _ => false,
        };
        // §15.21.3: `null` is comparable with a reference type only.
        if a.is_null(self.db) || b.is_null(self.db) {
            let other = if a.is_null(self.db) { b } else { a };
            return self.is_reference_like(other);
        }
        if a.is_error(self.db) || b.is_error(self.db) {
            return true;
        }
        let (a_num, b_num) = (self.is_numeric_operand(a), self.is_numeric_operand(b));
        let (a_bool, b_bool) = (boolean_like(a), boolean_like(b));
        if a_num && b_num {
            return true;
        }
        if a_bool && b_bool {
            return true;
        }
        if a_num || b_num {
            return false;
        }
        if !self.is_reference_like(a) || !self.is_reference_like(b) {
            return false;
        }
        // A reference pair where one operand unboxes to a primitive and the
        // other does not (§15.21.3): comparable only when the types are
        // related (`Number` vs `Integer`), not when provably unrelated
        // (`String` vs `Integer`).
        let a_unboxes = matches!(a.kind(self.db), TyKind::Reference { name, .. } if unboxed_primitive(name.as_str()).is_some());
        let b_unboxes = matches!(b.kind(self.db), TyKind::Reference { name, .. } if unboxed_primitive(name.as_str()).is_some());
        if a_unboxes != b_unboxes {
            return crate::java::subtyping::is_subtype(self.db, &self.scope, &a, &b)
                || crate::java::subtyping::is_subtype(self.db, &self.scope, &b, &a);
        }
        true
    }

    /// Whether `from` can be cast to `to` by a casting conversion
    /// ([JLS §5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.5)):
    /// identity, primitive widening/narrowing with boxing/unboxing, or a
    /// reference widening/narrowing cast.
    fn castable(&self, from: Ty, to: Ty) -> bool {
        if from == to {
            return true;
        }
        // §5.1.10: a wildcard is not a valid expression type — a value typed
        // by one carries its capture instead, so the cast is decided against
        // the captured type variable.
        let from = if matches!(from.kind(self.db), TyKind::Wildcard(_)) {
            capture_conversion(self.db, &self.scope, from)
        } else {
            from
        };
        if from.is_null(self.db) && self.is_reference_like(to) {
            return true;
        }
        match (from.kind(self.db), to.kind(self.db)) {
            // §5.5: primitive-to-primitive casts are always casting conversions
            // (widening or narrowing, never to `boolean` — see the reference arm).
            (TyKind::Primitive(_), TyKind::Primitive(_)) => true,
            // §5.1.7: boxing, optionally followed by a reference widening.
            (TyKind::Primitive(f), TyKind::Reference { .. }) => {
                let boxed = Ty::reference(self.db, boxed_type(*f), Vec::new());
                boxed == to || crate::java::subtyping::is_subtype(self.db, &self.scope, &boxed, &to)
            }
            // §5.1.8/§5.5: unboxing, optionally followed by a *widening*
            // primitive conversion — an unbox-then-narrow cast (`(int) aLong`)
            // is not a casting conversion. Any other reference needs a
            // narrowing reference conversion ([§5.1.6.3]) to the wrapper
            // class *of the target* followed by unboxing: `(int) obj` is a
            // casting conversion (Object → Integer narrows), while
            // `(char) anInteger` is not (Integer and Character are provably
            // distinct finals, [§5.5.1]).
            (TyKind::Reference { name, .. }, TyKind::Primitive(t)) => {
                match unboxed_primitive(name.as_str()) {
                    Some(p) => p == *t || crate::java::subtyping::widening_primitive(p, *t),
                    None => {
                        let boxed = Ty::reference(self.db, boxed_type(*t), Vec::new());
                        self.reference_castable(from, boxed)
                    }
                }
            }
            // §5.5.1: no class cast to an array except the object supertypes.
            (TyKind::Reference { name, .. }, TyKind::Array(_)) => matches!(
                name.as_str(),
                "java.lang.Object" | "java.lang.Cloneable" | "java.io.Serializable"
            ),
            (TyKind::Array(_), TyKind::Array(_)) => true,
            (TyKind::Array(_), TyKind::Reference { name, .. }) => matches!(
                name.as_str(),
                "java.lang.Object" | "java.lang.Cloneable" | "java.io.Serializable"
            ),
            (TyKind::TypeVar { .. }, TyKind::Reference { .. }) => true,
            (TyKind::Reference { .. }, TyKind::Reference { .. }) => {
                self.reference_castable(from, to)
            }
            _ => false,
        }
    }

    /// Whether a reference-to-reference casting conversion exists
    /// ([JLS §5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.5),
    /// [§5.5.1], [§5.1.6.3]): the cast fails only when the two types are
    /// *provably distinct* — both class-like (`class`/`enum`/`record`) with
    /// neither a subtype of the other, which single inheritance makes final:
    /// no common subclass can ever implement both. Casts involving interfaces,
    /// arrays or unresolvable types always succeed here (the runtime check may
    /// still fail; that is not a compile-time error per §5.5.1).
    fn reference_castable(&self, from: Ty, to: Ty) -> bool {
        let sub = crate::java::subtyping::is_subtype(self.db, &self.scope, &from, &to);
        let sup = crate::java::subtyping::is_subtype(self.db, &self.scope, &to, &from);
        if sub || sup {
            return true;
        }
        match (
            crate::java::subtyping::class_like_and_final(self.db, &self.scope, &from),
            crate::java::subtyping::class_like_and_final(self.db, &self.scope, &to),
        ) {
            (Some((true, _)), Some((true, _))) => false,
            _ => true,
        }
    }

    /// Infers a `switch` selector and checks its type
    /// ([JLS §14.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.1)):
    /// the selector must be convertible to `int` — a primitive `char`,
    /// `byte`, `short` or `int`, one of their boxed types, or an unboxing
    /// reference — or be a `String` or an enum type.
    fn infer_switch_selector(&mut self, scrutinee: ExprId) -> Ty {
        let ty = self.infer_expr(scrutinee);
        if !ty.is_error(self.db) && !self.switchable(&ty) {
            self.report(TypeError::SwitchSelectorType {
                expr: scrutinee,
                found: ty,
            });
        }
        ty
    }

    /// §14.11.1/§15.28: whether the arms of a switch expression cover every
    /// selector value. A `default` label is always exhaustive; an enum
    /// selector requires every constant to be named by some label.
    fn switch_is_exhaustive(&self, selector: &Ty, arms: &[SwitchArm]) -> bool {
        let mut covered: Vec<Name> = Vec::new();
        for arm in arms {
            for label in &arm.labels {
                if let SwitchLabel::Expr(e) = label
                    && let ExprData::Var(name) = self.tree.expr(*e).clone()
                {
                    covered.push(name);
                }
            }
        }
        let has_default = arms.iter().any(|arm| {
            arm.labels.iter().any(|label| {
                matches!(label, SwitchLabel::Expr(e) if matches!(self.tree.expr(*e).clone(), ExprData::Missing))
            })
        });
        if has_default {
            return true;
        }
        match crate::java::subtyping::enum_constants(self.db, &self.scope, selector) {
            Some(constants) => constants
                .iter()
                .all(|constant| covered.iter().any(|covered| covered == constant)),
            None => true,
        }
    }

    /// §14.11.1: whether the selector type is supported by `switch`. A    /// primitive selector must be one of the int-compatible types
    /// (`char`, `byte`, `short`, `int`); any reference or type-variable
    /// selector is supported ([§14.11]: pattern labels match arbitrary
    /// reference types), while `long`, `float`, `double` and `boolean` are
    /// never selectable.
    fn switchable(&self, ty: &Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(p) => matches!(
                p,
                PrimitiveType::Byte
                    | PrimitiveType::Short
                    | PrimitiveType::Char
                    | PrimitiveType::Int
            ),
            TyKind::Reference { .. } | TyKind::TypeVar { .. } | TyKind::Array(_) => true,
            _ => false,
        }
    }

    /// A `case` label of a switch with the given selector
    /// ([JLS §14.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.1)):
    /// against an enum selector a bare-name label is the enum constant of that
    /// name ([§8.9.1]) — ordinary name resolution does not see constants of a
    /// type, so it is resolved here; every other label is an ordinary
    /// constant expression, which must be assignable to the selector.
    fn infer_switch_label(&mut self, label: ExprId, selector: &Ty) {
        if let ExprData::Var(name) = self.tree.expr(label).clone()
            && let Some(constants) =
                crate::java::subtyping::enum_constants(self.db, &self.scope, selector)
            && constants.iter().any(|constant| constant == &name)
        {
            self.types.insert(label, selector.clone());
            return;
        }
        let ty = self.infer_expr(label);
        // The `default` label lowers as a `Missing` expression and has no
        // type; anything else must be assignable to the selector ([§14.11.1]).
        if !ty.is_error(self.db)
            && !selector.is_error(self.db)
            && !matches!(self.tree.expr(label).clone(), ExprData::Missing)
            && !crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, selector)
            // A label sits in assignment context ([§5.2]), so an int
            // *constant* also narrows to a `byte`, `short` or `char`
            // selector when its value is representable there ([§5.1.3]) —
            // `case 16` of a `byte` selector is legal.
            && !self.constant_narrowable(label, ty.clone(), selector.clone())
        {
            self.report(TypeError::IncompatibleTypes {
                expr: label,
                found: ty,
                expected: *selector,
            });
        }
        // §14.11.1/§15.28: a primitive- or String-selector label must be a
        // constant expression; labels of one switch may not repeat.
        self.check_case_label(label, selector);
    }

    /// Whether `expr` is a constant expression of value `v` that narrows to
    /// the primitive type `dst` in assignment context
    /// ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2)):
    /// an int-typed constant expression ([§4.12.4], [§15.28]) may narrow to
    /// `byte`, `short` or `char` when its value is representable in the
    /// target type ([§5.1.3] narrowing of constants).
    fn constant_narrowable(&self, expr: ExprId, src: Ty, dst: Ty) -> bool {
        let (TyKind::Primitive(p), TyKind::Primitive(d)) = (src.kind(self.db), dst.kind(self.db))
        else {
            return false;
        };
        if *p != PrimitiveType::Int
            || !matches!(
                d,
                PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char
            )
        {
            return false;
        }
        // §5.2 in assignment context via a switch expression ([§15.28]): the
        // switch is a poly expression and every *result expression* — each
        // arrow-arm value and each `yield` value — is checked against the
        // target on its own, so each must be a representable int constant.
        if let ExprData::Switch { arms, .. } = self.tree.expr(expr) {
            return self.switch_results_narrowable(arms, *d);
        }
        self.const_int_value(expr)
            .is_some_and(|value| crate::java::subtyping::fits_primitive(value, *d))
    }

    /// Whether every result expression of `arms` is an int constant that
    /// narrows to `d` ([JLS §5.2] applied per result expression of a switch
    /// expression in assignment context, [§15.28]).
    fn switch_results_narrowable(&self, arms: &[SwitchArm], d: PrimitiveType) -> bool {
        let mut results = Vec::new();
        for arm in arms {
            self.collect_switch_results(&arm.body, &mut results);
        }
        !results.is_empty()
            && results.into_iter().all(|expr| {
                self.const_int_value(expr)
                    .is_some_and(|value| crate::java::subtyping::fits_primitive(value, d))
            })
    }

    /// The result expressions of a switch arm: an arrow arm's value
    /// expression, and the `yield` values of a block arm.
    fn collect_switch_results(&self, stmts: &[StmtId], out: &mut Vec<ExprId>) {
        for &stmt in stmts {
            match self.tree.stmt(stmt) {
                StmtData::Expr(expr) => out.push(*expr),
                StmtData::Yield(expr) => out.push(*expr),
                StmtData::Block(inner) => self.collect_switch_results(inner, out),
                _ => {}
            }
        }
    }

    /// The value of an int-typed constant expression
    /// ([JLS §4.12.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4),
    /// [§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)):
    /// literals, parenthesized forms, the constant operators, and simple
    /// names of constant variables — evaluated by [`crate::java::const_eval`].
    fn const_int_value(&self, id: ExprId) -> Option<i64> {
        self.const_value(id).and_then(|value| value.as_int())
    }

    /// The value of a constant expression ([§15.28]) in the current
    /// environment of constant variables ([§4.12.4]).
    fn const_value(&self, id: ExprId) -> Option<Const> {
        ConstEnv::new(&self.tree, &self.const_locals).eval(id)
    }

    /// §4.12.2: whether `ty` is a *raw type* — a reference to a generic class
    /// ([§8.1.2]) used without its type arguments.
    fn is_raw_type(&self, ty: &Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Reference { name, args } if args.is_empty() => {
                !ty.is_error(self.db)
                    && crate::java::resolve::class_is_generic(self.db, &self.scope, name)
            }
            _ => false,
        }
    }

    /// §4.7: whether `ty` is a reifiable type — one whose runtime
    /// representation is fully known at compile time
    /// ([JLS §4.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.7)):
    /// a primitive, a non-generic class or interface, a raw type, an
    /// unbounded-wildcard parameterization (`List<?>`), or an array of
    /// reifiable. A type variable, or a parameterized type with a type-variable,
    /// concrete or bounded-wildcard argument (`List<String>`,
    /// `List<? extends Number>`, `ArrayList<T>`), is not.
    fn is_reifiable(&self, ty: &Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(_) => true,
            TyKind::Reference { args, .. } => args
                .iter()
                .all(|a| matches!(a.kind(self.db), TyKind::Wildcard(None))),
            TyKind::Array(inner) => self.is_reifiable(inner),
            _ => false,
        }
    }

    /// §15.20.2/[§4.7]: a type reference in `instanceof` position is
    /// bounds-checked ([§4.5.1]) and must be reifiable — a type variable or a
    /// parameterized type with a non-wildcard argument cannot be tested.
    fn check_instanceof_target(&mut self, expr: ExprId, spanned: &SpannedTypeRef) {
        self.check_type_argument_bounds(DiagLocation::Expr(expr), spanned);
        let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, spanned);
        if !ty.is_error(self.db) && !self.is_reifiable(&ty) {
            self.report(TypeError::IllegalGenericInstanceOf { expr, ty });
        }
    }

    /// The written type reference of a pattern ([JLS §14.30]) — the pattern
    /// type of a `TYPE_PATTERN`/`RECORD_PATTERN`, `None` for a `MatchAll`.
    fn pattern_type_ref(&self, id: PatternId) -> Option<SpannedTypeRef> {
        match self.tree.pattern(id).clone() {
            PatternData::Type(tp) => Some(tp.ty),
            PatternData::Record(rp) => Some(rp.ty),
            PatternData::MatchAll => None,
        }
    }

    /// §4.12.2: reports a declared local whose type is a raw type.
    fn warn_raw_declared_type(&mut self, local: LocalId) {
        let Some(ty) = self.locals.get(&local).copied() else {
            return;
        };
        if let TyKind::Reference { .. } = ty.kind(self.db)
            && self.is_raw_type(&ty)
        {
            self.report(TypeError::RawTypeUse { local, ty });
        }
    }

    /// §4.5.1: reports a parameterized type reference whose type argument is
    /// not within the bounds of the type parameter it fills
    /// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
    /// e.g. `M<String>` for `class M<T extends Number>`. The check is
    /// conservative (see [`type_argument_bound_violation`]): anything the
    /// subtype machinery cannot decide is silently skipped. `location` anchors
    /// the diagnostic to the local, expression or pattern the type reference
    /// belongs to; the report's range is the reference name itself, matching
    /// javac's caret.
    fn check_type_argument_bounds(&mut self, location: DiagLocation, spanned: &SpannedTypeRef) {
        let TypeRef::Reference {
            name: _,
            generic_args,
        } = &spanned.ty
        else {
            return;
        };
        if generic_args.is_empty() {
            return;
        }
        let resolved = resolve_type_ref(self.db, &self.scope, &self.resolver, &spanned.ty);
        let TyKind::Reference {
            name: fqn, args, ..
        } = resolved.kind(self.db)
        else {
            return;
        };
        let Some((param, arg, bound)) =
            type_argument_bound_violation(self.db, &self.scope, fqn, args)
        else {
            return;
        };
        let range = spanned.first_ref().and_then(|r| r.range);
        self.report(TypeError::TypeArgumentOutOfBounds {
            location,
            name: param,
            arg,
            bound,
            range,
        });
    }

    /// §5.1.9/§5.2: an assignment whose source is a raw type and whose target
    /// is parameterized succeeds by *unchecked conversion*; report it.
    fn warn_unchecked(&mut self, expr: ExprId, src: &Ty, dst: &Ty) {
        if src.is_error(self.db) || dst.is_error(self.db) || !self.is_raw_type(src) {
            return;
        }
        let parameterized =
            matches!(dst.kind(self.db), TyKind::Reference { args, .. } if !args.is_empty());
        let plain_subtype = crate::java::subtyping::is_subtype(self.db, &self.scope, src, dst);
        if parameterized && !plain_subtype {
            self.report(TypeError::UncheckedConversion {
                expr,
                from: *src,
                to: *dst,
            });
        }
    }

    /// §14.11.1: a `case` label of a switch whose selector is int-compatible
    /// or `String` must be a constant expression ([§15.28]); two labels of
    /// one switch may not declare the same value. Enum selectors are exempt —
    /// their bare-name labels are resolved as constants above.
    fn check_case_label(&mut self, label: ExprId, selector: &Ty) {
        if matches!(self.tree.expr(label).clone(), ExprData::Missing)
            || selector.is_error(self.db)
            || crate::java::subtyping::enum_constants(self.db, &self.scope, selector).is_some()
        {
            return;
        }
        let required = self.switchable(selector) || self.is_string(*selector);
        match self.const_value(label) {
            Some(value) => {
                let key = match &value {
                    Const::Int { v, .. } => format!("int:{v}"),
                    Const::Bool(b) => format!("bool:{b}"),
                    Const::Str(s) => format!("str:{s}"),
                };
                let display = match &value {
                    Const::Int { v, .. } => v.to_string(),
                    Const::Bool(b) => b.to_string(),
                    Const::Str(s) => format!("\"{s}\""),
                };
                // Only int-compatible and String labels can repeat across
                // arms ([§14.11.1]); pattern labels are checked elsewhere.
                if required {
                    let cases = self
                        .case_values
                        .last_mut()
                        .expect("switch case stack non-empty");
                    if cases.insert(key, ()).is_some() {
                        self.report(TypeError::DuplicateCaseLabel {
                            expr: label,
                            value: display,
                        });
                    }
                }
            }
            None => {
                // A closed form over literals and operators must evaluate;
                // a simple name is only an error when it names a *local*
                // that is not a constant variable — an unresolvable name
                // may be a constant field or static import, which this
                // layer does not track (reported as NoSuchField etc.).
                let closed = match self.tree.expr(label).clone() {
                    ExprData::Literal(_)
                    | ExprData::Paren(_)
                    | ExprData::Unary { .. }
                    | ExprData::Binary { .. }
                    | ExprData::Conditional { .. } => true,
                    ExprData::Var(name) => self.lookup_local(&name).is_some(),
                    _ => false,
                };
                if required && closed {
                    self.report(TypeError::NonConstantCaseLabel { expr: label });
                }
            }
        }
    }

    /// §11.2: whether the type is a *checked* exception — assignable to
    /// `Throwable` but not to `RuntimeException` or `Error`
    /// ([§11.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-11.html#jls-11.1.1)).
    fn is_checked(&self, ty: &Ty) -> bool {
        let throwable = Ty::reference(self.db, "java.lang.Throwable", Vec::new());
        if !crate::java::subtyping::is_assignable(self.db, &self.scope, ty, &throwable) {
            return false;
        }
        let unchecked = ["java.lang.RuntimeException", "java.lang.Error"];
        !unchecked.iter().any(|name| {
            let supertype = Ty::reference(self.db, *name, Vec::new());
            crate::java::subtyping::is_assignable(self.db, &self.scope, ty, &supertype)
        })
    }

    /// §11.2: at the end of a body, every remaining checked exception that no
    /// catch clause discharged and no `throws` clause declares is reported at
    /// the expression that threw it. A precise rethrow contributes several
    /// entries for one `throw` expression; like javac, the first uncovered
    /// type is reported and the rest of that statement's set is left alone.
    fn check_thrown_liability(&mut self) {
        let declared = self.enclosing_throws.clone();
        let pending = std::mem::take(&mut self.thrown);
        let mut reported: FxHashSet<ExprId> = FxHashSet::default();
        for (ty, expr) in pending {
            if !self.is_checked(&ty) {
                continue;
            }
            let discharged = declared.iter().any(|target| {
                crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, target)
            });
            if !discharged && reported.insert(expr) {
                self.report(TypeError::UnreportedException { expr, thrown: ty });
            }
        }
    }

    fn infer_expr(&mut self, id: ExprId) -> Ty {
        let expr = self.tree.expr(id).clone();
        let ty = match expr {
            ExprData::Literal(Literal::Int(_)) => self.primitive(PrimitiveType::Int),
            ExprData::Literal(Literal::Long(_)) => self.primitive(PrimitiveType::Long),
            ExprData::Literal(Literal::Char(_)) => self.primitive(PrimitiveType::Char),
            ExprData::Literal(Literal::Float) => self.primitive(PrimitiveType::Float),
            ExprData::Literal(Literal::Double) => self.primitive(PrimitiveType::Double),
            ExprData::Literal(Literal::Boolean(_)) => self.primitive(PrimitiveType::Boolean),
            ExprData::Literal(Literal::Str(_)) => self.string(),
            // §3.10.8: the null literal has the null type.
            ExprData::Null => Ty::null(self.db),
            // §15.8.3: `this` is the type of the enclosing class; a qualified
            // `TypeName.this` is the class or interface `TypeName`. §8.1.3: in
            // a static context — a static method body, a static field
            // initializer, a static initializer or an enum constant — no
            // enclosing instance exists, so `this` (bare or qualified) is a
            // compile-time error.
            ExprData::This { qualifier } => {
                if self.static_context {
                    self.report(TypeError::NonStaticThisFromStaticContext {
                        expr: id,
                        keyword: NonStaticThisKind::This,
                    });
                }
                // §8.8.7.1: `this` names the object whose supertype
                // constructor has not run yet.
                if self.before_super {
                    self.report(TypeError::CannotReferenceBeforeSuper {
                        expr: id,
                        name: Name::new("this"),
                    });
                }
                match qualifier {
                    Some(type_name) => {
                        resolve_type_ref(self.db, &self.scope, &self.resolver, &type_name)
                    }
                    None => self.enclosing_class.unwrap_or_else(|| self.error()),
                }
            }
            // §15.8.4: like `this`, `super` names the enclosing instance and
            // is illegal in a static context.
            ExprData::Super { .. } => {
                if self.static_context {
                    self.report(TypeError::NonStaticThisFromStaticContext {
                        expr: id,
                        keyword: NonStaticThisKind::Super,
                    });
                }
                // §8.8.7.1: the supertype constructor has not run yet.
                if self.before_super {
                    self.report(TypeError::CannotReferenceBeforeSuper {
                        expr: id,
                        name: Name::new("super"),
                    });
                }
                self.error()
            }
            // §15.8.2: `T.class` has type `Class<T>`.
            ExprData::ClassLit(tyref) => {
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
                Ty::reference(self.db, "java.lang.Class", vec![inner])
            }
            ExprData::Var(name) => self.var(id, name),
            ExprData::NamePath(name) => self.name_path(id, name),
            ExprData::FieldAccess { target, name } => self.field_access(id, target, name),
            // §15.13: the type of `array[index]` is the array's element type.
            ExprData::ArrayAccess { array, index } => {
                // §15.26: the array expression and the index of a subscript
                // expression are both evaluated for their values — `a[i] = v`
                // writes the element, not `a` or `i`. The mutating flag set
                // for the assignment target must not reach them, or a
                // `final` array or index variable would be misreported.
                let _ = self.infer_read_expr(index);
                let array_ty = self.infer_read_expr(array);
                if array_ty.is_array(self.db) {
                    array_ty
                        .element(self.db)
                        .copied()
                        .unwrap_or_else(|| self.error())
                } else {
                    self.error()
                }
            }
            ExprData::MethodCall {
                receiver,
                name,
                type_args,
                args,
            } => {
                // §15.12.1/[§15.12.2.2]: explicit type arguments
                // (`obj.<String>m(...)`) instantiate the method's type
                // parameters directly instead of by inference.
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
                self.method_call(id, receiver, name, &args, self.target, explicit)
            }
            // §8.8.7.1: `this(args)` delegates to another constructor of the
            // enclosing class.
            ExprData::CtorCall { args, target } => self.ctor_call(id, &args, target),
            // §15.9: a class instance creation has the type of the created
            // class; the diamond operator ([§15.9.2]) instantiates the type
            // arguments from the target type.
            ExprData::New {
                ty,
                args,
                diamond,
                members,
                receiver,
                ..
            } => {
                // §15.9: the enclosing instance of a qualified creation
                // (`primary.new Inner(...)`) is inferred standalone; it has
                // no effect on the created type's own inference here.
                if let Some(receiver) = receiver {
                    let _ = self.with_target(None, |this| this.infer_expr(receiver));
                }
                self.new_expr(id, ty, diamond, &args, self.target, !members.is_empty())
            }
            // §15.10: `new T[n][m]` has type `T[n][m]` (an array nested as
            // deep as there are dimensions); an array creation initializer
            // (§10.6) fills the element expressions. A non-reifiable component
            // type ([§4.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.7)) —
            // a type variable or a type with type arguments — is not allowed.
            ExprData::NewArray {
                ty,
                dims,
                initializer,
            } => {
                for &dim in &dims {
                    let _ = self.infer_expr(dim);
                }
                if let Some(elems) = initializer {
                    for elem in elems {
                        let _ = self.infer_expr(elem);
                    }
                }
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                let non_reifiable = match inner.kind(self.db) {
                    TyKind::TypeVar { .. } => true,
                    TyKind::Reference { args, .. } => !args.is_empty(),
                    _ => false,
                };
                if non_reifiable {
                    self.types.insert(id, self.error());
                    self.report(TypeError::GenericArrayCreation {
                        expr: id,
                        ty: inner,
                    });
                }
                let mut result = inner;
                for _ in 0..dims.len() {
                    result = Ty::array(self.db, result);
                }
                result
            }
            // §10.6: an array initializer has an array type whose element type
            // is derived from the elements.
            ExprData::ArrayInit(elements) => {
                let element = if elements.is_empty() {
                    self.error()
                } else {
                    let mut element = self.infer_expr(elements[0]);
                    for &element_expr in &elements[1..] {
                        let next = self.infer_expr(element_expr);
                        element = self.conditional_type(element, next);
                    }
                    element
                };
                Ty::array(self.db, element)
            }
            ExprData::Unary { op, expr } => self.unary(expr, op),
            // §15.14: a postfix increment/decrement has the type of its
            // operand.
            ExprData::Postfix { expr, .. } => {
                self.mutating = true;
                let ty = self.infer_expr(expr);
                self.mutating = false;
                ty
            }
            ExprData::Binary { op, lhs, rhs } => self.binary(op, lhs, rhs),
            // §15.26: an assignment expression has the type of its left-hand
            // side; the right-hand side is a poly expression with the left
            // side's type as target ([JLS §18.5.2.4]). For a plain assignment
            // the right-hand side must be assignable to the left-hand side
            // ([§15.26.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.26.1),
            // [§5.2]).
            ExprData::Assign { op, lhs, rhs } => {
                // §15.26.1: the left-hand side of a *simple* assignment is
                // written, not read — the definite-assignment check does not
                // apply to it.
                self.mutating = true;
                let lhs_ty = if matches!(op, AssignOp::Assign) {
                    self.writing = true;
                    let ty = self.infer_expr(lhs);
                    self.writing = false;
                    ty
                } else {
                    self.infer_expr(lhs)
                };
                self.mutating = false;
                let rhs_ty = self.with_target(Some(lhs_ty), |this| this.infer_expr(rhs));
                // §16: a simple assignment definitely assigns its left-hand
                // local; a compound assignment or increment reads it first,
                // so it does not discharge a blank local. Writing a local
                // also drops its precise rethrow set ([§11.2.2]): the
                // parameter is no longer effectively final.
                if matches!(op, AssignOp::Assign)
                    && let ExprData::Var(name) = self.tree.expr(lhs).clone()
                    && let Some(local) = self.lookup_local(&name)
                {
                    self.flow.definite.insert(local);
                    self.rethrow_sets.remove(&local);
                }
                // §16/[§8.3.1.2]: a *plain* assignment to a blank `final`
                // field of the enclosing class initializes it (or, if the
                // field is already assigned, is the already-assigned error
                // reported at the write's target) — either way the field is
                // now assigned, so it joins the field-assignment tracking.
                if matches!(op, AssignOp::Assign) {
                    self.record_field_write(lhs);
                }
                if matches!(op, AssignOp::Assign)
                    && !lhs_ty.is_error(self.db)
                    && !rhs_ty.is_error(self.db)
                    && !crate::java::subtyping::is_assignable(self.db, &self.scope, &rhs_ty, &lhs_ty)
                    // §5.2: an in-range int constant narrows to the target
                    // primitive ([§5.1.3]).
                    && !self.constant_narrowable(rhs, rhs_ty, lhs_ty)
                {
                    self.report(TypeError::IncompatibleTypes {
                        expr: rhs,
                        found: rhs_ty,
                        expected: lhs_ty,
                    });
                }
                // §5.1.9: a raw source assigned to a parameterized target is
                // an unchecked conversion — report the warning.
                if matches!(op, AssignOp::Assign) {
                    self.warn_unchecked(rhs, &rhs_ty, &lhs_ty);
                }
                lhs_ty
            }
            // §15.16: a cast has the type named in the cast. Per §15.16 and
            // §5.5, a lambda or method reference operand is a poly expression
            // ([§15.27.2], [§15.13.2]) whose type is the target type named by
            // the cast — the target is propagated into the operand so it is
            // typed against the cast type. (A cast of a poly *invocation* is a
            // standalone expression per §15.16, so plain method calls are not
            // given the target here.)
            ExprData::Cast { ty, expr } => {
                // §4.5.1: a cast type argument that is not within bounds.
                self.check_type_argument_bounds(DiagLocation::Expr(expr), &ty);
                if expr_is_poly(&self.tree, expr) {
                    let cast_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                    let _ = self.with_target(Some(cast_ty), |this| this.infer_expr(expr));
                    cast_ty
                } else {
                    let operand = self.infer_expr(expr);
                    let cast_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                    // §5.5/§15.16: a cast that is prohibited by the casting
                    // conversion rules is a compile-time error. A cast to a
                    // type variable (or to `T[]`) is always allowed — its
                    // erasure check happens at runtime ([§5.5.1]).
                    if !operand.is_error(self.db)
                        && !cast_ty.is_error(self.db)
                        && !cast_ty.contains_type_var(self.db)
                        && !self.castable(operand, cast_ty)
                    {
                        self.types.insert(expr, self.error());
                        self.report(TypeError::BadCast {
                            expr,
                            found: operand,
                            target: cast_ty,
                        });
                    }
                    cast_ty
                }
            }
            // §15.20.2: `instanceof` always has type `boolean`; the reference type of
            // the check must be reifiable ([§4.7]), and a pattern test
            // ([§14.30]) additionally resolves the pattern, recording the type
            // of each variable it binds ([§14.30.1], [§14.30.2]).
            ExprData::InstanceOf { expr, pattern, ty } => {
                let _ = self.infer_expr(expr);
                if let Some(ty) = &ty {
                    self.check_instanceof_target(expr, ty);
                }
                if let Some(pattern) = pattern {
                    let _ = self.pattern_type(pattern);
                    if let Some(spanned) = self.pattern_type_ref(pattern) {
                        self.check_instanceof_target(expr, &spanned);
                    }
                }
                self.primitive(PrimitiveType::Boolean)
            }
            // §15.25: a conditional expression's type follows the rules of
            // §15.25.2/§15.25.3 (identity, numeric promotion, then lub). When
            // the target is a functional interface and both arms are poly
            // (lambdas/method refs), the conditional is itself a poly
            // expression with the target type ([§15.25.2]): the target is
            // propagated into the arms so they are typed against it. The same
            // applies when the arms are poly invocations (e.g. nested generic
            // method calls) — the target is still propagated into the arms so
            // they can resolve against it.
            ExprData::Conditional { cond, then, els } => {
                self.check_condition(cond);
                // §14.30.3: a pattern in a conditional condition binds its
                // variables in the arm where they are definitely matched —
                // the condition's true flow in the then-arm
                // (`v instanceof T t ? t.f() : ...`), its false flow in the
                // else-arm (`!(v instanceof T t) ? "" : t.f()`).
                let (cond_true, cond_false) = self.pattern_flow(cond).unwrap_or_default();
                // §16.1.5: the definite-assignment outcome of a conditional:
                // the then-arm runs under the condition's *true* flow, the
                // else-arm under its *false* flow, and the expression's
                // after-flow is their join — a local is definitely assigned
                // after `c ? b : d` only when both arms assigned it. The
                // boolean-outcome flow of the condition is threaded into the
                // arm whose branch the value takes.
                let (cond_true_flow, cond_false_flow) = self.take_bool_outcomes();
                // §15.25.2: a conditional whose arms are poly expressions is a poly
                // expression with the target type; the target propagates into
                // the arms so they are typed against it — `List<TableCandidate>
                // c = b ? new ArrayList<>() : outerCandidates(...)` infers the
                // diamond arm from the declared type instead of degrading to
                // `List<?>`. javac propagates the target even when only one
                // arm is poly (`b ? new ArrayList<>() : someField`); a
                // conditional of two concrete arms keeps its own type.
                let cond_has_poly_arm =
                    expr_is_poly_ext(&self.tree, then) || expr_is_poly_ext(&self.tree, els);
                if cond_has_poly_arm && self.target.is_some() {
                    let (then_end, then_ty) = {
                        self.scopes.push(FxHashMap::default());
                        self.flow = cond_true_flow;
                        for binding in &cond_true {
                            self.scope_binding(*binding);
                        }
                        let ty = self.infer_expr(then);
                        let end = self.flow.clone();
                        self.scopes.pop();
                        (end, ty)
                    };
                    let (els_end, els_ty) = {
                        self.scopes.push(FxHashMap::default());
                        self.flow = cond_false_flow;
                        for binding in &cond_false {
                            self.scope_binding(*binding);
                        }
                        let ty = self.infer_expr(els);
                        let end = self.flow.clone();
                        self.scopes.pop();
                        (end, ty)
                    };
                    // §16.1.5: join the two arms' end flows.
                    let mut joined = then_end;
                    joined.join_definite(&els_end);
                    // §16.1.5 (non-boolean arms) / §16.1.7: both outcomes of the
                    // conditional equal its after-flow — an enclosing boolean
                    // context sees the arm-join on either outcome.
                    self.bool_outcomes = Some((joined.clone(), joined.clone()));
                    self.flow = joined;
                    // §15.25.2: the conditional's type is the target only when
                    // the arms actually accept it — a lambda arm against a
                    // *non-functional* target (`Object c = b ? s -> s : ...`)
                    // is itself ill-typed, and the conditional degrades to the
                    // error type rather than silently standing for the target.
                    if then_ty.is_error(self.db) || els_ty.is_error(self.db) {
                        self.error()
                    } else {
                        self.target.expect("target checked above")
                    }
                } else {
                    let (then_end, then_ty) = {
                        self.scopes.push(FxHashMap::default());
                        self.flow = cond_true_flow;
                        for binding in &cond_true {
                            self.scope_binding(*binding);
                        }
                        let ty = self.infer_expr(then);
                        let end = self.flow.clone();
                        self.scopes.pop();
                        (end, ty)
                    };
                    let (els_end, els_ty) = {
                        self.scopes.push(FxHashMap::default());
                        self.flow = cond_false_flow;
                        for binding in &cond_false {
                            self.scope_binding(*binding);
                        }
                        let ty = self.infer_expr(els);
                        let end = self.flow.clone();
                        self.scopes.pop();
                        (end, ty)
                    };
                    let mut joined = then_end;
                    joined.join_definite(&els_end);
                    // §16.1.5 (non-boolean arms) / §16.1.7: both outcomes of the
                    // conditional equal its after-flow.
                    self.bool_outcomes = Some((joined.clone(), joined.clone()));
                    self.flow = joined;
                    let ty = self.conditional_type(then_ty, els_ty);
                    // §15.25: a boolean operand against an unrelated
                    // primitive makes the conditional ill-typed — report the
                    // degradation (the arms' types, not the expression's).
                    if ty.is_error(self.db)
                        && !then_ty.is_error(self.db)
                        && !els_ty.is_error(self.db)
                    {
                        self.report(TypeError::IncompatibleOperand {
                            expr: id,
                            op: "?:",
                            found: then_ty,
                            other: Some(els_ty),
                        });
                    }
                    ty
                }
            }
            // §15.27/§15.13: lambdas and method references are poly
            // expressions ([§15.27.2], [§15.13.2]); their type is the target
            // functional interface ([§15.27.3]), which comes from the context
            // (a declaration initializer, an assignment, a return, or a method
            // invocation's resolved formal).
            ExprData::Lambda { params, body } => self.lambda_type(id, &params, body),
            ExprData::MethodRef {
                qualifier,
                type_name,
                name,
            } => self.method_ref_type(id, qualifier, type_name.as_ref(), &name),
            // §15.28: a switch expression's type is derived from its arm
            // result types; a `yield` value inside an arm has the switch
            // expression's type as target ([JLS §14.21]).
            ExprData::Switch { scrutinee, arms } => {
                let selector = self.infer_switch_selector(scrutinee);
                self.switch_targets.push(self.target);
                self.case_values.push(FxHashMap::default());
                // §15.28: a switch expression is an expression form — an arm
                // that completes abruptly (`throw`) is one alternative result
                // of *this* expression and must not leak the statement-level
                // abrupt-completion state into the enclosing block ([§14.22]).
                // Each arm is probed from the pre-switch state.
                let before_exited = self.exited;
                let before_flow = self.flow.clone();
                let mut result_tys: Vec<Ty> = Vec::new();
                // §16.1.9 extended to expressions ([§14.11.1], [§15.28]): a
                // local assigned on every normal-completing arm is definitely
                // assigned after the switch expression. Each arm's end state
                // joins by intersection over the non-abrupt arms — the same
                // join the statement form performs. The blank-`final`-field
                // touched set joins by union (any arm that touched a field
                // rules out a later assignment to it, [§8.3.1.2]).
                let mut arm_end_states: Vec<(Flow, bool)> = Vec::new();
                for arm in &arms {
                    self.exited = before_exited;
                    self.flow = before_flow.clone();
                    // §14.30.2/§14.30.3: a pattern label's variables are in
                    // scope in the arm's statements.
                    self.scopes.push(FxHashMap::default());
                    for label in &arm.labels {
                        match label {
                            SwitchLabel::Expr(e) => {
                                let _ = self.infer_switch_label(*e, &selector);
                            }
                            SwitchLabel::Pattern(p) => {
                                let _ = self.pattern_type(*p);
                                for binding in self.pattern_bindings_of(*p) {
                                    self.scope_binding(binding);
                                }
                            }
                            // §14.11.1: a `when` guard must be a boolean;
                            // its pattern bindings are already in scope.
                            SwitchLabel::Guard(cond) => {
                                self.check_condition(*cond);
                            }
                        }
                    }
                    for &stmt in &arm.body {
                        let data = self.tree.stmt(stmt).clone();
                        match &data {
                            // §15.28: the value of an arrow arm
                            // `case L -> expr` is the expression itself.
                            StmtData::Expr(expr) => result_tys.push(self.infer_expr(*expr)),
                            // §14.21: a block arm yields its value with the
                            // switch expression's type as target.
                            StmtData::Yield(expr) => {
                                let target = self.switch_targets.last().copied().flatten();
                                result_tys
                                    .push(self.with_target(target, |this| this.infer_expr(*expr)));
                            }
                            // §15.28: an arrow arm with a block body, or a
                            // block arm, produces its value through the block's
                            // final `yield` statement.
                            StmtData::Block(stmts) => {
                                self.infer_stmt_data(stmt, &data);
                                if let Some(&last) = stmts.last()
                                    && let StmtData::Yield(expr) = self.tree.stmt(last).clone()
                                    && let Some(ty) = self.types.get(&expr).copied()
                                {
                                    result_tys.push(ty);
                                }
                            }
                            _ => self.infer_stmt_data(stmt, &data),
                        }
                    }
                    // Record the arm's end state: abrupt completion (throw /
                    // return) contributes no path; a normal-completing arm
                    // contributes its definite-assignment set.
                    arm_end_states.push((self.flow.clone(), self.exited));
                    self.flow = before_flow.clone();
                    self.exited = before_exited;
                    self.scopes.pop();
                }
                // §16.1.9: the join of the arm paths — locals assigned on
                // *every* non-abrupt path are definitely assigned after the
                // switch expression. With only abrupt arms the pre-switch
                // state stands (the expression never completed normally).
                let mut joined: Option<Flow> = None;
                for (end_state, exited) in &arm_end_states {
                    if *exited {
                        continue;
                    }
                    match &mut joined {
                        None => joined = Some(end_state.clone()),
                        Some(acc) => acc.join_definite(end_state),
                    }
                }
                if let Some(joined) = joined {
                    self.flow.definite.extend(joined.definite.clone());
                    self.flow.union_touched(&joined);
                }
                self.case_values.pop();
                self.switch_targets.pop();
                // §14.11.1/§15.28: a switch *expression* must be exhaustive —
                // every selector value has a matching arm or there is a
                // `default`. Against an enum selector, coverage means every
                // constant is named by some label ([§14.11.1]); sealed-class
                // hierarchies are not yet modelled and are not checked.
                if !self.switch_is_exhaustive(&selector, arms.as_slice()) {
                    self.report(TypeError::NotExhaustive { expr: id });
                }
                if result_tys.is_empty() {
                    self.error()
                } else {
                    let mut ty = result_tys[0];
                    for &result in &result_tys[1..] {
                        ty = self.conditional_type(ty, result);
                    }
                    ty
                }
            }
            ExprData::Paren(inner) => self.infer_expr(inner),
            // §15.8.6 (a preview feature removed in JLS 23): a string template
            // types as `String` (the `STR` processor); each embedded expression
            // is inferred.
            ExprData::Template { args } => {
                for arg in args {
                    let _ = self.infer_expr(arg);
                }
                self.string()
            }
            ExprData::Missing => self.error(),
        };
        self.types.insert(id, ty);
        // §16.1.7: every expression but the conditional boolean forms
        // (`&&`, `||`, `!`, `?:`) leaves both outcomes equal to the
        // after-expression flow. The forms set [`Self::bool_outcomes`]
        // themselves inside their handlers; this default must not clobber
        // those.
        if !self.is_bool_flow_expr(id) {
            let flow = self.flow.clone();
            self.bool_outcomes = Some((flow.clone(), flow));
        }
        ty
    }

    /// Infers `expr` as a *read*, suppressing the mutating / writing flags
    /// that a surrounding assignment target set ([§15.26], [§16]). A receiver
    /// expression — the target of a qualified field access or the array of a
    /// subscript — is always evaluated for its value, so `final` variables in
    /// receiver position must not be mistaken for the assigned target.
    fn infer_read_expr(&mut self, id: ExprId) -> Ty {
        let saved_mutating = self.mutating;
        let saved_writing = self.writing;
        self.mutating = false;
        self.writing = false;
        let ty = self.infer_expr(id);
        self.mutating = saved_mutating;
        self.writing = saved_writing;
        ty
    }

    /// A bare name: a local variable or parameter, or — when no local — a
    /// field of the implicit receiver ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)).
    fn var(&mut self, expr: ExprId, name: Name) -> Ty {
        // §6.3/[§15.27.2]: a lambda parameter is in scope throughout the
        // lambda body, shadowing every enclosing local and parameter (and the
        // enclosing class's fields, [§6.5.6.1]). It is checked ahead of the
        // enclosing locals so a bare name in a lambda body binds to its own
        // parameter, not to a same-named outer local ([§6.4]).
        for scope in self.lambda_params.iter().rev() {
            if let Some(ty) = scope.get(&name) {
                return *ty;
            }
        }
        if let Some(local) = self.lookup_local(&name) {
            // §8.3.1.2/[§16]: writing a `final` local that is not *blank*
            // (a parameter, a catch/foreach/resource variable, or one with an
            // initializer) is an error; a blank final may still be assigned
            // once, by definite assignment.
            if self.mutating
                && self.tree.local(local).is_final
                && !self.blank_finals.contains(&local)
            {
                self.report(TypeError::CannotAssignToFinalVariable {
                    expr,
                    name: name.clone(),
                });
            }
            // §16: a local's value may be read only after it is definitely
            // assigned on every path to the read. Reads past an exit
            // (return/break/throw) are not checked — the path is unreachable —
            // and the left-hand side of a simple assignment is written, not
            // read ([§15.26.1]).
            if !self.exited && !self.writing && !self.flow.definite.contains(&local) {
                self.report(TypeError::NotDefinitelyAssigned {
                    expr,
                    name: name.clone(),
                });
                return self.error();
            }
            return self
                .locals
                .get(&local)
                .copied()
                .unwrap_or_else(|| self.error());
        }
        // §7.5.4: a simple name may name a statically imported member — a
        // static field read through its declaring type.
        if let Some(ty) = self.static_import_field(name.as_str()) {
            return ty;
        }
        if let Some(field) = self.pick_field_of_chain(name.as_str()) {
            // §8.3.3: a simple-name read of a same-class field declared
            // textually later, of the same static/instance kind, is an
            // illegal forward reference. A qualified read (`this.b`) takes
            // the field-access path and stays legal.
            if self.forward_names.iter().any(|forward| forward == &name) {
                self.report(TypeError::IllegalForwardReference {
                    expr,
                    name: name.clone(),
                });
                return self.error();
            }
            // §15.11/[§8.1.3]: an *instance* field of the implicit receiver
            // is reachable only through `this`, which a static context has
            // none of — a simple-name read of one is a compile-time error. A
            // static field (or a field read in an instance context) stays
            // legal.
            if self.static_context && !field.is_static {
                self.report(TypeError::NonStaticFieldFromStaticContext {
                    expr,
                    name: name.clone(),
                });
            }
            // §8.8.7.1: an instance field of the object under construction is
            // not usable before the supertype constructor has run.
            if self.before_super && !field.is_static {
                self.report(TypeError::CannotReferenceBeforeSuper {
                    expr,
                    name: name.clone(),
                });
            }
            // §8.3.1.2/[§16]: writing a `final` field through its simple name
            // (implicit `this`) is only legal as the blank-final
            // initialization; a write to a blank final that an earlier
            // statement (or path) has already assigned is the
            // already-assigned error.
            if self.mutating && field.is_final {
                match self.final_field_write_verdict(&field, true) {
                    FinalFieldWrite::Legal => {}
                    FinalFieldWrite::AlreadyAssigned => {
                        self.report(TypeError::VariableAlreadyAssigned {
                            expr,
                            name: name.clone(),
                        });
                    }
                    FinalFieldWrite::CannotAssign => {
                        self.report(TypeError::CannotAssignToFinalVariable {
                            expr,
                            name: name.clone(),
                        });
                    }
                }
            }
            return field.ty;
        }
        // §6.5: a simple name that resolves to nothing is a compile-time
        // error.
        self.report(TypeError::CannotResolveName {
            expr,
            name: name.clone(),
        });
        self.error()
    }

    /// A static field ([§7.5.4]) named by a static import: `import static
    /// pkg.Type.FIELD`, or the on-demand form `import static pkg.Type.*`,
    /// makes the simple name `FIELD` a static member access (§15.11.1).
    fn static_import_field(&self, simple: &str) -> Option<Ty> {
        for (owner, member) in self.resolver.static_import_owners(simple) {
            let receiver = Ty::reference(self.db, owner.as_str(), Vec::new());
            let access = self.access.with_mode(InvocationMode::Static);
            if let Some(field) = pick_field(self.db, &self.scope, &receiver, &member, &access)
                .filter(|field| field.is_static)
            {
                return Some(field.ty);
            }
        }
        None
    }

    /// A qualified name in expression position: `Type.field` (a static field
    /// access, [§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1))
    /// when the prefix resolves to a type; a simple non-local name falls back
    /// to a field of the implicit receiver.
    fn name_path(&mut self, expr: ExprId, name: Name) -> Ty {
        let text = name.as_str();
        let (prefix, last) = match text.rsplit_once('.') {
            Some((prefix, last)) => (prefix, last),
            None => ("", text),
        };
        if prefix.is_empty() {
            if let Some(ty) = self.static_import_field(last) {
                return ty;
            }
            if let Some(field) = self.pick_field_of_chain(last) {
                // §15.11/[§8.1.3]: a simple-name read of an instance field of
                // the implicit receiver from a static context.
                if self.static_context && !field.is_static {
                    self.report(TypeError::NonStaticFieldFromStaticContext {
                        expr,
                        name: name.clone(),
                    });
                }
                return field.ty;
            }
            // §6.5: a simple name that resolves to nothing is a compile-time
            // error.
            self.report(TypeError::CannotResolveName {
                expr,
                name: name.clone(),
            });
            return self.error();
        }
        let Some(prefix_ty) = self.resolve_type_name_checked(&Name::new(prefix)) else {
            // The prefix names no type: report the whole path as missing.
            self.report(TypeError::CannotResolveName {
                expr,
                name: name.clone(),
            });
            return self.error();
        };
        if let Some(field) = pick_field(self.db, &self.scope, &prefix_ty, last, &self.access) {
            return field.ty;
        }
        // §15.11: a qualified name whose last component is no member of the
        // resolved prefix is a compile-time error.
        self.report(TypeError::NoSuchField {
            expr,
            name: Name::new(last),
        });
        self.error()
    }

    /// A bare type name in receiver position: `Type.name` — a static member
    /// access ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1))
    /// or `Type.method(...)` call
    /// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
    /// whose receiver is a type, not a value. `None` when `name` is a local
    /// variable or does not resolve to a type.
    fn type_name_ty(&self, name: &Name) -> Option<Ty> {
        if self.lookup_local(name).is_some() {
            return None;
        }
        let tyref = TypeRef::Reference {
            name: name.clone(),
            generic_args: Vec::new(),
        };
        let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
        // The name is a type only when its canonical FQN resolves on the
        // classpath; a name that no candidate resolved to (a field or method
        // of the implicit receiver) is not.
        let TyKind::Reference { name: resolved, .. } = ty.kind(self.db) else {
            return None;
        };
        (hir::fqn_resolve(self.db, &self.scope, resolved.as_str()).is_some()).then_some(ty)
    }

    /// The type named by a pure name chain (`Type`, `pkg.Type`,
    /// `java.util.Map.Entry`) — a (possibly qualified) type name used as a
    /// static member receiver ([§6.5.5], [§15.11.1]). Every segment must be a
    /// plain name — no segment may be a local variable — and the canonical
    /// fully qualified name must resolve on the classpath. `None` otherwise,
    /// so ordinary instance field chains keep their expression treatment.
    fn dotted_type_name(&self, id: ExprId) -> Option<Ty> {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = id;
        loop {
            match self.tree.expr(cur).clone() {
                ExprData::FieldAccess {
                    target: Some(t),
                    name,
                } => {
                    parts.push(name.as_str().to_owned());
                    cur = t;
                }
                // A qualified name lowered to one `NamePath` node (`a.b.c` in
                // a construct the parser joins together); every segment must
                // be a plain name that is not a local variable.
                ExprData::NamePath(name) => {
                    let segments: Vec<&str> = name.as_str().split('.').collect();
                    if segments
                        .iter()
                        .any(|segment| self.lookup_local(&Name::new(segment)).is_some())
                    {
                        return None;
                    }
                    parts.extend(segments.into_iter().map(str::to_owned));
                    break;
                }
                ExprData::Var(name) => {
                    if self.lookup_local(&name).is_some() {
                        return None;
                    }
                    parts.push(name.as_str().to_owned());
                    break;
                }
                _ => return None,
            }
        }
        parts.reverse();
        let tyref = TypeRef::Reference {
            name: Name::new(&parts.join(".")),
            generic_args: Vec::new(),
        };
        let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
        let TyKind::Reference { name: resolved, .. } = ty.kind(self.db) else {
            return None;
        };
        hir::fqn_resolve(self.db, &self.scope, resolved.as_str())
            .is_some()
            .then_some(ty)
    }

    fn field_access(&mut self, expr: ExprId, target: Option<ExprId>, name: Name) -> Ty {
        let Some(target) = target else {
            return self.var(expr, name);
        };
        // `super.field` — a field of the direct superclass ([§15.11.1],
        // [§15.12.1]): the receiver is the superclass type and the access
        // context is the super invocation mode.
        if matches!(self.tree.expr(target).clone(), ExprData::Super { .. }) {
            // §8.1.3: `super` names the enclosing instance, which does not
            // exist in a static context. Reported at the `super` keyword.
            if self.static_context {
                self.report(TypeError::NonStaticThisFromStaticContext {
                    expr: target,
                    keyword: NonStaticThisKind::Super,
                });
            }
            // §8.8.7.1: `super`'s own supertype constructor has not run yet.
            if self.before_super {
                self.report(TypeError::CannotReferenceBeforeSuper {
                    expr,
                    name: name.clone(),
                });
            }
            let receiver = self.super_ty();
            let access = self.access.with_mode(InvocationMode::Super);
            // §15.11.1: a `super` field access selects an instance member of
            // the direct superclass; a static field via `super` is illegal.
            return match pick_field(self.db, &self.scope, &receiver, name.as_str(), &access) {
                Some(field) if field.is_static => {
                    self.report(TypeError::NoSuchField {
                        expr,
                        name: name.clone(),
                    });
                    self.error()
                }
                Some(field) => field.ty,
                None => {
                    // §15.11: no field of the name on the superclass.
                    self.report(TypeError::NoSuchField {
                        expr,
                        name: name.clone(),
                    });
                    self.error()
                }
            };
        }
        // `Type.name` — the receiver expression is a pure name chain that
        // resolves to a type, not a value ([§15.11.1]): a bare name or a
        // qualified name such as `java.util.Collections`.
        let (receiver, is_static) = match self.dotted_type_name(target) {
            Some(ty) => (ty, true),
            // §15.26: the receiver of a qualified field access is evaluated
            // for its *value* even on the left-hand side of an assignment —
            // `a.b = v` writes `b`, not `a` ([§15.11.1]). The mutating /
            // writing flags set for the assignment target must not reach the
            // receiver, or a `final` variable holding the object (`final
            // Holder h; h.field = v`) would be misreported as reassigned.
            None => (self.infer_read_expr(target), false),
        };
        // §10.7: every array type has a public final `length` field.
        if receiver.is_array(self.db) && name.as_str() == "length" {
            return self.primitive(PrimitiveType::Int);
        }
        match pick_field(self.db, &self.scope, &receiver, name.as_str(), &self.access) {
            Some(field) => {
                // §8.3.1.2/[§16]: writing a `final` field is legal only as the
                // blank-final initialization through a bare `this` receiver in
                // the matching initializer context of the field's own class;
                // a write to a blank final already assigned is the
                // already-assigned error; every other final-field write is an
                // error.
                if self.mutating && field.is_final {
                    let bare_this = matches!(
                        self.tree.expr(target).clone(),
                        ExprData::This { qualifier: None }
                    );
                    match self.final_field_write_verdict(&field, bare_this) {
                        FinalFieldWrite::Legal => {}
                        FinalFieldWrite::AlreadyAssigned => {
                            self.report(TypeError::VariableAlreadyAssigned {
                                expr,
                                name: name.clone(),
                            });
                        }
                        FinalFieldWrite::CannotAssign => {
                            self.report(TypeError::CannotAssignToFinalVariable {
                                expr,
                                name: name.clone(),
                            });
                        }
                    }
                }
                field.ty
            }
            // `Type.Name` read without a call — or used as the receiver of a
            // `Type.method(...)` call — is the type itself when `Name` is a
            // *nested type* member of `Type` ([§6.5.5.2], [§15.11.1]). For a
            // *source* receiver the member set is complete, so a `Type.Name`
            // that is neither a field nor a nested type — an unknown enum
            // constant ([§8.9.2]) or a misspelled static field — is reported
            // like any missing member ([§15.11]). Library receivers keep the
            // conservative fallback: their records may be partial, and real
            // loaded libraries surface their static members through the
            // member set ([`LibraryIndex`]).
            None if is_static => {
                // §6.6: a private / protected / package-private static field
                // exists but is not accessible from the enclosing class.
                if self.report_illegal_field_access(expr, receiver, name.as_str()) {
                    return self.error();
                }
                let is_source = self.receiver_fqn(&receiver).is_some_and(|fqn| {
                    matches!(
                        hir::fqn_resolve(self.db, &self.scope, fqn),
                        Some(hir::Resolved::Source(_))
                    )
                });
                // §8.9.2/[§15.11]: report a `Type.Name` that is neither a field
                // nor a nested type as a missing member. For a *source*
                // receiver the member set is complete, so this is sound. A
                // *library* receiver keeps the conservative fallback EXCEPT for
                // an enum: enum constants are guaranteed complete in a
                // classfile, so an unknown constant is a real error, whereas a
                // non-enum library class may carry partial static-field records.
                let is_library_enum = self.receiver_is_library_enum(&receiver);
                if !(is_source || is_library_enum)
                    || self.receiver_has_nested_type(&receiver, name.as_str())
                {
                    receiver
                } else {
                    self.report(TypeError::NoSuchField {
                        expr,
                        name: name.clone(),
                    });
                    self.error()
                }
            }
            None => {
                // §6.6: a field of the name exists but is not accessible from
                // the enclosing class — report the access violation rather
                // than a missing member (§15.11).
                if self.report_illegal_field_access(expr, receiver, name.as_str()) {
                    return self.error();
                }
                // §15.11: no (accessible) field of the name on the receiver.
                self.report(TypeError::NoSuchField {
                    expr,
                    name: name.clone(),
                });
                self.error()
            }
        }
    }

    /// Whether `name` is a *member type* (a nested class, interface, enum,
    /// record or annotation, [JLS §6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2))
    /// of the class `receiver`. A qualified name `Type.Name` whose last
    /// component is a nested type is itself a type, not a field access, so it
    /// is not reported as a missing field. Source classes nest with dots
    /// ([JLS §6.7]); library nested classes use the `Outer$Inner` binary name
    /// ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)).
    fn receiver_has_nested_type(&self, receiver: &Ty, name: &str) -> bool {
        let Some(fqn) = self.receiver_fqn(receiver) else {
            return false;
        };
        let is_library = matches!(
            hir::fqn_resolve(self.db, &self.scope, fqn),
            Some(hir::Resolved::Library(_))
        );
        let candidate = if is_library {
            format!("{fqn}${name}")
        } else {
            format!("{fqn}.{name}")
        };
        hir::fqn_resolve(self.db, &self.scope, &candidate).is_some()
    }

    /// The canonical fully qualified name of a reference receiver type, or
    /// `None` for a non-reference (array, type variable, primitive, error).
    fn receiver_fqn(&self, receiver: &Ty) -> Option<&str> {
        let TyKind::Reference { name, .. } = receiver.kind(self.db) else {
            return None;
        };
        Some(name.as_str())
    }

    /// Whether the reference receiver resolves to an *enum* class on the
    /// classpath (a library class carrying ACC_ENUM, [JVMS §4.1]). Enum
    /// constants are the only static members whose presence in a classfile is
    /// guaranteed complete, so an unknown constant of a library enum is a
    /// genuine §8.9.2/§15.11 error rather than a partial-record artifact.
    fn receiver_is_library_enum(&self, receiver: &Ty) -> bool {
        let Some(fqn) = self.receiver_fqn(receiver) else {
            return false;
        };
        let resolved = hir::fqn_resolve(self.db, &self.scope, fqn);
        let Some(hir::Resolved::Library(class)) = resolved else {
            return false;
        };
        let Some(record) = hir::class_record(self.db, &class) else {
            return false;
        };
        let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
            return false;
        };
        syntax::stub::ClassKind::from_flags(class.flags, class.is_record)
            == syntax::stub::ClassKind::Enum
    }

    fn pick_field_of(&mut self, receiver: Option<Ty>, name: &str) -> Option<FieldData> {
        let receiver = receiver?;
        pick_field(self.db, &self.scope, &receiver, name, &self.access)
    }

    /// The statements of one block ([JLS §14.2]). §14.22: a statement is
    /// reachable only if the preceding statement can complete normally — a
    /// statement after an abruptly-completing one is a compile-time error.
    /// It is still inferred (as reachable) so its own diagnostics stay
    /// complete, matching javac's cadence of one report per block.
    fn infer_block_statements(&mut self, stmts: &[StmtId]) {
        let mut reported_unreachable = false;
        let mut recovered_exit = false;
        for &stmt in stmts {
            if self.exited && !reported_unreachable {
                self.report(TypeError::UnreachableStatement { stmt });
                reported_unreachable = true;
                recovered_exit = true;
                // Keep inferring the tail (as reachable) for its own
                // diagnostics; §14.22 makes it unreachable, so this recovery
                // does not change the block's completion behavior.
                self.exited = false;
            }
            self.infer_stmt(stmt);
        }
        // An earlier abrupt exit still stands: the unreachable tail cannot
        // let the block complete normally.
        if recovered_exit {
            self.exited = true;
        }
    }

    /// §16.2.10/[§16.2.12]: records the current flow at a `break` that targets
    /// an enclosing loop, so a constant-condition loop can join its break
    /// paths into the after-loop state (JLS Example 16-1). An *unlabeled*
    /// break targets the nearest enclosing loop **or** switch ([§14.15]) — so
    /// one inside a `switch` is a switch's break, not a loop's, and is not
    /// recorded. A labeled break records on the loop whose label names it (the
    /// frame exists for any label the break legally targets — a loop is
    /// always labeled before its body is inferred); a labeled break out of a
    /// labeled *block* matches no frame and contributes nothing.
    fn record_break_flow(&mut self, label: Option<&Name>) {
        if let Some(name) = label {
            for frame in self.loop_breaks.iter_mut().rev() {
                if frame.label.as_ref().is_some_and(|n| n == name) {
                    frame.flows.push(self.flow.clone());
                    return;
                }
            }
            return;
        }
        if self.switch_depth == 0
            && let Some(innermost) = self.loop_breaks.last_mut()
        {
            innermost.flows.push(self.flow.clone());
        }
    }

    /// A simple-name field read inside the body: the implicit receiver is not
    /// just the immediately enclosing class but every enclosing declaration,
    /// innermost first ([§6.5.5.1], [§8.3]).
    fn pick_field_of_chain(&mut self, name: &str) -> Option<FieldData> {
        for class in std::iter::once(&self.enclosing_class)
            .flatten()
            .chain(self.enclosing_chain.iter())
        {
            if let Some(field) = pick_field(self.db, &self.scope, class, name, &self.access) {
                return Some(field);
            }
        }
        None
    }

    /// §16/[§8.3.1.2]: records the *plain* assignment `lhs`'s target into
    /// [`Self::field_touched`] — a blank `final` field of the enclosing class
    /// that is assigned by `name = …` or `this.name = …`. Called for every
    /// simple assignment; the field's name enters the "might already be
    /// assigned" set so a later write to the same blank final is the
    /// already-assigned error ([`TypeError::VariableAlreadyAssigned`]). The
    /// assignment itself is validated (as legal blank-final initialization or
    /// as the cannot-assign error) at the field-write check in [`Self::var`]
    /// and [`Self::field_access`].
    fn record_field_write(&mut self, lhs: ExprId) {
        let name = match self.tree.expr(lhs).clone() {
            ExprData::Var(name) => name,
            ExprData::FieldAccess { target, name } => {
                // Only a bare `this.name` (the implicit receiver) can be the
                // class's own field; `Type.name` and `obj.name` assign a
                // member of another object and never touch the enclosing
                // class's blank finals.
                let Some(target) = target else {
                    return;
                };
                if !matches!(
                    self.tree.expr(target).clone(),
                    ExprData::This { qualifier: None }
                ) {
                    return;
                }
                name
            }
            _ => return,
        };
        let Some(field) = self.pick_field_of_chain(name.as_str()) else {
            return;
        };
        // §8.3.1.2/[§16]: only *blank* finals are one-shot — a final with an
        // initializer can never be assigned, and is reported elsewhere; it is
        // not "assigned here".
        if !field.is_final || !self.field_is_blank(&field) {
            return;
        }
        self.flow.field_touched.insert(name.as_str().to_owned());
    }

    /// Whether `field` is a *blank* (initializer-less) `final` field of a
    /// source class ([JLS §8.3.1.2]): the once-only initialization rule
    /// applies to a field with no initializer (and to a record component,
    /// which is implicitly blank, [§8.10.4]).
    fn field_is_blank(&self, field: &FieldData) -> bool {
        let Some(owner_file) = field.owner_file else {
            return false;
        };
        let tree = hir::file_item_tree(self.db, owner_file);
        let mut found_blank = false;
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: hir_def::java::item_tree::ItemId,
            name: &str,
            found: &mut bool,
        ) {
            if *found {
                return;
            }
            match tree.data(id) {
                hir_def::java::item_tree::ItemData::Field(f) if f.name.as_str() == name => {
                    *found = f.initializer.is_none();
                    return;
                }
                // §8.10.1: a record's components become blank final fields.
                hir_def::java::item_tree::ItemData::Record(r) => {
                    if r.components.iter().any(|c| c.name.as_str() == name) {
                        *found = true;
                        return;
                    }
                }
                _ => {}
            }
            for &child in tree.data(id).body() {
                walk(tree, child, name, found);
            }
        }
        for &top in &tree.top {
            walk(&tree, top, &field.name, &mut found_blank);
        }
        found_blank
    }

    /// §8.3.1.2/[§16]: the verdict on writing the `final` field `field`
    /// through `bare_this` at the current position. [`FinalFieldWrite::Legal`]
    /// is the once-only blank-`final` initialization — a blank (initializer-
    /// less) field of the class being initialized, written through its own
    /// `this` receiver from the matching initializer context: a blank
    /// `static final` from a static initializer or static field initializer
    /// ([§8.7], [§8.3.2]), a blank `final` instance field from an instance
    /// initializer, instance field initializer or a constructor ([§8.6],
    /// [§8.3.2], [§8.8]). [`FinalFieldWrite::AlreadyAssigned`] is a second
    /// write to a blank final that some earlier statement or alternative path
    /// has *already* assigned ([§16]); every other `final`-field write is
    /// [`FinalFieldWrite::CannotAssign`].
    fn final_field_write_verdict(&self, field: &FieldData, bare_this: bool) -> FinalFieldWrite {
        if !bare_this || self.lambda_depth > 0 {
            return FinalFieldWrite::CannotAssign;
        }
        // The context must match the field's static/instance kind.
        let ctx_ok = if field.is_static {
            self.init_ctx == Some(InitCtx::Static)
        } else {
            self.in_constructor || self.init_ctx == Some(InitCtx::Instance)
        };
        if !ctx_ok {
            return FinalFieldWrite::CannotAssign;
        }
        // The field must belong to the class being initialized.
        let same_class = self
            .enclosing_class
            .as_ref()
            .and_then(|ty| ty.as_reference(self.db))
            .is_some_and(|(fqn, _)| fqn.as_str() == field.owner);
        if !same_class {
            return FinalFieldWrite::CannotAssign;
        }
        // The field must be blank — a source field with no initializer (its
        // initializer is what makes a later assignment a double-write). A
        // record component field is implicitly blank ([§8.10.4]): the
        // canonical and compact constructors may assign it, and no component
        // ever carries an initializer.
        if !self.field_is_blank(field) {
            return FinalFieldWrite::CannotAssign;
        }
        // A blank final may be assigned only once: a write to one already
        // assigned on some path is the already-assigned error, not a fresh
        // initialization.
        if self.flow.field_touched.contains(field.name.as_str()) {
            return FinalFieldWrite::AlreadyAssigned;
        }
        FinalFieldWrite::Legal
    }

    /// §6.6: the access probe of a field access ([§15.11]) — after the
    /// accessible [`pick_field`] missed on `receiver`, a field of the name may
    /// still exist there with a more restrictive access than the access site
    /// allows. Reports the field as `IllegalAccess` and returns `true` when
    /// so; the caller falls back to a no-such-member error otherwise.
    fn report_illegal_field_access(&mut self, expr: ExprId, receiver: Ty, name: &str) -> bool {
        let Some(field) =
            pick_field_ignoring_access(self.db, &self.scope, &receiver, name, &self.access)
        else {
            return false;
        };
        self.report(TypeError::IllegalAccess {
            expr,
            kind: IllegalAccessKind::Field,
            name: Name::new(&field.name),
            owner: Name::new(&field.owner),
            access: access_keyword(field.access),
        });
        true
    }

    /// §6.6: the access probe of a method invocation
    /// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)) —
    /// after the accessible [`member_set`] missed on `receiver`, a method of
    /// the name may still exist there with a more restrictive access. The
    /// invocation mode ([§15.12.3]) is honored via `ctx` — a static form of an
    /// instance method (or `super` of a static one) is a mode mismatch, not an
    /// access violation, so it stays a no-such-member error. Reports the
    /// most-derived one as `IllegalAccess` and returns `true` when so.
    fn report_illegal_method_access(
        &mut self,
        expr: ExprId,
        receiver: Ty,
        name: &str,
        ctx: &InvocationContext,
    ) -> bool {
        let members = member_set_ignoring_access(self.db, &self.scope, &receiver, name, ctx);
        let Some(method) = members.first() else {
            return false;
        };
        self.report(TypeError::IllegalAccess {
            expr,
            kind: IllegalAccessKind::Method,
            name: Name::new(&method.name),
            owner: Name::new(&method.owner),
            access: access_keyword(method.access),
        });
        true
    }

    /// The type a (possibly qualified) *type* name denotes, probed candidate
    /// by candidate ([§6.5.5.1], [§6.5.5.2]) — unlike
    /// [`crate::java::resolve::resolve_type_ref`], which degrades an unresolvable
    /// name to its most-qualified candidate for display, this reports
    /// failure so expression-position receivers can fall back to value
    /// treatment.
    fn resolve_type_name_checked(&self, name: &Name) -> Option<Ty> {
        let candidates = crate::java::resolve::candidate_fqns(&self.resolver, name);
        for candidate in candidates {
            if hir::fqn_resolve(self.db, &self.scope, candidate.as_str()).is_some() {
                return Some(Ty::reference(self.db, candidate.as_str(), Vec::new()));
            }
        }
        None
    }

    /// An explicit constructor invocation `this(args)` / `super(args)`
    /// ([JLS §8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
    /// the candidates are the constructors of the enclosing class (for
    /// `this`) or of its direct superclass (for `super`) — methods named
    /// after the target class with no return type — resolved by the same
    /// applicability phases as a method invocation
    /// ([§15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2)).
    /// The invocation is a statement form and has no value.
    fn ctor_call(
        &mut self,
        expr: ExprId,
        args: &[ExprId],
        target: hir_expand::body::CtorCallTarget,
    ) -> Ty {
        let receiver_ty = match target {
            hir_expand::body::CtorCallTarget::This => self.enclosing_class.clone(),
            // `super(args)`: the candidates are the constructors of the
            // direct superclass ([§8.8.7.1]), not of the enclosing class.
            hir_expand::body::CtorCallTarget::Super => Some(self.super_ty()),
        }
        .filter(|ty| !ty.is_error(self.db));
        let Some(receiver_ty) = receiver_ty else {
            for arg in args {
                let _ = self.infer_expr(*arg);
            }
            return self.error();
        };
        // Constructors are declared under the class's simple name in source;
        // library classes keep the JVMS `<init>` name ([JVMS §4.6]), so the
        // member lookup normalizes by how the target resolves ([§8.8.7.1]).
        let (fqn, _) = receiver_ty.as_reference(self.db).expect("class reference");
        let owner = Name::new(fqn.as_str().rsplit('.').next().unwrap_or(fqn.as_str()));
        let name = match hir::fqn_resolve(self.db, &self.scope, fqn.as_str()) {
            Some(hir::Resolved::Library(_)) => Name::new("<init>"),
            _ => owner.clone(),
        };
        // §8.8.7.1: an explicit `super(...)` delegation accesses the
        // superclass's constructors through the `super` keyword, whose
        // invocation mode is `Super` ([§15.12.1]) — a *protected* superclass
        // constructor may be invoked from a subclass this way even when the
        // receiver-type rule of [§6.6.2] would reject a virtual access.
        // (`this(...)` delegation, by contrast, is a virtual invocation.)
        let mode = match target {
            hir_expand::body::CtorCallTarget::Super => InvocationMode::Super,
            hir_expand::body::CtorCallTarget::This => InvocationMode::Virtual,
        };
        let access = self.access.with_mode(mode);
        let arg_kinds = self.arg_kinds(args);
        match self.resolve_call(&receiver_ty, &name, &arg_kinds, None, &access, None) {
            Some((method, deferred)) => {
                self.reinfer_deferred(&method, &deferred);
                // §11.2.1: a delegating constructor's declared exceptions add
                // to the enclosing liability.
                for thrown in &method.throws {
                    if self.is_checked(thrown) {
                        self.thrown.push((thrown.clone(), expr));
                    }
                }
                // §8.8.7.1/[§16]: a `this(...)` delegation runs the target
                // constructor before the rest of this body, so any blank
                // `final` field the target assigns is already assigned for
                // the remainder of *this* constructor — a later write to it
                // is the already-assigned error. (A `super(...)` delegation
                // assigns only the superclass's fields, which this class
                // cannot initialize.)
                if target == hir_expand::body::CtorCallTarget::This
                    && let Some(owner_file) = method.owner_file
                    && let Some(method_item) = find_method_item(self.db, owner_file, &method)
                    && let Some(types) = body_types(self.db, owner_file, method_item)
                {
                    for touched in &types.field_touched {
                        self.flow.field_touched.insert(touched.clone());
                    }
                }
            }
            None => {
                // The concrete arguments were already inferred (and their
                // diagnostics reported) by `arg_kinds`; only the poly
                // arguments still need their standalone types.
                reinfer_poly_standalone(self, &arg_kinds);
                let members =
                    member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
                if members.is_empty() {
                    // §8.8.7.1: no constructor of that signature exists.
                    self.report(TypeError::NoSuchConstructor {
                        expr,
                        name: name.clone(),
                    });
                } else {
                    let name = name.clone();
                    let found = args.len();
                    self.report_wrong_arity(expr, name, Some(owner), &members, &arg_kinds, found);
                }
            }
        }
        // §8.8.7.1: the explicit constructor invocation (and the implicit
        // one) is the point past which the object is fully usable — its own
        // arguments were inferred *inside* the before-super window above
        // (`this(...)`/`super(...)` argument expressions may not reference
        // the un-initialized instance either).
        self.before_super = false;
        self.primitive(PrimitiveType::Void)
    }

    /// Builds the `cant.apply.symbol` diagnostic for a failed invocation:
    /// the *closest* candidate by arity carries the `required:` list, and the
    /// inferred argument types carry the `found:` list — javac's verbatim
    /// message block ([JLS §15.12.2]). When some candidate has exactly the
    /// given arity, the reason is the first argument-to-formal conversion
    /// failure against it (`incompatible types: …`); otherwise the arities
    /// differ and javac's argument-list-length text applies. `owner` is
    /// `Some` for a constructor invocation ([§15.9], [§8.8.7.1]) and opens
    /// the message with javac's `constructor {Owner}() cannot be applied…`.
    fn report_wrong_arity(
        &mut self,
        expr: ExprId,
        name: Name,
        owner: Option<Name>,
        members: &[crate::java::method::MethodData],
        arg_kinds: &[ArgInfo],
        found: usize,
    ) {
        let expected = members
            .first()
            .map(|m| m.params.len())
            .unwrap_or(arg_kinds.len());
        let best = members
            .iter()
            .min_by_key(|m| m.params.len().abs_diff(found))
            .cloned();
        let required = best
            .as_ref()
            .map(|m| m.params.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        // The actual argument types: a concrete argument carries its own
        // type; a poly argument (or an error/void one) has no standalone type
        // and renders as `<poly>`.
        let found_tys: Vec<Option<Ty>> = arg_kinds
            .iter()
            .map(|info| match info.leaves.as_slice() {
                [ArgKind::Concrete(ty)] => Some(*ty),
                _ => self
                    .types
                    .get(&info.id)
                    .copied()
                    .filter(|ty| !ty.is_error(self.db) && !ty.is_void_like(self.db)),
            })
            .collect();
        // The source range of every actual argument ([JLS §15.12.2]): the
        // "bad arguments" the diagnostic underlines, IntelliJ-style.
        let arg_ranges: Vec<TextRange> = arg_kinds
            .iter()
            .filter_map(|info| self.tree.expr_range(info.id))
            .collect();
        // The reason lines: against a same-arity candidate, every concrete
        // argument that does not convert (loosely) to its formal ([§5.3]) is
        // a mismatch; the first renders the `reason:` line, each also surfaces
        // at its own range as `related_information`.
        let mut bad_args = Vec::new();
        if let Some(best) = best.as_ref()
            && best.params.len() == found
        {
            for (idx, (info, formal)) in arg_kinds.iter().zip(&best.params).enumerate() {
                if info.poly {
                    continue;
                }
                if let [ArgKind::Concrete(ty)] = info.leaves.as_slice()
                    && !crate::java::subtyping::is_assignable(self.db, &self.scope, ty, formal)
                {
                    bad_args.push((idx, *ty, *formal));
                }
            }
        }
        // §15.12.2: when the invocation supplies *more* arguments than the
        // closest candidate takes, the surplus arguments are the offending
        // ones — the diagnostic points at them (IntelliJ-style) instead of
        // the whole argument list. No argument is specifically at fault when
        // the closest candidate is not unique (the arity is ambiguous) or the
        // call is too short; the diagnostic then stays on the member name.
        let min_distance = members
            .iter()
            .map(|m| m.params.len().abs_diff(found))
            .min()
            .unwrap_or(0);
        let ambiguous = members
            .iter()
            .filter(|m| m.params.len().abs_diff(found) == min_distance)
            .count()
            > 1;
        let surplus: Vec<usize> = match &best {
            Some(best) if !ambiguous && best.params.len() < found => {
                (best.params.len()..found).collect()
            }
            _ => Vec::new(),
        };
        // §15.12.2: every incompatible argument beyond the first is reported
        // as its own diagnostic at its own range — a split so each bad
        // argument draws its own error line (IntelliJ-style) instead of riding
        // as `related_information` on the first one, which an editor renders
        // only on that first range. Collected before `bad_args` moves into the
        // primary diagnostic, reported after it so the summary comes first.
        let extra_bad: Vec<(ExprId, Ty, Ty)> = bad_args
            .iter()
            .skip(1)
            .filter_map(|(idx, found, expected)| {
                arg_kinds.get(*idx).map(|info| (info.id, *found, *expected))
            })
            .collect();
        let _ = expected;
        self.report(TypeError::WrongArity {
            expr,
            name,
            owner,
            found,
            expected,
            required,
            found_tys,
            arg_ranges,
            bad_args,
            surplus,
        });
        for (expr, found, expected) in extra_bad {
            self.report(TypeError::IncompatibleTypes {
                expr,
                found,
                expected,
            });
        }
    }

    fn method_call(
        &mut self,
        expr: ExprId,
        receiver: Option<ExprId>,
        name: Name,
        args: &[ExprId],
        target: Option<Ty>,
        explicit_type_args: Option<Vec<Ty>>,
    ) -> Ty {
        let (receiver_ty, mode, method_name_form) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        let arg_kinds = self.arg_kinds(args);
        // §15.12.1: no member of the name on the receiver is a compile-time
        // error; members of the name that are all inapplicable (§15.12.2) is a
        // wrong-argument-count error.
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        match self.resolve_call(
            &receiver_ty,
            &name,
            &arg_kinds,
            target,
            &access,
            explicit_type_args,
        ) {
            Some((method, deferred)) => {
                // §18.5.2.2/§18.5.2.4: the resolved formal parameters are the
                // target types of the poly arguments — the lambda, method
                // reference or nested invocation is re-inferred against the
                // instantiated formal ([JLS §18.5.2.4]).
                self.reinfer_deferred(&method, &deferred);
                // §15.12.3: an unqualified invocation (`MethodName` form) of an
                // instance method from a static context — where `this` is
                // unavailable ([§8.1.3]) — is a compile-time error; javac
                // rejects the selected instance method here rather than
                // excluding it from the member set, so an overload that is less
                // specific than a static candidate still resolves first.
                if self.static_context && method_name_form && !method.is_static {
                    self.report(TypeError::NonStaticMethodFromStaticContext {
                        expr,
                        name: name.clone(),
                    });
                }
                // §8.8.7.1: an unqualified invocation of an instance method of
                // the class under construction before the supertype constructor
                // has run is a compile-time error.
                if self.before_super && method_name_form && !method.is_static {
                    self.report(TypeError::CannotReferenceBeforeSuper {
                        expr,
                        name: name.clone(),
                    });
                }
                // §11.2.1: an invocation of a method that declares checked
                // exceptions adds them to the enclosing liability.
                for thrown in &method.throws {
                    if self.is_checked(thrown) {
                        self.thrown.push((thrown.clone(), expr));
                    }
                }
                method.ret
            }
            // On total failure the poly arguments keep their standalone types
            // (a lambda or method reference without a target is the error
            // type; a nested invocation resolves in isolation), so the
            // recorded types stay those of the argument expressions as
            // independent expressions. The concrete arguments were already
            // inferred by `arg_kinds` — re-inferring them would duplicate
            // their diagnostics.
            None => {
                reinfer_poly_standalone(self, &arg_kinds);
                // §15.12.1: no method of the name on the receiver. A receiver
                // that itself failed to type (an unassigned local, a failed
                // call) has reported its own error — do not cascade.
                if members.is_empty() {
                    if !receiver_ty.is_error(self.db) {
                        // §6.6: methods of the name exist but are not
                        // accessible from the enclosing class.
                        if !self.report_illegal_method_access(
                            expr,
                            receiver_ty,
                            name.as_str(),
                            &access,
                        ) {
                            self.report(TypeError::NoSuchMethod {
                                expr,
                                name: name.clone(),
                            });
                        }
                    }
                } else {
                    // §15.12.2: members of the name exist but none is
                    // applicable to the actual arguments.
                    let name = name.clone();
                    let found = args.len();
                    self.report_wrong_arity(expr, name, None, &members, &arg_kinds, found);
                }
                self.error()
            }
        }
    }

    /// The receiver type, invocation mode and *form* of an invocation
    /// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
    /// a bare type name in receiver position is a static invocation whose
    /// receiver is a type, not a value; an unqualified call is an implicit
    /// `this` invocation; a `super` receiver is the superclass of the
    /// enclosing class. The third element reports whether the invocation has
    /// the simple `MethodName` form — an unqualified name that is not a static
    /// import ([§7.5.4]) — which §15.12.3 restricts in static contexts
    /// ([§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)).
    fn receiver_info(
        &mut self,
        receiver: Option<ExprId>,
        name: &Name,
    ) -> (Ty, InvocationMode, bool) {
        match receiver {
            Some(receiver) => {
                // `Type.method(...)` — a static invocation whose receiver
                // expression is a pure type name ([§15.12.1]): a bare name or
                // a qualified name such as `java.util.Collections`.
                if let Some(ty) = self.dotted_type_name(receiver) {
                    return (ty, InvocationMode::Static, false);
                }
                match self.tree.expr(receiver).clone() {
                    // `super.method(...)` — a super invocation whose receiver is
                    // the superclass of the enclosing class ([§15.12.1]).
                    ExprData::Super { qualifier: None } => {
                        // §8.1.3: `super` names the enclosing instance, which
                        // does not exist in a static context. Reported at the
                        // `super` keyword, not the invoked member.
                        if self.static_context {
                            self.report(TypeError::NonStaticThisFromStaticContext {
                                expr: receiver,
                                keyword: NonStaticThisKind::Super,
                            });
                        }
                        (self.super_ty(), InvocationMode::Super, false)
                    }
                    // §15.11.2/§15.12.1: `I.super.m(...)` — a qualified-super
                    // invocation selects the default method of the *named*
                    // interface; the receiver type is `I` itself and the mode
                    // restricts candidates to instance members.
                    ExprData::Super {
                        qualifier: Some(qualifier),
                    } => (
                        {
                            if self.static_context {
                                self.report(TypeError::NonStaticThisFromStaticContext {
                                    expr: receiver,
                                    keyword: NonStaticThisKind::Super,
                                });
                            }
                            resolve_type_ref(self.db, &self.scope, &self.resolver, &qualifier)
                        },
                        InvocationMode::Interface,
                        false,
                    ),
                    // §15.12.1: the receiver of an invocation is inferred
                    // *standalone* — the invocation's own target type does
                    // not reach it, or a chained generic call would inherit
                    // an unrelated expectation (`x.collect(toList())` would
                    // demand `concat(...)`'s stream to be a `List`).
                    _ => (
                        self.with_target(None, |this| this.infer_expr(receiver)),
                        InvocationMode::Virtual,
                        false,
                    ),
                }
            }
            // An unqualified call is an implicit `this` invocation
            // ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
            // unless a static import ([§7.5.4]) names the method through its
            // declaring type. The simple `MethodName` form is subject to
            // §15.12.3's static-context restriction ([§8.1.3]).
            None => {
                if let Some(ty) = self.static_import_method_receiver(name.as_str()) {
                    return (ty, InvocationMode::Static, false);
                }
                (
                    self.unqualified_method_receiver(name.as_str()),
                    InvocationMode::Virtual,
                    true,
                )
            }
        }
    }

    /// The receiver of a simple `MethodName` invocation
    /// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
    /// [§6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)):
    /// the *innermost enclosing declaration* that has a member of the name —
    /// not just the immediately enclosing class. An inner class holds an
    /// enclosing instance ([§8.1.3]), so an instance method of an outer class
    /// is invokable by simple name from an inner body (`log(...)` inside an
    /// anonymous listener). Resolution stops at the first level declaring the
    /// name; a subsequent illegal use is reported there rather than silently
    /// skipped ([§15.12.3]).
    fn unqualified_method_receiver(&self, name: &str) -> Ty {
        let mut levels: Vec<Ty> = Vec::new();
        if let Some(class) = &self.enclosing_class {
            levels.push(class.clone());
        }
        levels.extend(self.enclosing_chain.iter().cloned());
        for class in &levels {
            if !member_set(self.db, &self.scope, class, name, &self.access).is_empty() {
                return class.clone();
            }
        }
        self.enclosing_class.unwrap_or_else(|| self.error())
    }

    /// The receiver of an unqualified call that names a statically imported
    /// method ([§7.5.4]): the declaring type, when that type has a static
    /// member of the name. `None` otherwise — the call falls back to the
    /// implicit `this` receiver.
    fn static_import_method_receiver(&self, simple: &str) -> Option<Ty> {
        for (owner, member) in self.resolver.static_import_owners(simple) {
            let receiver = Ty::reference(self.db, owner.as_str(), Vec::new());
            let access = self.access.with_mode(InvocationMode::Static);
            let has_static = member_set(self.db, &self.scope, &receiver, &member, &access)
                .iter()
                .any(|method| method.is_static);
            if has_static {
                return Some(receiver);
            }
        }
        None
    }

    /// The type of `super` in the current class: the direct superclass
    /// ([JLS §8.1.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.4)),
    /// with the enclosing class's type arguments substituted. The direct
    /// supertypes of a class list the superclass first
    /// ([`supertypes_impl`]).
    fn super_ty(&self) -> Ty {
        let Some(enclosing) = self.enclosing_class else {
            return self.error();
        };
        supertypes_impl(self.db, &self.scope, &enclosing)
            .first()
            .copied()
            .unwrap_or_else(|| self.error())
    }

    /// The kinds of the actual arguments of an invocation for joint inference:
    /// each argument decomposes into its poly leaves ([JLS §15.2]) — a lambda,
    /// method reference or method invocation, or a parenthesized or conditional
    /// expression of them. An argument with no poly leaves is a concrete
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

    /// Resolves an invocation `receiver.name(args)` by the applicability
    /// phases of [JLS §15.12.2]: strict invocation
    /// ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
    /// then loose invocation ([§15.12.2.3]), then variable arity
    /// ([§15.12.2.4]); the most specific applicable method
    /// ([§15.12.2.5]) wins. Returns the inferred invocation type and the poly
    /// arguments to re-infer against the resolved formal parameters
    /// ([JLS §18.5.2.4]). `None` when no method is applicable or the
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

    /// The most specific applicable candidate in `phase`, or `None` when none
    /// applies or the applicable ones are ambiguous ([JLS §15.12.2.5]). Each
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

    /// Instantiates `method` against `arg_kinds` in `phase`, contributing the
    /// argument and target constraints to `inference`. When `resolve` is set
    /// the inference is solved to the invocation type
    /// ([JLS §15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6));
    /// otherwise only consistency is checked — the nested poly invocation
    /// probe of §18.5.2.4, which must not fix variables before the enclosing
    /// invocation's constraints are all in. `None` when `method` is not
    /// applicable in this phase. The poly arguments to re-infer against the
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

    /// Contributes one poly leaf (or concrete argument) against the formal
    /// parameter `formal` ([JLS §18.5.2.2]): a concrete argument constrains
    /// the formal by `⟨S → T⟩`; a lambda or method reference is deferred to the
    /// resolved formal; a nested invocation is resolved against the formal by
    /// contributing its constraints to the shared table ([JLS §18.5.2.4]).
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

    /// §15.9.3: a diamond `new Foo<>()` argument registers the created
    /// class's type variables in the shared inference table and constrains
    /// the parameterized created type against the enclosing formal —
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

    /// Replaces the capture variables of a captured type ([JLS §5.1.10]) by
    /// the wildcard bounds they stand for — `CAP#n` with lower bound `L`
    /// becomes `L`, otherwise its first upper bound. Applied recursively to
    /// type arguments and array elements, so constraints derived against a
    /// captured SAM signature reach the underlying inference variables
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

    /// Infers a lambda's parameter scopes and body against the single abstract
    /// method `sam`, returning the body's result type
    /// ([JLS §15.27.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.3)):
    /// an expression lambda's body type directly, a block lambda's type from
    /// its `return` expressions (`None` when no return carries a value). The
    /// types are recorded speculatively; the final re-inference against the
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

    /// Resolves a nested poly invocation argument against the target formal
    /// ([JLS §18.5.2.4]) by contributing the constraints of its most specific
    /// applicable candidate to the enclosing invocation's inference table.
    /// Each candidate is probed against its own snapshot of the table — the
    /// fresh bound set of [JLS §18.5.1] — so no candidate sees another's
    /// constraints; the applicable ones are collected, and the most specific
    /// ([§15.12.2.5], [JLS §18.5.4]) wins. Only the winner's constraints are
    /// lifted into the enclosing table, the B3 of
    /// [JLS §18.5.2.1]/[§18.5.2.2]; the losing candidates leave no trace, so
    /// a less specific candidate can never poison the enclosing inference.
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

    /// The most specific applicable candidate of `members` against the target
    /// `formal`, contributed to the shared table. `false` when none applies in
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

    /// Re-infers the poly arguments against the resolved formal parameters of
    /// the chosen candidate ([JLS §18.5.2.4]): the lambda, method reference or
    /// nested invocation is typed by its target — the instantiated formal — so
    /// its expression tree records the target-dependent types.
    fn reinfer_deferred(&mut self, method: &MethodData, deferred: &[(ExprId, usize)]) {
        for (arg, index) in deferred {
            if let Some(formal) = method.params.get(*index) {
                let _ = self.with_target(Some(*formal), |this| this.infer_expr(*arg));
            }
        }
    }

    /// A class instance creation ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)):
    /// the created class's type. Constructors are resolved so the arguments
    /// are checked against the same joint inference as a method invocation —
    /// `new Job(() -> {})` types the lambda argument against the resolved
    /// constructor's formal parameter. Source constructors are named after the
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

    /// §15.9: a class instance creation must instantiate a class and not a
    /// type variable, an interface, an abstract class or an enum
    /// ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)).
    /// Source declarations are known directly; library classes are not flagged
    /// (their access flags are not surfaced). Returns whether the creation was
    /// rejected — the caller must not double-report a missing constructor on
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

    /// §15.9.2: the diamond operator — the created class's type arguments are
    /// inferred from the target type. When the target is a reference type
    /// whose erasure is the created class ([§15.9.2.1]), its type arguments
    /// are taken; a *supertype* target (`List<String> l =
    /// new ArrayList<>();`) is handled by unifying the created class's
    /// parameterized supertype named by the target with the target's
    /// arguments — every supertype argument that is one of the created
    /// class's own type variables binds that variable. Otherwise the class
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

    /// The declared type parameters of the class-like declaration `fqn` as
    /// bare type-variable types ([JLS §8.1.2]); empty for non-generic classes
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

    /// The declared type parameters of the class-like declaration `fqn` with
    /// their resolved bounds ([JLS §8.1.2], [§4.4]); empty for non-generic
    /// classes and unresolvable names. Used by the diamond-from-constructor
    /// inference ([§15.9.2.2]) to register the class's type variables as
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

    /// §15.9.2.2: `new Foo<>()` with no target type infers the created
    /// class's type arguments from the constructor's formal parameters —
    /// `new Analyzer<>(new BasicInterpreter())` infers `Analyzer<BasicValue>`
    /// through the `Analyzer(Interpreter<V>)` constructor. The class's type
    /// parameters are registered as inference variables and constrained by
    /// the argument types exactly like a generic method invocation; the first
    /// applicable constructor's solution instantiates the class. Falls back
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
    /// The type of a lambda expression ([JLS §15.27.2]): the target
    /// functional interface ([§15.27.3], [JLS §18.5.2.4]). The lambda's
    /// parameters are typed from the single abstract method of the target
    /// ([JLS §9.8]) and its body is inferred against the SAM's return type —
    /// a return statement inside a lambda body returns from the lambda, not
    /// from the enclosing method.
    fn lambda_type(
        &mut self,
        expr: ExprId,
        params: &[(Name, Option<SpannedTypeRef>, TextRange)],
        body: LambdaBody,
    ) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        // §9.8/§15.27.3: a lambda expression's target must be a functional
        // interface — one with exactly one abstract method.
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
            if !target.is_error(self.db) {
                self.report(TypeError::NotAFunctionalInterface {
                    expr,
                    target: target,
                });
            }
            return self.error();
        };
        if sam.params.len() != params.len() {
            return self.error();
        }
        self.lambda_params.push(FxHashMap::default());
        for ((name, declared, range), formal) in params.iter().zip(&sam.params) {
            let ty = match declared {
                Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                // An inferred parameter takes the SAM formal's type
                // ([§15.27.3]); a captured super wildcard (`Consumer<?
                // super Element>`) contributes its *lower* bound, whose
                // members the parameter actually has.
                None => {
                    let ty = *formal;
                    match ty.kind(self.db) {
                        TyKind::TypeVar {
                            lower: Some(lower), ..
                        } => *lower,
                        _ => ty,
                    }
                }
            };
            self.check_lambda_param_duplicate(expr, name, *range);
            self.lambda_params
                .last_mut()
                .expect("lambda param scope pushed")
                .insert(name.clone(), ty);
        }
        let saved_ret = self.enclosing_ret.replace(self.decapture(&sam.ret));
        // The lambda body is its own flow context ([§15.27.2]): whether it
        // completes normally and which locals it definitely assigns say
        // nothing about the statement that contains the lambda. Its
        // checked-exception liability is likewise settled against the
        // *functional interface's* throws clause ([§15.27.3], [§11.2.1]) —
        // neither discharged by, nor propagated to, the enclosing method.
        let saved_throws = std::mem::replace(&mut self.enclosing_throws, sam.throws.clone());
        let saved_exited = self.exited;
        let saved_flow = self.flow.clone();
        // §15.27/[§8.3.1.2]: a lambda body is never the once-only blank-final
        // field initialization — the lambda may execute any number of times —
        // so a blank-final write inside it is always an error. Track the
        // depth so the write check rejects them.
        self.lambda_depth += 1;
        let thrown_before: FxHashSet<ExprId> = self.thrown.iter().map(|(_, e)| *e).collect();
        match body {
            // §15.27.2: an expression lambda's body is a poly expression
            // whose target is the SAM's return type — decaptured ([§5.1.10])
            // like [`Self::infer_lambda_body_result`]'s target, so a nested
            // generic invocation constrains the wildcard bound instead of
            // dead-ending on the capture variable.
            LambdaBody::Expr(expr) => {
                let _ =
                    self.with_target(Some(self.decapture(&sam.ret)), |this| this.infer_expr(expr));
            }
            LambdaBody::Block(stmt) => self.infer_stmt(stmt),
        }
        self.lambda_depth -= 1;
        self.settle_lambda_thrown(&sam.throws, &thrown_before);
        self.enclosing_throws = saved_throws;
        self.exited = saved_exited;
        self.flow = saved_flow;
        self.enclosing_ret = saved_ret;
        self.lambda_params.pop();
        target
    }

    /// Settles the checked-exception liability accumulated while inferring a
    /// lambda body ([§15.27.3]): every entry added by the body — entries whose
    /// throwing expression was not seen before the body — is checked against
    /// the functional interface's throws clause, reported there when no
    /// declared exception covers it, and always drained so that none of the
    /// body's liability leaks into the enclosing method's.
    fn settle_lambda_thrown(&mut self, throws: &[Ty], before: &FxHashSet<ExprId>) {
        let new_entries: Vec<(Ty, ExprId)> = self
            .thrown
            .iter()
            .filter(|(_, expr)| !before.contains(expr))
            .cloned()
            .collect();
        for (ty, expr) in &new_entries {
            if !throws.iter().any(|declared| {
                crate::java::subtyping::is_assignable(self.db, &self.scope, ty, declared)
            }) {
                self.report(TypeError::UnreportedException {
                    expr: *expr,
                    thrown: *ty,
                });
            }
        }
        if !new_entries.is_empty() {
            self.thrown.retain(|(_, expr)| before.contains(expr));
        }
    }

    /// The type of a method reference ([JLS §15.13.2]): the target functional
    /// interface. The referenced method is resolved against the SAM's
    /// parameters ([§15.13.3]) so the qualifier is inferred.
    fn method_ref_type(
        &mut self,
        expr: ExprId,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
    ) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        // §9.8/§15.13: a method reference's target must be a functional
        // interface too.
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
            if !target.is_error(self.db) {
                self.report(TypeError::NotAFunctionalInterface {
                    expr,
                    target: target,
                });
            }
            return self.error();
        };
        self.resolve_method_ref(qualifier, type_name, name, &sam.params);
        target
    }

    /// The reference type, member-lookup context and *form* of a method
    /// reference ([JLS §15.13.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.1)):
    /// a *type qualifier* — a bare name, `pkg.Type` or a nested `Outer.Inner`
    /// — resolves a type and yields a type-qualified (`Type::m`) reference; an
    /// instance qualifier is inferred as an expression and yields a bound
    /// (`expr::m`) reference. A type-qualified reference (`Path::of`,
    /// `List::stream`) may resolve a static method of an interface as well as
    /// of a class, so the member lookup uses `TypeQualified` mode: the
    /// virtual-invocation member filter of §15.12.3 must not swallow those
    /// static members, or `map`'s `<R>` stays unbound and the chain types
    /// `List<Object>`. `None` when neither qualifier form applies.
    fn method_ref_target(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
    ) -> Option<(Ty, InvocationContext, bool)> {
        let (ref_ty, type_qualified) = match (type_name, qualifier) {
            (Some(tyref), _) => (
                resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                true,
            ),
            (None, Some(expr)) => {
                if let Some(ty) = self.dotted_type_name(expr) {
                    (ty, true)
                } else if let ExprData::Var(name) = self.tree.expr(expr).clone()
                    && let Some(ty) = self.type_name_ty(&name)
                {
                    (ty, true)
                } else {
                    (self.infer_expr(expr), false)
                }
            }
            _ => return None,
        };
        // §15.13.1: a *type-qualified* reference (`Path::of`, `List::stream`)
        // may resolve a static method of an interface as well as of a class,
        // so the virtual-invocation member filter of §15.12.3 must not swallow
        // it — the static members of `Path` (`of(String, String…)`,
        // `of(URI)`) are invisible to `Virtual` mode, which left `map`'s `<R>`
        // unbound and typed the chain `List<Object>`.
        let ctx = if type_qualified {
            self.access.with_mode(InvocationMode::TypeQualified)
        } else {
            self.access.clone()
        };
        Some((ref_ty, ctx, type_qualified))
    }

    /// The member set the referenced method `name` resolves to against the
    /// single abstract method's parameters ([JLS §15.13.3]) and the kind of
    /// reference — how the SAM's parameters map onto the method's. `List::stream`
    /// as a `Function<List<MatchDecision>, ? extends Stream<…>>` calls
    /// `stream()` on `List<MatchDecision>`, so its result is
    /// `Stream<MatchDecision>` rather than a raw `Stream` that would erase the
    /// element type and leave `R` unconstrained. `None` when the reference does
    /// not name a reference type.
    fn method_ref_members(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Option<(Vec<MethodData>, MethodRefKind)> {
        let (ref_ty, ctx, type_qualified) = self.method_ref_target(qualifier, type_name)?;
        if !matches!(ref_ty.kind(self.db), TyKind::Reference { .. }) {
            return None;
        }
        let methods_on_type = member_set(self.db, &self.scope, &ref_ty, name.as_str(), &ctx);
        // §15.13.1/§15.13.3: a *static* reference (`Type::m` naming a static
        // member) and a *bound* reference (`expr::m` — the qualifier value is
        // the receiver) take the SAM's parameters as the method's own; only an
        // *unbound* instance reference (`Type::m` naming an instance member)
        // takes the SAM's first parameter as the receiver, which carries the
        // type arguments the bare type name lacks.
        let kind = if type_qualified && methods_on_type.iter().any(|m| m.is_static) {
            MethodRefKind::Static
        } else if type_qualified {
            MethodRefKind::Unbound
        } else {
            MethodRefKind::Bound
        };
        let receiver = match kind {
            MethodRefKind::Static | MethodRefKind::Bound => ref_ty,
            MethodRefKind::Unbound => {
                // Use the SAM receiver only when it is a *parameterized*
                // reference once inference has made it concrete — an
                // uninstantiated inference variable (`EntityId::externalName`
                // probed against `Function<? super α, …>`) carries no members
                // yet, and a plain or raw receiver (`? super EntityId`,
                // `Object`) adds no type arguments, so the bare type-name
                // lookup keeps the existing resolution (`EntityId::kind`,
                // `String::length`) untouched.
                match sam_params.first() {
                    Some(first) => {
                        let first = self.decapture(first);
                        if matches!(
                            first.kind(self.db),
                            TyKind::Reference { args, .. } if !args.is_empty()
                        ) {
                            first
                        } else {
                            ref_ty
                        }
                    }
                    None => ref_ty,
                }
            }
        };
        let methods = member_set(self.db, &self.scope, &receiver, name.as_str(), &ctx);
        Some((methods, kind))
    }

    /// The method a method reference names that is *potentially applicable*
    /// against the single abstract method's parameters ([JLS §15.13.1] inexact
    /// references): the applicable overload of the member set
    /// [`Self::method_ref_members`] resolves against, chosen by the SAM's
    /// arity and per-parameter compatibility. `None` when the reference names
    /// no reference type or no overload is congruent.
    fn method_ref_candidate(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Option<MethodData> {
        let (methods, kind) = self.method_ref_members(qualifier, type_name, name, sam_params)?;
        self.pick_method_ref(&methods, sam_params, kind)
    }

    /// Whether the method reference is *congruent* with the functional
    /// interface it targets ([JLS §15.13.2]): the referenced member set
    /// contains an overload applicable to the SAM — the check
    /// [`Self::method_ref_candidate`] performs. A reference that names no
    /// member at all reports `cannot find symbol` itself ([§15.12.1]) and a
    /// constructor or array-creation reference is congruent by arity alone, so
    /// neither turns an overload inapplicable before its own diagnostic fires.
    /// `false` means the reference cannot be compatible with this SAM at all —
    /// the hint that lets overload resolution reject the wrong candidate (a
    /// `thenComparing(Comparator)` overload probed with a `Foo::length`
    /// argument, whose `compare` SAM takes one more parameter than the
    /// reference's function descriptor) instead of leaving it applicable and
    /// ambiguous.
    fn method_ref_congruent(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> bool {
        let Some((ref_ty, _, _)) = self.method_ref_target(qualifier, type_name) else {
            return true;
        };
        // §15.13.1/§15.13.4: `Type::new` is a constructor reference — also an
        // *array-creation* reference `T[]::new`; its congruence with a SAM is
        // a matter of constructor arity, resolved against the qualifier type.
        if matches!(
            ref_ty.kind(self.db),
            TyKind::Reference { .. } | TyKind::Array(_)
        ) && (name.as_str() == "new" || name.as_str() == "<missing>")
        {
            return true;
        }
        let Some((methods, kind)) = self.method_ref_members(qualifier, type_name, name, sam_params)
        else {
            return true;
        };
        if methods.is_empty() {
            // §15.12.1: an unknown name is a `cannot find symbol` on the
            // reference itself, not a reason to prefer another overload.
            return true;
        }
        self.pick_method_ref(&methods, sam_params, kind).is_some()
    }

    /// The return type a method reference contributes to inference
    /// ([JLS §15.13.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13.3)):
    /// the referenced method's declared return type instantiated with the
    /// qualifier's type arguments, or — for a constructor reference — the
    /// qualifier type itself. `<error>` when the reference does not resolve;
    /// the caller then simply skips the constraint.
    fn method_ref_return(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) -> Ty {
        let Some((ref_ty, _, _)) = self.method_ref_target(qualifier, type_name) else {
            return self.error();
        };
        // §15.13.1/§15.13.4: `Type::new` is a constructor reference whose
        // "return" is the class itself — also an *array-creation* reference
        // `T[]::new`, whose "return" is the array type (the bounds are the
        // parameters, §15.13.4). The lowering records the `new` token of a
        // constructor reference as `<missing>`, so a missing member name on a
        // type qualifier is a constructor reference too.
        if matches!(
            ref_ty.kind(self.db),
            TyKind::Reference { .. } | TyKind::Array(_)
        ) && (name.as_str() == "new" || name.as_str() == "<missing>")
        {
            return ref_ty;
        }
        if !matches!(ref_ty.kind(self.db), TyKind::Reference { .. }) {
            return self.error();
        }
        // §15.13.1: an *inexact* reference to an overloaded method selects the
        // applicable candidate by the target functional interface — the SAM's
        // arity and per-parameter compatibility — not the first declared
        // overload, whose result would otherwise steer the enclosing
        // inference (`list.stream().map(P::of)` binding `map`'s `<R>`) to
        // `Object` and reject a `List<P>` constructor argument.
        self.method_ref_candidate(qualifier, type_name, name, sam_params)
            .map(|method| method.ret)
            .unwrap_or_else(|| self.error())
    }

    /// The method of a method reference's member set that is a *potentially
    /// applicable* candidate against the single abstract method's parameter
    /// types ([JLS §15.13.1] inexact references): arity — `sam_params.len()`
    /// value parameters for a static or bound reference, one fewer for an
    /// unbound instance reference whose receiver is the SAM's first parameter
    /// ([§15.13.3]) — and each corresponding SAM parameter assignable
    /// to the method's parameter in a loose invocation context
    /// ([§15.12.2.3]). The most specific applicable candidate
    /// ([§15.12.2.5]) wins; `None` when no overload is congruent, so the
    /// reference does not steer inference with a result.
    fn pick_method_ref(
        &self,
        methods: &[MethodData],
        sam_params: &[Ty],
        kind: MethodRefKind,
    ) -> Option<MethodData> {
        let sam_count = sam_params.len();
        // The value parameters the referenced method must accept: all the
        // SAM's for a static or bound reference, all but the leading receiver
        // for an unbound instance reference.
        let base = match kind {
            MethodRefKind::Static | MethodRefKind::Bound => sam_count,
            MethodRefKind::Unbound => sam_count.saturating_sub(1),
        };
        // The SAM index of the referenced method's first value parameter: the
        // receiver is the SAM's first parameter for an unbound instance
        // reference, so the method's parameters start one slot in.
        let offset = match kind {
            MethodRefKind::Unbound => 1,
            MethodRefKind::Static | MethodRefKind::Bound => 0,
        };
        let mut applicable: Vec<MethodData> = Vec::new();
        for method in methods {
            let params = &method.params;
            // §15.12.2.4: a variable-arity method is applicable when the SAM
            // supplies at least its fixed parameters; the remainder pack into
            // the trailing array element.
            let (fixed, tail) = if method.varargs {
                if params.is_empty() || base < params.len() - 1 {
                    continue;
                }
                let split = params.len() - 1;
                match params[split].element(self.db) {
                    Some(element) => (&params[..split], Some((*element, base - split))),
                    None => continue,
                }
            } else {
                if params.len() != base {
                    continue;
                }
                (params.as_slice(), None)
            };
            let mut ok = true;
            for (i, method_param) in fixed.iter().enumerate() {
                if !self.method_ref_param_compatible(sam_params.get(i + offset), method_param) {
                    ok = false;
                    break;
                }
            }
            if ok && let Some((element, tail_count)) = &tail {
                for k in 0..*tail_count {
                    if !self.method_ref_param_compatible(
                        sam_params.get(fixed.len() + k + offset),
                        element,
                    ) {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                applicable.push(method.clone());
            }
        }
        if applicable.is_empty() {
            return None;
        }
        let pairs: Vec<(MethodData, MethodData)> =
            applicable.iter().map(|m| (m.clone(), m.clone())).collect();
        crate::java::method::choose_most_specific(self.db, &self.scope, &pairs)
            .or_else(|| applicable.into_iter().next())
    }

    /// Whether the referenced method's parameter accepts the SAM's
    /// corresponding parameter ([JLS §15.13.2]): the SAM type converts to the
    /// method's type in a loose invocation context ([§15.12.2.3]). A SAM
    /// parameter still carrying unresolved inference variables cannot be
    /// decided — the candidate stays applicable rather than steering inference
    /// away from a valid overload.
    fn method_ref_param_compatible(&self, sam_param: Option<&Ty>, method_param: &Ty) -> bool {
        let Some(decaptured) = sam_param.map(|sam_param| self.decapture(sam_param)) else {
            return false;
        };
        if decaptured.contains_infer_var(self.db) || method_param.contains_infer_var(self.db) {
            return true;
        }
        crate::java::subtyping::is_assignable(self.db, &self.scope, &decaptured, method_param)
    }

    fn resolve_method_ref(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&SpannedTypeRef>,
        name: &Name,
        sam_params: &[Ty],
    ) {
        // §15.13.1: resolve the applicable overload against the SAM — the same
        // selection [`Self::method_ref_return`] feeds the inference constraints
        // with — so a reference to a name that resolves only to inapplicable
        // overloads does not silently type against the first declaration.
        let _ = self.method_ref_candidate(qualifier, type_name, name, sam_params);
    }

    fn unary(&mut self, expr: ExprId, op: UnaryOp) -> Ty {
        // §15.15.1/§15.15.2: `++`/`--` mutate their operand.
        self.mutating = matches!(op, UnaryOp::Inc | UnaryOp::Dec);
        let inner = self.infer_expr(expr);
        self.mutating = false;
        match op {
            // §15.15.6: `!` has type `boolean` and its operand must be a
            // `boolean` ([§15.15.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.15.6)).
            UnaryOp::Not => {
                if !self.is_boolean(inner) {
                    if !inner.is_error(self.db) {
                        self.types.insert(expr, self.error());
                        self.report(TypeError::NonBooleanCondition { expr, found: inner });
                    }
                    self.error()
                } else {
                    // §16.1.4: `!a` swaps the true and false outcome flows.
                    let (true_flow, false_flow) = self.take_bool_outcomes();
                    self.bool_outcomes = Some((false_flow, true_flow));
                    self.primitive(PrimitiveType::Boolean)
                }
            }
            // §15.15.1-3: unary numeric promotion (§5.6.1).
            UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => {
                let promoted = self.unary_promotion(inner);
                if promoted.is_error(self.db) {
                    if !inner.is_error(self.db) {
                        self.types.insert(expr, self.error());
                        self.report(TypeError::IncompatibleOperand {
                            expr,
                            op: unary_op_symbol(op),
                            found: inner,
                            other: None,
                        });
                    }
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.15.1/§15.15.2: `++`/`--` have the operand's type.
            UnaryOp::Inc | UnaryOp::Dec => inner,
        }
    }

    /// The type of a conditional expression over two operands, following the
    /// rules of [§15.25.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25.2)
    /// and [§15.25.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25.3):
    /// identical types keep their type, operands convertible to a numeric type
    /// (primitive or boxed, [§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8))
    /// follow binary numeric promotion ([§5.6.2]), the null type yields the
    /// reference operand, and reference types fall back to the least upper
    /// bound ([§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
    fn conditional_type(&self, then_ty: Ty, els_ty: Ty) -> Ty {
        if then_ty == els_ty {
            return then_ty;
        }
        // §15.25: when at least one operand is primitive *and* the other
        // unboxes to a primitive too, the primitive rules apply. A primitive
        // against an unrelated reference takes the least upper bound below,
        // as do two references: two boxed numerics never promote (`c ?
        // Integer : Long` is `Number`, not `long`).
        let then_prim = matches!(then_ty.kind(self.db), TyKind::Primitive(_));
        let els_prim = matches!(els_ty.kind(self.db), TyKind::Primitive(_));
        if then_prim || els_prim {
            let l = self.unboxed_operand(then_ty);
            let r = self.unboxed_operand(els_ty);
            let both_primitive = matches!(l.kind(self.db), TyKind::Primitive(_))
                && matches!(r.kind(self.db), TyKind::Primitive(_));
            if both_primitive {
                // §15.25: the boolean rules — a `boolean`/`Boolean` mix has
                // type `boolean`; a boolean against any other primitive is
                // ill-typed (not silently promoted).
                let boolean = matches!(l.kind(self.db), TyKind::Primitive(PrimitiveType::Boolean))
                    || matches!(r.kind(self.db), TyKind::Primitive(PrimitiveType::Boolean));
                if boolean {
                    return match (l.kind(self.db), r.kind(self.db)) {
                        (
                            TyKind::Primitive(PrimitiveType::Boolean),
                            TyKind::Primitive(PrimitiveType::Boolean),
                        ) => self.primitive(PrimitiveType::Boolean),
                        _ => self.error(),
                    };
                }
                return self.binary_numeric_promotion(l, r);
            }
        }
        // §15.25: the null rules — `cond ? null : T` has type T (and
        // symmetrically). An array is a reference type ([§4.3.1]), so a
        // null/arm-array pair keeps the array type instead of taking a
        // meaningless lub. A *primitive* arm against `null` is boxed
        // ([§5.1.7]): `cond ? null : 5` has type `Integer` and
        // `cond ? true : null` has type `Boolean` (javac assigns them to the
        // boxed types), not the lub of the primitive and `null`.
        if then_ty.is_null(self.db) && (els_ty.is_reference(self.db) || els_ty.is_array(self.db)) {
            return els_ty;
        }
        if els_ty.is_null(self.db) && (then_ty.is_reference(self.db) || then_ty.is_array(self.db)) {
            return then_ty;
        }
        if then_ty.is_null(self.db) && els_ty.is_primitive(self.db) {
            return self.box_primitive(els_ty);
        }
        if els_ty.is_null(self.db) && then_ty.is_primitive(self.db) {
            return self.box_primitive(then_ty);
        }
        // §5.1.10: the lub of two references is never a wildcard — a bare
        // `?` from the lcta degenerates to its capture so the expression has
        // a valid type.
        let lub = least_upper_bound(self.db, &self.scope, &[then_ty, els_ty]);
        if matches!(lub.kind(self.db), TyKind::Wildcard(_)) {
            capture_conversion(self.db, &self.scope, lub)
        } else {
            lub
        }
    }

    /// The operand of a conditional in its unboxed form ([JLS §5.1.8]): a
    /// primitive keeps its type; a boxed reference unboxes; anything else is
    /// left for [`Self::binary_numeric_promotion`] to reject.
    fn unboxed_operand(&self, ty: Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::Primitive(_) => ty,
            TyKind::Reference { name, .. } => match unboxed_primitive(name.as_str()) {
                Some(p) => Ty::primitive(self.db, p),
                None => ty,
            },
            _ => ty,
        }
    }

    /// Unary numeric promotion ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)):
    /// a boxed operand is first unboxed ([§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
    /// then `byte`, `short` and `char` promote to `int`; everything else keeps
    /// its type.
    fn unary_promotion(&self, ty: Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::Primitive(PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char) => {
                self.primitive(PrimitiveType::Int)
            }
            TyKind::Primitive(_) => ty,
            // A boxed primitive operand is unboxed before the promotion
            // applies (§5.6.1, §5.1.8): `-Integer` is `int`, `~Long` is `long`.
            TyKind::Reference { name, .. } => match unboxed_primitive(name.as_str()) {
                Some(p) => self.unary_promotion(Ty::primitive(self.db, p)),
                None => self.error(),
            },
            _ => self.error(),
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: ExprId, rhs: ExprId) -> Ty {
        // §15.23: `&&`/`||` always have type `boolean`. §14.30.3: a pattern
        // variable of the left operand is in scope in the right-hand operand
        // (flow scoping), so the operands are inferred once, in that order —
        // the ordinary two-operand pass below would re-infer the right-hand
        // operand without the pattern variables in scope.
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            // §16.1.2/[§16.1.3]: the definite-assignment outcomes of `a && b`
            // (and `a || b`): the left operand is inferred first and its
            // true/false flows captured; the right operand then runs under the
            // left's *matched* flow (`&&` → true, `||` → false), and the whole
            // expression's outcomes join the two ways the value arises. This
            // is what lets `if (v > 0 && (k = read()) >= 0) use(k)` (JLS
            // Example 16-1) treat `k` as definitely assigned in the guarded
            // code.
            self.check_condition(lhs);
            let (lhs_true_flow, lhs_false_flow) = self.take_bool_outcomes();
            self.scopes.push(FxHashMap::default());
            // §6.3.2: the pattern variables of the left operand that are
            // *definitely matched* when the right operand evaluates are in
            // scope there — for `a && b` the true flow of `a` (b runs only
            // when a matched), for `a || b` its false flow (b runs only when
            // a failed, so a negated pattern `!(x instanceof T t)` has t
            // matched there).
            if let Some((lhs_true, lhs_false)) = self.pattern_flow(lhs) {
                let matched = match op {
                    BinaryOp::And => lhs_true,
                    _ => lhs_false,
                };
                self.flow = match op {
                    BinaryOp::And => lhs_true_flow.clone(),
                    _ => lhs_false_flow.clone(),
                };
                for binding in matched {
                    self.scope_binding(binding);
                }
            } else {
                self.flow = match op {
                    BinaryOp::And => lhs_true_flow.clone(),
                    _ => lhs_false_flow.clone(),
                };
            }
            self.check_condition(rhs);
            let (rhs_true_flow, rhs_false_flow) = self.take_bool_outcomes();
            self.scopes.pop();
            // §16.1.2/[§16.1.3]: `a && b` is true only via (a true, b true);
            // false via (a false) or (a true, b false). `a || b` is true via
            // (a true) or (a false, b true); false only via (a false,
            // b false). The join ([§16.1]) intersects the definite sets and
            // unions the touched fields.
            let (true_flow, false_flow) = match op {
                BinaryOp::And => {
                    let mut false_flow = lhs_false_flow;
                    false_flow.join_definite(&rhs_false_flow);
                    (rhs_true_flow, false_flow)
                }
                _ => {
                    let mut true_flow = lhs_true_flow;
                    true_flow.join_definite(&rhs_true_flow);
                    (true_flow, rhs_false_flow)
                }
            };
            // §16.1.2/[§16.1.3]: the after-expression flow for a non-condition
            // consumer is the join of both outcomes.
            let mut joined = true_flow.clone();
            joined.join_definite(&false_flow);
            self.flow = joined;
            self.bool_outcomes = Some((true_flow, false_flow));
            return self.primitive(PrimitiveType::Boolean);
        }
        let lhs_ty = self.infer_expr(lhs);
        let rhs_ty = self.infer_expr(rhs);
        match op {
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Add | BinaryOp::Sub => {
                // §15.18.1: `+` with a `String` operand is string
                // concatenation and has type `String`.
                if matches!(op, BinaryOp::Add) && (self.is_string(lhs_ty) || self.is_string(rhs_ty))
                {
                    return self.string();
                }
                let promoted = self.binary_numeric_promotion(lhs_ty, rhs_ty);
                if promoted.is_error(self.db) {
                    // §15.17/§15.18/§15.22: a numeric operator on a non-numeric
                    // operand is a compile-time error.
                    if !lhs_ty.is_error(self.db) && !rhs_ty.is_error(self.db) {
                        let bad = if !self.is_numeric_operand(lhs_ty) {
                            lhs
                        } else {
                            rhs
                        };
                        self.types.insert(bad, self.error());
                        let (bad_ty, other_ty) = if bad == lhs {
                            (lhs_ty, rhs_ty)
                        } else {
                            (rhs_ty, lhs_ty)
                        };
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
                            found: bad_ty,
                            other: Some(other_ty),
                        });
                    }
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.22.1/§15.22.2: the bitwise/logical operators are binary
            // numeric promotion on numeric operands (§15.22.1) or boolean
            // logical operators on `boolean` operands (§15.22.2); a `boolean`
            // mixed with a non-`boolean` operand is an error.
            BinaryOp::BitAnd | BinaryOp::BitXor | BinaryOp::BitOr => {
                let (a_bool, b_bool) = (self.is_boolean(lhs_ty), self.is_boolean(rhs_ty));
                if a_bool && b_bool {
                    return self.primitive(PrimitiveType::Boolean);
                }
                if a_bool != b_bool {
                    if !lhs_ty.is_error(self.db) && !rhs_ty.is_error(self.db) {
                        let bad = if a_bool { rhs } else { lhs };
                        self.types.insert(bad, self.error());
                        let (bad_ty, other_ty) = if bad == lhs {
                            (lhs_ty, rhs_ty)
                        } else {
                            (rhs_ty, lhs_ty)
                        };
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
                            found: bad_ty,
                            other: Some(other_ty),
                        });
                    }
                    return self.error();
                }
                let promoted = self.binary_numeric_promotion(lhs_ty, rhs_ty);
                if promoted.is_error(self.db) {
                    if !lhs_ty.is_error(self.db) && !rhs_ty.is_error(self.db) {
                        let bad = if !self.is_numeric_operand(lhs_ty) {
                            lhs
                        } else {
                            rhs
                        };
                        self.types.insert(bad, self.error());
                        let (bad_ty, other_ty) = if bad == lhs {
                            (lhs_ty, rhs_ty)
                        } else {
                            (rhs_ty, lhs_ty)
                        };
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
                            found: bad_ty,
                            other: Some(other_ty),
                        });
                    }
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.19: a shift has the unary-promoted type of the left operand, and
            // each of the operands undergoes unary numeric promotion
            // ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1))
            // — a non-numeric operand on either side is an error.
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::UShr => {
                let promoted = self.unary_promotion(lhs_ty);
                let rhs_numeric = self.is_numeric_operand(rhs_ty);
                if promoted.is_error(self.db) || !rhs_numeric {
                    if !lhs_ty.is_error(self.db) && !rhs_ty.is_error(self.db) {
                        let bad = if promoted.is_error(self.db) { lhs } else { rhs };
                        self.types.insert(bad, self.error());
                        let (bad_ty, other_ty) = if bad == lhs {
                            (lhs_ty, rhs_ty)
                        } else {
                            (rhs_ty, lhs_ty)
                        };
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
                            found: bad_ty,
                            other: Some(other_ty),
                        });
                    }
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.20-15.24: relational, equality and boolean-logical
            // expressions have type `boolean`; §15.20/§15.21 demand comparable
            // operands.
            BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge
            | BinaryOp::Eq
            | BinaryOp::Ne => {
                if !self.comparable(lhs_ty, rhs_ty)
                    && !lhs_ty.is_error(self.db)
                    && !rhs_ty.is_error(self.db)
                {
                    self.types.insert(lhs, self.error());
                    self.report(TypeError::IncomparableTypes {
                        expr: lhs,
                        op: binary_op_symbol(op),
                        found: lhs_ty,
                        other: rhs_ty,
                    });
                }
                self.primitive(PrimitiveType::Boolean)
            }
            // Handled above: `&&`/`||` are inferred with pattern flow scoping.
            BinaryOp::And | BinaryOp::Or => self.primitive(PrimitiveType::Boolean),
        }
    }

    /// Binary numeric promotion ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2)):
    /// the promoted type is the "widest" of the two operand types; `byte`,
    /// `short` and `char` promote to `int`. A boxed reference operand is first
    /// unboxed ([§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
    /// so `Integer + Integer` and `int + Integer` both promote to `int`
    /// ([§5.6.2]). A non-numeric reference operand cannot be unboxed and makes
    /// the expression ill-typed.
    fn binary_numeric_promotion(&self, lhs: Ty, rhs: Ty) -> Ty {
        // §5.6.2: `byte`, `short` and `char` promote to `int`; the wider of
        // the two operand types is the promoted type. The same applies to a
        // boxed operand after unboxing ([§5.1.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.8)),
        // so `Integer + Integer` and `Character + Character` both promote to
        // `int` ([§5.6.2]).
        let promote = |ty: Ty| match ty.kind(self.db) {
            TyKind::Primitive(p) => Some(numeric_promotion(*p)),
            TyKind::Reference { name, .. } => {
                unboxed_primitive(name.as_str()).map(numeric_promotion)
            }
            _ => None,
        };
        let (lhs, rhs) = (promote(lhs), promote(rhs));
        let promoted = match (lhs, rhs) {
            (Some(PrimitiveType::Double), _) | (_, Some(PrimitiveType::Double)) => {
                PrimitiveType::Double
            }
            (Some(PrimitiveType::Float), _) | (_, Some(PrimitiveType::Float)) => {
                PrimitiveType::Float
            }
            (Some(PrimitiveType::Long), _) | (_, Some(PrimitiveType::Long)) => PrimitiveType::Long,
            (Some(PrimitiveType::Int), _) | (_, Some(PrimitiveType::Int)) => PrimitiveType::Int,
            _ => return self.error(),
        };
        // §5.6.2: an operand that did not promote — a reference type that
        // cannot be unboxed, or a non-numeric type — makes the expression
        // ill-typed even when the other operand promotes.
        if lhs.is_none() || rhs.is_none() {
            return self.error();
        }
        self.primitive(promoted)
    }

    /// The element type of an `Iterable<T>` for a for-each loop
    /// ([§14.14.2.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2.1)):
    /// the `T` of the `Iterable<T>` — the `E` of the `Iterator<E>` returned by
    /// `iterator()`. `None` when the type is not an `Iterable` (the caller
    /// reports [`TypeError::NonIterableForEach`]).
    fn iterable_element(&self, iterable: Ty) -> Option<Ty> {
        let iterator = pick_method(
            self.db,
            &self.scope,
            &iterable,
            "iterator",
            &[],
            &self.access,
            None,
        )?;
        pick_method(
            self.db,
            &self.scope,
            &iterator.ret,
            "next",
            &[],
            &self.access,
            None,
        )
        .map(|method| method.ret)
    }

    fn declare_local(&mut self, id: LocalId) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => self.error(),
        };
        self.bind_local(id, local.name, ty);
    }

    /// Declares a formal parameter, which is definitely assigned throughout
    /// its body ([JLS §16.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html#jls-16.1.5),
    /// [§16.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html#jls-16.1.9)).
    fn declare_param(&mut self, id: LocalId) {
        self.declare_local(id);
        self.flow.definite.insert(id);
    }

    fn declare_local_ty(&mut self, id: LocalId, fallback: Ty) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => fallback,
        };
        self.bind_local(id, local.name, ty);
    }

    fn bind_local(&mut self, id: LocalId, name: Name, ty: Ty) {
        // §6.4: a local variable, parameter, exception parameter, loop or
        // resource variable may not duplicate a name already declared as a
        // local in the same or an *enclosing* scope — local variables do not
        // shadow each other ([§6.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4)).
        // A local declared inside a lambda body likewise may not shadow an
        // enclosing lambda parameter ([§15.27.2], [§6.4]).
        let duplicate = self.lookup_local(&name).is_some()
            || self
                .lambda_params
                .iter()
                .any(|scope| scope.contains_key(&name));
        self.locals.insert(id, ty);
        // Every binding form except a bare declarator initializes its local
        // (parameters, initializers, catch/foreach/resource variables,
        // pattern bindings), so it is definitely assigned ([§16]).
        self.flow.definite.insert(id);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name.clone(), id);
        // The later binding is reported, at the duplicate's own name range.
        if duplicate {
            self.report(TypeError::VariableAlreadyDefined { local: id, name });
        }
    }

    fn lookup_local(&self, name: &Name) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&local) = scope.get(name) {
                return Some(local);
            }
        }
        None
    }

    /// §6.4: a lambda parameter may not duplicate a name already declared in
    /// an enclosing scope — an enclosing lambda parameter or a local variable,
    /// parameter, catch/foreach/resource variable of the enclosing body.
    /// (Lambda parameters *do* shadow enclosing locals; the duplicate is only
    /// an error when the enclosing declaration is in scope at the lambda —
    /// a local declared textually after the lambda is not.) Reported at the
    /// parameter's own name range, matching javac.
    fn check_lambda_param_duplicate(&mut self, expr: ExprId, name: &Name, range: TextRange) {
        // §15.27.1: the lambda's own parameter list may not repeat a name —
        // `(x, x) -> ...` is a compile-time error. The innermost (just-pushed)
        // frame already holds the earlier parameters, so `.iter().rev()`
        // finds a same-list duplicate first, then an enclosing lambda's
        // parameter ([§6.4] innermost-out).
        let duplicate = self
            .lambda_params
            .iter()
            .rev()
            .any(|scope| scope.contains_key(name))
            || self.lookup_local(name).is_some();
        if duplicate {
            self.report(TypeError::LambdaParameterAlreadyDefined {
                lambda: expr,
                name: name.clone(),
                range,
            });
        }
    }

    // -- patterns ([JLS §14.30]) ---------------------------------------------

    /// Resolves the type of a pattern ([JLS §14.30]), recording the type of
    /// each variable it binds ([§14.30.1], [§14.30.2]) into [`Self::locals`].
    /// Returns the pattern's type; the match-all `_` ([§14.30.3]) has none.
    fn pattern_type(&mut self, id: PatternId) -> Ty {
        match self.tree.pattern(id).clone() {
            PatternData::Type(tp) => {
                let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &tp.ty);
                if let Some(binding) = tp.binding {
                    self.locals.insert(binding, ty);
                }
                ty
            }
            PatternData::Record(rp) => {
                let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &rp.ty);
                // §14.30.2: the number of nested patterns must equal the
                // record's component count — a mismatch means the pattern can
                // never match. Conservative: records whose components cannot
                // be recovered are skipped.
                if let Some(expected) = self.record_component_count(&ty)
                    && expected != rp.components.len()
                {
                    self.report(TypeError::IncorrectNumberOfPatternComponents {
                        pattern: id,
                        expected,
                        found: rp.components.len(),
                    });
                }
                for &component in &rp.components {
                    let _ = self.pattern_type(component);
                }
                ty
            }
            PatternData::MatchAll => self.error(),
        }
    }

    /// The number of components ([§8.10.1]) of the record type `ty`, from its
    /// source declaration or its classfile `Record` attribute. `None` when
    /// `ty` is not a record or its components cannot be recovered — the
    /// pattern-arity check ([§14.30.2]) then stays silent.
    fn record_component_count(&self, ty: &Ty) -> Option<usize> {
        let TyKind::Reference { name, .. } = ty.kind(self.db) else {
            return None;
        };
        let resolved = hir::fqn_resolve(self.db, &self.scope, name.as_str())?;
        match resolved {
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(self.db, source.file);
                match tree.data(source.item) {
                    ItemData::Record(record) => Some(record.components.len()),
                    _ => None,
                }
            }
            hir::Resolved::Library(resolved_class) => {
                let record = hir::class_record(self.db, &resolved_class)?;
                match record.as_ref() {
                    hir::ClassOrModuleRecord::Class(class) if class.is_record => {
                        Some(class.record_components.len())
                    }
                    _ => None,
                }
            }
        }
    }

    /// The pattern variables a pattern binds, recursively through record
    /// components ([JLS §14.30.1], [§14.30.2]).
    fn pattern_bindings_of(&self, id: PatternId) -> Vec<LocalId> {
        match self.tree.pattern(id).clone() {
            PatternData::Type(tp) => tp.binding.into_iter().collect(),
            PatternData::Record(rp) => rp
                .components
                .iter()
                .flat_map(|&c| self.pattern_bindings_of(c))
                .collect(),
            PatternData::MatchAll => Vec::new(),
        }
    }

    /// The pattern variables of the `instanceof` expression `id`, recursing
    /// through parenthesization and `&&`/`||` chains ([JLS §14.30.3]) — the
    /// bindings whose scope extends to the enclosing `if` then-arm (or the
    /// right-hand operand of the `&&`/`||`). `None` when `id` is not a
    /// pattern-carrying condition.
    /// The pattern bindings of a boolean expression on each flow outcome:
    /// `(true_flow, false_flow)`
    /// ([JLS §14.30.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.3)
    /// flow scoping). An `instanceof` pattern binds on its true flow only;
    /// `&&` joins the flows of its operands (`a && b` matches on `a`'s true
    /// flow *and* `b`'s, fails on either failure), `||` symmetrically, `!`
    /// swaps the flows, and parentheses are transparent.
    fn pattern_flow(&self, id: ExprId) -> Option<(Vec<LocalId>, Vec<LocalId>)> {
        match self.tree.expr(id).clone() {
            ExprData::InstanceOf { pattern, .. } => Some((
                pattern
                    .map(|p| self.pattern_bindings_of(p))
                    .unwrap_or_default(),
                Vec::new(),
            )),
            ExprData::Paren(inner) => self.pattern_flow(inner),
            ExprData::Unary {
                op: UnaryOp::Not,
                expr: operand,
            } => self
                .pattern_flow(operand)
                .map(|(true_flow, false_flow)| (false_flow, true_flow)),
            ExprData::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
            } => {
                let (lhs_true, lhs_false) = self.pattern_flow(lhs).unwrap_or_default();
                let (rhs_true, rhs_false) = self.pattern_flow(rhs).unwrap_or_default();
                let mut true_flow = lhs_true;
                true_flow.extend(rhs_true);
                let mut false_flow = lhs_false;
                false_flow.extend(rhs_false);
                Some((true_flow, false_flow))
            }
            _ => None,
        }
    }

    fn pattern_binding_ids(&self, id: ExprId) -> Option<Vec<LocalId>> {
        match self.tree.expr(id).clone() {
            ExprData::InstanceOf { pattern, .. } => pattern.map(|p| self.pattern_bindings_of(p)),
            ExprData::Paren(inner) => self.pattern_binding_ids(inner),
            ExprData::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
            } => {
                let mut bindings = self.pattern_binding_ids(lhs).unwrap_or_default();
                bindings.extend(self.pattern_binding_ids(rhs).unwrap_or_default());
                Some(bindings)
            }
            _ => None,
        }
    }

    /// Establishes the scope of a pattern variable ([JLS §14.30.3]) in the
    /// current innermost scope. The variable's type was already recorded by
    /// [`Self::pattern_type`] during expression inference.
    fn scope_binding(&mut self, id: LocalId) {
        let name = self.tree.local(id).name.clone();
        // A pattern variable is definitely assigned wherever it is in scope
        // ([§16.1.13]): it is bound exactly when the pattern matched.
        self.flow.definite.insert(id);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name, id);
    }

    fn infer_stmt(&mut self, id: StmtId) {
        let stmt = self.tree.stmt(id).clone();
        self.infer_stmt_data(id, &stmt);
    }

    fn infer_stmt_data(&mut self, id: StmtId, stmt: &StmtData) {
        match stmt {
            StmtData::Empty => {}
            StmtData::Block(stmts) => {
                self.scopes.push(FxHashMap::default());
                self.infer_block_statements(stmts);
                self.scopes.pop();
            }
            // §14.4/§6.3: the declarators of one declaration statement share
            // the *enclosing* scope, so this is inferred without pushing one
            // (unlike a [`StmtData::Block`]).
            StmtData::DeclGroup(stmts) => {
                for &stmt in stmts {
                    self.infer_stmt(stmt);
                }
            }
            StmtData::Decl { local, initializer } => {
                // §14.4.1: a `var` declaration has no written type — the
                // initializer is inferred standalone and its type becomes the
                // local's (§15.2: a `var` initializer is never poly). A `var`
                // without an initializer is a compile-time error
                // ([§14.4.1]): report it and degrade the local to `error`
                // rather than panic.
                if self.tree.local(*local).ty.is_none() {
                    let Some(initializer) = initializer else {
                        self.report(TypeError::VarWithoutInitializer { local: *local });
                        let local_data = self.tree.local(*local).clone();
                        self.bind_local(*local, local_data.name, self.error());
                        return;
                    };
                    // §14.4.1: an array initializer has no standalone type, so
                    // `var x = { 1, 2, 3 };` cannot be a `var` declaration.
                    if matches!(self.tree.expr(*initializer).clone(), ExprData::ArrayInit(_)) {
                        let local_data = self.tree.local(*local).clone();
                        self.report(TypeError::VarArrayInitializer { local: *local });
                        self.bind_local(*local, local_data.name, self.error());
                        return;
                    }
                    let ty = self.infer_expr(*initializer);
                    let local_data = self.tree.local(*local).clone();
                    self.bind_local(*local, local_data.name, ty);
                    return;
                }
                self.declare_local(*local);
                // §4.12.2: a declared type naming a generic class without its
                // arguments is a raw type — legal, reported as a warning.
                self.warn_raw_declared_type(*local);
                // §4.5.1: a declared type argument that is not within the
                // bounds of the type parameter it fills.
                if let Some(tyref) = self.tree.local(*local).ty.clone() {
                    self.check_type_argument_bounds(DiagLocation::Local(*local), &tyref);
                }
                if initializer.is_none() {
                    // §16: a declarator without an initializer is not
                    // definitely assigned until a later assignment reaches it.
                    self.flow.definite.remove(local);
                    // §4.12.4: a `final` declarator with no initializer is a
                    // *blank* final — it may still be assigned once by
                    // definite assignment, so it is not an un-assignable
                    // final yet.
                    let local_data = self.tree.local(*local).clone();
                    if local_data.is_final {
                        self.blank_finals.insert(*local);
                    }
                }
                if let Some(initializer) = initializer {
                    // The initializer is a poly expression whose target is the
                    // declared type of the local ([JLS §14.4]).
                    let target = self.locals.get(local).copied();
                    let init_ty = self.with_target(target, |this| this.infer_expr(*initializer));
                    // §4.12.4: a `final` local whose initializer is itself a
                    // constant expression is a *constant variable* — later
                    // reads of it are constant expressions ([§15.28]).
                    let local_data = self.tree.local(*local).clone();
                    if local_data.is_final
                        && let Some(value) = self.const_value(*initializer)
                    {
                        self.const_locals.insert(*local, value);
                    }
                    let (Some(target), false) = (target, init_ty.is_error(self.db)) else {
                        return;
                    };
                    if target.is_error(self.db) {
                        return;
                    }
                    // §10.6: an array-initializer expression in a declaration
                    // gets its element type from the target ([§15.2]) — an
                    // empty `{}` types as the declared array — so its
                    // standalone type (an `error[]` for empty) is not the
                    // assignability operand.
                    if matches!(self.tree.expr(*initializer).clone(), ExprData::ArrayInit(_)) {
                        return;
                    }
                    // §5.2: an int-typed constant expression whose value fits
                    // the target narrows in assignment context ([§5.1.3]).
                    if !crate::java::subtyping::is_assignable(
                        self.db,
                        &self.scope,
                        &init_ty,
                        &target,
                    ) && !self.constant_narrowable(*initializer, init_ty, target)
                    {
                        self.report(TypeError::IncompatibleTypes {
                            expr: *initializer,
                            found: init_ty,
                            expected: target,
                        });
                    }
                    // §5.1.9: a raw source assigned to a parameterized target
                    // is an unchecked conversion — report the warning.
                    self.warn_unchecked(*initializer, &init_ty, &target);
                }
            }
            StmtData::Expr(expr) => {
                let _ = self.infer_expr(*expr);
            }
            // §14.7: a labeled statement puts its label in scope for the
            // nested `break label`/`continue label`; whether it is a loop
            // decides which of the two a labeled `continue` may target.
            StmtData::Labeled { label, stmt } => {
                let name = self.tree.label(*label).0.clone();
                let is_loop = matches!(
                    self.tree.stmt(*stmt),
                    StmtData::While { .. }
                        | StmtData::DoWhile { .. }
                        | StmtData::For { .. }
                        | StmtData::ForEach { .. }
                );
                self.labels.push((name, is_loop));
                self.infer_stmt(*stmt);
                self.labels.pop();
            }
            StmtData::If { cond, then, els } => {
                // §16.2.7: the flow before the condition is the pre-statement
                // flow — the fallback when every branch exits and the code
                // after the `if` is unreachable.
                let before = self.flow.clone();
                let before_exited = self.exited;
                self.check_condition(*cond);
                // §16.1.2–[§16.1.5]: the condition's true flow enters the then
                // arm, its false flow the else arm — an assignment made only on
                // the condition's true flow (e.g. the right operand of a `&&`
                // that runs only when the left matched, JLS Example 16-1) is
                // definitely assigned inside the guarded then arm.
                let (cond_true_flow, cond_false_flow) = self.take_bool_outcomes();
                // §16.1.1: a condition that is a boolean *constant expression*
                // of value `true` (or `false`) can never take the other branch
                // — the impossible branch's flow is vacuous, so only the taken
                // arm constrains the code after the `if`. `final int c = 5;
                // if (c > 2)` folds (JLS Example 16-2's contrast).
                let const_bool = self.const_bool(*cond);
                // §14.30.3: the condition's true-flow pattern bindings are in
                // scope in the `then` arm; its false-flow bindings in the
                // `else` arm.
                let (true_flow, false_flow) = self.pattern_flow(*cond).unwrap_or_default();
                self.scopes.push(FxHashMap::default());
                self.flow = cond_true_flow;
                for binding in &true_flow {
                    self.scope_binding(*binding);
                }
                // §16: after the `if`, a local is definitely assigned only if
                // it is assigned on *both* paths — the then branch and (when
                // present) the else branch; a branch that exits contributes
                // no constraint. A blank `final` field is *touched* if either
                // surviving path assigned it ([§8.3.1.2]): after the `if`, a
                // later write to it is the already-assigned error.
                self.infer_stmt(*then);
                self.scopes.pop();
                let then_flow = std::mem::replace(&mut self.flow, before.clone());
                let mut then_exited = std::mem::replace(&mut self.exited, before_exited);
                // §16.1.1: a constant-`false` condition's then arm can never
                // run — its flow is vacuous, so the arm contributes nothing to
                // the join and the after-`if` state is the else arm's alone.
                if const_bool == Some(false) {
                    then_exited = true;
                }
                // The else path's end state; the else-less form's false path
                // is the condition's false flow. A constant condition makes
                // one branch impossible: its flow is vacuous and it can never
                // reach the join ([§16.1.1]).
                let mut else_flow = cond_false_flow.clone();
                let mut else_exited = before_exited;
                if let Some(els) = els {
                    self.scopes.push(FxHashMap::default());
                    self.flow = cond_false_flow;
                    for binding in &false_flow {
                        self.scope_binding(*binding);
                    }
                    self.infer_stmt(*els);
                    self.scopes.pop();
                    else_flow = std::mem::replace(&mut self.flow, before.clone());
                    else_exited = std::mem::replace(&mut self.exited, before_exited);
                } else if then_exited && !before_exited {
                    // §14.30.3/§16: when the then arm completes abruptly, the
                    // only way past this statement is the condition's false
                    // flow — its pattern bindings stay in scope after it
                    // (`if (!(x instanceof T v)) return;` makes `v` known).
                    self.flow = cond_false_flow;
                    for binding in &false_flow {
                        self.scope_binding(*binding);
                    }
                    else_flow = self.flow.clone();
                }
                // §16.1.1: a constant-`true` condition's else arm (or the
                // else-less false path) can never run — it is vacuous.
                if const_bool == Some(true) {
                    else_exited = true;
                }
                // §16.1/§8.3.1.2: join the surviving paths. A path that
                // completes abruptly never reaches the join; when no path
                // does, the following code is unreachable and `before` stands.
                let then_survives = !then_exited;
                let else_survives = !else_exited;
                match (then_survives, else_survives) {
                    (true, true) => {
                        // Both paths fall through: definite intersects,
                        // touched unions.
                        let mut then_flow = then_flow;
                        then_flow
                            .definite
                            .retain(|local| else_flow.definite.contains(local));
                        then_flow.union_touched(&else_flow);
                        self.flow = then_flow;
                    }
                    (true, false) => {
                        // Only the then path falls through.
                        self.flow = then_flow;
                    }
                    (false, true) => {
                        // Only the else path falls through — it already holds
                        // `before` plus its own writes.
                        self.flow = else_flow;
                    }
                    (false, false) => {
                        // Both exit: the following code is unreachable.
                        self.flow = before.clone();
                    }
                }
                self.exited = then_exited && else_exited;
            }
            StmtData::While { cond, body } => {
                // §16.2.10: the flow before the condition is the pre-loop
                // state — the fallback for the after-loop join.
                let before = self.flow.clone();
                self.check_condition(*cond);
                // §16.1.10: the body may run zero times, so nothing it
                // assigns is definitely assigned after the loop, and a blank
                // `final` field it touches stays *definitely unassigned* after
                // it ([§8.3.1.2]) — a later write is still a fresh
                // initialization, not an already-assigned one.
                // §16.2.10: a `while` whose condition is a constant expression
                // ([§16.1.1]) of value `true` can never complete through the
                // condition — the only way past it is a `break` — so the
                // assignments the body made before each `break` carry past the
                // loop. The body is inferred under the condition's true flow;
                // the loop's end flow is the join of the recorded break flows
                // (JLS Example 16-1).
                let const_bool = self.const_bool(*cond);
                let (cond_true_flow, _) = self.take_bool_outcomes();
                self.loop_depth += 1;
                self.loop_breaks.push(LoopBreakFrame::new(None));
                if const_bool == Some(true) {
                    self.flow = cond_true_flow;
                }
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                self.flow = if const_bool == Some(true) {
                    // Only the break paths survive: a constant-true loop never
                    // reaches the join through its condition. When no break was
                    // recorded the loop cannot complete normally, so the
                    // pre-loop state stands (the code after is unreachable).
                    frame.joined().unwrap_or(before)
                } else {
                    before
                };
                self.exited = false;
            }
            StmtData::DoWhile { body, cond } => {
                // §16.1.11: a do-loop's body runs at least once, so its
                // assignments carry past the loop when the body falls
                // through; an exiting body constrains nothing. A do-loop whose
                // condition is a constant `true` ([§16.1.1]) never exits
                // through the condition — only its `break` paths reach the
                // join ([§16.2.11]), like a constant-true `while`.
                let before = self.flow.clone();
                let const_bool = self.const_bool(*cond);
                self.loop_depth += 1;
                self.loop_breaks.push(LoopBreakFrame::new(None));
                self.infer_stmt(*body);
                if const_bool == Some(true) {
                    // The body's fall-through feeds the condition (always
                    // true); only the recorded breaks escape the loop.
                } else if self.exited {
                    self.flow = before.clone();
                }
                self.check_condition(*cond);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                if const_bool == Some(true) {
                    self.flow = frame.joined().unwrap_or(before);
                }
                self.exited = false;
            }
            StmtData::For {
                init,
                cond,
                step,
                body,
            } => {
                self.scopes.push(FxHashMap::default());
                for &init in init {
                    self.infer_stmt(init);
                }
                // A missing condition is an implicit constant `true`
                // ([§16.2.12]: a condition-less `for (;;)` never completes
                // through a false value).
                let const_bool = match cond {
                    Some(c) => self.const_bool(*c),
                    None => Some(true),
                };
                if let Some(cond) = cond {
                    self.check_condition(*cond);
                }
                // §16.2.10: the body runs under the condition's *true* flow
                // when the condition is a constant `true`; capture the
                // outcomes right here — the step expressions below would
                // overwrite [`Self::bool_outcomes`].
                let (cond_true_flow, _) = self.take_bool_outcomes();
                for &step in step {
                    let _ = self.infer_expr(step);
                }
                // §16.1.14: like `while`, the body may run zero times — except
                // a `for` whose condition is a constant `true` **or absent**,
                // which — like a constant-true `while` ([§16.2.12]) — escapes
                // only through its `break` paths.
                let before = self.flow.clone();
                self.loop_depth += 1;
                self.loop_breaks.push(LoopBreakFrame::new(None));
                if const_bool == Some(true) {
                    self.flow = cond_true_flow;
                }
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                self.flow = if const_bool == Some(true) {
                    frame.joined().unwrap_or(before)
                } else {
                    before
                };
                self.exited = false;
                self.scopes.pop();
            }
            StmtData::ForEach {
                var,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr(*iterable);
                let element = if iterable_ty.is_array(self.db) {
                    iterable_ty
                        .element(self.db)
                        .copied()
                        .unwrap_or_else(|| self.error())
                } else {
                    // §14.14.2.1: the expression must be `Iterable<T>`; the
                    // loop variable takes `T`, the element type of the
                    // `Iterator<E>` that `iterator()` returns.
                    match self.iterable_element(iterable_ty) {
                        Some(element) => element,
                        None => {
                            if !iterable_ty.is_error(self.db) {
                                // §14.14.2: a for-each over a non-iterable
                                // reference type is a compile-time error.
                                self.report(TypeError::NonIterableForEach {
                                    expr: *iterable,
                                    found: iterable_ty,
                                });
                            }
                            self.error()
                        }
                    }
                };
                self.scopes.push(FxHashMap::default());
                self.declare_local_ty(*var, element);
                // §16.1.11: like `while`, the body may run zero times.
                let before = self.flow.clone();
                self.loop_depth += 1;
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                self.flow = before;
                self.exited = false;
                self.scopes.pop();
            }
            StmtData::Switch { scrutinee, arms } => {
                let selector = self.infer_switch_selector(*scrutinee);
                self.case_values.push(FxHashMap::default());
                self.scopes.push(FxHashMap::default());
                self.switch_depth += 1;
                // §14.22: every arm is an alternative flow path starting from
                // the pre-switch state; the switch completes normally iff at
                // least one arm completes normally, and a local is definitely
                // assigned after the switch only when it is assigned on every
                // normal-completing arm ([§16.1.9]). A blank `final` field is
                // touched if any surviving arm touched it ([§8.3.1.2]).
                let before = self.flow.clone();
                let before_exited = self.exited;
                let mut paths: Vec<(Flow, bool)> = Vec::new();
                for arm in arms {
                    self.flow = before.clone();
                    self.exited = before_exited;
                    // §14.30.2/§14.30.3: a pattern label's variables are in
                    // scope in the arm's statements.
                    self.scopes.push(FxHashMap::default());
                    for label in &arm.labels {
                        match label {
                            SwitchLabel::Expr(e) => {
                                let _ = self.infer_switch_label(*e, &selector);
                            }
                            SwitchLabel::Pattern(p) => {
                                let _ = self.pattern_type(*p);
                                for binding in self.pattern_bindings_of(*p) {
                                    self.scope_binding(binding);
                                }
                            }
                            // §14.11.1: a `when` guard must be a boolean;
                            // its pattern bindings are already in scope.
                            SwitchLabel::Guard(cond) => {
                                self.check_condition(*cond);
                            }
                        }
                    }
                    for &stmt in &arm.body {
                        self.infer_stmt(stmt);
                    }
                    let end_state = std::mem::replace(&mut self.flow, before.clone());
                    let exits = std::mem::replace(&mut self.exited, before_exited);
                    paths.push((end_state, exits));
                    self.scopes.pop();
                }
                // The join of §16.1.9: only normal-completing arms reach the
                // statement after the switch; when no arm does, the switch
                // completes abruptly and the following code is unreachable.
                let mut live_joined: Option<Flow> = None;
                for (path, exited) in &paths {
                    if *exited {
                        continue;
                    }
                    match &mut live_joined {
                        None => live_joined = Some(path.clone()),
                        Some(acc) => acc.join_definite(path),
                    }
                }
                self.flow = live_joined.unwrap_or_else(|| before.clone());
                self.exited = paths.iter().all(|(_, exited)| *exited);
                self.scopes.pop();
                self.case_values.pop();
                self.switch_depth -= 1;
            }
            StmtData::Return(Some(expr)) | StmtData::Yield(expr) => {
                // A returned expression is a poly expression whose target is
                // the method's return type ([JLS §14.17]); a `yield` value has
                // the enclosing switch expression's type as target
                // ([JLS §14.21], see [`InferCtx::switch_targets`]).
                let target = if matches!(stmt, StmtData::Yield(_)) {
                    self.switch_targets.last().copied().flatten()
                } else {
                    self.enclosing_ret
                };
                let ty = self.with_target(target, |this| this.infer_expr(*expr));
                // §15.27.3: inside a speculative block-lambda body the valued
                // returns are its result expressions — record them so
                // [`Self::infer_lambda_body_result`] can constrain the SAM
                // return type.
                if matches!(stmt, StmtData::Return(_))
                    && let Some(frame) = self.lambda_returns.last_mut()
                {
                    frame.push(ty);
                }
                if matches!(stmt, StmtData::Return(_)) {
                    match self.enclosing_ret {
                        // §14.17: a value returned from a `void` method or a
                        // constructor is an error.
                        None => {
                            if !ty.is_error(self.db) {
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty,
                                    expected: Ty::void(self.db),
                                });
                            }
                        }
                        Some(ret) => {
                            if !ty.is_error(self.db)
                                && !ret.is_error(self.db)
                                && !crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, &ret)
                                // §5.2: an in-range int constant narrows to a
                                // primitive return type ([§5.1.3]).
                                && !self.constant_narrowable(*expr, ty, ret)
                            {
                                // §14.17: the returned value must be assignable
                                // to the return type ([§5.2]).
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty,
                                    expected: ret,
                                });
                            }
                        }
                    }
                }
                // §16: a `return` exits this path; a `yield` stays inside its
                // switch expression.
                if matches!(stmt, StmtData::Return(_)) {
                    self.exited = true;
                }
            }
            StmtData::Throw(expr) => {
                // §14.18: the operand of a `throw` statement is not a poly
                // expression ([JLS §15.2]) — it is inferred standalone — and
                // must be assignable to `Throwable` ([§5.2]); a non-throwable
                // operand marks the expression as an error.
                let ty = self.infer_expr(*expr);
                let throwable = Ty::reference(self.db, "java.lang.Throwable", Vec::new());
                if !ty.is_error(self.db)
                    && !crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, &throwable)
                {
                    self.types.insert(*expr, self.error());
                    // §14.18: a non-throwable operand is a compile-time error.
                    self.report(TypeError::IncompatibleTypes {
                        expr: *expr,
                        found: ty,
                        expected: throwable,
                    });
                } else if let ExprData::Var(name) = self.tree.expr(*expr).clone()
                    && let Some(local) = self.lookup_local(&name)
                    && let Some(precise) = self.rethrow_sets.get(&local).cloned()
                {
                    // §11.2.2: a `throw` of an (effectively final) catch
                    // parameter throws precisely the checked exceptions the
                    // try block can throw and the clause can catch — not the
                    // parameter's declared type. An empty set means only
                    // unchecked exceptions can reach the parameter, so the
                    // throw adds no liability.
                    for ty in precise {
                        self.thrown.push((ty, *expr));
                    }
                } else if self.is_checked(&ty) {
                    // §11.2.2/§14.18: a `throw` of a checked exception adds it
                    // to the enclosing liability.
                    self.thrown.push((ty, *expr));
                }
                // §16: control does not continue past a `throw` on this path.
                self.exited = true;
            }
            StmtData::Return(None) => {
                // §16: control does not continue past an exit on this path.
                self.exited = true;
            }
            // §14.15: `break` exits the nearest enclosing `switch` or loop
            // ([§14.11.1]), or the statement named by its label.
            StmtData::Break(label) => {
                self.exited = true;
                match label {
                    Some(label) => {
                        let name = self.tree.label(*label).0.clone();
                        // §14.15: a labeled `break` requires the label of an
                        // enclosing statement in scope.
                        if !self.labels.iter().any(|(n, _)| *n == name) {
                            self.report(TypeError::UndefinedLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            });
                        }
                        // §16.2.10: the break's flow joins the after-loop state
                        // of a constant-condition loop (JLS Example 16-1).
                        self.record_break_flow(Some(&name));
                    }
                    None => {
                        // §14.15: an unlabeled `break` needs an enclosing
                        // `switch` or loop to exit.
                        if self.loop_depth == 0 && self.switch_depth == 0 {
                            self.report(TypeError::BreakOutsideSwitchOrLoop { stmt: id });
                        }
                        self.record_break_flow(None);
                    }
                }
            }
            // §14.16: `continue` skips to the next iteration of the nearest
            // enclosing loop (or the loop named by its label).
            StmtData::Continue(label) => {
                self.exited = true;
                match label {
                    Some(label) => {
                        let name = self.tree.label(*label).0.clone();
                        // §14.16: a labeled `continue` requires a labeled
                        // *loop* in scope.
                        match self.labels.iter().rev().find(|(n, _)| *n == name) {
                            Some((_, is_loop)) if *is_loop => {}
                            Some(_) => self.report(TypeError::NotALoopLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            }),
                            None => self.report(TypeError::UndefinedLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            }),
                        }
                    }
                    None => {
                        // §14.16: an unlabeled `continue` needs an enclosing
                        // loop.
                        if self.loop_depth == 0 {
                            self.report(TypeError::ContinueOutsideLoop { stmt: id });
                        }
                    }
                }
            }
            StmtData::Synchronized { expr, body } => {
                let _ = self.infer_expr(*expr);
                self.infer_stmt(*body);
            }
            StmtData::Try {
                resources,
                body,
                catches,
                finally,
            } => {
                self.scopes.push(FxHashMap::default());
                // §11.2.3: the liability index is snapshotted before the
                // *resource initializers* as well as the body — exceptions
                // thrown while acquiring a resource gate the catch clauses
                // exactly like exceptions from the block itself.
                let thrown_before: FxHashSet<ExprId> =
                    self.thrown.iter().map(|(_, expr)| *expr).collect();
                for resource in resources {
                    // §14.20.3: each declaration resource is a local variable
                    // declaration; a `var` resource infers its type from its
                    // initializer ([§14.4.1]).
                    if self.tree.local(resource.local).ty.is_none() {
                        let Some(initializer) = resource.initializer else {
                            self.declare_local(resource.local);
                            continue;
                        };
                        let ty = self.infer_expr(initializer);
                        let local = self.tree.local(resource.local).clone();
                        self.bind_local(resource.local, local.name, ty);
                    } else {
                        self.declare_local(resource.local);
                        if let Some(initializer) = resource.initializer {
                            // The initializer is a poly expression whose
                            // target is the resource's declared type.
                            let target = self.locals.get(&resource.local).copied();
                            let _ = self.with_target(target, |this| this.infer_expr(initializer));
                        }
                    }
                    // §14.20.3: a resource's type must be a subtype of
                    // `java.lang.AutoCloseable` — an exception is thrown if it
                    // is not ([§14.20.3]). Reported at the resource
                    // initializer (javac's caret), IntelliJ-style. A resource
                    // type that does not resolve on the classpath is already
                    // reported (`CannotResolveType`), so it is skipped.
                    let auto_closeable =
                        Ty::reference(self.db, "java.lang.AutoCloseable", Vec::new());
                    if let Some(resource_ty) = self.locals.get(&resource.local).copied()
                        && let Some(initializer) = resource.initializer
                        && !resource_ty.is_error(self.db)
                        && (match resource_ty.kind(self.db) {
                            TyKind::Reference { name, .. } => {
                                hir::fqn_resolve(self.db, &self.scope, name.as_str()).is_some()
                            }
                            _ => true,
                        })
                        && !crate::java::subtyping::is_assignable(
                            self.db,
                            &self.scope,
                            &resource_ty,
                            &auto_closeable,
                        )
                    {
                        self.report(TypeError::IncompatibleTypes {
                            expr: initializer,
                            found: resource_ty,
                            expected: auto_closeable,
                        });
                    }
                }
                // §16.1.8: the try block may exit via any catch, so a local
                // is definitely assigned after the statement only if it is
                // assigned at the end of *every* path — the intersection of
                // the try block and each catch clause. A `finally` always
                // runs, so its assignments override.
                let before = self.flow.clone();
                let before_exited = self.exited;
                self.infer_stmt(*body);
                // The end-of-body state is one path; each catch clause adds
                // another, starting from the pre-try state.
                // Each path is its end state plus whether the path reaches
                // the join at all (a clause that completed abruptly does not).
                let mut paths: Vec<(Flow, bool)> = vec![(self.flow.clone(), self.exited)];
                let mut all_exits = self.exited;
                // exceptions assignable to its declared type; a clause whose
                // type is a subtype of an *earlier* clause's type is
                // unreachable ([§14.20]).
                // §11.2.3: the checked exceptions thrown by the try block —
                // the liability it *added* while being inferred. Entries from
                // before the try may have been drained by a nested catch in
                // the meantime (a discharge removes them wherever they sit),
                // so diff by the throwing expression's id rather than by
                // index.
                let try_thrown: Vec<Ty> = self
                    .thrown
                    .iter()
                    .filter(|(_, expr)| !thrown_before.contains(expr))
                    .map(|(ty, _)| ty.clone())
                    .collect();
                let mut catch_tys: Vec<Ty> = Vec::new();
                for clause in catches {
                    // Each catch clause is an alternative path that starts
                    // from the pre-try state ([§16.1.8]).
                    self.flow = before.clone();
                    self.exited = before_exited;
                    self.scopes.push(FxHashMap::default());
                    // The entries still pending when this clause begins — the
                    // basis of its precise rethrow set ([§11.2.2]); captured
                    // before the clause discharges any of them.
                    let outstanding: Vec<(Ty, ExprId)> = self.thrown.clone();
                    // §14.20: a multi-catch parameter declares several
                    // alternative types; each gates the clause and discharges
                    // independently.
                    let clause_tys: Vec<Ty> = {
                        let declared = &self.tree.local(clause.param).ty;
                        let types: &[SpannedTypeRef] = if clause.param_types.is_empty() {
                            declared.as_slice()
                        } else {
                            &clause.param_types
                        };
                        types
                            .iter()
                            .map(|ty| resolve_type_ref(self.db, &self.scope, &self.resolver, ty))
                            .filter(|ty| !ty.is_error(self.db))
                            .collect()
                    };
                    if clause_tys.is_empty() {
                        // An unresolvable parameter type: the clause still
                        // infers its body for its own diagnostics.
                        self.declare_local(clause.param);
                        self.infer_stmt(clause.body);
                        let end_state = std::mem::replace(&mut self.flow, before.clone());
                        let exits = std::mem::replace(&mut self.exited, before_exited);
                        paths.push((end_state, exits));
                        all_exits &= exits;
                        self.scopes.pop();
                        continue;
                    }
                    // §14.20: a catch parameter type must be a class, never a
                    // type variable ([§4.4]) — a type parameter is not a class,
                    // so `catch (T t)` is rejected.
                    if clause_tys
                        .iter()
                        .any(|ty| matches!(ty.kind(self.db), TyKind::TypeVar { .. }))
                    {
                        self.report(TypeError::CannotCatchTypeVariable {
                            local: clause.param,
                        });
                    }
                    if clause_tys.iter().all(|clause_ty| {
                        catch_tys.iter().any(|earlier| {
                            crate::java::subtyping::is_subtype(
                                self.db,
                                &self.scope,
                                &clause_ty.clone(),
                                earlier,
                            )
                        })
                    }) {
                        // §11.2.3: every alternative is already caught by an
                        // earlier clause.
                        self.report(TypeError::AlreadyCaught {
                            local: clause.param,
                            caught: clause_tys.clone(),
                        });
                    } else {
                        // §11.2.3: an alternative may name a *checked*
                        // exception only when it is related by subtyping to
                        // something the try block can throw — either the
                        // block can throw a class assignable to it, or it is
                        // itself assignable to a thrown class (a defensive
                        // catch of a thrown exception's subclass). Unchecked
                        // types are always fair game, and so are `Exception`
                        // and its superclasses: a catch-all must stay legal
                        // even when the block provably throws nothing.
                        // A multi-catch with some dead and some live
                        // alternatives keeps the live ones — javac reports
                        // the dead alternatives individually.
                        let exception = Ty::reference(self.db, "java.lang.Exception", Vec::new());
                        for clause_ty in &clause_tys {
                            let covers_unchecked = crate::java::subtyping::is_assignable(
                                self.db,
                                &self.scope,
                                &exception,
                                clause_ty,
                            );
                            if self.is_checked(clause_ty)
                                && !covers_unchecked
                                && !try_thrown.iter().any(|thrown| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        thrown,
                                        clause_ty,
                                    )
                                })
                                && !try_thrown.iter().any(|thrown| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        clause_ty,
                                        thrown,
                                    )
                                })
                            {
                                self.report(TypeError::CatchNeverThrown {
                                    local: clause.param,
                                    caught: *clause_ty,
                                });
                            }
                        }
                        self.thrown.retain(|(thrown, _)| {
                            !clause_tys.iter().any(|clause_ty| {
                                crate::java::subtyping::is_assignable(
                                    self.db,
                                    &self.scope,
                                    thrown,
                                    clause_ty,
                                )
                            })
                        });
                        catch_tys.extend(clause_tys.iter().cloned());
                    }
                    // §11.2.2: the clause's precise rethrow set — the
                    // checked exceptions the try block can throw that are
                    // assignable to this clause and were *not* discharged by
                    // an earlier one (they are no longer pending). A `throw`
                    // of the parameter inside the body throws these, provided
                    // the parameter stays effectively final.
                    let precise: Vec<Ty> = outstanding
                        .iter()
                        .filter(|(thrown, expr)| {
                            !thrown_before.contains(expr)
                                && clause_tys.iter().any(|clause_ty| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        thrown,
                                        clause_ty,
                                    )
                                })
                        })
                        .map(|(ty, _)| ty.clone())
                        .collect();
                    if !precise.is_empty() {
                        self.rethrow_sets.insert(clause.param, precise);
                    }
                    self.declare_local(clause.param);
                    self.infer_stmt(clause.body);
                    // The clause's end state joins the other paths.
                    let end_state = std::mem::replace(&mut self.flow, before.clone());
                    let exits = std::mem::replace(&mut self.exited, before_exited);
                    paths.push((end_state, exits));
                    all_exits &= exits;
                    self.scopes.pop();
                }
                // The intersection of every *live* path: a local is
                // definitely assigned after the try only if each path that
                // reaches the join assigned it ([§16.1.8]). A path whose
                // clause completed abruptly (threw or returned) never reaches
                // the join and constrains nothing; when no path reaches it,
                // the following code is unreachable and the pre-try state
                // stands.
                let mut live_joined: Option<Flow> = None;
                for (path, exited) in &paths {
                    if *exited {
                        continue;
                    }
                    match &mut live_joined {
                        None => live_joined = Some(path.clone()),
                        Some(acc) => acc.join_definite(path),
                    }
                }
                self.flow = live_joined.unwrap_or_else(|| before.clone());
                self.exited = all_exits;
                self.scopes.pop();
                if let Some(finally) = finally {
                    // §16.1.8/§14.20.2: the `finally` block *always* runs, even
                    // when the try block and every catch clause complete
                    // abruptly — so it is entered *reachable* regardless of
                    // `all_exits` (reporting its first statement as
                    // unreachable would be a false positive). Its end state is
                    // the try statement's end state: its assignments override
                    // the joined set, and the statement completes abruptly iff
                    // the finally completes abruptly, otherwise exactly as the
                    // joined paths did.
                    let before_finally_exited = self.exited;
                    self.exited = false;
                    self.infer_stmt(*finally);
                    let finally_exited = self.exited;
                    self.exited = finally_exited || before_finally_exited;
                }
            }
            StmtData::Assert { cond, msg } => {
                // §14.16: the assertion condition must be a boolean.
                self.check_condition(*cond);
                if let Some(msg) = msg {
                    let _ = self.infer_expr(*msg);
                }
            }
            // §14.3: a local class declaration declares a type, not a value —
            // it has no effect on expression typing.
            StmtData::LocalClass { .. } => {}
            StmtData::Missing => {}
        }
    }
}

/// The names a field initializer may not read by simple name
/// ([JLS §8.3.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3.3)):
/// same-class fields declared textually *after* `field` with the same
/// static/instance kind. A cross-kind read (an instance initializer reading a
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

/// Whether the innermost enclosing declaration of the item is a static
/// context ([JLS §8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)):
/// a construct occurs in a static context when the innermost method, field,
/// constructor, instance initializer or static initializer that encloses it is
/// a static method, a static field or a static initializer. Constructors and
/// instance initializers are never static contexts. An enum constant is an
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

/// The self-type of the innermost enclosing class-like declaration
/// ([JLS §8.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.2)):
/// within the body of a generic class `C<T1..Tn>` the simple name `C` denotes
/// the parameterized type `C<T1..Tn>`, so member lookup against the enclosing
/// class must be instantiated with the declared type variables — not the raw
/// type, whose instance members are erased ([§4.8]). `None` when `item` lies
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

/// §8.3.1.2/[§16]: the blank `final` fields of the enclosing class that
/// *earlier* initializer bodies — which run before `item`'s body — may have
/// assigned on some path ([§8.3.2], [§8.6], [§8.7], [§8.8]):
///
/// - for a **static** field initializer or static initializer: the earlier
///   static field initializers and static initializers of the same class;
/// - for an **instance** field initializer or instance initializer: the
///   earlier instance field initializers and instance initializers;
/// - for a **constructor**: *every* instance field initializer and instance
///   initializer (the implicit `super()` precedes the whole body, so they
///   have all run; [§8.8.7.1]).
///
/// The seed is the union of those bodies' own already-touched sets — the
/// already-assigned analysis is path-sensitive *within* each body, so a
/// write only reachable through a path that cannot have run is not seeded. A
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

/// The source `ItemId` of the source method `method` (whose [`MethodData`]
/// was resolved from `method.owner_file`), found by name and arity — the
/// overloads of one class are distinguished by their parameter count for the
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

/// The source symbol of a unary operator, for [`TypeError::IncompatibleOperand`].
fn unary_op_symbol(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Plus => "+",
        UnaryOp::Minus => "-",
        UnaryOp::BitNot => "~",
        UnaryOp::Inc => "++",
        UnaryOp::Dec => "--",
        UnaryOp::Not => "!",
    }
}

/// The source symbol of a binary operator, for [`TypeError::IncompatibleOperand`].
fn binary_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
        BinaryOp::UShr => ">>>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitXor => "^",
        BinaryOp::BitOr => "|",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

/// Whether `expr` is a poly expression ([JLS §15.2]): a lambda or method
/// reference, or a parenthesized or conditional expression whose arms are
/// poly. Such an expression has no standalone type; its type is the target
/// functional interface ([JLS §15.27.3]).
fn expr_is_poly(tree: &BodyTree, id: ExprId) -> bool {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. } | ExprData::MethodRef { .. } => true,
        ExprData::Paren(inner) => expr_is_poly(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            expr_is_poly(tree, then) && expr_is_poly(tree, els)
        }
        _ => false,
    }
}

/// Whether `expr` is a poly expression, additionally treating a method
/// invocation as poly ([JLS §18.5.2.4]): a nested generic invocation is a poly
/// expression whose type is inferred against the target (its enclosing
/// invocation's resolved formal). Used for the §15.25.2 conditional rule where
/// a conditional with poly invocation arms must be treated as poly, without
/// deferring invocation arguments during overload resolution.
fn expr_is_poly_ext(tree: &BodyTree, id: ExprId) -> bool {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. } | ExprData::MethodRef { .. } | ExprData::MethodCall { .. } => true,
        // §15.9.3: a diamond class instance creation is a poly expression in
        // an invocation or assignment context.
        ExprData::New { diamond: true, .. } => true,
        ExprData::Paren(inner) => expr_is_poly_ext(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            expr_is_poly_ext(tree, then) && expr_is_poly_ext(tree, els)
        }
        _ => false,
    }
}

/// Whether `expr` is (possibly parenthesized) a method invocation, used to
/// recognize conditional expressions whose arms are poly invocations.
fn expr_is_call(tree: &BodyTree, id: ExprId) -> bool {
    match tree.expr(id).clone() {
        ExprData::MethodCall { .. } => true,
        ExprData::Paren(inner) => expr_is_call(tree, inner),
        _ => false,
    }
}

/// The form of a method reference ([JLS §15.13.1]): how its target's single
/// abstract method's parameters map onto the referenced method's. A *static*
/// reference (`Type::m` naming a static member) and a *bound* reference
/// (`expr::m` — the qualifier value is the receiver) take the SAM's parameters
/// as the method's own; an *unbound* instance reference (`Type::m` naming an
/// instance member) takes the SAM's first parameter as the receiver
/// ([§15.13.3]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodRefKind {
    Static,
    Unbound,
    Bound,
}

/// The kinds of the actual arguments of a method invocation for the joint
/// inference of §18.5.2.4: a concrete argument has a standalone type; a poly
/// argument is a lambda or method reference deferred to its target formal
/// ([JLS §18.5.2.2], [§15.27.3], [§15.13.2]), a nested method invocation
/// whose inference shares the enclosing invocation's table
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)),
/// or a diamond class instance creation, which is a poly expression in an
/// invocation context ([JLS §15.9.3]).
#[derive(Clone)]
enum ArgKind {
    /// An argument with a concrete standalone type.
    Concrete(Ty),
    /// A lambda or method reference; the arity check of §15.12.2.2/§15.12.2.3
    /// is run against the target formal's single abstract method. A method
    /// reference is not arity-checkable without resolving the referenced
    /// method, so its arity is `None`. The lambda's body additionally
    /// constrains the SAM return type's instantiation ([JLS §18.5.2.2]).
    Lambda { id: ExprId, arity: Option<usize> },
    /// A nested method invocation, resolved against the target formal by
    /// contributing its constraints to the enclosing invocation's table.
    Invocation { id: ExprId },
    /// A diamond `new Foo<>()` in an invocation context
    /// ([JLS §15.9.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.3)):
    /// its type arguments are inferred from the enclosing invocation's formal
    /// — `synchronizedList(new ArrayList<>())` against a `List<String>`
    /// target. The created class's type variables are registered in the
    /// shared table and constrained by the formal.
    DiamondNew { id: ExprId },
}

/// One actual argument of an invocation: its poly leaves — each contributing a
/// constraint to the candidate's inference — and whether the argument itself is
/// a poly expression whose type is the target formal and so must be re-inferred
/// against it after resolution ([JLS §18.5.2.4]).
struct ArgInfo {
    /// The argument expression, re-inferred against the resolved formal.
    id: ExprId,
    /// Whether the argument is a poly expression: its type is the target
    /// formal, so it is deferred to the post-resolution re-inference.
    poly: bool,
    /// The poly leaves of the argument ([JLS §15.2]), each contributed against
    /// the formal during candidate probing. A concrete argument has a single
    /// `Concrete` leaf.
    leaves: Vec<ArgKind>,
}

/// An applicable candidate in [`InferCtx::choose_candidate`]: the declared
/// method, its inferred invocation type, and the deferred poly arguments to
/// re-infer against the resolved formal parameters ([JLS §18.5.2.4]).
type ApplicableCandidate = (MethodData, MethodData, Vec<(ExprId, usize)>);

/// Infers the *poly* arguments standalone — the recovery when an invocation
/// has no applicable method ([§15.12.2]): a lambda or method reference keeps
/// its error type and a nested invocation resolves in isolation, so every
/// argument expression still carries a recorded type. The inference is truly
/// standalone ([§15.12.2.6]): the enclosing context's target does not reach
/// an *argument* position — only its invocation formal constrains it — so it
/// is cleared here. The concrete arguments were already inferred while
/// collecting [`ArgInfo`] and are left untouched.
fn reinfer_poly_standalone(ctx: &mut InferCtx<'_>, arg_kinds: &[ArgInfo]) {
    for info in arg_kinds.iter().filter(|info| info.poly) {
        let _ = ctx.with_target(None, |this| this.infer_expr(info.id));
    }
}

/// The poly leaves of an argument ([JLS §15.2]): a lambda, method reference or
/// method invocation, or the leaves of a parenthesized or conditional
/// expression whose arms are poly ([JLS §18.5.2.4]). An argument that is not a
/// poly expression has no leaves — it is inferred standalone.
fn poly_leaves(tree: &BodyTree, id: ExprId) -> Vec<ExprId> {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. }
        | ExprData::MethodRef { .. }
        | ExprData::MethodCall { .. }
        // §15.9.3: a diamond class instance creation is a poly expression in
        // an invocation context.
        | ExprData::New { diamond: true, .. } => vec![id],
        ExprData::Paren(inner) => poly_leaves(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            // §15.25.2: a conditional is a poly expression only when both arms
            // are poly.
            if expr_is_poly_ext(tree, then) && expr_is_poly_ext(tree, els) {
                let mut leaves = poly_leaves(tree, then);
                leaves.extend(poly_leaves(tree, els));
                leaves
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// The parameter count of a lambda argument ([§15.12.2.2]), used to check the
/// applicability of an overload candidate against the lambda's arity. A method
/// reference is not arity-checkable without resolving it, so it is `None`.
fn poly_arity(tree: &BodyTree, id: ExprId) -> Option<usize> {
    match tree.expr(id).clone() {
        ExprData::Lambda { params, .. } => Some(params.len()),
        ExprData::Paren(inner) => poly_arity(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            poly_arity(tree, then).filter(|n| poly_arity(tree, els) == Some(*n))
        }
        _ => None,
    }
}

impl InferCtx<'_> {
    /// Whether a block lambda contains a `return` statement carrying a value —
    /// the syntactic core of value compatibility
    /// ([JLS §15.27.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.3),
    /// [§14.17](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.17)):
    /// every path must return a value or throw. A block without any valued
    /// `return` is only void-compatible, so it cannot target a functional
    /// interface whose function type produces a result.
    fn lambda_block_has_value(&self, body: &LambdaBody) -> bool {
        let LambdaBody::Block(stmt) = *body else {
            // An expression lambda's value compatibility is decided against
            // its inferred result, not syntactically.
            return true;
        };
        self.stmt_has_valued_return(stmt)
    }

    fn stmt_has_valued_return(&self, stmt: StmtId) -> bool {
        match self.tree.stmt(stmt).clone() {
            StmtData::Return(Some(_)) | StmtData::Yield(_) => true,
            StmtData::Return(None)
            | StmtData::Empty
            | StmtData::Decl { .. }
            | StmtData::Expr(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::Throw(_)
            | StmtData::Assert { .. }
            | StmtData::LocalClass { .. }
            | StmtData::Missing => false,
            StmtData::Block(stmts) | StmtData::DeclGroup(stmts) => {
                stmts.iter().any(|stmt| self.stmt_has_valued_return(*stmt))
            }
            StmtData::Labeled { stmt, .. }
            | StmtData::While { body: stmt, .. }
            | StmtData::DoWhile { body: stmt, .. }
            | StmtData::ForEach { body: stmt, .. }
            | StmtData::Synchronized { body: stmt, .. } => self.stmt_has_valued_return(stmt),
            StmtData::If { then, els, .. } => {
                self.stmt_has_valued_return(then)
                    || els.is_some_and(|els| self.stmt_has_valued_return(els))
            }
            StmtData::For { body: stmt, .. } => self.stmt_has_valued_return(stmt),
            StmtData::Switch { arms, .. } => arms.iter().any(|arm| {
                arm.body
                    .iter()
                    .any(|stmt| self.stmt_has_valued_return(*stmt))
            }),
            StmtData::Try {
                body,
                catches,
                finally,
                ..
            } => {
                self.stmt_has_valued_return(body)
                    || catches
                        .iter()
                        .any(|catch| self.stmt_has_valued_return(catch.body))
                    || finally.is_some_and(|finally| self.stmt_has_valued_return(finally))
            }
        }
    }
}

/// The keyword naming the access of a member
/// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// for the `has {access} access` wording of an [`IllegalAccess`]
/// (e.g. javac's `secret has private access in p.Priv`).
fn access_keyword(access: Access) -> &'static str {
    match access {
        Access::Private => "private",
        Access::Protected => "protected",
        Access::Package => "package-private",
        Access::Public => "public",
    }
}

/// §6.5.6.1/[§15.27.2]: a body-tree scan — independent of inference — that
/// finds (a) every local variable that is *mutated* anywhere in the body
/// (`mutated`, keyed by name), and (b) every reference to an enclosing local
/// inside a lambda expression (`captures`), with the reference's expression.
/// A local that is both captured by a lambda and mutated anywhere in its scope
/// is not *effectively final*, and every capture of it is an error.
fn effective_final_scan(
    bodies: &BodyTree,
    body_id: BodyId,
) -> (FxHashSet<Name>, Vec<(Name, ExprId)>) {
    fn resolve<'a>(scopes: &'a [FxHashSet<Name>], name: &Name) -> Option<usize> {
        scopes.iter().rposition(|scope| scope.contains(name))
    }
    struct Scan<'a> {
        bodies: &'a BodyTree,
        scopes: Vec<FxHashSet<Name>>,
        lambda_entries: Vec<usize>,
        mutated: FxHashSet<Name>,
        captures: Vec<(Name, ExprId)>,
    }
    impl Scan<'_> {
        fn in_lambda(&self) -> bool {
            !self.lambda_entries.is_empty()
        }
        fn walk_stmt(&mut self, stmt: StmtId) {
            use hir_expand::body::StmtData;
            let data = self.bodies.stmt(stmt);
            match data.clone() {
                StmtData::Empty | StmtData::Missing => {}
                StmtData::Block(inner) => {
                    self.scopes.push(FxHashSet::default());
                    for s in inner {
                        self.walk_stmt(s);
                    }
                    self.scopes.pop();
                }
                StmtData::Decl { local, initializer } => {
                    let name = self.bodies.local(local).name.clone();
                    self.scopes.last_mut().expect("scope").insert(name);
                    if let Some(initializer) = initializer {
                        self.walk_expr(initializer);
                    }
                }
                StmtData::DeclGroup(inner) => {
                    for s in inner {
                        self.walk_stmt(s);
                    }
                }
                StmtData::Expr(expr) => self.walk_expr(expr),
                StmtData::Labeled { stmt, .. } => self.walk_stmt(stmt),
                StmtData::If { cond, then, els } => {
                    self.walk_expr(cond);
                    self.walk_stmt(then);
                    if let Some(els) = els {
                        self.walk_stmt(els);
                    }
                }
                StmtData::While { cond, body } | StmtData::DoWhile { body, cond } => {
                    self.walk_expr(cond);
                    self.walk_stmt(body);
                }
                StmtData::For {
                    init,
                    cond,
                    step,
                    body,
                } => {
                    self.scopes.push(FxHashSet::default());
                    for s in init {
                        self.walk_stmt(s);
                    }
                    if let Some(cond) = cond {
                        self.walk_expr(cond);
                    }
                    for e in step {
                        self.walk_expr(e);
                    }
                    self.walk_stmt(body);
                    self.scopes.pop();
                }
                StmtData::ForEach {
                    var,
                    iterable,
                    body,
                } => {
                    self.scopes.push(FxHashSet::default());
                    let name = self.bodies.local(var).name.clone();
                    self.scopes.last_mut().expect("scope").insert(name);
                    self.walk_expr(iterable);
                    self.walk_stmt(body);
                    self.scopes.pop();
                }
                StmtData::Switch { scrutinee, arms } => {
                    self.walk_expr(scrutinee);
                    for arm in arms {
                        for label in arm.labels {
                            if let hir_expand::body::SwitchLabel::Expr(e) = label {
                                self.walk_expr(e);
                            }
                        }
                        for s in arm.body {
                            self.walk_stmt(s);
                        }
                    }
                }
                StmtData::Return(ret) => {
                    if let Some(ret) = ret {
                        self.walk_expr(ret);
                    }
                }
                StmtData::Throw(expr) => self.walk_expr(expr),
                StmtData::Break(_) | StmtData::Continue(_) => {}
                StmtData::Yield(expr) => self.walk_expr(expr),
                StmtData::Synchronized { expr, body } => {
                    self.walk_expr(expr);
                    self.walk_stmt(body);
                }
                StmtData::Try {
                    resources,
                    body,
                    catches,
                    finally,
                } => {
                    for resource in resources {
                        let name = self.bodies.local(resource.local).name.clone();
                        self.scopes.last_mut().expect("scope").insert(name);
                        if let Some(init) = resource.initializer {
                            self.walk_expr(init);
                        }
                    }
                    self.walk_stmt(body);
                    for clause in catches {
                        self.scopes.push(FxHashSet::default());
                        let name = self.bodies.local(clause.param).name.clone();
                        self.scopes.last_mut().expect("scope").insert(name);
                        self.walk_stmt(clause.body);
                        self.scopes.pop();
                    }
                    if let Some(finally) = finally {
                        self.walk_stmt(finally);
                    }
                }
                StmtData::Assert { cond, msg } => {
                    self.walk_expr(cond);
                    if let Some(msg) = msg {
                        self.walk_expr(msg);
                    }
                }
                StmtData::LocalClass { .. } => {}
            }
        }
        fn walk_expr(&mut self, expr: ExprId) {
            use hir_expand::body::ExprData;
            let data = self.bodies.expr(expr);
            match data.clone() {
                ExprData::Var(name) | ExprData::NamePath(name) => {
                    if self.in_lambda()
                        && resolve(&self.scopes, &name).is_some_and(|frame| {
                            self.lambda_entries
                                .last()
                                // §15.27.2/[§6.3]: the lambda's own parameter
                                // frame is the last pushed before the entry
                                // marker — index `entry - 1`. A reference that
                                // resolves to it (or deeper, to a local of the
                                // lambda body) is *not* a capture of an
                                // enclosing variable; only a frame strictly
                                // before it is.
                                .is_some_and(|&entry| frame + 1 < entry)
                        })
                    {
                        self.captures.push((name, expr));
                    }
                }
                ExprData::Literal(_)
                | ExprData::Null
                | ExprData::This { .. }
                | ExprData::Super { .. }
                | ExprData::Missing => {}
                ExprData::Template { args } => {
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::ClassLit(_) => {}
                ExprData::FieldAccess { target, .. } => {
                    if let Some(target) = target {
                        self.walk_expr(target);
                    }
                }
                ExprData::ArrayAccess { array, index } => {
                    self.walk_expr(array);
                    self.walk_expr(index);
                }
                ExprData::MethodCall { receiver, args, .. } => {
                    if let Some(receiver) = receiver {
                        self.walk_expr(receiver);
                    }
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::New { args, receiver, .. } => {
                    if let Some(receiver) = receiver {
                        self.walk_expr(receiver);
                    }
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::CtorCall { args, .. } => {
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::NewArray {
                    dims, initializer, ..
                } => {
                    for d in dims {
                        self.walk_expr(d);
                    }
                    if let Some(elems) = initializer {
                        for e in elems {
                            self.walk_expr(e);
                        }
                    }
                }
                ExprData::ArrayInit(elems) => {
                    for e in elems {
                        self.walk_expr(e);
                    }
                }
                ExprData::Unary { expr: inner, op } => {
                    if matches!(
                        op,
                        hir_expand::body::UnaryOp::Inc | hir_expand::body::UnaryOp::Dec
                    ) {
                        self.mark_mutation(inner);
                    }
                    self.walk_expr(inner);
                }
                ExprData::Postfix { expr: inner, .. } => {
                    self.mark_mutation(inner);
                    self.walk_expr(inner);
                }
                ExprData::Binary { lhs, rhs, .. } => {
                    self.walk_expr(lhs);
                    self.walk_expr(rhs);
                }
                ExprData::Assign { lhs, rhs, .. } => {
                    self.mark_mutation(lhs);
                    self.walk_expr(lhs);
                    self.walk_expr(rhs);
                }
                ExprData::Cast { expr: inner, .. } => self.walk_expr(inner),
                ExprData::InstanceOf {
                    expr: inner,
                    pattern,
                    ..
                } => {
                    self.walk_expr(inner);
                    let _ = pattern;
                }
                ExprData::Conditional { cond, then, els } => {
                    self.walk_expr(cond);
                    self.walk_expr(then);
                    self.walk_expr(els);
                }
                ExprData::Lambda { params, body } => {
                    self.scopes.push(FxHashSet::default());
                    for (name, _, _) in &params {
                        self.scopes.last_mut().expect("scope").insert(name.clone());
                    }
                    self.lambda_entries.push(self.scopes.len());
                    match body {
                        hir_expand::body::LambdaBody::Expr(inner) => self.walk_expr(inner),
                        hir_expand::body::LambdaBody::Block(stmt) => self.walk_stmt(stmt),
                    }
                    self.lambda_entries.pop();
                    self.scopes.pop();
                }
                ExprData::MethodRef { qualifier, .. } => {
                    if let Some(qualifier) = qualifier {
                        self.walk_expr(qualifier);
                    }
                }
                ExprData::Switch { scrutinee, arms } => {
                    self.walk_expr(scrutinee);
                    for arm in arms {
                        for label in arm.labels {
                            if let hir_expand::body::SwitchLabel::Expr(e) = label {
                                self.walk_expr(e);
                            }
                        }
                        for s in arm.body {
                            self.walk_stmt(s);
                        }
                    }
                }
                ExprData::Paren(inner) => self.walk_expr(inner),
            }
        }
        fn mark_mutation(&mut self, expr: ExprId) {
            if let ExprData::Var(name) = self.bodies.expr(expr).clone() {
                self.mutated.insert(name);
            }
        }
    }
    let mut scan = Scan {
        bodies,
        scopes: vec![FxHashSet::default()],
        lambda_entries: Vec::new(),
        mutated: FxHashSet::default(),
        captures: Vec::new(),
    };
    for &param in &bodies.body(body_id).params {
        let name = bodies.local(param).name.clone();
        scan.scopes.last_mut().expect("scope").insert(name);
    }
    for &stmt in &bodies.body(body_id).stmts {
        scan.walk_stmt(stmt);
    }
    (scan.mutated, scan.captures)
}
