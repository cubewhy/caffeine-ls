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
use hir_expand::span::SpannedTypeRef;

use super::diagnostics::{KotlinTypeError, MismatchTarget};
use super::method::{self, CallArg};
use super::resolve::KotlinResolver;
use crate::jvm::db::TyDatabase;
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
    Java(Box<crate::jvm::member::MethodData>),
    /// A Java source or classfile field.
    JavaField(Box<crate::jvm::member::FieldData>),
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

/// The receiver a call with a lambda argument is written on: the expression a
/// written receiver is — which the walk infers — a type a caller already has, or
/// no receiver at all (an unqualified call).
#[derive(Clone, Copy)]
enum CallReceiver<'a> {
    Expr(&'a ExprId),
    Type(Ty),
    Implicit,
}

/// The arguments of a call as the member set reads them: each argument's type
/// with the *name* it was written under, when it writes one
/// ([`hir_expand::body::ExprData::MethodCall`]).
fn call_args<'a>(
    types: &'a [Ty],
    names: &'a [Option<Name>],
    trailing: Option<usize>,
) -> Vec<CallArg<'a>> {
    types
        .iter()
        .enumerate()
        .map(|(index, ty)| CallArg {
            name: names
                .get(index)
                .and_then(|name| name.as_ref().map(Name::as_str)),
            ty: *ty,
            trailing: trailing == Some(index),
        })
        .collect()
}

/// Infers the body of the declaration `item` in `file`.
pub fn infer_item(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
) -> KotlinBodyTypes {
    infer(db, file, item, Inferred::Body)
}

/// Infers the *initializer expressions* of the declaration `item` — the `= expr`
/// a property writes or the `by expr` it delegates to
/// ([KLS
/// `declarations.html#property-declaration`](https://kotlinlang.org/spec/declarations.html#property-declaration)):
/// what a property without a written type is typed by.
pub fn infer_initializer(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
) -> KotlinBodyTypes {
    infer(db, file, item, Inferred::Initializer)
}

/// What an inference pass walks: a declaration's body, or the initializer
/// expressions a property declares in its place. A declaration has one or the
/// other, never both, so each entry point is asked for what exists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inferred {
    Body,
    Initializer,
}

fn infer(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
    inferred: Inferred,
) -> KotlinBodyTypes {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return KotlinBodyTypes::default();
    };
    let body = tree.data(item).body_id();
    if inferred == Inferred::Body && body.is_none() {
        return KotlinBodyTypes::default();
    }
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
            body,
            ..Default::default()
        },
        // The narrowed types of locals that an `is` test established, keyed by
        // the local — a smart cast ([KLS
        // `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)).
        narrowed: FxHashMap::default(),
        assigned: rustc_hash::FxHashSet::default(),
        scopes: Vec::new(),
        lambda_receivers: Vec::new(),
        expected_lambdas: Vec::new(),
    };
    // The parameters of the enclosing classifier's primary constructor are in
    // scope in every body of the class body ([KLS
    // `declarations.html#constructor-declaration-scopes`](https://kotlinlang.org/spec/declarations.html#constructor-declaration-scopes)),
    // and a local declaration sees the bindings of the body that declares it.
    ctx.seed_enclosing_parameters();
    ctx.seed_captures();
    ctx.seed_extension_receiver();
    match inferred {
        Inferred::Body => {
            ctx.infer_body(body.expect("a body-carrying declaration"));
        }
        Inferred::Initializer => {
            let KotlinItemData::Property(data) = tree.data(item) else {
                return ctx.types;
            };
            if let Some(expr) = data.initializer_expr {
                ctx.infer_expr(expr);
            }
            if let Some(expr) = data.delegate_expr {
                ctx.infer_expr(expr);
            }
        }
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
    /// The lexically visible bindings, innermost scope last: a body's parameters,
    /// a block's declarations, a `catch` clause's parameter, a loop variable and
    /// a lambda's parameters each introduce one, and a name resolves to the
    /// innermost declaration of it that is in scope
    /// ([KLS
    /// `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
    /// A scope is a list, not a map: two declarations of one name in one scope is
    /// an error kotlinc reports, and the *first* is the one a later read means.
    scopes: Vec<Vec<(Name, Binding)>>,
    /// The implicit receivers a lambda contributes, innermost last, *in addition
    /// to* the enclosing classifiers ([`Self::implicit_receivers`]).
    ///
    /// A lambda written where a `T.() -> R` is expected has `T` as its `this` —
    /// `apply { fill = … }` — and the two forms are the same type by the time the
    /// classfile has erased them (`T.() -> R` *is* `kotlin.Function1<T, R>`), so
    /// a parameter-less lambda's expected parameter is taken as a receiver as
    /// well as as its `it`. That is *permissive*: a member the receiver declares
    /// resolves inside `also { … }` too, where kotlinc reads `this` as the
    /// enclosing receiver — never a false `unresolved reference` for the
    /// receiver forms the model cannot tell apart.
    lambda_receivers: Vec<Ty>,
    /// The function type each lambda literal being inferred is inferred
    /// *against*, innermost last: what its `it` and its untyped parameters take
    /// their types from ([KLS
    /// `type-inference.html#function-literals`](https://kotlinlang.org/spec/type-inference.html#function-literals)).
    expected_lambdas: Vec<Ty>,
}

/// What a written name resolves to in the scope that declares it.
#[derive(Clone, Debug)]
enum Binding {
    /// A body local — a parameter, a declared local, a loop variable, a catch
    /// parameter, a pattern binding — which the result records by its id.
    Local(LocalId, Ty),
    /// A lambda's parameter, which is not a local of the file's arena: a
    /// [`hir_expand::body::LambdaParam`] carries a name and a type, not a
    /// binding. The `it` of a parameter-less lambda is one of these too.
    Parameter(Ty),
}

impl Binding {
    fn ty(&self) -> Ty {
        match self {
            Binding::Local(_, ty) | Binding::Parameter(ty) => *ty,
        }
    }
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

