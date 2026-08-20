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
        BinaryOp, BodyId, BodyTree, ExprData, ExprId, LambdaBody, Literal, LocalId, StmtData,
        StmtId, UnaryOp,
    },
    item_tree::ItemId,
    name::Name,
};
use rustc_hash::FxHashMap;
use syntax::stub::{PrimitiveType, TypeRef};
use vfs::FileId;

use crate::{
    db::{TyDatabase, enclosing_class_query, type_params_map_query},
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
    let body_id = match item_data(&tree, item)? {
        hir_expand::item_tree::ItemData::Method(method) => method.body,
        hir_expand::item_tree::ItemData::StaticInit(init) => init.body,
        hir_expand::item_tree::ItemData::InstanceInit(init) => init.body,
        _ => None,
    }?;
    let scope = scope_for_file(db, file);
    let type_params = type_params_map_query(db, db.file_text(file));
    let resolver = Resolver::new(&tree, type_params, item);
    let access = access_context(db, file, item);
    let enclosing_class = enclosing_class_query(db, db.file_text(file))
        .get(&item)
        .map(|name| Ty::reference(db, name.as_str(), Vec::new()));
    // The return type of the enclosing method — the target type of a return
    // statement ([JLS §14.17], [JLS §18.5.2.4]).
    let enclosing_ret = match item_data(&tree, item) {
        Some(hir_expand::item_tree::ItemData::Method(method)) => method
            .sig
            .ret
            .as_ref()
            .map(|ret| resolve_type_ref(db, &scope, &resolver, ret)),
        _ => None,
    };

    let mut ctx = InferCtx {
        db,
        scope,
        tree: tree.bodies.clone(),
        resolver,
        access,
        enclosing_class,
        enclosing_ret,
        types: FxHashMap::default(),
        locals: FxHashMap::default(),
        scopes: vec![FxHashMap::default()],
        lambda_params: Vec::new(),
        target: None,
        switch_targets: Vec::new(),
    };
    for &param in &tree.bodies.body(body_id).params {
        ctx.declare_local(param);
    }
    for &stmt in &tree.bodies.body(body_id).stmts {
        ctx.infer_stmt(stmt);
    }
    Some(BodyTypes {
        body: Some(body_id),
        exprs: ctx.types,
        locals: ctx.locals,
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
    types: FxHashMap<ExprId, Ty>,
    locals: FxHashMap<LocalId, Ty>,
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
}

impl<'a> InferCtx<'a> {
    fn error(&self) -> Ty {
        Ty::error(self.db)
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

    fn infer_expr(&mut self, id: ExprId) -> Ty {
        let expr = self.tree.expr(id).clone();
        let ty = match expr {
            ExprData::Literal(Literal::Int) => self.primitive(PrimitiveType::Int),
            ExprData::Literal(Literal::Long) => self.primitive(PrimitiveType::Long),
            ExprData::Literal(Literal::Char) => self.primitive(PrimitiveType::Char),
            ExprData::Literal(Literal::Float) => self.primitive(PrimitiveType::Float),
            ExprData::Literal(Literal::Double) => self.primitive(PrimitiveType::Double),
            ExprData::Literal(Literal::Boolean) => self.primitive(PrimitiveType::Boolean),
            ExprData::Literal(Literal::Str) => self.string(),
            // §3.10.8: the null literal has the null type.
            ExprData::Null => Ty::null(self.db),
            // §15.8.3: `this` is the type of the enclosing class.
            ExprData::This { .. } => self.enclosing_class.unwrap_or_else(|| self.error()),
            ExprData::Super => self.error(),
            // §15.8.2: `T.class` has type `Class<T>`.
            ExprData::ClassLit(tyref) => {
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
                Ty::reference(self.db, "java.lang.Class", vec![inner])
            }
            ExprData::Var(name) => self.var(name),
            ExprData::NamePath(name) => self.name_path(name),
            ExprData::FieldAccess { target, name } => self.field_access(target, name),
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
            } => self.method_call(receiver, name, &args, self.target),
            // §15.9: a class instance creation has the type of the created
            // class.
            ExprData::New { ty, args } => self.new_expr(ty, &args),
            // §15.10: `new T[n][m]` has type `T[n][m]` (an array nested as
            // deep as there are dimensions); an array creation initializer
            // (§10.6) fills the element expressions.
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
            ExprData::Unary { op, expr } => self.unary(op, expr),
            // §15.14: a postfix increment/decrement has the type of its
            // operand.
            ExprData::Postfix { expr, .. } => self.infer_expr(expr),
            ExprData::Binary { op, lhs, rhs } => self.binary(op, lhs, rhs),
            // §15.26: an assignment expression has the type of its left-hand
            // side; the right-hand side is a poly expression with the left
            // side's type as target ([JLS §18.5.2.4]).
            ExprData::Assign { lhs, rhs, .. } => {
                let lhs_ty = self.infer_expr(lhs);
                let _ = self.with_target(Some(lhs_ty), |this| this.infer_expr(rhs));
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
                    let _ = self.infer_expr(expr);
                    resolve_type_ref(self.db, &self.scope, &self.resolver, &ty)
                }
            }
            // §15.20.2: `instanceof` always has type `boolean`.
            ExprData::InstanceOf { expr, .. } => {
                let _ = self.infer_expr(expr);
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
                let _ = self.infer_expr(cond);
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
                    self.conditional_type(then_ty, els_ty)
                }
            }
            // §15.27/§15.13: lambdas and method references are poly
            // expressions ([§15.27.2], [§15.13.2]); their type is the target
            // functional interface ([§15.27.3]), which comes from the context
            // (a declaration initializer, an assignment, a return, or a method
            // invocation's resolved formal).
            ExprData::Lambda { params, body } => self.lambda_type(&params, body),
            ExprData::MethodRef {
                qualifier,
                type_name,
                name,
            } => self.method_ref_type(qualifier, type_name.as_ref(), &name),
            // §15.28: a switch expression's type is derived from its arm
            // result types; a `yield` value inside an arm has the switch
            // expression's type as target ([JLS §14.21]).
            ExprData::Switch { scrutinee, arms } => {
                let _ = self.infer_expr(scrutinee);
                self.switch_targets.push(self.target);
                let mut result_tys: Vec<Ty> = Vec::new();
                for arm in arms {
                    for &label in &arm.labels {
                        let _ = self.infer_expr(label);
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
                }
                self.switch_targets.pop();
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
            ExprData::Missing => self.error(),
        };
        self.types.insert(id, ty);
        ty
    }

    /// A bare name: a local variable or parameter, or — when no local — a
    /// field of the implicit receiver ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)).
    fn var(&mut self, name: Name) -> Ty {
        if let Some(local) = self.lookup_local(&name) {
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
        if let Some(field) = self.pick_field_of(self.enclosing_class, name.as_str()) {
            return field.ty;
        }
        self.error()
    }

    /// A qualified name in expression position: `Type.field` (a static field
    /// access, [§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1))
    /// when the prefix resolves to a type; a simple non-local name falls back
    /// to a field of the implicit receiver.
    fn name_path(&mut self, name: Name) -> Ty {
        let text = name.as_str();
        let (prefix, last) = match text.rsplit_once('.') {
            Some((prefix, last)) => (prefix, last),
            None => ("", text),
        };
        if prefix.is_empty() {
            if let Some(field) = self.pick_field_of(self.enclosing_class, last) {
                return field.ty;
            }
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

    fn field_access(&mut self, target: Option<ExprId>, name: Name) -> Ty {
        let Some(target) = target else {
            return self.var(name);
        };
        // `super.field` — a field of the direct superclass ([§15.11.1],
        // [§15.12.1]): the receiver is the superclass type and the access
        // context is the super invocation mode.
        if matches!(self.tree.expr(target).clone(), ExprData::Super) {
            let receiver = self.super_ty();
            let access = self.access.with_mode(InvocationMode::Super);
            return match pick_field(self.db, &self.scope, &receiver, name.as_str(), &access) {
                Some(field) => field.ty,
                None => self.error(),
            };
        }
        // `Type.name` — the receiver expression is a bare name that resolves
        // to a type, not a value ([§15.11.1]).
        let (receiver, is_static) = if let ExprData::Var(type_name) = self.tree.expr(target).clone()
            && let Some(ty) = self.type_name_ty(&type_name)
        {
            (ty, true)
        } else {
            (self.infer_expr(target), false)
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
            None => self.error(),
        }
    }

    fn pick_field_of(&mut self, receiver: Option<Ty>, name: &str) -> Option<FieldData> {
        let receiver = receiver?;
        pick_field(self.db, &self.scope, &receiver, name, &self.access)
    }

    fn method_call(
        &mut self,
        receiver: Option<ExprId>,
        name: Name,
        args: &[ExprId],
        target: Option<Ty>,
    ) -> Ty {
        let (receiver_ty, mode) = self.receiver_info(receiver);
        let access = self.access.with_mode(mode);
        let arg_kinds = self.arg_kinds(args);
        match self.resolve_call(&receiver_ty, &name, &arg_kinds, target, &access) {
            Some((method, deferred)) => {
                // §18.5.2.2/§18.5.2.4: the resolved formal parameters are the
                // target types of the poly arguments — the lambda, method
                // reference or nested invocation is re-inferred against the
                // instantiated formal ([JLS §18.5.2.4]).
                self.reinfer_deferred(&method, &deferred);
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
    fn receiver_info(&mut self, receiver: Option<ExprId>) -> (Ty, InvocationMode) {
        match receiver {
            Some(receiver) => match self.tree.expr(receiver).clone() {
                // `Type.method(...)` — a static invocation whose receiver
                // expression is a bare type name ([§15.12.1]).
                ExprData::Var(type_name) => {
                    if let Some(ty) = self.type_name_ty(&type_name) {
                        return (ty, InvocationMode::Static);
                    }
                    (self.infer_expr(receiver), InvocationMode::Virtual)
                }
                // `super.method(...)` — a super invocation whose receiver is
                // the superclass of the enclosing class ([§15.12.1]).
                ExprData::Super => (self.super_ty(), InvocationMode::Super),
                _ => (self.infer_expr(receiver), InvocationMode::Virtual),
            },
            // An unqualified call is an implicit `this` invocation
            // ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)).
            None => (
                self.enclosing_class.unwrap_or_else(|| self.error()),
                InvocationMode::Virtual,
            ),
        }
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
        let (receiver_ty, mode) = self.receiver_info(receiver);
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
    fn new_expr(&mut self, ty: TypeRef<Name>, args: &[ExprId]) -> Ty {
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let constructor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => simple_name(name.as_str()),
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

    /// The type of a lambda expression ([JLS §15.27.2]): the target
    /// functional interface ([§15.27.3], [JLS §18.5.2.4]). The lambda's
    /// parameters are typed from the single abstract method of the target
    /// ([JLS §9.8]) and its body is inferred against the SAM's return type —
    /// a return statement inside a lambda body returns from the lambda, not
    /// from the enclosing method.
    fn lambda_type(&mut self, params: &[(Name, Option<TypeRef<Name>>)], body: LambdaBody) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
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
        qualifier: Option<ExprId>,
        type_name: Option<&TypeRef<Name>>,
        name: &Name,
    ) -> Ty {
        let Some(target) = self.target else {
            return self.error();
        };
        let Some(sam) = single_abstract_method(self.db, &self.scope, &target) else {
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
                _ => simple_name(fqn.as_str()),
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

    fn unary(&mut self, op: UnaryOp, expr: ExprId) -> Ty {
        let inner = self.infer_expr(expr);
        match op {
            // §15.15.6: `!` has type `boolean`.
            UnaryOp::Not => self.primitive(PrimitiveType::Boolean),
            // §15.15.1-3: unary numeric promotion (§5.6.1).
            UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => self.unary_promotion(inner),
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
        // §15.25.2: both operands convertible to a numeric type — a primitive
        // or a boxed reference operand, unboxed (§5.1.8) — apply binary
        // numeric promotion.
        let numeric = |ty: Ty| match ty.kind(self.db) {
            TyKind::Primitive(p) => Some(*p),
            TyKind::Reference { name, .. } => unboxed_primitive(name.as_str()),
            _ => None,
        };
        if let (Some(l), Some(r)) = (numeric(then_ty), numeric(els_ty)) {
            return self
                .binary_numeric_promotion(Ty::primitive(self.db, l), Ty::primitive(self.db, r));
        }
        if then_ty.is_null(self.db) && els_ty.is_reference(self.db) {
            return els_ty;
        }
        if els_ty.is_null(self.db) && then_ty.is_reference(self.db) {
            return then_ty;
        }
        least_upper_bound(self.db, &self.scope, &[then_ty, els_ty])
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
        let lhs_ty = self.infer_expr(lhs);
        let rhs_ty = self.infer_expr(rhs);
        match op {
            BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::BitAnd
            | BinaryOp::BitXor
            | BinaryOp::BitOr => {
                // §15.18.1: `+` with a `String` operand is string
                // concatenation and has type `String`.
                if matches!(op, BinaryOp::Add) && (self.is_string(lhs_ty) || self.is_string(rhs_ty))
                {
                    self.string()
                } else {
                    self.binary_numeric_promotion(lhs_ty, rhs_ty)
                }
            }
            // §15.19: a shift has the unary-promoted type of the left operand.
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::UShr => {
                let promoted = self.unary_promotion(lhs_ty);
                if promoted.is_error(self.db) {
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.20-15.24: relational, equality and boolean-logical
            // expressions have type `boolean`.
            BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::And
            | BinaryOp::Or => self.primitive(PrimitiveType::Boolean),
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

    /// The element type of a for-each iterable
    /// ([§14.14.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2)):
    /// the element type for arrays; for a reference type, the `T` of an
    /// `Iterable<T>` — the `E` of the `Iterator<E>` returned by `iterator()`
    /// ([§14.14.2.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2.1)).
    fn element_type(&self, iterable: Ty) -> Ty {
        if iterable.is_array(self.db) {
            return iterable
                .element(self.db)
                .copied()
                .unwrap_or_else(|| self.error());
        }
        // §14.14.2.1: the expression must be `Iterable<T>`; the loop variable
        // takes `T`, the element type of the `Iterator<E>` that `iterator()`
        // returns.
        let iterator = match pick_method(
            self.db,
            &self.scope,
            &iterable,
            "iterator",
            &[],
            &self.access,
            None,
        ) {
            Some(method) => method.ret,
            None => return self.error(),
        };
        match pick_method(
            self.db,
            &self.scope,
            &iterator,
            "next",
            &[],
            &self.access,
            None,
        ) {
            Some(method) => method.ret,
            None => self.error(),
        }
    }

    fn declare_local(&mut self, id: LocalId) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => self.error(),
        };
        self.bind_local(id, local.name, ty);
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
            StmtData::Decl { local, initializer } => {
                self.declare_local(*local);
                if let Some(initializer) = initializer {
                    // The initializer is a poly expression whose target is the
                    // declared type of the local ([JLS §14.4]).
                    let target = self.locals.get(local).copied();
                    let _ = self.with_target(target, |this| this.infer_expr(*initializer));
                }
            }
            StmtData::Expr(expr) => {
                let _ = self.infer_expr(*expr);
            }
            StmtData::Labeled { stmt, .. } => self.infer_stmt(*stmt),
            StmtData::If { cond, then, els } => {
                let _ = self.infer_expr(*cond);
                self.infer_stmt(*then);
                if let Some(els) = els {
                    self.infer_stmt(*els);
                }
            }
            StmtData::While { cond, body } => {
                let _ = self.infer_expr(*cond);
                self.infer_stmt(*body);
            }
            StmtData::DoWhile { body, cond } => {
                self.infer_stmt(*body);
                let _ = self.infer_expr(*cond);
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
                    let _ = self.infer_expr(*cond);
                }
                for &step in step {
                    let _ = self.infer_expr(step);
                }
                self.infer_stmt(*body);
                self.scopes.pop();
            }
            StmtData::ForEach {
                var,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr(*iterable);
                let element = self.element_type(iterable_ty);
                self.scopes.push(FxHashMap::default());
                self.declare_local_ty(*var, element);
                self.infer_stmt(*body);
                self.scopes.pop();
            }
            StmtData::Switch { scrutinee, arms } => {
                let _ = self.infer_expr(*scrutinee);
                self.scopes.push(FxHashMap::default());
                for arm in arms {
                    for &label in &arm.labels {
                        let _ = self.infer_expr(label);
                    }
                    for &stmt in &arm.body {
                        self.infer_stmt(stmt);
                    }
                }
                self.scopes.pop();
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
                let _ = self.with_target(target, |this| this.infer_expr(*expr));
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
                }
            }
            StmtData::Return(None) | StmtData::Break(_) | StmtData::Continue(_) => {}
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
                for &resource in resources {
                    self.declare_local(resource);
                }
                self.infer_stmt(*body);
                for clause in catches {
                    self.scopes.push(FxHashMap::default());
                    self.declare_local(clause.param);
                    self.infer_stmt(clause.body);
                    self.scopes.pop();
                }
                self.scopes.pop();
                if let Some(finally) = finally {
                    self.infer_stmt(*finally);
                }
            }
            StmtData::Assert { cond, msg } => {
                let _ = self.infer_expr(*cond);
                if let Some(msg) = msg {
                    let _ = self.infer_expr(*msg);
                }
            }
            StmtData::Missing => {}
        }
    }
}

/// The simple name of a possibly-qualified FQN: everything after the last
/// `$` (nested classes are named `Outer$Inner`).
fn simple_name(fqn: &str) -> String {
    match fqn.rfind(['$', '.']) {
        Some(i) => fqn[i + 1..].to_owned(),
        None => fqn.to_owned(),
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
