//! Expression-level type inference over the lowered body IR
//! ([`hir_expand::body`]).
//!
//! [`body_types`] infers the type of every expression ([JLS §15]) and local
//! variable ([JLS §14.4]) of a method, constructor or initializer body, given
//! the declaration types computed by [`crate::db::item_ty_query`] and the
//! body IR of `hir-def`. Names are resolved lexically ([JLS §6.3]); field and
//! method access is resolved by [`crate::method::pick_field`] /
//! [`crate::method::pick_method`] under the access context of the call site
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

use std::sync::Arc;

use hir_expand::{
    body::{
        AssignOp, BinaryOp, BodyId, BodyTree, ExprData, ExprId, LambdaBody, Literal, LocalId,
        PatternData, PatternId, StmtData, StmtId, SwitchArm, SwitchLabel, UnaryOp,
    },
    item_tree::{ItemData, ItemId},
    name::Name,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::{PrimitiveType, TypeRef};
use vfs::FileId;

use crate::{
    const_eval::{Const, ConstEnv},
    db::{TyDatabase, enclosing_class_query, type_params_map_query},
    diagnostics::TypeError,
    inference::{Constraint, Inference, InvocationPhase, least_upper_bound},
    method::{
        FieldData, InvocationContext, InvocationMode, MethodData, access_context, member_set,
        more_specific, pick_field, pick_method, single_abstract_method,
    },
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    subtyping::supertypes_impl,
    ty::{Ty, TyKind, boxed_type, numeric_promotion, unboxed_primitive},
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
}

/// Infers the types of the body of `item` in `file`, memoized per (file,
/// item) by the tracked query in [`crate::db`]. `None` when the item has no
/// body (a declaration without statements) or is not a body-carrying item.
pub fn body_types(db: &dyn TyDatabase, file: FileId, item: ItemId) -> Option<Arc<BodyTypes>> {
    crate::db::body_types_query(db, crate::db::ItemKey::new(db, file, item))
}

pub(crate) fn body_types_impl(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
) -> Option<BodyTypes> {
    let tree = hir::file_item_tree(db, file);
    let scope = scope_for_file(db, file);
    let type_params = type_params_map_query(db, db.file_text(file));
    let resolver = Resolver::new(&tree, type_params, item);
    let access = access_context(db, file, item);
    let enclosing_class = enclosing_class_query(db, db.file_text(file))
        .get(&item)
        .map(|name| Ty::reference(db, name.as_str(), Vec::new()));
    let mut ctx = InferCtx {
        db,
        scope,
        tree: tree.bodies.clone(),
        resolver,
        access,
        enclosing_class,
        enclosing_ret: None,
        enclosing_throws: Vec::new(),
        thrown: Vec::new(),
        forward_names: Vec::new(),
        definite: FxHashSet::default(),
        exited: false,
        writing: false,
        types: FxHashMap::default(),
        locals: FxHashMap::default(),
        diagnostics: Vec::new(),
        scopes: vec![FxHashMap::default()],
        lambda_params: Vec::new(),
        target: None,
        switch_targets: Vec::new(),
        const_locals: FxHashMap::default(),
        case_values: Vec::new(),
    };
    let mut body = None;
    match item_data(&tree, item)? {
        // A method or constructor body ([§8.4]); the return type is the
        // target of a `return` ([§14.17], [§18.5.2.4]).
        hir_expand::item_tree::ItemData::Method(method) => {
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
            match method.body {
                Some(body_id) => {
                    body = Some(body_id);
                    for &param in &tree.bodies.body(body_id).params {
                        ctx.declare_param(param);
                    }
                    for &stmt in &tree.bodies.body(body_id).stmts {
                        ctx.infer_stmt(stmt);
                    }
                    // §11.2: the body must discharge its checked exceptions.
                    ctx.check_thrown_liability();
                }
                // An annotation type element default ([JLS §9.6.2]): a poly
                // expression whose target is the element's return type.
                None => {
                    let default = method.default_expr?;
                    let _ = ctx.with_target(ctx.enclosing_ret, |this| this.infer_expr(default));
                }
            }
        }
        hir_expand::item_tree::ItemData::StaticInit(init) => {
            let body_id = init.body?;
            body = Some(body_id);
            for &param in &tree.bodies.body(body_id).params {
                ctx.declare_param(param);
            }
            for &stmt in &tree.bodies.body(body_id).stmts {
                ctx.infer_stmt(stmt);
            }
        }
        hir_expand::item_tree::ItemData::InstanceInit(init) => {
            let body_id = init.body?;
            body = Some(body_id);
            for &param in &tree.bodies.body(body_id).params {
                ctx.declare_param(param);
            }
            for &stmt in &tree.bodies.body(body_id).stmts {
                ctx.infer_stmt(stmt);
            }
            // §11.2.2: an initializer cannot declare throws, so every
            // remaining checked exception is unreported.
            ctx.check_thrown_liability();
        }
        // A field initializer ([§8.3.3]): a poly expression whose target is
        // the field's declared type.
        hir_expand::item_tree::ItemData::Field(field) => {
            let initializer = field.initializer_expr?;
            let target = resolve_type_ref(db, &ctx.scope, &ctx.resolver, &field.ty);
            // §8.3.3: the names this initializer may not read by simple name
            // — same-class fields of the same static/instance kind declared
            // textually after it.
            ctx.forward_names = forward_field_names(&tree, item, field.modifiers.static_);
            let _ = ctx.with_target(Some(target), |this| this.infer_expr(initializer));
        }
        // Enum constant arguments ([§8.9.1]) — inferred standalone (the
        // constructor resolution is out of scope here).
        hir_expand::item_tree::ItemData::EnumConstant(constant) => {
            if constant.argument_exprs.is_empty() {
                return None;
            }
            for &arg in &constant.argument_exprs {
                let _ = ctx.infer_expr(arg);
            }
        }
        _ => return None,
    }
    Some(BodyTypes {
        body,
        exprs: ctx.types,
        locals: ctx.locals,
        diagnostics: ctx.diagnostics,
    })
}

struct InferCtx<'a> {
    db: &'a dyn TyDatabase,
    scope: hir::ResolutionScope,
    tree: Arc<BodyTree>,
    resolver: Resolver,
    access: InvocationContext,
    enclosing_class: Option<Ty>,
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
    /// The locals definitely assigned at the current position
    /// ([§16](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html)):
    /// parameters start assigned; a declarator with an initializer or the
    /// target of a simple assignment joins. Branch-sensitive — see
    /// [`InferCtx::infer_stmt_data`].
    definite: FxHashSet<LocalId>,
    /// Whether control flow has exited (return/break/continue/throw/yield)
    /// on every path to the current position ([§14.22] in effect): reads past
    /// this point are not definite-assignment errors.
    exited: bool,
    /// Whether the expression currently being inferred is the left-hand side
    /// of a simple assignment — a write, not a value read ([§15.26.1],
    /// [§16]).
    writing: bool,
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
}

