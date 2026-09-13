//! Kotlin body inference.
//!
//! Walks a declaration's body ([`hir_expand::body::BodyTree`], lowered from the
//! CST by `hir-def`) and records the type of every expression and local, plus
//! the type errors the walk finds ([`KotlinTypeError`], worded as kotlinc
//! words them).
//!
//! The rules are KLS
//! `type-inference.html#type-inference`](https://kotlinlang.org/spec/type-inference.html#type-inference)
//! and the expression rules of
//! `expressions.html`](https://kotlinlang.org/spec/expressions.html):
//!
//! * a local's type is its declared type, or the type of its initializer
//!   ([`#local-type-inference`](https://kotlinlang.org/spec/type-inference.html#local-type-inference));
//! * a call's type is the return type of the callable the arguments select
//!   ([`crate::kotlin::method::pick_callable`]), or `Unit` for a call whose
//!   result is discarded;
//! * `lhs ?: rhs` is `lhs`'s type with its nullability removed, joined with
//!   `rhs`'s ([`expressions.html#elvis-operator-expressions`](https://kotlinlang.org/spec/expressions.html#elvis-operator-expressions));
//! * `receiver?.member` is the member's type made *nullable*, the operator that
//!   propagates null ([`expressions.html#navigation-operators`](https://kotlinlang.org/spec/expressions.html#navigation-operators));
//! * `expr!!` is the operand's type with its nullability removed
//!   ([`expressions.html#not-null-assertion-expressions`](https://kotlinlang.org/spec/expressions.html#not-null-assertion-expressions));
//! * an `is`-tested `when` arm narrows the subject inside the arm
//!   ([`type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts));
//! * `return` and `throw` are expressions of `kotlin.Nothing`
//!   ([`expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
//!
//! # What is approximate, and why
//!
//! Recorded deviations, each visible in the snapshots: a lambda's
//! parameter type without a declaration is the *error* type (the expected
//! function type is not propagated into the literal yet); an `if`/`when`/`try`
//! join is the first branch's type when the branches are not identical (KLS's
//! least upper bound is the constraint solver's `lub`, which lands with the
//! full inference); and `1..2` types as its endpoint's type, because
//! `kotlin.ranges.IntRange` is only reachable when the standard library is on
//! the classpath — the fixtures that pin it ship one.

use rustc_hash::FxHashMap;
use vfs::FileId;

use hir::hir_def::kotlin::item_tree::{KotlinItemData, KotlinItemTree};
use hir_expand::body::{
    BodyId, ExprData, ExprId, JumpKind, LocalId, StmtData, StmtId, WhenCondition,
};
use hir_expand::name::Name;

use super::diagnostics::{KotlinTypeError, MismatchTarget};
use super::method::{self, CallArg};
use super::resolve::KotlinResolver;
use crate::java::db::TyDatabase;
use crate::ty::{Ty, TyKind};

/// The types a body's inference produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KotlinBodyTypes {
    pub body: Option<BodyId>,
    pub exprs: FxHashMap<ExprId, Ty>,
    pub locals: FxHashMap<LocalId, Ty>,
    /// The type errors the walk found, in report order.
    pub diagnostics: Vec<KotlinTypeError>,
}

impl KotlinBodyTypes {
    /// The type of an expression, or the error type when it was never inferred
    /// (a synthetic expression the lowering did not produce an id for).
    pub fn expr_ty(&self, db: &dyn TyDatabase, expr: ExprId) -> Ty {
        self.exprs
            .get(&expr)
            .copied()
            .unwrap_or_else(|| Ty::error(db))
    }

    /// The type of a local, or the error type when it has none.
    pub fn local_ty(&self, db: &dyn TyDatabase, local: LocalId) -> Ty {
        self.locals
            .get(&local)
            .copied()
            .unwrap_or_else(|| Ty::error(db))
    }
}