    /// Runs `f` in a new lexical scope: the declarations it adds are visible
    /// inside it and gone after — a block, a lambda body and a `catch` clause
    /// each introduce one.
    fn in_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.scopes.push(Vec::new());
        let result = f(self);
        self.scopes.pop();
        result
    }

    /// Declares `name` in the innermost scope, recording a body local in the
    /// result's local map as well.
    fn declare(&mut self, name: Name, binding: Binding) {
        if let Binding::Local(local, ty) = &binding {
            self.types.locals.insert(*local, *ty);
        }
        match self.scopes.last_mut() {
            Some(scope) => scope.push((name, binding)),
            // A declaration outside any scope — the walk always opens one for the
            // body it walks — is visible for the rest of the body.
            None => self.scopes.push(vec![(name, binding)]),
        }
    }

    /// The innermost visible binding of `name`, if any is in scope.
    fn binding(&self, name: &Name) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| {
                scope
                    .iter()
                    .rev()
                    .find(|(declared, _)| declared == name)
                    .map(|(_, binding)| binding)
            })
            .cloned()
    }

    /// The body local the innermost binding of `name` is, when it is one.
    fn local_binding(&self, name: &Name) -> Option<(LocalId, Ty)> {
        match self.binding(name)? {
            Binding::Local(local, ty) => Some((local, ty)),
            Binding::Parameter(_) => None,
        }
    }

    /// Seeds the bindings a *local* declaration captures: a local function's or
    /// a local class's body is a body of its own, and the bindings of the body
    /// that declares it are in scope there — a local class may capture the
    /// locals of the function it is declared in ([KLS
    /// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration),
    /// [`#local-function-declaration`](https://kotlinlang.org/spec/declarations.html#local-function-declaration)
    /// scope a local declaration to the body that declares it).
    ///
    /// The outer body's own inference is what knows its locals, and it is
    /// memoized, so the capture site asks for it — the one-way edge from an
    /// inner body to the outer one, exactly as the Java layer's `capture_site`.
    fn seed_captures(&mut self) {
        // The *outermost* local declaration of this item's ancestry is the one
        // whose owner declares it: an `init` block of an object literal is a
        // member of the literal, and it is the literal that the enclosing body
        // declares.
        let mut item = self.item;
        let owner = loop {
            if self.tree.is_local_type(item) {
                break self.tree.parent_of(item);
            }
            match self.tree.parent_of(item) {
                Some(parent) => item = parent,
                None => return,
            }
        };
        let Some(owner) = owner else {
            return;
        };
        let outer = super::db::declaration_types(self.db, self.file, owner);
        for (local, ty) in outer.locals.iter() {
            let name = self.bodies.local(*local).name.clone();
            self.declare(name, Binding::Local(*local, *ty));
        }
    }

    /// Seeds the *extension receiver* of the declaration being inferred: inside
    /// `fun Box.doubled()`, `this` is the box and its members are in scope
    /// unqualified ([KLS
    /// `expressions.html#this-expressions`](https://kotlinlang.org/spec/expressions.html#this-expressions)
    /// makes the receiver the implicit `this` of an extension body).
    fn seed_extension_receiver(&mut self) {
        let receiver = match self.tree.data(self.item) {
            KotlinItemData::Function(data) => data.receiver.as_ref(),
            KotlinItemData::Property(data) => data.receiver.as_ref(),
            _ => None,
        };
        let Some(receiver) = receiver else {
            return;
        };
        let ty = super::ty::ty_from_type_ref(self.db, &self.resolver, &receiver.ty);
        self.lambda_receivers.push(ty);
    }

    /// Seeds the parameters of the enclosing classifier's *primary* constructor:
    /// they are in scope in every initializer and `init` block of the class
    /// ("the primary constructor parameter scope is downward-linked to the
    /// classifier initialization scope", [KLS
    /// `declarations.html#constructor-declaration-scopes`](https://kotlinlang.org/spec/declarations.html#constructor-declaration-scopes)),
    /// which are bodies of their own and would otherwise not see them.
    fn seed_enclosing_parameters(&mut self) {
        let Some(item) = self.tree.parent_of(self.item) else {
            return;
        };
        let KotlinItemData::Class(class) = self.tree.data(item) else {
            return;
        };
        let Some(constructor) = class.primary_constructor else {
            return;
        };
        let KotlinItemData::Constructor(data) = self.tree.data(constructor) else {
            return;
        };
        for (param, local) in data.params.iter().zip(&data.param_locals) {
            let ty = super::ty::ty_from_type_ref(self.db, &self.resolver, &param.param.ty.ty);
            self.assigned.insert(*local);
            self.declare(param.param.name.clone(), Binding::Local(*local, ty));
        }
    }

    /// Infers a body: its parameters are the outermost scope, then the
    /// statements it declares.
    fn infer_body(&mut self, body: BodyId) {
        let params = self.bodies.body(body).params.clone();
        let stmts = self.bodies.body(body).stmts.clone();
        self.in_scope(|ctx| {
            for param in params {
                // A parameter always carries a value: it is a `val`, and the only
                // writes it accepts are none.
                ctx.assigned.insert(param);
                let ty = ctx
                    .bodies
                    .local(param)
                    .ty
                    .as_ref()
                    .map(|ty| super::ty::ty_from_type_ref(ctx.db, &ctx.resolver, &ty.ty))
                    .unwrap_or_else(|| Ty::error(ctx.db));
                ctx.declare(
                    ctx.bodies.local(param).name.clone(),
                    Binding::Local(param, ty),
                );
            }
            for stmt in stmts {
                ctx.infer_stmt(stmt);
            }
        });
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
                // In scope from its declaration to the end of the block that
                // declares it ([KLS
                // `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
                let name = self.bodies.local(local).name.clone();
                self.declare(name, Binding::Local(local, ty));
            }
            // `val x by d`: the local's type is what the *delegate* declares for
            // it, through the same rule a delegated property's is
            // (<https://kotlinlang.org/docs/delegated-properties.html>).
            StmtData::DeclDelegated { local, delegate } => {
                let delegate_ty = self.infer_expr(delegate);
                let owner = self.delegation_owner();
                let ty = super::db::delegated_value_ty(
                    self.db,
                    self.file,
                    self.item,
                    &self.resolver,
                    owner,
                    delegate_ty,
                );
                self.assigned.insert(local);
                let name = self.bodies.local(local).name.clone();
                self.declare(name, Binding::Local(local, ty));
            }
            StmtData::Destructuring {
                pattern,
                initializer,
            } => {
                // The pattern binds one local per component, each the `componentN`
                // the initializer's type declares
                // ([KLS `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)).
                let initializer_ty = self.infer_expr(initializer);
                let parts: Vec<LocalId> = match self.bodies.pattern(pattern).clone() {
                    hir_expand::body::PatternData::Destructuring { parts } => parts,
                    _ => Vec::new(),
                };
                let components = self.destructured_types(&initializer_ty, parts.len());
                for (index, part) in parts.into_iter().enumerate() {
                    let ty = components
                        .get(index)
                        .copied()
                        .unwrap_or_else(|| self.error());
                    self.assigned.insert(part);
                    let name = self.bodies.local(part).name.clone();
                    self.declare(name, Binding::Local(part, ty));
                }
            }
            StmtData::Expr(expr) => {
                self.infer_expr(expr);
            }
            // A block is a scope of its own ([KLS
            // `scopes-and-identifiers.html#scopes-and-identifiers`]): the
            // declarations it makes are gone when it ends, and a nested one may
            // shadow them.
            StmtData::Block(stmts) => {
                self.in_scope(|ctx| {
                    for stmt in stmts {
                        ctx.infer_stmt(stmt);
                    }
                });
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
                let element = self.iterable_element_ty(&iterable_ty);
                // The loop variable — and each binding of a destructuring one —
                // is scoped to the loop body.
                self.in_scope(|ctx| {
                    ctx.assigned.insert(var);
                    match pattern {
                        Some(pattern) => {
                            let parts: Vec<LocalId> = match ctx.bodies.pattern(pattern).clone() {
                                hir_expand::body::PatternData::Destructuring { parts } => parts,
                                _ => Vec::new(),
                            };
                            let components = ctx.destructured_types(&element, parts.len());
                            for (index, part) in parts.into_iter().enumerate() {
                                let ty = components
                                    .get(index)
                                    .copied()
                                    .unwrap_or_else(|| ctx.error());
                                ctx.assigned.insert(part);
                                let name = ctx.bodies.local(part).name.clone();
                                ctx.declare(name, Binding::Local(part, ty));
                            }
                        }
                        None => {
                            let name = ctx.bodies.local(var).name.clone();
                            ctx.declare(name, Binding::Local(var, element));
                        }
                    }
                    ctx.infer_stmt(body);
                });
            }
            // A local declaration is an item of the file's own tree, lowered
            // where it stands ([`StmtData::LocalClass`]); its body is inferred
            // when it is asked for, not while the declaring body is walked.
            StmtData::LocalClass { .. } | StmtData::LocalFunction { .. } => {}
            StmtData::Return(value) => {
                if let Some(value) = value {
                    self.infer_expr(value);
                }
            }
            StmtData::Missing => {}
            other => {
                // What is left is the Java statement forms — an `if` or `for`
                // *statement*, a `switch`, a `return`, a `throw`, a `break`, a
                // `synchronized`, a `try` with resources: Kotlin lowers every one
                // of its own constructs to an expression or an `ExprData` of its
                // own, and the walk stays total by ignoring these.
                let _ = other;
            }
        }
    }

    /// The type a `for (v in xs)` loop binds `v` to
    /// ([KLS
    /// `control--and-data-flow-analysis.html#for-loops`](https://kotlinlang.org/spec/control--and-data-flow-analysis.html#for-loops)
    /// resolves `iterator()` on the iterable and takes `next()`'s type): the
    /// `Iterator<T>` argument the convention returns when it resolves, else the
    /// iterable's own argument — a mapped collection's element — else an array's
    /// element.
    fn iterable_element_ty(&mut self, iterable: &Ty) -> Ty {
        let iterator = method::pick_operator_callable(
            self.db,
            &self.scope,
            iterable,
            &Name::new("iterator"),
            None,
            self.site(),
        )
        .map(|member| member.ty(self.db));
        if let Some(element) = iterator.as_ref().and_then(|ty| self.element_of(ty)) {
            return element;
        }
        self.element_of(iterable)
            .or_else(|| match iterable.kind(self.db) {
                TyKind::Array(inner) => Some(**inner),
                _ => None,
            })
            .unwrap_or_else(|| self.error())
    }

    /// The element type of a collection type: its first type argument
    /// (`Iterator<T>`, `Iterable<T>`, `List<T>`, `Set<T>`), or `None` when it
    /// takes none.
    fn element_of(&self, ty: &Ty) -> Option<Ty> {
        match ty.kind(self.db) {
            TyKind::Reference { args, .. } => args.first().copied(),
            TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => self.element_of(inner),
            _ => None,
        }
    }

    /// The types a destructuring pattern of `count` names binds, each the
    /// `componentN()` of the initializer's type with `N` its 1-based position
    /// ([KLS
    /// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)).
    /// A type whose components this model cannot resolve — a classfile
    /// `Pair`/`Triple`, a `Map.Entry`, whose `componentN` the classfile does not
    /// declare as a Kotlin member — falls back to its own type arguments, which
    /// for those three *are* the components.
    fn destructured_types(&mut self, initializer: &Ty, count: usize) -> Vec<Ty> {
        let fallback: Vec<Ty> = match initializer.kind(self.db) {
            TyKind::Reference { args, .. } => args.clone(),
            _ => Vec::new(),
        };
        (1..=count)
            .map(|index| {
                let component = method::pick_operator_callable(
                    self.db,
                    &self.scope,
                    initializer,
                    &Name::new(&format!("component{index}")),
                    None,
                    self.site(),
                )
                .map(|member| member.ty(self.db));
                component.unwrap_or_else(|| {
                    fallback
                        .get(index - 1)
                        .copied()
                        .unwrap_or_else(|| self.error())
                })
            })
            .collect()
    }

    /// The `thisRef` a `by` delegate receives at this site: the receiver the
    /// declaration site's `this` is — an enclosing classifier or a lambda's
    /// receiver — and `null` when there is none
    /// (<https://kotlinlang.org/docs/delegated-properties.html>).
    fn delegation_owner(&self) -> Ty {
        self.implicit_receivers()
            .first()
            .copied()
            .unwrap_or_else(|| Ty::null(self.db))
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
            ExprData::This { qualifier } => self.enclosing_receiver_ty(qualifier.as_ref(), false),
            ExprData::Super { qualifier } => self.enclosing_receiver_ty(qualifier.as_ref(), true),
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
                    match self.name_path(expr) {
                        Some(path) => match self.resolver.class_fqn(&path) {
                            Some(fqn) => Ty::reference(self.db, fqn, Vec::new()),
                            None => {
                                let receiver = self.infer_expr(target);
                                self.member_ty(expr, &receiver, &name)
                            }
                        },
                        None => {
                            let receiver = self.infer_expr(target);
                            self.member_ty(expr, &receiver, &name)
                        }
                    }
                }
                None => self.infer_name(expr, &name),
            },
            ExprData::SafeAccess { receiver, member } => {
                let receiver_ty = self.infer_expr(receiver);
                // The member is the access a non-null receiver makes — the
                // lowering writes it without a receiver of its own — and the
                // result is nullable *when the receiver is*
                // ([KLS `expressions.html#navigation-operators`](https://kotlinlang.org/spec/expressions.html#navigation-operators)):
                // `x?.port` of a non-null `x` is an `Int`, and kotlinc accepts
                // it where one is expected.
                let member_ty = self.safe_member_ty(member, &receiver_ty);
                match receiver_ty.is_nullable(self.db) {
                    true => Ty::nullable(self.db, member_ty),
                    false => member_ty,
                }
            }
            ExprData::NullAssert { expr: inner } => {
                let inner_ty = self.infer_expr(inner);
                inner_ty.strip_nullability(self.db)
            }
            ExprData::MethodCall {
                receiver,
                name,
                args,
                arg_names,
                trailing,
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
                match args
                    .iter()
                    .position(|arg| matches!(self.bodies.expr(*arg), ExprData::Lambda { params, .. } if params.is_empty()))
                {
                    Some(index) => self.call_with_expected_lambda(
                        expr,
                        match receiver.as_ref() {
                            Some(receiver) => CallReceiver::Expr(receiver),
                            None => CallReceiver::Implicit,
                        },
                        &name,
                        &args,
                        &arg_names,
                        trailing,
                        index,
                    ),
                    None => {
                        let arg_types: Vec<Ty> =
                            args.iter().map(|arg| self.infer_expr(*arg)).collect();
                        match receiver {
                            Some(receiver) => {
                                // A fully qualified *constructor* call: the lowering
                                // folds the class's simple name into the member name and
                                // the rest of the path into the receiver, so
                                // `a.b.C(1)` is a call of `C` on `a.b`. The *path* is
                                // what resolves to a classifier, and the call is then a
                                // constructor invocation on it.
                                let qualified = self
                                    .name_path(receiver)
                                    .and_then(|path| self.resolver.class_fqn(&format!("{path}.{name}")));
                                match qualified {
                                    Some(fqn) => {
                                        let class = Ty::reference(self.db, fqn, Vec::new());
                                        self.constructor_ty(
                                            expr,
                                            &class,
                                            &name,
                                            &arg_types,
                                            &arg_names,
                                            trailing,
                                        )
                                    }
                                    None => {
                                        let receiver_ty = self.infer_expr(receiver);
                                        self.call_ty(
                                            expr,
                                            &receiver_ty,
                                            &name,
                                            &arg_types,
                                            &arg_names,
                                            trailing,
                                        )
                                    }
                                }
                            }
                            None => self.call_without_receiver(
                                expr, &name, &arg_types, &arg_names, trailing,
                            ),
                        }
                    }
                }
            }
            ExprData::InfixCall {
                receiver,
                name,
                arg,
            } => {
                let receiver_ty = self.infer_expr(receiver);
                let arg_ty = self.infer_expr(arg);
                self.call_ty(expr, &receiver_ty, &name, &[arg_ty], &[], None)
            }
            ExprData::Binary { op, lhs, rhs } => {
                let lhs_ty = self.infer_expr(lhs);
                let rhs_ty = self.infer_expr(rhs);
                self.binary_ty(op, lhs_ty, rhs_ty)
            }
            ExprData::Unary { op, expr: inner } => {
                let inner_ty = self.infer_expr(inner);
                self.unary_ty(op, inner_ty)
            }
            ExprData::Postfix { op, expr: inner } => {
                let inner_ty = self.infer_expr(inner);
                // `x++` is `x = x.inc()`: the expression's own value is the
                // operand's type ([KLS
                // `operator-overloading.html#postfix-increments-and-decrements`](https://kotlinlang.org/spec/operator-overloading.html#postfix-increments-and-decrements)).
                let name = match op {
                    hir_expand::body::PostfixOp::Inc => "inc",
                    hir_expand::body::PostfixOp::Dec => "dec",
                };
                self.operator_call_ty(&inner_ty, name, None)
                    .unwrap_or(inner_ty)
            }
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
                let index_ty = self.infer_expr(index);
                // `a[i]` is `a.get(i)` ([KLS
                // `operator-overloading.html#indexed-access`](https://kotlinlang.org/spec/operator-overloading.html#indexed-access));
                // an array has no `get` in its classfile, so its element type is
                // the fallback.
                self.operator_call_ty(&array_ty, "get", Some(index_ty))
                    .or_else(|| self.element_of(&array_ty))
                    .unwrap_or_else(|| self.error())
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
                // A condition that tests a local narrows it in the branch the
                // test selects ([KLS
                // `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)),
                // and the narrowing is the branch's own.
                let (when_true, when_false) = self.branch_narrowings(cond);
                let then_ty = self.narrowed_branch(when_true, then);
                let els_ty = self.narrowed_branch(when_false, els);
                super::subtyping::lub(self.db, &self.scope, &then_ty, &els_ty)
            }
            ExprData::When { subject, arms } => self.infer_when(subject, &arms),
            ExprData::Try {
                body,
                catches,
                finally,
            } => {
                // A `try`'s value is the value of the block that ran ([KLS
                // `expressions.html#try-expressions`](https://kotlinlang.org/spec/expressions.html#try-expressions)):
                // the join of its body's and its `catch` blocks' types, with
                // `finally` contributing none.
                let mut value = Some(self.block_ty(body));
                for catch in catches {
                    // A catch parameter's type is the first of its declared
                    // types (Kotlin has no multi-catch), and it is scoped to its
                    // own block.
                    let ty = catch
                        .param_types
                        .first()
                        .map(|ty| super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty))
                        .unwrap_or_else(|| self.error());
                    let name = self.bodies.local(catch.param).name.clone();
                    let caught = self.in_scope(|ctx| {
                        ctx.declare(name, Binding::Local(catch.param, ty));
                        ctx.block_ty(catch.body)
                    });
                    value = Some(match value {
                        Some(previous) => {
                            super::subtyping::lub(self.db, &self.scope, &previous, &caught)
                        }
                        None => caught,
                    });
                }
                if let Some(finally) = finally {
                    self.infer_stmt(finally);
                }
                value.unwrap_or_else(|| self.builtin("Unit"))
            }
            ExprData::Block(stmt) => self.block_ty(stmt),
            ExprData::Lambda { params, body } => self.infer_lambda(&params, body),
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
            ExprData::Range {
                lhs,
                rhs,
                inclusive,
            } => {
                let lhs_ty = self.infer_expr(lhs);
                let rhs_ty = self.infer_expr(rhs);
                // `a..b` is `a.rangeTo(b)` ([KLS
                // `operator-overloading.html#ranges`](https://kotlinlang.org/spec/operator-overloading.html#ranges))
                // and `a..<b` its `rangeUntil`; the left operand's own type
                // remains the fallback for a range whose classifier the
                // classpath does not supply.
                let name = match inclusive {
                    true => "rangeTo",
                    false => "rangeUntil",
                };
                self.operator_call_ty(&lhs_ty, name, Some(rhs_ty))
                    .unwrap_or(lhs_ty)
            }
            ExprData::Spread { expr: inner } => self.infer_expr(inner),
            // `return` and `throw` have type `Nothing`
            // ([KLS `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
            ExprData::Jump { kind, value, label } => {
                if let Some(value) = value {
                    let actual = self.infer_expr(value);
                    // A `return`'s value answers the return type the enclosing
                    // declaration writes ([KLS
                    // `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
                    // A *labelled* one — `return@lambda x` — answers the target
                    // the label names, which may be a lambda rather than the
                    // enclosing declaration, so it is not checked (a recorded
                    // deviation).
                    if matches!(kind, JumpKind::Return) && label.is_none() {
                        self.check_return(actual, value);
                    }
                }
                self.builtin("Nothing")
            }
            ExprData::Paren(inner) => self.infer_expr(inner),
            // An object literal's type is the anonymous class its body declares
            // ([KLS
            // `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals)):
            // the item the lowering anchored the literal to.
            ExprData::ObjectLiteral { item } => super::db::item_ty(self.db, self.file, item),
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
            // The narrowing belongs to the arm: it is undone before the next
            // one, which tests something else ([KLS
            // `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)).
            let body_ty = self.narrowed_branch(narrowed, arm.body);
            result = Some(match result {
                Some(previous) => super::subtyping::lub(self.db, &self.scope, &previous, &body_ty),
                None => body_ty,
            });
        }
        result.unwrap_or_else(|| self.builtin("Unit"))
    }

    /// Infers `expr` with `narrowing` in force, and lifts it afterwards — the
    /// scope of a smart cast is the expression it was established for.
    fn narrowed_branch(&mut self, narrowing: Option<(LocalId, Ty)>, expr: ExprId) -> Ty {
        if let Some((local, ty)) = narrowing {
            self.narrowed.insert(local, ty);
            let inferred = self.infer_expr(expr);
            self.narrowed.remove(&local);
            return inferred;
        }
        self.infer_expr(expr)
    }

    /// The narrowing a *condition* establishes on each of its branches, as
    /// `(when true, when false)`:
    ///
    /// * `x is T` narrows `x` to `T` where the test holds, and `x !is T` — which
    ///   the lowering spells `!(x is T)` — where it does not
    ///   ([KLS
    ///   `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts));
    /// * `x == null` and `x != null` narrow to the non-null half on the branch
    ///   that implies it.
    ///
    /// A narrowing applies only to a *local*, and only when the narrowed type is
    /// assignable to the type the local already has — a test that could not hold
    /// for its declared type establishes nothing.
    fn branch_narrowings(&self, cond: ExprId) -> (Option<(LocalId, Ty)>, Option<(LocalId, Ty)>) {
        match self.bodies.expr(cond).clone() {
            ExprData::InstanceOf {
                expr, ty: Some(ty), ..
            } => {
                let Some((local, declared)) = self.tested_local(expr) else {
                    return (None, None);
                };
                let narrowed = super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty);
                if !super::subtyping::is_assignable(self.db, &self.scope, &narrowed, &declared) {
                    return (None, None);
                }
                (Some((local, narrowed)), None)
            }
            // `x !is T` and `!(x is T)`: the same narrowing on the *other*
            // branch.
            ExprData::Unary {
                op: hir_expand::body::UnaryOp::Not,
                expr: inner,
            } => match self.branch_narrowings(inner) {
                (Some((local, ty)), None) => (None, Some((local, ty))),
                (None, Some((local, ty))) => (Some((local, ty)), None),
                other => other,
            },
            ExprData::Binary { op, lhs, rhs } => {
                let (tested, other) = match self.bodies.expr(rhs) {
                    ExprData::Null => (lhs, rhs),
                    _ => match self.bodies.expr(lhs) {
                        ExprData::Null => (rhs, lhs),
                        _ => return (None, None),
                    },
                };
                let _ = other;
                let negated = match op {
                    hir_expand::body::BinaryOp::Ne => true,
                    hir_expand::body::BinaryOp::Eq => false,
                    _ => return (None, None),
                };
                let Some((local, declared)) = self.tested_local(tested) else {
                    return (None, None);
                };
                let narrowed = declared.strip_nullability(self.db);
                if narrowed == declared {
                    return (None, None);
                }
                match negated {
                    true => (Some((local, narrowed)), None),
                    false => (None, Some((local, narrowed))),
                }
            }
            _ => (None, None),
        }
    }

    /// The local a condition *tests*, when it tests one, with the type it has.
    fn tested_local(&self, expr: ExprId) -> Option<(LocalId, Ty)> {
        let ExprData::Var(name) = self.bodies.expr(expr).clone() else {
            return None;
        };
        self.local_binding(&name)
    }

    /// The types a *lambda* literal has ([KLS
    /// `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals)):
    /// its parameters take the types of the function type it is inferred
    /// *against* — a written type wins — and `it` is the parameter-less form's
    /// single parameter. Its type is the function type of its parameters and its
    /// result.
    fn infer_lambda(
        &mut self,
        params: &[hir_expand::body::LambdaParam],
        body: hir_expand::body::LambdaBody,
    ) -> Ty {
        let expected = self.expected_lambdas.last().copied();
        // What the position the lambda stands in declares: a function type's
        // parameters and the receiver that type gives it
        // ([`Self::lambda_receivers`]), or — where a *Java* functional interface
        // is expected — the single abstract method's parameters
        // (<https://kotlinlang.org/docs/java-interop.html#sam-conversions>,
        // which KLS does not cover: `listFiles { it }`'s `it` is the
        // `FilenameFilter`'s parameter).
        let (declared_params, receiver) = match expected {
            Some(expected) => match self.function_arity(&expected) {
                Some(arity) => (
                    // A classfile writes a function type's parameters as
                    // projections — `Function1<? super T, Unit>` — so the type
                    // each parameter *is* is what the projection bounds.
                    (0..arity)
                        .filter_map(|index| self.function_parameter_ty(&expected, index))
                        .map(|ty| self.decapture(&ty))
                        .collect::<Vec<_>>(),
                    self.function_parameter_ty(&expected, 0)
                        .map(|ty| self.decapture(&ty)),
                ),
                None => match crate::jvm::member_set::single_abstract_method(
                    self.db,
                    &self.scope,
                    &expected,
                ) {
                    // A functional interface's parameters are *parameters*: `it`
                    // is its single one, and `this` stays the enclosing receiver.
                    Some(sam) => (
                        sam.params
                            .iter()
                            .map(|param| self.decapture(&super::ty::ty_from_java(self.db, *param)))
                            .collect(),
                        None,
                    ),
                    None => (Vec::new(), None),
                },
            },
            None => (Vec::new(), None),
        };
        let param_tys: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                param
                    .ty
                    .as_ref()
                    .map(|ty| super::ty::ty_from_type_ref(self.db, &self.resolver, &ty.ty))
                    .or_else(|| declared_params.get(index).copied())
                    .unwrap_or_else(|| self.error())
            })
            .collect();
        // `it` is the parameter-less lambda's single parameter, and the same
        // type is the *receiver* a `T.() -> R` position gives the lambda — the
        // two are one type once the classfile has erased them
        // ([`Self::lambda_receivers`]).
        let parameter = receiver;
        let it = params
            .is_empty()
            .then(|| declared_params.first().copied())
            .flatten();
        if let Some(receiver) = parameter {
            self.lambda_receivers.push(receiver);
        }
        let ret = self.in_scope(|ctx| {
            for (param, ty) in params.iter().zip(&param_tys) {
                ctx.declare(param.name.clone(), Binding::Parameter(*ty));
                // A destructuring parameter binds one name per `componentN` of
                // the parameter the function type declares ([KLS
                // `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)).
                if !param.destructured.is_empty() {
                    let components = ctx.destructured_types(ty, param.destructured.len());
                    for (name, component) in param.destructured.iter().zip(components) {
                        ctx.declare(name.clone(), Binding::Parameter(component));
                    }
                }
            }
            if let Some(it) = it {
                ctx.declare(Name::new("it"), Binding::Parameter(it));
            }
            match body {
                hir_expand::body::LambdaBody::Expr(expr) => ctx.infer_expr(expr),
                hir_expand::body::LambdaBody::Block(stmt) => ctx.block_ty(stmt),
            }
        });
        if parameter.is_some() {
            self.lambda_receivers.pop();
        }
        // The lambda's own type: the parameters it *declares*, or — for the
        // parameter-less form the position declares a function type for — that
        // type's arity, because `T.() -> R` and `(T) -> R` are one type here
        // ([`Self::lambda_receivers`]) and the call's own type arguments are
        // bound from what this type is.
        let mut args = match params.is_empty() {
            true => declared_params,
            false => param_tys,
        };
        args.push(ret);
        let function = self.builtin(&format!("Function{}", args.len().saturating_sub(1)));
        match function.kind(self.db) {
            TyKind::Reference { name, .. } => Ty::reference(self.db, name.clone(), args),
            _ => self.error(),
        }
    }

    /// The result type of a *binary* operator over the operand types it is
    /// written with: Kotlin's built-in rules first — they win over any
    /// declaration ([KLS
    /// `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)) —
    /// then the operator's own convention ([`super::operator`]).
    fn binary_ty(&mut self, op: hir_expand::body::BinaryOp, lhs: Ty, rhs: Ty) -> Ty {
        if let Some(ty) = super::operator::builtin_binary_ty(self.db, op, &lhs, &rhs) {
            return ty;
        }
        let Some(name) = super::operator::binary_convention(op) else {
            return self.error();
        };
        // A comparison's convention answers the comparison's *sign*, and the
        // operator's own type is a `Boolean` whatever the convention returns.
        let compared = matches!(
            op,
            hir_expand::body::BinaryOp::Lt
                | hir_expand::body::BinaryOp::Gt
                | hir_expand::body::BinaryOp::Le
                | hir_expand::body::BinaryOp::Ge
        );
        match self.operator_call_ty(&lhs, name, Some(rhs)) {
            Some(ty) => match compared {
                true => self.builtin("Boolean"),
                false => ty,
            },
            None => self.error(),
        }
    }

    /// The result type of a *unary* operator over its operand
    /// ([KLS
    /// `operator-overloading.html#unary-operations`](https://kotlinlang.org/spec/operator-overloading.html#unary-operations)):
    /// `!` is `Boolean` negation, which no declaration takes part in, and the
    /// others are their convention — with a built-in numeric operand the operand
    /// itself, which is what kotlinc types `-1` as.
    fn unary_ty(&mut self, op: hir_expand::body::UnaryOp, operand: Ty) -> Ty {
        if matches!(op, hir_expand::body::UnaryOp::Not) {
            return self.builtin("Boolean");
        }
        if let TyKind::Reference { name, .. } = operand.kind(self.db)
            && super::operator::builtin_binary_ty(
                self.db,
                hir_expand::body::BinaryOp::Add,
                &operand,
                &Ty::reference(self.db, "kotlin.Int", Vec::new()),
            )
            .is_some()
            && name.as_str() != "kotlin.String"
        {
            return operand;
        }
        let Some(name) = super::operator::unary_convention(op) else {
            return operand;
        };
        self.operator_call_ty(&operand, name, None)
            .unwrap_or(operand)
    }

    /// The result type of an *operator convention* on `receiver`, resolved like
    /// any other call of the convention's name
    /// ([`method::pick_operator_callable`]).
    fn operator_call_ty(&self, receiver: &Ty, name: &str, arg: Option<Ty>) -> Option<Ty> {
        let member = method::pick_operator_callable(
            self.db,
            &self.scope,
            receiver,
            &Name::new(name),
            arg,
            self.site(),
        )?;
        match arg {
            Some(arg) => Some(member.call_ty(self.db, &[arg])),
            None => Some(member.ty(self.db)),
        }
    }

    /// Checks a `return`'s value against the return type the enclosing
    /// declaration writes ([KLS
    /// `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
    /// A declaration that writes none has nothing to check against — its own
    /// type is what the body inference produces ([`super::db`]).
    fn check_return(&mut self, actual: Ty, value: ExprId) {
        let declared = match self.tree.data(self.item) {
            KotlinItemData::Function(data) => data.ret.as_ref(),
            KotlinItemData::Property(data) => data.ty.as_ref(),
            KotlinItemData::Accessor(_) => None,
            _ => None,
        };
        let Some(declared) = declared else {
            return;
        };
        let expected = super::ty::ty_from_type_ref(self.db, &self.resolver, &declared.ty);
        let range = self.bodies.expr_range(value);
        self.check_binding(MismatchTarget::Return, expected, actual, range);
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
            {
                let (local, _) = self.tested_local(*expr)?;
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
        // The lexically visible bindings, innermost scope first: a local, a
        // parameter, a lambda's parameter, its `it`
        // ([KLS
        // `scopes-and-identifiers.html#scopes-and-identifiers`](https://kotlinlang.org/spec/scopes-and-identifiers.html#scopes-and-identifiers)).
        if let Some(binding) = self.binding(name) {
            if let Binding::Local(local, ty) = binding {
                // A smart cast narrows the local the test established it for
                // ([KLS
                // `type-inference.html#smart-casts`](https://kotlinlang.org/spec/type-inference.html#smart-casts)).
                if let Some(narrowed) = self.narrowed.get(&local) {
                    return *narrowed;
                }
                self.types
                    .resolved
                    .insert(expr, KotlinResolvedMember::Local(local));
                return ty;
            }
            return binding.ty();
        }
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
        // A *library* top-level property — a facade's static field or static
        // getter — which a Kotlin file reads by name.
        if let Some(member) =
            method::library_top_level_callable(self.db, &self.scope, self.file, name, &[])
            && matches!(
                member.kind,
                method::MemberKind::Getter | method::MemberKind::Property
            )
        {
            let ty = member.ty(self.db);
            self.record_member(expr, &member);
            return ty;
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

    /// The type of the member a *safe access* performs, resolved on the
    /// receiver the access guards: the member expression the lowering writes for
    /// `x?.m` carries no receiver of its own, so the guard's receiver is what it
    /// resolves on.
    fn safe_member_ty(&mut self, member: ExprId, receiver: &Ty) -> Ty {
        let ty = match self.bodies.expr(member).clone() {
            ExprData::FieldAccess { name, .. } => self.member_ty(member, receiver, &name),
            ExprData::MethodCall {
                name,
                args,
                arg_names,
                trailing,
                ..
            } => {
                // A lambda argument's expected type is the candidate's parameter,
                // exactly as for a call with a written receiver: `p?.let { it }`
                // binds `it` from `let`'s function type.
                match args.iter().position(|arg| {
                    matches!(self.bodies.expr(*arg), ExprData::Lambda { params, .. } if params.is_empty())
                }) {
                    Some(index) => self.expected_lambda_call(
                        member,
                        CallReceiver::Type(*receiver),
                        &name,
                        &args,
                        &arg_names,
                        trailing,
                        index,
                    ),
                    None => {
                        let arg_tys: Vec<Ty> =
                            args.iter().map(|arg| self.infer_expr(*arg)).collect();
                        self.call_ty(member, receiver, &name, &arg_tys, &arg_names, trailing)
                    }
                }
            }
            _ => self.infer_expr(member),
        };
        self.types.exprs.insert(member, ty);
        ty
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
    fn call_ty(
        &mut self,
        expr: ExprId,
        receiver: &Ty,
        name: &Name,
        arg_tys: &[Ty],
        arg_names: &[Option<Name>],
        trailing: Option<usize>,
    ) -> Ty {
        let args = call_args(arg_tys, arg_names, trailing);
        match method::pick_callable(self.db, &self.scope, receiver, name, &args, self.site()) {
            Some(member) => {
                let ty = member.call_ty(self.db, arg_tys);
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
        receiver: CallReceiver<'_>,
        name: &Name,
        args: &[ExprId],
        arg_names: &[Option<Name>],
        trailing: Option<usize>,
        index: usize,
    ) -> Ty {
        self.expected_lambda_call(expr, receiver, name, args, arg_names, trailing, index)
    }

    /// [`Self::call_with_expected_lambda`], whose receiver is either the
    /// expression a written receiver is — which this infers — or a type the
    /// caller already has (a safe access infers its own receiver).
    fn expected_lambda_call(
        &mut self,
        expr: ExprId,
        receiver: CallReceiver<'_>,
        name: &Name,
        args: &[ExprId],
        arg_names: &[Option<Name>],
        trailing: Option<usize>,
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
        let call_args = call_args(&arg_types, arg_names, trailing);
        let candidate = match receiver {
            CallReceiver::Expr(receiver) => {
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
            CallReceiver::Type(receiver_ty) => method::pick_callable(
                self.db,
                &self.scope,
                &receiver_ty,
                name,
                &call_args,
                self.site(),
            ),
            // An unqualified call: a member of an enclosing classifier, a
            // top-level declaration of this file, then the ones an import
            // names — and only then the *class* the name denotes, whose
            // candidate set is its constructors (`Foo { … }`), because one name
            // may declare a function and a class at once.
            CallReceiver::Implicit => {
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
                        // A library's top-level declaration: the same shape, from
                        // the classpath's facades.
                        method::library_top_level_callable(
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
        // The lambda is inferred *against* the function type the candidate
        // declares at that index: it is what the lambda's parameters and its
        // `it` take their types from ([`Self::expected_lambdas`]).
        // The parameter the lambda lands on: its argument position, or the
        // *last* parameter's when it is the call's trailing lambda
        // (<https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas>).
        let expected = candidate
            .as_ref()
            .and_then(|member| {
                let position = match trailing == Some(index) {
                    true => member.params.len().checked_sub(1).unwrap_or(index),
                    false => index,
                };
                member.params.get(position).copied()
            })
            .map(|param| self.unwrap_flexible(&param));
        if let Some(expected) = expected {
            self.expected_lambdas.push(expected);
        }
        self.infer_expr(args[index]);
        if expected.is_some() {
            self.expected_lambdas.pop();
        }
        // The lambda is inferred *against* the parameter's function type, so its
        // own type is what that position determines — `lazy { 1 }` is a
        // `Lazy<Int>`, not a `Lazy<T>`.
        let mut arg_types = arg_types;
        if let Some(lambda_ty) = self.types.exprs.get(&args[index]).copied() {
            arg_types[index] = lambda_ty;
        }
        match candidate {
            Some(member) => {
                let ty = member.call_ty(self.db, &arg_types);
                self.record_member(expr, &member);
                ty
            }
            None => self.error(),
        }
    }

    /// The arity of a function type: the `N` of `kotlin.FunctionN` (or its
    /// classfile spelling, `kotlin.jvm.functions.FunctionN`), or `None` for a
    /// type that is not one.
    fn function_arity(&self, ty: &Ty) -> Option<usize> {
        let ty = match ty.kind(self.db) {
            TyKind::Flexible { lower, .. } => *lower,
            _ => *ty,
        };
        let TyKind::Reference { name, .. } = ty.kind(self.db) else {
            return None;
        };
        let text = name.as_str();
        let text = text
            .strip_prefix("kotlin.Function")
            .or_else(|| text.strip_prefix("kotlin.jvm.functions.Function"))?;
        text.parse().ok()
    }

    /// The *captured* form of a Java type: a wildcard argument is the type it
    /// bounds, since a use-site projection's own type is that of its bound
    /// ([JLS §4.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.5.1)).
    /// A classfile writes `Function1<? super T, ? extends R>` for `(T) -> R`, so
    /// the type argument a lambda's parameter and receiver come from is what the
    /// projection bounds.
    fn decapture(&self, ty: &Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::Wildcard(Some(bound)) => bound.ty,
            TyKind::Nullable(inner) => Ty::nullable(self.db, self.decapture(&inner)),
            _ => *ty,
        }
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
        // `kotlin.FunctionN` and the classfile's own spelling of it,
        // `kotlin.jvm.functions.FunctionN`, are the same classifier
        // ([`super::ty::mapped_type_name`]); a *declared* name is read as
        // written.
        let text = name.as_str();
        let text = text
            .strip_prefix("kotlin.Function")
            .or_else(|| text.strip_prefix("kotlin.jvm.functions.Function"))?;
        let arity: usize = text.parse().ok()?;
        // A `kotlin.FunctionN` carries `N` parameters followed by its result, so
        // an index *within* the arity is a parameter: `Function0<R>`'s single
        // argument is its result, and a lambda of it declares no parameter.
        (index < arity).then(|| args.get(index).copied()).flatten()
    }

    /// A platform type reduced to the type it is a flexible pair of, for the
    /// positions that need one type — a lambda's expected function type, which a
    /// Java member declares as a classfile signature.
    fn unwrap_flexible(&self, ty: &Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::Flexible { lower, .. } => *lower,
            _ => *ty,
        }
    }

    /// The type of a call written *without* a receiver ([KLS
    /// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)):
    ///
    /// * the name of a classifier is a *constructor* invocation — `Foo(1)` —
    ///   whose candidate set is the class's constructors;
    /// * otherwise the callee is a member of an enclosing classifier (innermost
    ///   first) or a top-level declaration of the file.
    fn call_without_receiver(
        &mut self,
        expr: ExprId,
        name: &Name,
        arg_tys: &[Ty],
        arg_names: &[Option<Name>],
        trailing: Option<usize>,
    ) -> Ty {
        let args = call_args(arg_tys, arg_names, trailing);
        if let Some(class) = self.class_receiver(name) {
            return self.constructor_ty(expr, &class, name, &arg_tys, arg_names, trailing);
        }
        for receiver in self.implicit_receivers() {
            if let Some(member) =
                method::pick_callable(self.db, &self.scope, &receiver, name, &args, self.site())
            {
                let ty = member.call_ty(self.db, arg_tys);
                self.record_member(expr, &member);
                return ty;
            }
        }
        if let Some(member) =
            method::top_level_callable(self.db, &self.scope, self.file, name, &args)
        {
            let ty = member.call_ty(self.db, arg_tys);
            self.record_member(expr, &member);
            return ty;
        }
        // A *library* top-level function: the file's facades of every package in
        // scope ([KLS
        // `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)
        // resolves the name against the top-level declarations in scope, and a
        // library's are the `<File>Kt` classes of the classpath).
        if let Some(member) =
            method::library_top_level_callable(self.db, &self.scope, self.file, name, &args)
        {
            let ty = member.call_ty(self.db, arg_tys);
            self.record_member(expr, &member);
            return ty;
        }
        // An *imported* top-level function: the declaration is in another file,
        // and it is what the name resolves to.
        if let Some((file, item)) = self.resolver.source_declaration(name)
            && let Some(member) =
                method::declaration_callable(self.db, &self.scope, file, item, name, &args)
        {
            let ty = member.call_ty(self.db, arg_tys);
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
    fn constructor_ty(
        &mut self,
        expr: ExprId,
        class: &Ty,
        name: &Name,
        arg_tys: &[Ty],
        arg_names: &[Option<Name>],
        trailing: Option<usize>,
    ) -> Ty {
        let args = call_args(arg_tys, arg_names, trailing);
        match method::pick_callable(self.db, &self.scope, class, name, &args, self.site()) {
            Some(member) => {
                self.record_member(expr, &member);
                class.clone()
            }
            None => self.error(),
        }
    }

    /// The type of the *classifier* a name denotes, if it denotes one — what
    /// makes `Foo(1)` a constructor call and `Foo.bar()` a static access. A
    /// local classifier is answered by its declaration, a named one by its
    /// canonical name.
    fn class_receiver(&self, name: &Name) -> Option<Ty> {
        if let Some(local) = self
            .resolver
            .local_class_reference(name.as_str(), Vec::new())
        {
            return Some(local);
        }
        let fqn = self.resolver.class_fqn(name.as_str())?;
        Some(Ty::reference(self.db, fqn, Vec::new()))
    }

    /// The type of the receiver `this` denotes inside the declaration whose body
    /// is being inferred — the innermost enclosing classifier, or the one a
    /// written qualifier names (`this@Outer`)
    /// ([KLS `expressions.html#this-expressions`](https://kotlinlang.org/spec/expressions.html#this-expressions)).
    /// `super` is the same classifier's supertype list's first entry, or the one
    /// a `super<Base>` qualifier names
    /// ([`#super-forms`](https://kotlinlang.org/spec/expressions.html#super-forms));
    /// an unmatched qualifier falls back to the innermost `this` and the first
    /// `super`.
    ///
    /// `this` inside an *extension* body — where the receiver is the extended
    /// type and not a classifier of this file — is not resolved (a recorded
    /// deviation).
    fn enclosing_receiver_ty(&self, qualifier: Option<&SpannedTypeRef>, super_: bool) -> Ty {
        let written = qualifier.and_then(|qualifier| qualifier.ty.as_reference_name().cloned());
        // The enclosing classifiers, innermost first.
        let mut classifiers = Vec::new();
        let mut current = Some(self.item);
        while let Some(id) = current {
            if self.tree.as_class(id).is_some() {
                classifiers.push(id);
            }
            current = self.tree.parent_of(id);
        }
        let Some(&innermost) = classifiers.first() else {
            // A `this` whose *enclosing* declaration is not a classifier can
            // still be a lambda's receiver — the permissive `T.() -> R` reading
            // ([`Self::lambda_receivers`]).
            return match (written.is_none() && !super_, self.lambda_receivers.last()) {
                (true, Some(receiver)) => *receiver,
                _ => self.error(),
            };
        };
        if !super_
            && written.is_none()
            && let Some(receiver) = self.lambda_receivers.last()
        {
            return *receiver;
        }
        if !super_ {
            let chosen = written
                .as_ref()
                .and_then(|written| {
                    classifiers
                        .iter()
                        .copied()
                        .find(|&id| self.tree.data(id).name() == Some(written))
                })
                .unwrap_or(innermost);
            return super::db::item_ty(self.db, self.file, chosen);
        }
        let ty = super::db::item_ty(self.db, self.file, innermost);
        let supertypes = super::subtyping::supertypes(self.db, &self.scope, &ty);
        let chosen = match &written {
            Some(written) => supertypes.iter().find(|supertype| {
                matches!(supertype.kind(self.db), TyKind::Reference { name, .. } if name.simple_name() == written.as_str())
            }),
            None => supertypes.first(),
        };
        chosen.copied().unwrap_or_else(|| self.error())
    }

    /// The types of the *implicit* receivers of an unqualified name: the
    /// enclosing classifiers, innermost first ([KLS
    /// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)).
    ///
    /// Each is the type of the classifier's own declaration, so an object
    /// literal's members are in scope inside its body exactly as a named class's
    /// are.
    fn implicit_receivers(&self) -> Vec<Ty> {
        // A lambda's receiver is the innermost one inside its body
        // ([`Self::lambda_receivers`]); the stack holds them innermost last.
        let mut out: Vec<Ty> = self.lambda_receivers.iter().rev().copied().collect();
        let mut current = Some(self.item);
        while let Some(id) = current {
            if self.tree.as_class(id).is_some() {
                out.push(super::db::item_ty(self.db, self.file, id));
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
        // A *compound* assignment (`x += y`) is the `plusAssign` convention
        // ([KLS
        // `operator-overloading.html#augmented-assignments`](https://kotlinlang.org/spec/operator-overloading.html#augmented-assignments)):
        // when the destination's type declares one, the write *is* that call and
        // neither the `val` rule nor the binding check applies. When it declares
        // none, kotlinc falls back to `x = x + y`, which is what the checks
        // below are.
        if let Some(convention) = super::operator::assign_convention(op)
            && let Some(name) = super::operator::binary_convention(match op {
                hir_expand::body::AssignOp::Add => hir_expand::body::BinaryOp::Add,
                hir_expand::body::AssignOp::Sub => hir_expand::body::BinaryOp::Sub,
                hir_expand::body::AssignOp::Mul => hir_expand::body::BinaryOp::Mul,
                hir_expand::body::AssignOp::Div => hir_expand::body::BinaryOp::Div,
                hir_expand::body::AssignOp::Rem => hir_expand::body::BinaryOp::Rem,
                _ => hir_expand::body::BinaryOp::Add,
            })
            && self.operator_call_ty(&lhs_ty, name, Some(rhs_ty)).is_some()
            && self
                .operator_call_ty(&lhs_ty, convention, Some(rhs_ty))
                .is_some()
        {
            let _ = &lhs_ty;
            return;
        }
        // A `val` — a read-only local — cannot be reassigned
        // ([KLS
        // `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)):
        // the binding's own mutability decides, which the lowering records for
        // every local ([`hir_expand::body::Local::is_mutable`]) — a parameter,
        // a loop variable and a pattern binding are `val`s too.
        if let Some((local, _)) = self.local_binding(&name) {
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