impl<'a> InferCtx<'a> {
    fn error(&self) -> Ty {
        Ty::error(self.db)
    }

    /// Records a type error. The construct it reports degrades to
    /// [`Ty::error`] so that downstream resolution of the body keeps working;
    /// the diagnostics layer collects these per file via
    /// [`BodyTypes::diagnostics`].
    fn report(&mut self, diagnostic: TypeError) {
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

    fn primitive(&self, p: PrimitiveType) -> Ty {
        Ty::primitive(self.db, p)
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
        if !self.is_boolean(ty) {
            self.types.insert(cond, self.error());
            self.report(TypeError::NonBooleanCondition { expr: cond });
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
            return crate::subtyping::is_subtype(self.db, &self.scope, &a, &b)
                || crate::subtyping::is_subtype(self.db, &self.scope, &b, &a);
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
                boxed == to || crate::subtyping::is_subtype(self.db, &self.scope, &boxed, &to)
            }
            // §5.1.8/§5.5: unboxing, optionally followed by a *widening*
            // primitive conversion — an unbox-then-narrow cast (`(int) aLong`)
            // is not a casting conversion.
            (TyKind::Reference { name, .. }, TyKind::Primitive(t)) => {
                match unboxed_primitive(name.as_str()) {
                    Some(p) => p == *t || crate::subtyping::widening_primitive(p, *t),
                    None => false,
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
        let sub = crate::subtyping::is_subtype(self.db, &self.scope, &from, &to);
        let sup = crate::subtyping::is_subtype(self.db, &self.scope, &to, &from);
        if sub || sup {
            return true;
        }
        match (
            crate::subtyping::class_like_and_final(self.db, &self.scope, &from),
            crate::subtyping::class_like_and_final(self.db, &self.scope, &to),
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
                found: ty.display(self.db).to_string(),
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
        match crate::subtyping::enum_constants(self.db, &self.scope, selector) {
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
                crate::subtyping::enum_constants(self.db, &self.scope, selector)
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
            && !crate::subtyping::is_assignable(self.db, &self.scope, &ty, selector)
            // A label sits in assignment context ([§5.2]), so an int
            // *constant* also narrows to a `byte`, `short` or `char`
            // selector when its value is representable there ([§5.1.3]) —
            // `case 16` of a `byte` selector is legal.
            && !self.constant_narrowable(label, ty.clone(), selector.clone())
        {
            self.report(TypeError::IncompatibleTypes {
                expr: label,
                found: ty.display(self.db).to_string(),
                expected: selector.display(self.db).to_string(),
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
        self.const_int_value(expr)
            .is_some_and(|value| crate::subtyping::fits_primitive(value, *d))
    }

    /// The value of an int-typed constant expression
    /// ([JLS §4.12.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4),
    /// [§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)):
    /// literals, parenthesized forms, the constant operators, and simple
    /// names of constant variables — evaluated by [`crate::const_eval`].
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
                    && crate::resolve::class_is_generic(self.db, &self.scope, name)
            }
            _ => false,
        }
    }

    /// §4.12.2: reports a declared local whose type is a raw type.
    fn warn_raw_declared_type(&mut self, local: LocalId) {
        let Some(ty) = self.locals.get(&local).copied() else {
            return;
        };
        if let TyKind::Reference { name, .. } = ty.kind(self.db)
            && self.is_raw_type(&ty)
        {
            let simple = name.as_str().rsplit('.').next().unwrap_or_default();
            self.report(TypeError::RawTypeUse {
                local,
                name: simple.to_owned(),
            });
        }
    }

    /// §5.1.9/§5.2: an assignment whose source is a raw type and whose target
    /// is parameterized succeeds by *unchecked conversion*; report it.
    fn warn_unchecked(&mut self, expr: ExprId, src: &Ty, dst: &Ty) {
        if src.is_error(self.db) || dst.is_error(self.db) || !self.is_raw_type(src) {
            return;
        }
        let parameterized =
            matches!(dst.kind(self.db), TyKind::Reference { args, .. } if !args.is_empty());
        let plain_subtype = crate::subtyping::is_subtype(self.db, &self.scope, src, dst);
        if parameterized && !plain_subtype {
            self.report(TypeError::UncheckedConversion {
                expr,
                from: src.display(self.db).to_string(),
                to: dst.display(self.db).to_string(),
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
            || crate::subtyping::enum_constants(self.db, &self.scope, selector).is_some()
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
        if !crate::subtyping::is_assignable(self.db, &self.scope, ty, &throwable) {
            return false;
        }
        let unchecked = ["java.lang.RuntimeException", "java.lang.Error"];
        !unchecked.iter().any(|name| {
            let supertype = Ty::reference(self.db, *name, Vec::new());
            crate::subtyping::is_assignable(self.db, &self.scope, ty, &supertype)
        })
    }

    /// §11.2: at the end of a body, every remaining checked exception that no
    /// catch clause discharged and no `throws` clause declares is reported at
    /// the expression that threw it.
    fn check_thrown_liability(&mut self) {
        let declared = self.enclosing_throws.clone();
        let pending = std::mem::take(&mut self.thrown);
        for (ty, expr) in pending {
            if !self.is_checked(&ty) {
                continue;
            }
            let discharged = declared
                .iter()
                .any(|target| crate::subtyping::is_assignable(self.db, &self.scope, &ty, target));
            if !discharged {
                self.report(TypeError::UnreportedException {
                    expr,
                    thrown: ty.display(self.db).to_string(),
                });
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
            // `TypeName.this` is the class or interface `TypeName`.
            ExprData::This { qualifier } => match qualifier {
                Some(type_name) => {
                    resolve_type_ref(self.db, &self.scope, &self.resolver, &type_name)
                }
                None => self.enclosing_class.unwrap_or_else(|| self.error()),
            },
            ExprData::Super { .. } => self.error(),
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
                let _ = self.infer_expr(index);
                let array_ty = self.infer_expr(array);
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
                args,
                ..
            } => self.method_call(id, receiver, name, &args, self.target),
            // §8.8.7.1: `this(args)` delegates to another constructor of the
            // enclosing class.
            ExprData::CtorCall { args } => self.ctor_call(id, &args),
            // §15.9: a class instance creation has the type of the created
            // class; the diamond operator ([§15.9.2]) instantiates the type
            // arguments from the target type.
            ExprData::New {
                ty, args, diamond, ..
            } => self.new_expr(id, ty, diamond, &args, self.target),
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
                        ty: inner.display(self.db).to_string(),
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
            ExprData::Postfix { expr, .. } => self.infer_expr(expr),
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
                let lhs_ty = if matches!(op, AssignOp::Assign) {
                    self.writing = true;
                    let ty = self.infer_expr(lhs);
                    self.writing = false;
                    ty
                } else {
                    self.infer_expr(lhs)
                };
                let rhs_ty = self.with_target(Some(lhs_ty), |this| this.infer_expr(rhs));
                // §16: a simple assignment definitely assigns its left-hand
                // local; a compound assignment or increment reads it first,
                // so it does not discharge a blank local.
                if matches!(op, AssignOp::Assign)
                    && let ExprData::Var(name) = self.tree.expr(lhs).clone()
                    && let Some(local) = self.lookup_local(&name)
                {
                    self.definite.insert(local);
                }
                if matches!(op, AssignOp::Assign)
                    && !lhs_ty.is_error(self.db)
                    && !rhs_ty.is_error(self.db)
                    && !crate::subtyping::is_assignable(self.db, &self.scope, &rhs_ty, &lhs_ty)
                    // §5.2: an in-range int constant narrows to the target
                    // primitive ([§5.1.3]).
                    && !self.constant_narrowable(rhs, rhs_ty, lhs_ty)
                {
                    self.report(TypeError::IncompatibleTypes {
                        expr: rhs,
                        found: rhs_ty.display(self.db).to_string(),
                        expected: lhs_ty.display(self.db).to_string(),
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
                if expr_is_poly(&self.tree, expr) {
                    let cast_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                    let _ = self.with_target(Some(cast_ty), |this| this.infer_expr(expr));
                    cast_ty
                } else {
                    let operand = self.infer_expr(expr);
                    let cast_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                    // §5.5/§15.16: a cast that is prohibited by the casting
                    // conversion rules is a compile-time error.
                    if !operand.is_error(self.db)
                        && !cast_ty.is_error(self.db)
                        && !self.castable(operand, cast_ty)
                    {
                        self.types.insert(expr, self.error());
                        self.report(TypeError::BadCast {
                            expr,
                            found: operand.display(self.db).to_string(),
                            target: cast_ty.display(self.db).to_string(),
                        });
                    }
                    cast_ty
                }
            }
            // §15.20.2: `instanceof` always has type `boolean`; a pattern test
            // ([§14.30]) additionally resolves the pattern, recording the type
            // of each variable it binds ([§14.30.1], [§14.30.2]).
            ExprData::InstanceOf { expr, pattern, .. } => {
                let _ = self.infer_expr(expr);
                if let Some(pattern) = pattern {
                    let _ = self.pattern_type(pattern);
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
                let cond_is_poly =
                    expr_is_poly_ext(&self.tree, then) && expr_is_poly_ext(&self.tree, els);
                let target_is_fi = self.target.is_some_and(|target| {
                    crate::method::single_abstract_method(self.db, &self.scope, &target).is_some()
                });
                let both_arms_calls =
                    expr_is_call(&self.tree, then) && expr_is_call(&self.tree, els);
                if cond_is_poly && self.target.is_some() && (target_is_fi || both_arms_calls) {
                    // §15.25.2: a conditional whose arms are poly expressions
                    // is a poly expression with the target type; the target
                    // propagates into the arms so they are typed against it.
                    let _ = self.infer_expr(then);
                    let _ = self.infer_expr(els);
                    self.target.expect("target checked above")
                } else {
                    let then_ty = self.infer_expr(then);
                    let els_ty = self.infer_expr(els);
                    let ty = self.conditional_type(then_ty, els_ty);
                    // §15.25: a boolean operand against an unrelated
                    // primitive makes the conditional ill-typed — report the
                    // degradation (the arms' types, not the expression's).
                    if ty.is_error(self.db)
                        && !then_ty.is_error(self.db)
                        && !els_ty.is_error(self.db)
                    {
                        self.report(TypeError::IncompatibleOperand { expr: id, op: "?:" });
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
                let mut result_tys: Vec<Ty> = Vec::new();
                for arm in &arms {
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
                                self.infer_stmt_data(&data);
                                if let Some(&last) = stmts.last()
                                    && let StmtData::Yield(expr) = self.tree.stmt(last).clone()
                                    && let Some(ty) = self.types.get(&expr).copied()
                                {
                                    result_tys.push(ty);
                                }
                            }
                            _ => self.infer_stmt_data(&data),
                        }
                    }
                    self.scopes.pop();
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
        ty
    }

    /// A bare name: a local variable or parameter, or — when no local — a
    /// field of the implicit receiver ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)).
    fn var(&mut self, expr: ExprId, name: Name) -> Ty {
        if let Some(local) = self.lookup_local(&name) {
            // §16: a local's value may be read only after it is definitely
            // assigned on every path to the read. Reads past an exit
            // (return/break/throw) are not checked — the path is unreachable —
            // and the left-hand side of a simple assignment is written, not
            // read ([§15.26.1]).
            if !self.exited && !self.writing && !self.definite.contains(&local) {
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
        // A lambda parameter shadows the enclosing class's fields ([§6.3]).
        for scope in self.lambda_params.iter().rev() {
            if let Some(ty) = scope.get(&name) {
                return *ty;
            }
        }
        // §7.5.4: a simple name may name a statically imported member — a
        // static field read through its declaring type.
        if let Some(ty) = self.static_import_field(name.as_str()) {
            return ty;
        }
        if let Some(field) = self.pick_field_of(self.enclosing_class, name.as_str()) {
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
        let (owner, member) = self.resolver.static_import_owner(simple)?;
        let receiver = Ty::reference(self.db, owner.as_str(), Vec::new());
        let access = self.access.with_mode(InvocationMode::Static);
        pick_field(self.db, &self.scope, &receiver, &member, &access)
            .filter(|field| field.is_static)
            .map(|field| field.ty)
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
            if let Some(field) = self.pick_field_of(self.enclosing_class, last) {
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
        let prefix_ty = {
            let tyref = TypeRef::Reference {
                name: Name::new(prefix),
                generic_args: Vec::new(),
            };
            resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref)
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
            None => (self.infer_expr(target), false),
        };
        // §10.7: every array type has a public final `length` field.
        if receiver.is_array(self.db) && name.as_str() == "length" {
            return self.primitive(PrimitiveType::Int);
        }
        match pick_field(self.db, &self.scope, &receiver, name.as_str(), &self.access) {
            Some(field) => field.ty,
            // `Type.name` read without a call — or used as the receiver of a
            // `Type.method(...)` call — is the type itself.
            None if is_static => receiver,
            None => {
                // §15.11: no (accessible) field of the name on the receiver.
                self.report(TypeError::NoSuchField {
                    expr,
                    name: name.clone(),
                });
                self.error()
            }
        }
    }

    fn pick_field_of(&mut self, receiver: Option<Ty>, name: &str) -> Option<FieldData> {
        let receiver = receiver?;
        pick_field(self.db, &self.scope, &receiver, name, &self.access)
    }

    /// An explicit constructor invocation `this(args)`
    /// ([JLS §8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
    /// the candidates are the constructors of the enclosing class — methods
    /// named after the class with no return type — resolved by the same
    /// applicability phases as a method invocation
    /// ([§15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2)).
    /// The invocation is a statement form and has no value.
    fn ctor_call(&mut self, expr: ExprId, args: &[ExprId]) -> Ty {
        let Some(receiver_ty) = self.enclosing_class else {
            for arg in args {
                let _ = self.infer_expr(*arg);
            }
            return self.error();
        };
        // Constructors are declared under the class's simple name.
        let (fqn, _) = receiver_ty.as_reference(self.db).expect("class reference");
        let simple = fqn.as_str().rsplit('.').next().unwrap_or(fqn.as_str());
        let name = Name::new(simple);
        let access = self.access.with_mode(InvocationMode::Virtual);
        let arg_kinds = self.arg_kinds(args);
        match self.resolve_call(&receiver_ty, &name, &arg_kinds, None, &access) {
            Some((method, deferred)) => {
                self.reinfer_deferred(&method, &deferred);
                // §11.2.1: a delegating constructor's declared exceptions add
                // to the enclosing liability.
                for thrown in &method.throws {
                    if self.is_checked(thrown) {
                        self.thrown.push((thrown.clone(), expr));
                    }
                }
            }
            None => {
                for arg in args {
                    let _ = self.infer_expr(*arg);
                }
                let members =
                    member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
                if members.is_empty() {
                    // §8.8.7.1: no constructor of that signature exists.
                    self.report(TypeError::NoSuchMethod {
                        expr,
                        name: name.clone(),
                    });
                } else {
                    self.report(TypeError::WrongArity {
                        expr,
                        name: name.clone(),
                        found: args.len(),
                        expected: members
                            .first()
                            .map(|m| m.params.len())
                            .unwrap_or(args.len()),
                    });
                }
            }
        }
        self.primitive(PrimitiveType::Void)
    }

    fn method_call(
        &mut self,
        expr: ExprId,
        receiver: Option<ExprId>,
        name: Name,
        args: &[ExprId],
        target: Option<Ty>,
    ) -> Ty {
        let (receiver_ty, mode) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        let arg_kinds = self.arg_kinds(args);
        // §15.12.1: no member of the name on the receiver is a compile-time
        // error; members of the name that are all inapplicable (§15.12.2) is a
        // wrong-argument-count error.
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        match self.resolve_call(&receiver_ty, &name, &arg_kinds, target, &access) {
            Some((method, deferred)) => {
                // §18.5.2.2/§18.5.2.4: the resolved formal parameters are the
                // target types of the poly arguments — the lambda, method
                // reference or nested invocation is re-inferred against the
                // instantiated formal ([JLS §18.5.2.4]).
                self.reinfer_deferred(&method, &deferred);
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
            // independent expressions.
            None => {
                for arg in args {
                    let _ = self.infer_expr(*arg);
                }
                if members.is_empty() {
                    // §15.12.1: no method of the name on the receiver.
                    self.report(TypeError::NoSuchMethod {
                        expr,
                        name: name.clone(),
                    });
                } else {
                    // §15.12.2: members of the name exist but none is
                    // applicable to the actual arguments.
                    self.report(TypeError::WrongArity {
                        expr,
                        name: name.clone(),
                        found: args.len(),
                        expected: members
                            .first()
                            .map(|m| m.params.len())
                            .unwrap_or(args.len()),
                    });
                }
                self.error()
            }
        }
    }

    /// The receiver type and invocation mode of an invocation
    /// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
    /// a bare type name in receiver position is a static invocation whose
    /// receiver is a type, not a value; an unqualified call is an implicit
    /// `this` invocation; a `super` receiver is the superclass of the
    /// enclosing class.
    fn receiver_info(&mut self, receiver: Option<ExprId>, name: &Name) -> (Ty, InvocationMode) {
        match receiver {
            Some(receiver) => {
                // `Type.method(...)` — a static invocation whose receiver
                // expression is a pure type name ([§15.12.1]): a bare name or
                // a qualified name such as `java.util.Collections`.
                if let Some(ty) = self.dotted_type_name(receiver) {
                    return (ty, InvocationMode::Static);
                }
                match self.tree.expr(receiver).clone() {
                    // `super.method(...)` — a super invocation whose receiver is
                    // the superclass of the enclosing class ([§15.12.1]).
                    ExprData::Super { qualifier: None } => (self.super_ty(), InvocationMode::Super),
                    // §15.11.2/§15.12.1: `I.super.m(...)` — a qualified-super
                    // invocation selects the default method of the *named*
                    // interface; the receiver type is `I` itself and the mode
                    // restricts candidates to instance members.
                    ExprData::Super {
                        qualifier: Some(qualifier),
                    } => (
                        resolve_type_ref(self.db, &self.scope, &self.resolver, &qualifier),
                        InvocationMode::Interface,
                    ),
                    _ => (self.infer_expr(receiver), InvocationMode::Virtual),
                }
            }
            // An unqualified call is an implicit `this` invocation
            // ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
            // unless a static import ([§7.5.4]) names the method through its
            // declaring type.
            None => {
                if let Some(ty) = self.static_import_method_receiver(name.as_str()) {
                    return (ty, InvocationMode::Static);
                }
                (
                    self.enclosing_class.unwrap_or_else(|| self.error()),
                    InvocationMode::Virtual,
                )
            }
        }
    }

    /// The receiver of an unqualified call that names a statically imported
    /// method ([§7.5.4]): the declaring type, when that type has a static
    /// member of the name. `None` otherwise — the call falls back to the
    /// implicit `this` receiver.
    fn static_import_method_receiver(&self, simple: &str) -> Option<Ty> {
        let (owner, member) = self.resolver.static_import_owner(simple)?;
        let receiver = Ty::reference(self.db, owner.as_str(), Vec::new());
        let access = self.access.with_mode(InvocationMode::Static);
        let has_static = member_set(self.db, &self.scope, &receiver, &member, &access)
            .iter()
            .any(|method| method.is_static);
        has_static.then_some(receiver)
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
            ArgInfo {
                id: arg,
                poly: false,
                leaves: vec![ArgKind::Concrete(self.infer_expr(arg))],
            }
        } else {
            ArgInfo {
                id: arg,
                poly: true,
                leaves: leaves
                    .iter()
                    .map(|leaf| match self.tree.expr(*leaf).clone() {
                        ExprData::Lambda { .. } | ExprData::MethodRef { .. } => ArgKind::Lambda {
                            arity: poly_arity(&self.tree, *leaf),
                        },
                        ExprData::MethodCall { .. } => ArgKind::Invocation { id: *leaf },
                        _ => unreachable!("a poly leaf is a lambda, method reference or call"),
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
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let members = member_set(self.db, &self.scope, receiver_ty, name.as_str(), ctx);
        for phase in [InvocationPhase::Strict, InvocationPhase::Loose] {
            if let Some(chosen) = self.choose_candidate(&members, arg_kinds, phase, false, target) {
                return Some(chosen);
            }
        }
        self.choose_candidate(&members, arg_kinds, InvocationPhase::Loose, true, target)
    }

    /// The most specific applicable candidate in `phase`, or `None` when none
    /// applies or the applicable ones are ambiguous ([JLS §15.12.2.5]). Each
    /// candidate is probed in its own fresh inference table.
    fn choose_candidate(
        &mut self,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        target: Option<Ty>,
    ) -> Option<(MethodData, Vec<(ExprId, usize)>)> {
        let mut applicable: Vec<ApplicableCandidate> = Vec::new();
        for member in members {
            let mut inference = Inference::new();
            let mut deferred = Vec::new();
            if let Some(invocation) = self.try_candidate(
                &mut inference,
                member,
                arg_kinds,
                phase,
                varargs,
                target,
                &mut deferred,
                true,
            ) {
                applicable.push((member.clone(), invocation, deferred));
            }
        }
        if applicable.is_empty() {
            return None;
        }
        if applicable.len() == 1 {
            let (_, invocation, deferred) = applicable.pop().expect("len checked");
            return Some((invocation, deferred));
        }
        let mut best: Option<usize> = None;
        for (i, (candidate, _, _)) in applicable.iter().enumerate() {
            let wins = applicable.iter().all(|(other, _, _)| {
                other == candidate || more_specific(self.db, &self.scope, candidate, other)
            });
            if wins {
                if best.is_some() {
                    return None;
                }
                best = Some(i);
            }
        }
        best.map(|i| {
            let (_, invocation, deferred) = applicable.remove(i);
            (invocation, deferred)
        })
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
        deferred: &mut Vec<(ExprId, usize)>,
        resolve: bool,
    ) -> Option<MethodData> {
        let (formals, ret, throws_formals) = inference.register_method(self.db, method);

        // §15.12.2.2/§15.12.2.3: a lambda is compatible with a function type
        // only when the parameter list has the same arity as the single
        // abstract method ([§15.27.3]). A candidate whose functional interface
        // does not fit a lambda argument is not applicable.
        for (i, info) in arg_kinds.iter().enumerate() {
            let Some(formal) = formals.get(i).copied() else {
                break;
            };
            for kind in &info.leaves {
                if let ArgKind::Lambda {
                    arity: Some(arity), ..
                } = kind
                    && let Some(sam) = single_abstract_method(self.db, &self.scope, &formal)
                    && sam.params.len() != *arity
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
        // an expected type, the constraint ⟨R → T⟩ joins the constraint set, so
        // the inference variables are bounded by the target type as well. Only
        // a generic method can have a poly invocation ([JLS §15.12.2.6]): a
        // non-generic method's return type is fixed, so a mismatched target
        // must not reject an otherwise-applicable invocation.
        if let Some(target) = target
            && !method.type_params.is_empty()
        {
            inference.add_constraint(Constraint::Sub(ret, target));
        }

        let build = |resolved: &FxHashMap<u64, Ty>| MethodData {
            name: method.name.clone(),
            owner: method.owner.clone(),
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
            access: method.access,
            declaring_package: method.declaring_package.clone(),
            declaring_top_level: method.declaring_top_level.clone(),
            declaring_interface: method.declaring_interface,
            type_params: method.type_params.clone(),
        };
        if resolve {
            let resolved = inference.solve_after(self.db, &self.scope, phase)?;
            Some(build(&resolved))
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
            ArgKind::Lambda { .. } => true,
            ArgKind::Invocation { id } => self.contribute_invocation(inference, *id, formal),
        }
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
    fn contribute_invocation(&mut self, inference: &mut Inference, id: ExprId, formal: Ty) -> bool {
        let ExprData::MethodCall {
            receiver,
            name,
            args,
            ..
        } = self.tree.expr(id).clone()
        else {
            return true;
        };
        let (receiver_ty, mode) = self.receiver_info(receiver, &name);
        let access = self.access.with_mode(mode);
        let members = member_set(self.db, &self.scope, &receiver_ty, name.as_str(), &access);
        let arg_kinds = self.arg_kinds(&args);
        for phase in [InvocationPhase::Strict, InvocationPhase::Loose] {
            if self.choose_nested_candidate(inference, &members, &arg_kinds, phase, false, &formal)
            {
                return true;
            }
        }
        self.choose_nested_candidate(
            inference,
            &members,
            &arg_kinds,
            InvocationPhase::Loose,
            true,
            &formal,
        )
    }

    /// The most specific applicable candidate of `members` against the target
    /// `formal`, contributed to the shared table. `false` when none applies in
    /// this phase or the applicable ones are ambiguous ([JLS §15.12.2.5]).
    fn choose_nested_candidate(
        &mut self,
        inference: &mut Inference,
        members: &[MethodData],
        arg_kinds: &[ArgInfo],
        phase: InvocationPhase,
        varargs: bool,
        formal: &Ty,
    ) -> bool {
        let base = inference.snapshot();
        let mut applicable: Vec<MethodData> = Vec::new();
        for member in members {
            inference.restore(base.clone());
            let mut deferred = Vec::new();
            if self
                .try_candidate(
                    inference,
                    member,
                    arg_kinds,
                    phase,
                    varargs,
                    Some(*formal),
                    &mut deferred,
                    false,
                )
                .is_some()
            {
                applicable.push(member.clone());
            }
        }
        if applicable.is_empty() {
            inference.restore(base);
            return false;
        }
        if applicable.len() > 1 {
            let mut best: Option<usize> = None;
            for (i, candidate) in applicable.iter().enumerate() {
                let wins = applicable.iter().all(|other| {
                    other == candidate || more_specific(self.db, &self.scope, candidate, other)
                });
                if wins {
                    if best.is_some() {
                        inference.restore(base);
                        return false;
                    }
                    best = Some(i);
                }
            }
            let Some(i) = best else {
                inference.restore(base);
                return false;
            };
            let winner = applicable.remove(i);
            inference.restore(base);
            // §18.5.2.1: lift the winner's constraints from the base
            // snapshot — the losing candidates are discarded with it.
            let mut deferred = Vec::new();
            let _ = self.try_candidate(
                inference,
                &winner,
                arg_kinds,
                phase,
                varargs,
                Some(*formal),
                &mut deferred,
                false,
            );
        }
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
        ty: TypeRef<Name>,
        diamond: bool,
        args: &[ExprId],
        target: Option<Ty>,
    ) -> Ty {
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        // §15.9.2: `new Foo<>()` — the created class's type arguments are
        // inferred from the target type ([§15.9.2.2]).
        let class_ty = if diamond {
            self.diamond_instantiation(class_ty, target)
        } else {
            class_ty
        };
        // §15.9: instantiating a type variable, an interface, an abstract
        // class or an enum with `new` is a compile-time error.
        self.check_instantiable(expr, class_ty);
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let constructor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        let arg_kinds = self.arg_kinds(args);
        let access = self.access.clone();
        if let Some((method, deferred)) = self.resolve_call(
            &class_ty,
            &Name::new(&constructor_name),
            &arg_kinds,
            None,
            &access,
        ) {
            self.reinfer_deferred(&method, &deferred);
        } else {
            for arg in args {
                let _ = self.infer_expr(*arg);
            }
        }
        class_ty
    }

    /// §15.9: a class instance creation must instantiate a class and not a
    /// type variable, an interface, an abstract class or an enum
    /// ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)).
    /// Source declarations are known directly; library classes are not flagged
    /// (their access flags are not surfaced).
    fn check_instantiable(&mut self, expr: ExprId, class_ty: Ty) {
        let (TyKind::TypeVar { .. } | TyKind::Reference { .. }) = class_ty.kind(self.db) else {
            return;
        };
        let non_instantiable = match class_ty.kind(self.db) {
            TyKind::TypeVar { .. } => true,
            TyKind::Reference { name, .. } => {
                let Some(hir::Resolved::Source(source)) =
                    hir::fqn_resolve(self.db, &self.scope, name.as_str())
                else {
                    return;
                };
                let tree = hir::file_item_tree(self.db, source.file);
                let Some(data) = crate::resolve::item_data(&tree, source.item) else {
                    return;
                };
                let non_instantiable = match data {
                    hir_expand::item_tree::ItemData::Interface(_)
                    | hir_expand::item_tree::ItemData::Enum(_)
                    | hir_expand::item_tree::ItemData::Annotation(_) => true,
                    hir_expand::item_tree::ItemData::Class(d) => d.modifiers.abstract_,
                    _ => false,
                };
                if !non_instantiable {
                    return;
                }
                true
            }
            _ => return,
        };
        if non_instantiable {
            let name = match class_ty.kind(self.db) {
                TyKind::TypeVar { name, .. } => name.as_str().to_owned(),
                TyKind::Reference { name, .. } => name.simple_name().to_owned(),
                _ => unreachable!(),
            };
            self.types.insert(expr, self.error());
            self.report(TypeError::CannotInstantiateTypeVar { expr, name });
        }
    }

    /// §15.9.2: the diamond operator — the created class's type arguments are
    /// inferred from the target type. When the target is a reference type
    /// whose erasure is the created class ([§15.9.2.1]), its type arguments
    /// are taken; otherwise the class is created raw
    /// ([§15.9.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.2.2)).
    /// (Supertype targets — `List<String> l = new ArrayList<>();` — are not
    /// tracked; full §15.9.2 inference is deferred.)
    fn diamond_instantiation(&self, class_ty: Ty, target: Option<Ty>) -> Ty {
        let Some(target) = target else {
            return class_ty;
        };
        let TyKind::Reference {
            name: target_name,
            args,
        } = target.kind(self.db)
        else {
            return class_ty;
        };
        let TyKind::Reference {
            name: class_name, ..
        } = class_ty.kind(self.db)
        else {
            return class_ty;
        };
        if target_name.as_str() == class_name.as_str() && !args.is_empty() {
            Ty::reference(self.db, class_name.as_str(), args.clone())
        } else {
            class_ty
        }
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
        params: &[(Name, Option<TypeRef<Name>>)],
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
                    target: target.display(self.db).to_string(),
                });
            }
            return self.error();
        };
        if sam.params.len() != params.len() {
            return self.error();
        }
        self.lambda_params.push(FxHashMap::default());
        for ((name, declared), formal) in params.iter().zip(&sam.params) {
            let ty = match declared {
                Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
                None => *formal,
            };
            self.lambda_params
                .last_mut()
                .expect("lambda param scope pushed")
                .insert(name.clone(), ty);
        }
        let saved_ret = self.enclosing_ret;
        self.enclosing_ret = Some(sam.ret);
        match body {
            // §15.27.2: an expression lambda's body is a poly expression
            // whose target is the SAM's return type.
            LambdaBody::Expr(expr) => {
                let _ = self.with_target(Some(sam.ret), |this| this.infer_expr(expr));
            }
            LambdaBody::Block(stmt) => self.infer_stmt(stmt),
        }
        self.enclosing_ret = saved_ret;
        self.lambda_params.pop();
        target
    }

    /// The type of a method reference ([JLS §15.13.2]): the target functional
    /// interface. The referenced method is resolved against the SAM's
    /// parameters ([§15.13.3]) so the qualifier is inferred.
    fn method_ref_type(
        &mut self,
        expr: ExprId,
        qualifier: Option<ExprId>,
        type_name: Option<&TypeRef<Name>>,
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
                    target: target.display(self.db).to_string(),
                });
            }
            return self.error();
        };
        self.resolve_method_ref(qualifier, type_name, name, &sam.params);
        target
    }

    /// Resolves the method or constructor referenced by
    /// `Type::name`, `Type::new` or `expr::name` against the single abstract
    /// method's parameters ([JLS §15.13.3]): a static reference takes the
    /// SAM's parameters as its own, an instance reference takes the SAM's
    /// first parameter as the receiver.
    fn resolve_method_ref(
        &mut self,
        qualifier: Option<ExprId>,
        type_name: Option<&TypeRef<Name>>,
        name: &Name,
        sam_params: &[Ty],
    ) {
        let ref_ty = match (type_name, qualifier) {
            (Some(tyref), _) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            // `Type::name` — a qualifier that is a bare name resolving to a
            // type is a type qualifier ([§15.13.1]); otherwise it is an
            // instance qualifier, inferred as an expression.
            (None, Some(expr)) => {
                if let ExprData::Var(name) = self.tree.expr(expr).clone()
                    && let Some(ty) = self.type_name_ty(&name)
                {
                    ty
                } else {
                    self.infer_expr(expr)
                }
            }
            _ => return,
        };
        let TyKind::Reference { name: fqn, .. } = ref_ty.kind(self.db) else {
            return;
        };
        // §15.13.1: `Type::new` is a constructor reference; the constructor
        // is named `<init>` for library classes ([JVMS §4.2]).
        let candidate_name = if name.as_str() == "new" {
            match hir::fqn_resolve(self.db, &self.scope, fqn.as_str()) {
                Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
                _ => fqn.simple_name().to_owned(),
            }
        } else {
            name.as_str().to_owned()
        };
        let methods = member_set(self.db, &self.scope, &ref_ty, &candidate_name, &self.access);
        for method in &methods {
            let expected = if method.is_static {
                sam_params.len()
            } else {
                sam_params.len().saturating_sub(1)
            };
            if method.params.len() == expected {
                break;
            }
        }
    }

    fn unary(&mut self, expr: ExprId, op: UnaryOp) -> Ty {
        let inner = self.infer_expr(expr);
        match op {
            // §15.15.6: `!` has type `boolean` and its operand must be a
            // `boolean` ([§15.15.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.15.6)).
            UnaryOp::Not => {
                if !self.is_boolean(inner) {
                    if !inner.is_error(self.db) {
                        self.types.insert(expr, self.error());
                        self.report(TypeError::NonBooleanCondition { expr });
                    }
                    self.error()
                } else {
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
        if then_ty.is_null(self.db) && els_ty.is_reference(self.db) {
            return els_ty;
        }
        if els_ty.is_null(self.db) && then_ty.is_reference(self.db) {
            return then_ty;
        }
        least_upper_bound(self.db, &self.scope, &[then_ty, els_ty])
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
            self.check_condition(lhs);
            self.scopes.push(FxHashMap::default());
            if let Some(bindings) = self.pattern_binding_ids(lhs) {
                for binding in bindings {
                    self.scope_binding(binding);
                }
            }
            self.check_condition(rhs);
            self.scopes.pop();
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
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
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
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
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
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
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
                        self.report(TypeError::IncompatibleOperand {
                            expr: bad,
                            op: binary_op_symbol(op),
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
                        found: lhs_ty.display(self.db).to_string(),
                        other: rhs_ty.display(self.db).to_string(),
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
        self.definite.insert(id);
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
        self.locals.insert(id, ty);
        // Every binding form except a bare declarator initializes its local
        // (parameters, initializers, catch/foreach/resource variables,
        // pattern bindings), so it is definitely assigned ([§16]).
        self.definite.insert(id);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name, id);
    }

    fn lookup_local(&self, name: &Name) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&local) = scope.get(name) {
                return Some(local);
            }
        }
        None
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
                for &component in &rp.components {
                    let _ = self.pattern_type(component);
                }
                ty
            }
            PatternData::MatchAll => self.error(),
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
        self.definite.insert(id);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name, id);
    }

    fn infer_stmt(&mut self, id: StmtId) {
        let stmt = self.tree.stmt(id).clone();
        self.infer_stmt_data(&stmt);
    }

    fn infer_stmt_data(&mut self, stmt: &StmtData) {
        match stmt {
            StmtData::Empty => {}
            StmtData::Block(stmts) => {
                self.scopes.push(FxHashMap::default());
                for &stmt in stmts {
                    self.infer_stmt(stmt);
                }
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
                if initializer.is_none() {
                    // §16: a declarator without an initializer is not
                    // definitely assigned until a later assignment reaches it.
                    self.definite.remove(local);
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
                    if !crate::subtyping::is_assignable(self.db, &self.scope, &init_ty, &target)
                        && !self.constant_narrowable(*initializer, init_ty, target)
                    {
                        self.report(TypeError::IncompatibleTypes {
                            expr: *initializer,
                            found: init_ty.display(self.db).to_string(),
                            expected: target.display(self.db).to_string(),
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
            StmtData::Labeled { stmt, .. } => self.infer_stmt(*stmt),
            StmtData::If { cond, then, els } => {
                self.check_condition(*cond);
                // §14.30.3: a pattern variable of the condition is in scope
                // in the `then` arm (flow scoping), not in the `else` arm.
                self.scopes.push(FxHashMap::default());
                if let Some(bindings) = self.pattern_binding_ids(*cond) {
                    for binding in bindings {
                        self.scope_binding(binding);
                    }
                }
                // §16: after the `if`, a local is definitely assigned only if
                // it is assigned on *both* paths — the then branch and (when
                // present) the else branch; a branch that exits contributes
                // no constraint.
                let before = self.definite.clone();
                let before_exited = self.exited;
                self.infer_stmt(*then);
                self.scopes.pop();
                let mut then_set = std::mem::replace(&mut self.definite, before.clone());
                let then_exited = std::mem::replace(&mut self.exited, before_exited);
                if let Some(els) = els {
                    self.infer_stmt(*els);
                }
                // The else-less form leaves `definite` at `before` already.
                if els.is_some() {
                    if then_exited && !self.exited {
                        // Only the else path falls through: keep its set.
                    } else if self.exited && !then_exited {
                        // Only the then path falls through: keep its set.
                    } else {
                        then_set.retain(|local| self.definite.contains(local));
                        self.definite = then_set;
                    }
                    self.exited = then_exited && self.exited;
                }
            }
            StmtData::While { cond, body } => {
                self.check_condition(*cond);
                // §16.1.10: the body may run zero times, so nothing it
                // assigns is definitely assigned after the loop.
                let before = self.definite.clone();
                self.infer_stmt(*body);
                self.definite = before;
                self.exited = false;
            }
            StmtData::DoWhile { body, cond } => {
                // §16.1.11: a do-loop's body runs at least once, so its
                // assignments carry past the loop when the body falls
                // through; an exiting body constrains nothing.
                let before = self.definite.clone();
                self.infer_stmt(*body);
                if self.exited {
                    self.definite = before;
                }
                self.check_condition(*cond);
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
                if let Some(cond) = cond {
                    self.check_condition(*cond);
                }
                for &step in step {
                    let _ = self.infer_expr(step);
                }
                // §16.1.14: like `while`, the body may run zero times.
                let before = self.definite.clone();
                self.infer_stmt(*body);
                self.definite = before;
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
                                    found: iterable_ty.display(self.db).to_string(),
                                });
                            }
                            self.error()
                        }
                    }
                };
                self.scopes.push(FxHashMap::default());
                self.declare_local_ty(*var, element);
                // §16.1.11: like `while`, the body may run zero times.
                let before = self.definite.clone();
                self.infer_stmt(*body);
                self.definite = before;
                self.exited = false;
                self.scopes.pop();
            }
            StmtData::Switch { scrutinee, arms } => {
                let selector = self.infer_switch_selector(*scrutinee);
                self.case_values.push(FxHashMap::default());
                self.scopes.push(FxHashMap::default());
                for arm in arms {
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
                    self.scopes.pop();
                }
                self.scopes.pop();
                self.case_values.pop();
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
                if matches!(stmt, StmtData::Return(_)) {
                    match self.enclosing_ret {
                        // §14.17: a value returned from a `void` method or a
                        // constructor is an error.
                        None => {
                            if !ty.is_error(self.db) {
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty.display(self.db).to_string(),
                                    expected: "void".to_owned(),
                                });
                            }
                        }
                        Some(ret) => {
                            if !ty.is_error(self.db)
                                && !ret.is_error(self.db)
                                && !crate::subtyping::is_assignable(self.db, &self.scope, &ty, &ret)
                                // §5.2: an in-range int constant narrows to a
                                // primitive return type ([§5.1.3]).
                                && !self.constant_narrowable(*expr, ty, ret)
                            {
                                // §14.17: the returned value must be assignable
                                // to the return type ([§5.2]).
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty.display(self.db).to_string(),
                                    expected: ret.display(self.db).to_string(),
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
                if !crate::subtyping::is_assignable(self.db, &self.scope, &ty, &throwable) {
                    self.types.insert(*expr, self.error());
                    // §14.18: a non-throwable operand is a compile-time error.
                    self.report(TypeError::IncompatibleTypes {
                        expr: *expr,
                        found: ty.display(self.db).to_string(),
                        expected: "java.lang.Throwable".to_owned(),
                    });
                } else if self.is_checked(&ty) {
                    // §11.2.2/§14.18: a `throw` of a checked exception adds it
                    // to the enclosing liability.
                    self.thrown.push((ty, *expr));
                }
                // §16: control does not continue past a `throw` on this path.
                self.exited = true;
            }
            StmtData::Return(None) | StmtData::Break(_) | StmtData::Continue(_) => {
                // §16: control does not continue past an exit on this path.
                self.exited = true;
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
                }
                // §16.1.8: the try block may exit via any catch, so a local
                // is definitely assigned after the statement only if it is
                // assigned at the end of *every* path — the intersection of
                // the try block and each catch clause. A `finally` always
                // runs, so its assignments override.
                let before = self.definite.clone();
                let before_exited = self.exited;
                self.infer_stmt(*body);
                // The end-of-body state is one path; each catch clause adds
                // another, starting from the pre-try state.
                let mut paths: Vec<FxHashSet<LocalId>> = vec![self.definite.clone()];
                let mut all_exits = self.exited;
                // exceptions assignable to its declared type; a clause whose
                // type is a subtype of an *earlier* clause's type is
                // unreachable ([§14.20]).
                let mut catch_tys: Vec<Ty> = Vec::new();
                for clause in catches {
                    // Each catch clause is an alternative path that starts
                    // from the pre-try state ([§16.1.8]).
                    self.definite = before.clone();
                    self.exited = before_exited;
                    self.scopes.push(FxHashMap::default());
                    let clause_ty = self
                        .tree
                        .local(clause.param)
                        .ty
                        .as_ref()
                        .map(|ty| resolve_type_ref(self.db, &self.scope, &self.resolver, ty));
                    if let Some(clause_ty) = &clause_ty {
                        if !clause_ty.is_error(self.db)
                            && catch_tys.iter().any(|earlier| {
                                crate::subtyping::is_subtype(
                                    self.db,
                                    &self.scope,
                                    &clause_ty.clone(),
                                    earlier,
                                )
                            })
                        {
                            // §11.2.3: already caught by an earlier clause.
                            self.report(TypeError::AlreadyCaught {
                                local: clause.param,
                            });
                        } else {
                            self.thrown.retain(|(thrown, _)| {
                                !crate::subtyping::is_assignable(
                                    self.db,
                                    &self.scope,
                                    thrown,
                                    clause_ty,
                                )
                            });
                            catch_tys.push(clause_ty.clone());
                        }
                    }
                    self.declare_local(clause.param);
                    self.infer_stmt(clause.body);
                    // The clause's end state joins the other paths.
                    paths.push(std::mem::replace(&mut self.definite, before.clone()));
                    all_exits &= std::mem::replace(&mut self.exited, before_exited);
                    self.scopes.pop();
                }
                // The intersection of every path: a local is definitely
                // assigned after the try only if each path assigned it.
                let mut joined = paths.pop().unwrap_or_default();
                for path in &paths {
                    joined.retain(|local| path.contains(local));
                }
                self.definite = joined;
                self.exited = all_exits;
                self.scopes.pop();
                if let Some(finally) = finally {
                    // §16.1.8: the `finally` block always runs, so its
                    // assignments override the joined state.
                    self.infer_stmt(*finally);
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
    tree: &hir_expand::item_tree::ItemTree,
    field: hir_expand::item_tree::ItemId,
    static_field: bool,
) -> Vec<Name> {
    // The class-like declaration owning `field`.
    fn owner_of(
        tree: &hir_expand::item_tree::ItemTree,
        id: hir_expand::item_tree::ItemId,
        target: hir_expand::item_tree::ItemId,
    ) -> Option<hir_expand::item_tree::ItemId> {
        let data = tree.data(id);
        let class_like = matches!(
            data,
            ItemData::Class(_) | ItemData::Interface(_) | ItemData::Enum(_) | ItemData::Record(_)
        );
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
                        if later.modifiers.static_ == static_field && item != field =>
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

/// The kinds of the actual arguments of a method invocation for the joint
/// inference of §18.5.2.4: a concrete argument has a standalone type; a poly
/// argument is a lambda or method reference deferred to its target formal
/// ([JLS §18.5.2.2], [§15.27.3], [§15.13.2]), or a nested method invocation
/// whose inference shares the enclosing invocation's table
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)).
#[derive(Clone)]
enum ArgKind {
    /// An argument with a concrete standalone type.
    Concrete(Ty),
    /// A lambda or method reference; the arity check of §15.12.2.2/§15.12.2.3
    /// is run against the target formal's single abstract method. A method
    /// reference is not arity-checkable without resolving the referenced
    /// method, so its arity is `None`.
    Lambda { arity: Option<usize> },
    /// A nested method invocation, resolved against the target formal by
    /// contributing its constraints to the enclosing invocation's table.
    Invocation { id: ExprId },
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

/// The poly leaves of an argument ([JLS §15.2]): a lambda, method reference or
/// method invocation, or the leaves of a parenthesized or conditional
/// expression whose arms are poly ([JLS §18.5.2.4]). An argument that is not a
/// poly expression has no leaves — it is inferred standalone.
fn poly_leaves(tree: &BodyTree, id: ExprId) -> Vec<ExprId> {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. } | ExprData::MethodRef { .. } | ExprData::MethodCall { .. } => {
            vec![id]
        }
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