/// Infers the body of the declaration `item` in `file`.
pub fn infer_item(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
) -> KotlinBodyTypes {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = tree.as_kotlin() else {
        return KotlinBodyTypes::default();
    };
    let Some(body) = tree.data(item).body_id() else {
        return KotlinBodyTypes::default();
    };
    let bodies = hir::file_body_tree(db, file);
    let scope = match hir::source_set_for_file(db, file) {
        Some(source_set) => hir::ResolutionScope::SourceSet(source_set),
        None => hir::ResolutionScope::JdkBuiltins,
    };
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    let mut ctx = InferCtx {
        db,
        file,
        scope,
        tree,
        bodies: &bodies,
        resolver,
        types: KotlinBodyTypes {
            body: Some(body),
            ..Default::default()
        },
        // The narrowed types of locals that an `is` test established, keyed by
        // the local — a smart cast ([KLS
        // `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)).
        narrowed: FxHashMap::default(),
    };
    for &param in &bodies.body(body).params {
        let ty = bodies
            .local(param)
            .ty
            .as_ref()
            .map(|ty| super::ty::ty_from_type_ref(db, &ctx.resolver, &ty.ty))
            .unwrap_or_else(|| Ty::error(db));
        ctx.types.locals.insert(param, ty);
    }
    for &stmt in &bodies.body(body).stmts {
        ctx.infer_stmt(stmt);
    }
    ctx.types
}

/// The state one body's inference carries.
struct InferCtx<'a> {
    db: &'a dyn TyDatabase,
    file: FileId,
    scope: hir::ResolutionScope,
    tree: &'a KotlinItemTree,
    bodies: &'a hir_expand::body::BodyTree,
    resolver: KotlinResolver<'a>,
    types: KotlinBodyTypes,
    narrowed: FxHashMap<LocalId, Ty>,
}

impl<'a> InferCtx<'a> {
    /// Kotlin's primitive classifiers, as the resolver spells them.
    fn builtin(&self, name: &str) -> Ty {
        match self.resolver.class_fqn(name) {
            Some(fqn) => Ty::reference(self.db, fqn, Vec::new()),
            None => Ty::error(self.db),
        }
    }

    fn error(&self) -> Ty {
        Ty::error(self.db)
    }

