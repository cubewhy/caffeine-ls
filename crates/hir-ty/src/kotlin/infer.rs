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
    /// The declaration every *name* of the body resolved to, keyed by the
    /// expression it was inferred at — the Kotlin twin of the Java layer's
    /// `ResolvedMember`, which the navigation layer reads to answer a
    /// go-to-definition inside a body.
    pub resolved: FxHashMap<ExprId, KotlinResolvedMember>,
}

/// The declaration a name of a Kotlin body resolved to
/// ([`KotlinBodyTypes::resolved`]).
#[derive(Debug, Clone, PartialEq)]
pub enum KotlinResolvedMember {
    /// A local binding: a parameter, a declared local, a loop variable, a
    /// pattern binding or a catch parameter, identified by its binding in the
    /// body.
    Local(LocalId),
    /// A Kotlin source declaration, possibly of another file than the body.
    Kotlin {
        file: FileId,
        item: hir_expand::ids::ItemId,
    },
    /// A Java source or classfile method — the instantiated JVM view a Kotlin
    /// receiver resolved through ([`crate::kotlin::jvm_view`]).
    Java(Box<crate::java::method::MethodData>),
    /// A Java source or classfile field.
    JavaField(Box<crate::java::method::FieldData>),
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
        item,
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
        assigned: rustc_hash::FxHashSet::default(),
        implicit_its: Vec::new(),
    };
    for &param in &bodies.body(body).params {
        // A parameter always carries a value: it is a `val`, and the only
        // writes it accepts are none.
        ctx.assigned.insert(param);
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
    /// The declaration the body belongs to: the *call site* every member
    /// lookup is attributed to ([`method::CallSite`]).
    item: hir_expand::ids::ItemId,
    scope: hir::ResolutionScope,
    tree: &'a KotlinItemTree,
    bodies: &'a hir_expand::body::BodyTree,
    resolver: KotlinResolver<'a>,
    types: KotlinBodyTypes,
    narrowed: FxHashMap<LocalId, Ty>,
    /// The locals that already carry a value — a declared initializer, or a
    /// deferred initialization a `val` received — so that the *first* write to
    /// a `val` is its initialization and a second one a reassignment
    /// ([KLS
    /// `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)).
    assigned: rustc_hash::FxHashSet<LocalId>,
    /// The `it` of the parameter-less lambdas whose bodies are being inferred,
    /// innermost last ([KLS
    /// `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals)).
    implicit_its: Vec<Ty>,
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
                // A declared initializer means the local already carries a
                // value, so a later write to a `val` is a reassignment.
                if initializer.is_some() {
                    self.assigned.insert(local);
                }
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
                    self.assigned.insert(part);
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
                pattern,
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
                let element = element.unwrap_or_else(|| self.error());
                // A destructuring loop variable binds one local per component,
                // each the component type of the element ([KLS
                // `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)
                // resolves them through `componentN()`); without the member
                // bridge the components are the error type, which is what the
                // loop variable is bound to.
                self.assigned.insert(var);
                match pattern {
                    Some(pattern) => {
                        let parts: Vec<LocalId> = match self.bodies.pattern(pattern).clone() {
                            hir_expand::body::PatternData::Destructuring { parts } => parts,
                            _ => Vec::new(),
                        };
                        let components = match element.kind(self.db) {
                            TyKind::Reference { args, .. } => args.clone(),
                            _ => Vec::new(),
                        };
                        for (index, part) in parts.into_iter().enumerate() {
                            let ty = components
                                .get(index)
                                .copied()
                                .unwrap_or_else(|| self.error());
                            self.types.locals.insert(part, ty);
                            self.assigned.insert(part);
                        }
                    }
                    None => {
                        self.types.locals.insert(var, element);
                    }
                }
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
                    // A fully qualified classifier reference is a *type* in
                    // expression position — `java.util.ArrayList` of
                    // `java.util.ArrayList<String>(16)`, and the receiver of
                    // `java.lang.System.currentTimeMillis()`.
                    if let Some(path) = self.name_path(expr)
                        && let Some(fqn) = self.resolver.class_fqn(&path)
                    {
                        return Ty::reference(self.db, fqn, Vec::new());
                    }
                    let receiver = self.infer_expr(target);
                    self.member_ty(expr, &receiver, &name)
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
                // A lambda literal that declares no parameters takes them from
                // the *expected* function type ([KLS
                // `type-inference.html#function-literals`](https://kotlinlang.org/spec/type-inference.html#function-literals),
                // and its `it` is that type's single parameter). The candidate
                // is therefore selected *before* the lambda is inferred — the
                // lambda's own type is unknown until then — and the lambda's
                // body is inferred against the parameter type the candidate
                // declares.
                if let Some(index) = args
                    .iter()
                    .position(|arg| matches!(self.bodies.expr(*arg), ExprData::Lambda { params, .. } if params.is_empty()))
                {
                    return self.call_with_expected_lambda(expr, receiver.as_ref(), &name, &args, index);
                }
                let arg_types: Vec<Ty> = args.iter().map(|arg| self.infer_expr(*arg)).collect();
                match receiver {
                    Some(receiver) => {
                        // A fully qualified *constructor* call: the lowering
                        // folds the class's simple name into the member name and
                        // the rest of the path into the receiver, so
                        // `a.b.C(1)` is a call of `C` on `a.b`. The *path* is
                        // what resolves to a classifier, and the call is then a
                        // constructor invocation on it.
                        if let Some(path) = self.name_path(receiver)
                            && let Some(fqn) = self.resolver.class_fqn(&format!("{path}.{name}"))
                        {
                            let class = Ty::reference(self.db, fqn, Vec::new());
                            return self.constructor_ty(expr, &class, &name, &arg_types);
                        }
                        let receiver_ty = self.infer_expr(receiver);
                        self.call_ty(expr, &receiver_ty, &name, &arg_types)
                    }
                    None => self.call_without_receiver(expr, &name, &arg_types),
                }
            }
            ExprData::InfixCall {
                receiver,
                name,
                arg,
            } => {
                let receiver_ty = self.infer_expr(receiver);
                let arg_ty = self.infer_expr(arg);
                self.call_ty(expr, &receiver_ty, &name, &[arg_ty])
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
            ExprData::Assign { op, lhs, rhs } => {
                let rhs_ty = self.infer_expr(rhs);
                let range = self.bodies.expr_range(rhs);
                self.infer_assignment(op, lhs, rhs_ty, range);
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
                    // A top-level callable reference (`::helper`): the name is
                    // a declaration of this file or of an import.
                    None => {
                        if let Some((file, item)) = self.resolver.source_declaration(&name) {
                            self.types
                                .resolved
                                .insert(expr, KotlinResolvedMember::Kotlin { file, item });
                            return super::db::item_ty(self.db, file, item);
                        }
                        self.builtin("Any")
                    }
                };
                self.member_ty(expr, &receiver_ty, &name)
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
        // The implicit parameter of a parameter-less lambda
        // ([KLS `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals)):
        // `it` is the enclosing lambda's single parameter, bound while its body
        // is inferred.
        if name.as_str() == "it"
            && let Some(ty) = self.implicit_its.last().copied()
        {
            return ty;
        }
        if let Some((local, ty)) = self
            .types
            .locals
            .iter()
            .map(|(local, ty)| (*local, *ty))
            .find(|(local, _)| self.bodies.local(*local).name == *name)
        {
            self.types
                .resolved
                .insert(expr, KotlinResolvedMember::Local(local));
            return ty;
        }
        if let Some((_, narrowed)) = self
            .narrowed
            .iter()
            .find(|(local, _)| self.bodies.local(**local).name == *name)
        {
            return *narrowed;
        }
        let _ = expr;
        // A member of an enclosing classifier, without a written receiver —
        // the innermost first ([KLS
        // `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)).
        for receiver in self.implicit_receivers() {
            let members = method::member_set(self.db, &self.scope, &receiver, name, self.site());
            if let Some(member) = members.first() {
                let ty = member.ty(self.db);
                self.record_member(expr, member);
                return ty;
            }
        }
        // A top-level declaration of this file: a property or a function's
        // value form (a callable reference's target).
        if let Some(&top) = self.tree.top.iter().find(|&&id| {
            matches!(
                self.tree.data(id),
                KotlinItemData::Property(_) | KotlinItemData::Function(_)
            ) && self.tree.data(id).name() == Some(name)
        }) {
            self.types.resolved.insert(
                expr,
                KotlinResolvedMember::Kotlin {
                    file: self.file,
                    item: top,
                },
            );
            return super::db::item_ty(self.db, self.file, top);
        }
        // A *top-level* declaration another file declares: the file's own
        // package, an explicit import, or a star import ([KLS
        // `packages-and-imports.html#importing`](https://kotlinlang.org/spec/packages-and-imports.html#importing)).
        if let Some((file, item)) = self.resolver.source_declaration(name) {
            self.types
                .resolved
                .insert(expr, KotlinResolvedMember::Kotlin { file, item });
            return super::db::item_ty(self.db, file, item);
        }
        // A classifier used as a *receiver*: `Foo.bar()` types its `Foo` as
        // the class.
        if let Some(class) = self.class_receiver(name) {
            return class;
        }
        // An unresolved *simple* name: kotlinc's `unresolved reference`.
        let range = self.bodies.expr_range(expr);
        self.types
            .diagnostics
            .push(KotlinTypeError::UnresolvedReference {
                expr,
                name: name.clone(),
                range,
            });
        self.error()
    }

    /// Records the declaration a resolved *member* expression names
    /// ([`KotlinBodyTypes::resolved`]).
    fn record_member(&mut self, expr: ExprId, member: &method::Member) {
        let resolved = match &member.target {
            method::MemberTarget::Kotlin { file, item } => KotlinResolvedMember::Kotlin {
                file: *file,
                item: *item,
            },
            method::MemberTarget::Java(method) => {
                KotlinResolvedMember::Java(Box::new(method.as_ref().clone()))
            }
            method::MemberTarget::JavaField(field) => {
                KotlinResolvedMember::JavaField(Box::new(field.as_ref().clone()))
            }
        };
        self.types.resolved.insert(expr, resolved);
    }

    /// The call site every member lookup is attributed to.
    fn site(&self) -> method::CallSite {
        method::CallSite {
            file: self.file,
            item: self.item,
        }
    }

    /// The type of the member `name` on `receiver`: a property's type, the
    /// return type of a function the member set resolves, the field's type of a
    /// Java field — each in its *Kotlin* form, so a classfile member's type is
    /// the platform type it denotes ([`method::Member::ty`]).
    fn member_ty(&mut self, expr: ExprId, receiver: &Ty, name: &Name) -> Ty {
        let members = method::member_set(self.db, &self.scope, receiver, name, self.site());
        match members.first() {
            Some(member) => {
                let ty = member.ty(self.db);
                self.record_member(expr, member);
                ty
            }
            None => self.error(),
        }
    }

    /// The type of a call: the return type of the candidate the arguments
    /// select, or the error type when the receiver's members declare no such
    /// callable — a call kotlinc reports as `unresolved reference`.
    fn call_ty(&mut self, expr: ExprId, receiver: &Ty, name: &Name, arg_tys: &[Ty]) -> Ty {
        let args: Vec<CallArg<'_>> = arg_tys
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        match method::pick_callable(self.db, &self.scope, receiver, name, &args, self.site()) {
            Some(member) => {
                let ty = member.ty(self.db);
                self.record_member(expr, &member);
                // A constructor call's type is the class it constructs — the
                // receiver, with the arguments the call wrote — not the
                // constructor's own `void` return.
                if member.kind == method::MemberKind::Constructor {
                    receiver.clone()
                } else {
                    ty
                }
            }
            None => self.error(),
        }
    }

    /// A call with a *parameter-less lambda* argument: the candidate is
    /// selected from the written arguments alone — the lambda's type *is* the
    /// candidate's parameter at its index, which is why the lambda is inferred
    /// second — and the lambda's `it` is that function type's first parameter
    /// type.
    fn call_with_expected_lambda(
        &mut self,
        expr: ExprId,
        receiver: Option<&ExprId>,
        name: &Name,
        args: &[ExprId],
        index: usize,
    ) -> Ty {
        // Every argument but the lambda; the lambda's own type is the error
        // type here, which is applicable to whatever parameter the candidate
        // declares ([`crate::kotlin::subtyping`] absorbs it).
        let arg_types: Vec<Ty> = args
            .iter()
            .enumerate()
            .map(|(position, arg)| {
                if position == index {
                    self.error()
                } else {
                    self.infer_expr(*arg)
                }
            })
            .collect();
        let call_args: Vec<CallArg<'_>> = arg_types
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        let candidate = match receiver {
            Some(receiver) => {
                let receiver_ty = self.infer_expr(*receiver);
                method::pick_callable(
                    self.db,
                    &self.scope,
                    &receiver_ty,
                    name,
                    &call_args,
                    self.site(),
                )
            }
            // An unqualified call: a member of an enclosing classifier, a
            // top-level declaration of this file, then the ones an import
            // names — and only then the *class* the name denotes, whose
            // candidate set is its constructors (`Foo { … }`), because one name
            // may declare a function and a class at once.
            None => {
                let mut candidate = None;
                for receiver_ty in self.implicit_receivers() {
                    if let Some(member) = method::pick_callable(
                        self.db,
                        &self.scope,
                        &receiver_ty,
                        name,
                        &call_args,
                        self.site(),
                    ) {
                        candidate = Some(member);
                        break;
                    }
                }
                candidate
                    .or_else(|| {
                        method::top_level_callable(
                            self.db,
                            &self.scope,
                            self.file,
                            name,
                            &call_args,
                        )
                    })
                    .or_else(|| {
                        let class = self.class_receiver(name)?;
                        method::pick_callable(
                            self.db,
                            &self.scope,
                            &class,
                            name,
                            &call_args,
                            self.site(),
                        )
                    })
                    .or_else(|| {
                        let (file, item) = self.resolver.source_declaration(name)?;
                        method::declaration_callable(
                            self.db,
                            &self.scope,
                            file,
                            item,
                            name,
                            &call_args,
                        )
                    })
            }
        };
        let it_ty = candidate
            .as_ref()
            .and_then(|member| member.params.get(index).copied())
            .and_then(|param| self.function_parameter_ty(&param, 0));
        if let Some(it_ty) = it_ty {
            self.implicit_its.push(it_ty);
        }
        self.infer_expr(args[index]);
        if it_ty.is_some() {
            self.implicit_its.pop();
        }
        match candidate {
            Some(member) => {
                let ty = member.ty(self.db);
                self.record_member(expr, &member);
                ty
            }
            None => self.error(),
        }
    }

    /// [`Self::call_with_expected_lambda`] for a *constructor* call: the
    /// candidate set is the class's constructors.
    fn constructor_with_lambda(
        &mut self,
        expr: ExprId,
        class: &Ty,
        name: &Name,
        args: &[ExprId],
        index: usize,
    ) -> Ty {
        let arg_types: Vec<Ty> = args
            .iter()
            .enumerate()
            .map(|(position, arg)| {
                if position == index {
                    self.error()
                } else {
                    self.infer_expr(*arg)
                }
            })
            .collect();
        let call_args: Vec<CallArg<'_>> = arg_types
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        let candidate =
            method::pick_callable(self.db, &self.scope, class, name, &call_args, self.site());
        let it_ty = candidate
            .as_ref()
            .and_then(|member| member.params.get(index).copied())
            .and_then(|param| self.function_parameter_ty(&param, 0));
        if let Some(it_ty) = it_ty {
            self.implicit_its.push(it_ty);
        }
        self.infer_expr(args[index]);
        if it_ty.is_some() {
            self.implicit_its.pop();
        }
        if let Some(member) = candidate {
            self.record_member(expr, &member);
        }
        class.clone()
    }

    /// The type of the `index`th *parameter* of a function type: the classifier
    /// `kotlin.FunctionN` carries its parameters followed by its return type
    /// ([KLS
    /// `type-system.html#function-types`](https://kotlinlang.org/spec/type-system.html#function-types)).
    /// A platform type is unwrapped — a Java function type is the Kotlin one.
    fn function_parameter_ty(&self, ty: &Ty, index: usize) -> Option<Ty> {
        let ty = match ty.kind(self.db) {
            TyKind::Flexible { lower, .. } => *lower,
            _ => *ty,
        };
        let TyKind::Reference { name, args, .. } = ty.kind(self.db) else {
            return None;
        };
        let text = name.as_str();
        let arity = text.strip_prefix("kotlin.Function")?;
        if arity.parse::<usize>().is_err() {
            return None;
        }
        args.get(index).copied()
    }

    /// The type of a call written *without* a receiver ([KLS
    /// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)):
    ///
    /// * the name of a classifier is a *constructor* invocation — `Foo(1)` —
    ///   whose candidate set is the class's constructors;
    /// * otherwise the callee is a member of an enclosing classifier (innermost
    ///   first) or a top-level declaration of the file.
    fn call_without_receiver(&mut self, expr: ExprId, name: &Name, arg_tys: &[Ty]) -> Ty {
        let args: Vec<CallArg<'_>> = arg_tys
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        if let Some(class) = self.class_receiver(name) {
            return self.constructor_ty(expr, &class, name, &arg_tys);
        }
        for receiver in self.implicit_receivers() {
            if let Some(member) =
                method::pick_callable(self.db, &self.scope, &receiver, name, &args, self.site())
            {
                let ty = member.ty(self.db);
                self.record_member(expr, &member);
                return ty;
            }
        }
        if let Some(member) =
            method::top_level_callable(self.db, &self.scope, self.file, name, &args)
        {
            let ty = member.ty(self.db);
            self.record_member(expr, &member);
            return ty;
        }
        // An *imported* top-level function: the declaration is in another file,
        // and it is what the name resolves to.
        if let Some((file, item)) = self.resolver.source_declaration(name)
            && let Some(member) =
                method::declaration_callable(self.db, &self.scope, file, item, name, &args)
        {
            let ty = member.ty(self.db);
            self.record_member(expr, &member);
            return ty;
        }
        self.error()
    }

    /// The dotted name an expression writes, when it is a *name path*: a
    /// `Var` or a chain of field accesses on one — `java.util.ArrayList`. What
    /// a fully qualified classifier reference looks like in expression
    /// position.
    fn name_path(&self, expr: ExprId) -> Option<String> {
        match self.bodies.expr(expr).clone() {
            ExprData::Var(name) | ExprData::NamePath(name) => Some(name.to_string()),
            ExprData::FieldAccess {
                target: Some(target),
                name,
            } => {
                let mut path = self.name_path(target)?;
                path.push('.');
                path.push_str(name.as_str());
                Some(path)
            }
            _ => None,
        }
    }

    /// The type of a *constructor* call on the classifier type `class` under
    /// the class's own name: the member set of a class type under that name is
    /// its constructors, and the call's type is the class it constructs.
    fn constructor_ty(&mut self, expr: ExprId, class: &Ty, name: &Name, arg_tys: &[Ty]) -> Ty {
        let args: Vec<CallArg<'_>> = arg_tys
            .iter()
            .map(|ty| CallArg {
                name: None,
                ty: *ty,
            })
            .collect();
        match method::pick_callable(self.db, &self.scope, class, name, &args, self.site()) {
            Some(member) => {
                self.record_member(expr, &member);
                class.clone()
            }
            None => self.error(),
        }
    }

    /// The type of the *classifier* a name denotes, if it denotes one — what
    /// makes `Foo(1)` a constructor call and `Foo.bar()` a static access.
    fn class_receiver(&self, name: &Name) -> Option<Ty> {
        let fqn = self.resolver.class_fqn(name.as_str())?;
        Some(Ty::reference(self.db, fqn, Vec::new()))
    }

    /// The types of the *implicit* receivers of an unqualified name: the
    /// enclosing classifiers, innermost first ([KLS
    /// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)).
    fn implicit_receivers(&self) -> Vec<Ty> {
        let mut out = Vec::new();
        let mut current = Some(self.item);
        while let Some(id) = current {
            if self.tree.as_class(id).is_some()
                && let Some(fqn) = hir::source_class_fqn(self.db, self.file, id)
            {
                out.push(Ty::reference(self.db, fqn, Vec::new()));
            }
            current = self.tree.parent_of(id);
        }
        out
    }

    /// An assignment: the destination must be a `var`, and the value
    /// assignable to its type.
    fn infer_assignment(
        &mut self,
        op: hir_expand::body::AssignOp,
        lhs: ExprId,
        rhs_ty: Ty,
        range: Option<rowan::TextRange>,
    ) {
        let lhs_ty = self.infer_expr(lhs);
        // A *compound* assignment (`x += y`) is the `plusAssign` convention
        // ([KLS
        // `operator-overloading.html#augmented-assignments`](https://kotlinlang.org/spec/operator-overloading.html#augmented-assignments)):
        // `val list = mutableListOf(); list += x` calls `list.plusAssign(x)`,
        // so it is not a reassignment at all. Whether the operator exists is
        // the operator-lookup's question and is not checked yet — a recorded
        // gap, which kotlinc reports as `unresolved reference: plusAssign`.
        if op != hir_expand::body::AssignOp::Assign {
            return;
        }
        let name = match self.bodies.expr(lhs).clone() {
            ExprData::Var(name) => name,
            _ => return,
        };
        // A `val` — a read-only local — cannot be reassigned
        // ([KLS
        // `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)):
        // the binding's own mutability decides, which the lowering records for
        // every local ([`hir_expand::body::Local::is_mutable`]) — a parameter,
        // a loop variable and a pattern binding are `val`s too.
        if let Some(&local) = self
            .types
            .locals
            .keys()
            .find(|local| self.bodies.local(**local).name == name)
        {
            // A `val` declared without an initializer is *deferred
            // initialization*: `val x: T` followed by a single `x = …` is how
            // Kotlin gives a `val` its value on every path, and kotlinc accepts
            // it — only a *second* write is a reassignment.
            if !self.bodies.local(local).is_mutable && self.assigned.contains(&local) {
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
            self.assigned.insert(local);
            self.check_binding(MismatchTarget::Assignment, lhs_ty, rhs_ty, range);
            return;
        }
        // A *property* write: `x = v` or `x.y = v` on an enclosing receiver.
        // Whether the property has a setter is what the member set answers
        // ([`crate::kotlin::method::MemberKind::Setter`]).
        self.check_binding(MismatchTarget::Assignment, lhs_ty, rhs_ty, range);
    }
}