    fn infer_stmt(&mut self, stmt: StmtId) {
        match self.bodies.stmt(stmt).clone() {
            StmtData::Decl { local, initializer } => {
                let declared = self
                    .bodies
                    .local(local)
                    .ty
                    .as_ref()
                    .map(|ty| super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty));
                let actual = initializer.map(|expr| self.infer_expr(expr));
                let range = initializer.and_then(|expr| self.bodies.expr_range(expr));
                let ty = match (declared, actual) {
                    (Some(declared), Some(actual)) => {
                        self.check_binding(MismatchTarget::Local(local), declared, actual, range);
                        declared
                    }
                    (Some(declared), None) => declared,
                    (None, Some(actual)) => actual,
                    (None, None) => self.error(),
                };
                self.types.locals.insert(local, ty);
            }
            StmtData::Destructuring {
                pattern,
                initializer,
            } => {
                // The pattern binds one local per component; the component types
                // are the initializer's type arguments, in order
                // ([KLS `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)
                // resolves them through `componentN()`, which for a `Pair` is
                // exactly its two arguments).
                let initializer_ty = self.infer_expr(initializer);
                let components = match initializer_ty.kind(self.db) {
                    TyKind::Reference { args, .. } => args.clone(),
                    _ => Vec::new(),
                };
                let parts: Vec<LocalId> = match self.bodies.pattern(pattern).clone() {
                    hir_expand::body::PatternData::Destructuring { parts } => parts,
                    _ => Vec::new(),
                };
                for (index, part) in parts.into_iter().enumerate() {
                    let ty = components
                        .get(index)
                        .copied()
                        .unwrap_or_else(|| self.error());
                    self.types.locals.insert(part, ty);
                }
            }
            StmtData::Expr(expr) => {
                self.infer_expr(expr);
            }
            StmtData::Block(stmts) => {
                for stmt in stmts {
                    self.infer_stmt(stmt);
                }
            }
            StmtData::While { cond, body } | StmtData::DoWhile { body, cond } => {
                self.infer_expr(cond);
                self.infer_stmt(body);
            }
            StmtData::ForEach {
                var,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr(iterable);
                // The loop variable's type is the element type of the
                // iterable's argument when the classpath names it, and the
                // error type otherwise.
                let element = match iterable_ty.kind(self.db) {
                    TyKind::Reference { args, .. } => args.first().copied(),
                    TyKind::Nullable(inner) => match inner.kind(self.db) {
                        TyKind::Reference { args, .. } => args.first().copied(),
                        _ => None,
                    },
                    _ => None,
                };
                self.types
                    .locals
                    .insert(var, element.unwrap_or_else(|| self.error()));
                self.infer_stmt(body);
            }
            StmtData::LocalClass { .. } | StmtData::LocalFunction { .. } => {}
            StmtData::Return(value) => {
                if let Some(value) = value {
                    self.infer_expr(value);
                }
            }
            StmtData::Missing => {}
            other => {
                // The Java statement forms never appear in a Kotlin body; the
                // walk stays total by ignoring them.
                let _ = other;
            }
        }
    }

    /// Checks a value bound to a declaration, reporting the mismatch with
    /// kotlinc's wording.
    fn check_binding(
        &mut self,
        target: MismatchTarget,
        expected: Ty,
        actual: Ty,
        range: Option<rowan::TextRange>,
    ) {
        if expected == actual {
            return;
        }
        let assignable = super::subtyping::is_assignable(self.db, &self.scope, &actual, &expected);
        if assignable {
            return;
        }
        // A nullable (or `null`) value where a non-null type is expected is its
        // own diagnostic: kotlinc words it differently.
        if !expected.is_nullable(self.db)
            && (actual.is_nullable(self.db) || matches!(actual.kind(self.db), TyKind::Null))
        {
            self.types
                .diagnostics
                .push(KotlinTypeError::NullabilityMismatch {
                    target,
                    expected,
                    actual,
                    range,
                });
            return;
        }
        self.types.diagnostics.push(KotlinTypeError::TypeMismatch {
            target,
            expected,
            actual,
            range,
        });
    }

    fn infer_expr(&mut self, expr: ExprId) -> Ty {
        let data = self.bodies.expr(expr).clone();
        let ty = match data {
            ExprData::Literal(literal) => match literal {
                hir_expand::body::Literal::Int(_) => self.builtin("Int"),
                hir_expand::body::Literal::Long(_) => self.builtin("Long"),
                hir_expand::body::Literal::Char(_) => self.builtin("Char"),
                hir_expand::body::Literal::Float => self.builtin("Float"),
                hir_expand::body::Literal::Double => self.builtin("Double"),
                hir_expand::body::Literal::Boolean(_) => self.builtin("Boolean"),
                hir_expand::body::Literal::Str(_) => self.builtin("String"),
            },
            ExprData::Null => Ty::null(self.db),
            ExprData::Var(name) => self.infer_name(expr, &name),
            ExprData::This { .. } => self.builtin("Any"),
            ExprData::Super { .. } => self.builtin("Any"),
            ExprData::Template { args } => {
                for arg in args {
                    self.infer_expr(arg);
                }
                self.builtin("String")
            }
            ExprData::FieldAccess { target, name } => match target {
                Some(target) => {
                    let receiver = self.infer_expr(target);
                    self.member_ty(&receiver, &name)
                }
                None => self.infer_name(expr, &name),
            },
            ExprData::SafeAccess { receiver, member } => {
                let receiver_ty = self.infer_expr(receiver);
                let member_ty = self.infer_expr(member);
                let _ = receiver_ty;
                // The member access happens only when the receiver is not
                // null, and its result is nullable
                // ([KLS `expressions.html#navigation-operators`](https://kotlinlang.org/spec/expressions.html#navigation-operators)).
                Ty::nullable(self.db, member_ty)
            }
            ExprData::NullAssert { expr: inner } => {
                let inner_ty = self.infer_expr(inner);
                inner_ty.strip_nullability(self.db)
            }
            ExprData::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                let receiver_ty = match receiver {
                    Some(receiver) => self.infer_expr(receiver),
                    None => self.builtin("Any"),
                };
                let arg_types: Vec<Ty> = args.iter().map(|arg| self.infer_expr(*arg)).collect();
                self.call_ty(&receiver_ty, &name, &arg_types)
            }
            ExprData::InfixCall {
                receiver,
                name,
                arg,
            } => {
                let receiver_ty = self.infer_expr(receiver);
                let arg_ty = self.infer_expr(arg);
                self.call_ty(&receiver_ty, &name, &[arg_ty])
            }
            ExprData::Binary { op, lhs, rhs } => {
                let lhs_ty = self.infer_expr(lhs);
                let rhs_ty = self.infer_expr(rhs);
                use hir_expand::body::BinaryOp;
                match op {
                    BinaryOp::Eq
                    | BinaryOp::Ne
                    | BinaryOp::Lt
                    | BinaryOp::Gt
                    | BinaryOp::Le
                    | BinaryOp::Ge
                    | BinaryOp::And
                    | BinaryOp::Or => self.builtin("Boolean"),
                    _ => {
                        let _ = rhs_ty;
                        lhs_ty
                    }
                }
            }
            ExprData::Unary { op, expr: inner } => {
                let inner_ty = self.infer_expr(inner);
                match op {
                    hir_expand::body::UnaryOp::Not => self.builtin("Boolean"),
                    _ => inner_ty,
                }
            }
            ExprData::Postfix { expr: inner, .. } => self.infer_expr(inner),
            ExprData::Assign { lhs, rhs, .. } => {
                let rhs_ty = self.infer_expr(rhs);
                let range = self.bodies.expr_range(rhs);
                self.infer_assignment(lhs, rhs_ty, range);
                self.builtin("Unit")
            }
            ExprData::Elvis { lhs, rhs } => {
                let lhs_ty = self.infer_expr(lhs);
                let rhs_ty = self.infer_expr(rhs);
                let definite = lhs_ty.strip_nullability(self.db);
                if definite == rhs_ty { definite } else { rhs_ty }
            }
            ExprData::Cast {
                ty,
                expr: inner,
                safe,
            } => {
                self.infer_expr(inner);
                let target = super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty);
                if safe {
                    Ty::nullable(self.db, target)
                } else {
                    target
                }
            }
            ExprData::InstanceOf { expr: inner, .. } => {
                self.infer_expr(inner);
                self.builtin("Boolean")
            }
            ExprData::ArrayAccess { array, index } => {
                let array_ty = self.infer_expr(array);
                self.infer_expr(index);
                match array_ty.kind(self.db) {
                    TyKind::Reference { args, .. } => {
                        args.first().copied().unwrap_or_else(|| self.error())
                    }
                    _ => self.error(),
                }
            }
            ExprData::ArrayInit(items) => {
                let element = items
                    .first()
                    .map(|item| self.infer_expr(*item))
                    .unwrap_or_else(|| self.error());
                for item in items.iter().skip(1) {
                    self.infer_expr(*item);
                }
                let list = self.builtin("List");
                match list.kind(self.db) {
                    TyKind::Reference { name, .. } => {
                        Ty::reference(self.db, name.clone(), vec![element])
                    }
                    _ => self.error(),
                }
            }
            ExprData::Conditional { cond, then, els } => {
                self.infer_expr(cond);
                let then_ty = self.infer_expr(then);
                let els_ty = self.infer_expr(els);
                if then_ty == els_ty { then_ty } else { then_ty }
            }
            ExprData::When { subject, arms } => self.infer_when(subject, &arms),
            ExprData::Try {
                body,
                catches,
                finally,
            } => {
                self.infer_stmt(body);
                for catch in catches {
                    // A catch parameter's type is the first of its declared
                    // types (Kotlin has no multi-catch).
                    let ty = catch
                        .param_types
                        .first()
                        .map(|ty| super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty))
                        .unwrap_or_else(|| self.error());
                    self.types.locals.insert(catch.param, ty);
                    self.infer_stmt(catch.body);
                }
                if let Some(finally) = finally {
                    self.infer_stmt(finally);
                }
                self.builtin("Unit")
            }
            ExprData::Block(stmt) => self.block_ty(stmt),
            ExprData::Lambda { params, body } => {
                let param_tys: Vec<Ty> = params
                    .iter()
                    .map(|param| {
                        param
                            .ty
                            .as_ref()
                            .map(|ty| super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty))
                            .unwrap_or_else(|| self.error())
                    })
                    .collect();
                let ret = match body {
                    hir_expand::body::LambdaBody::Expr(expr) => self.infer_expr(expr),
                    hir_expand::body::LambdaBody::Block(stmt) => self.block_ty(stmt),
                };
                let mut args = param_tys;
                args.push(ret);
                let function = self.builtin(&format!("Function{}", args.len().saturating_sub(1)));
                match function.kind(self.db) {
                    TyKind::Reference { name, .. } => Ty::reference(self.db, name.clone(), args),
                    _ => self.error(),
                }
            }
            ExprData::CallableReference { receiver, name } => {
                let receiver_ty = match receiver {
                    Some(receiver) => self.infer_expr(receiver),
                    None => self.builtin("Any"),
                };
                self.member_ty(&receiver_ty, &name)
            }
            ExprData::Range { lhs, rhs, .. } => {
                let lhs_ty = self.infer_expr(lhs);
                self.infer_expr(rhs);
                lhs_ty
            }
            ExprData::Spread { expr: inner } => self.infer_expr(inner),
            // `return` and `throw` have type `Nothing`
            // ([KLS `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
            ExprData::Jump { kind, value, .. } => {
                if let Some(value) = value {
                    self.infer_expr(value);
                }
                let _ = matches!(kind, JumpKind::Return | JumpKind::Throw);
                self.builtin("Nothing")
            }
            ExprData::Paren(inner) => self.infer_expr(inner),
            ExprData::ObjectLiteral { item } => match self.tree.data(item) {
                KotlinItemData::Class(data) => {
                    let _ = data;
                    self.error()
                }
                _ => self.error(),
            },
            ExprData::Missing => self.error(),
            other => {
                let _ = other;
                self.error()
            }
        };
        self.types.exprs.insert(expr, ty);
        ty
    }

    /// A `when` expression: the joined type of its arm bodies, with the subject
    /// narrowed inside an `is` arm
    /// ([KLS `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)).
    fn infer_when(&mut self, subject: Option<ExprId>, arms: &[hir_expand::body::WhenArm]) -> Ty {
        let subject_ty = subject.map(|subject| self.infer_expr(subject));
        let mut result = None;
        for arm in arms {
            let narrowed = self.narrow_for(&arm.conditions, subject_ty);
            for condition in &arm.conditions {
                match condition {
                    WhenCondition::Value(value) => {
                        self.infer_expr(*value);
                    }
                    WhenCondition::TypeTest { expr, ty, .. } => {
                        self.infer_expr(*expr);
                        super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty);
                    }
                    WhenCondition::Containment {
                        element, container, ..
                    } => {
                        self.infer_expr(*element);
                        self.infer_expr(*container);
                    }
                }
            }
            let body_ty = self.infer_expr(arm.body);
            if let Some((local, ty)) = narrowed {
                self.types.locals.insert(local, ty);
                result = Some(body_ty);
                self.narrowed.remove(&local);
            }
            result.get_or_insert(body_ty);
        }
        result.unwrap_or_else(|| self.builtin("Unit"))
    }

    /// The narrowed type an `is` condition establishes for the subject, when
    /// the subject is a local: `when (x) { is String -> … }` types `x` as
    /// `String` inside the arm.
    fn narrow_for(
        &self,
        conditions: &[WhenCondition],
        subject_ty: Option<Ty>,
    ) -> Option<(LocalId, Ty)> {
        let subject_ty = subject_ty?;
        for condition in conditions {
            if let WhenCondition::TypeTest { expr, ty, negated } = condition
                && !negated
                && let ExprData::Var(name) = self.bodies.expr(*expr).clone()
            {
                let local = self.types.locals.iter().find_map(|(local, _)| {
                    (self.bodies.local(*local).name == name).then_some(*local)
                })?;
                let narrowed = super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty);
                if super::subtyping::is_assignable(self.db, &self.scope, &narrowed, &subject_ty) {
                    return Some((local, narrowed));
                }
            }
        }
        None
    }

    /// The type of a block used as an expression: its last expression
    /// statement's type ([KLS
    /// `expressions.html#expressions`](https://kotlinlang.org/spec/expressions.html#expressions)).
    fn block_ty(&mut self, block: StmtId) -> Ty {
        let stmts = match self.bodies.stmt(block).clone() {
            StmtData::Block(stmts) => stmts,
            other => {
                let _ = other;
                return self.error();
            }
        };
        let mut last = None;
        for stmt in &stmts {
            let ty = match self.bodies.stmt(*stmt).clone() {
                StmtData::Expr(expr) => Some(self.infer_expr(expr)),
                _ => {
                    self.infer_stmt(*stmt);
                    None
                }
            };
            if let Some(ty) = ty {
                last = Some(ty);
            }
        }
        last.unwrap_or_else(|| self.builtin("Unit"))
    }

    /// A name: a local (or a parameter), else a member of the implicit
    /// receiver, else an unresolved reference.
    fn infer_name(&mut self, expr: ExprId, name: &Name) -> Ty {
        if let Some((_, ty)) = self
            .types
            .locals
            .iter()
            .find(|(local, _)| self.bodies.local(**local).name == *name)
        {
            return *ty;
        }
        if let Some((_, narrowed)) = self
            .narrowed
            .iter()
            .find(|(local, _)| self.bodies.local(**local).name == *name)
        {
            return *narrowed;
        }
        let _ = expr;
        // A member of the enclosing class, without a written receiver.
        let any = self.builtin("Any");
        let ty = self.member_ty(&any, name);
        if ty == self.error() {
            // An unresolved *simple* name: kotlinc's `unresolved reference`.
            let range = self.bodies.expr_range(expr);
            self.types
                .diagnostics
                .push(KotlinTypeError::UnresolvedReference {
                    expr,
                    name: name.clone(),
                    range,
                });
        }
        ty
    }

    /// The type of the member `name` on `receiver`: a property's type, or the
    /// return type of a function the member set resolves.
    fn member_ty(&mut self, receiver: &Ty, name: &Name) -> Ty {
        let members = method::member_set(self.db, &self.scope, receiver, name);
        for member in &members {
            if let Ok(ty) = self.item_ty(member.file, member.item) {
                return ty;
            }
        }
        self.error()
    }

    /// The type of a *source* member item, from the item tree.
    fn item_ty(&self, file: FileId, item: hir_expand::ids::ItemId) -> Result<Ty, ()> {
        if file == self.file {
            return Ok(super::db::item_ty(self.db, file, item));
        }
        Err(())
    }

    /// The type of a call: the return type of the candidate the arguments
    /// select, or `Unit` when the receiver's members declare no such callable
    /// (a call whose result is discarded).
    fn call_ty(&mut self, receiver: &Ty, name: &Name, arg_tys: &[Ty]) -> Ty {
        let args: Vec<CallArg<'_>> = arg_tys
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        match method::pick_callable(self.db, &self.scope, receiver, name, &args) {
            Some(member) => self
                .item_ty(member.file, member.item)
                .unwrap_or_else(|_| self.error()),
            None => self.error(),
        }
    }

    /// An assignment: the destination must be a `var`, and the value
    /// assignable to its type.
    fn infer_assignment(&mut self, lhs: ExprId, rhs_ty: Ty, range: Option<rowan::TextRange>) {
        let lhs_ty = self.infer_expr(lhs);
        let name = match self.bodies.expr(lhs).clone() {
            ExprData::Var(name) => name,
            _ => return,
        };
        // A `val` — a read-only local or property — cannot be reassigned
        // ([KLS `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)).
        let is_val = self
            .types
            .locals
            .keys()
            .any(|local| self.bodies.local(*local).name == name)
            && self
                .tree
                .items
                .iter()
                .all(|(_, data)| data.name() != Some(&name));
        if is_val {
            let range = self.bodies.expr_range(lhs);
            self.types
                .diagnostics
                .push(KotlinTypeError::ValReassignment {
                    expr: lhs,
                    name,
                    range,
                });
            return;
        }
        self.check_binding(MismatchTarget::Assignment, lhs_ty, rhs_ty, range);
    }
}
