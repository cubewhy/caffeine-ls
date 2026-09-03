//! Expression inference ([JLS §15.1]): the bottom-up type of every
//! expression — names, field access, literals, arrays, casts, conditionals,
//! unary/binary operators, invocations, `new` expressions, lambdas and
//! method references — plus the write-vs-read context flags.

use hir_expand::{
    body::{AssignOp, ExprData, ExprId, Literal, StmtData, SwitchLabel},
    name::Name,
};
use rustc_hash::FxHashMap;
use stacksafe::stacksafe;
use syntax::stub::{PrimitiveType, TypeRef};

use crate::java::{
    diagnostics::{DiagLocation, NonStaticThisKind, TypeError},
    method::{FieldData, InvocationMode, pick_field},
    resolve::resolve_type_ref,
    ty::{Ty, TyKind, boxed_type},
};

use super::{FinalFieldWrite, Flow, InferCtx, poly::*};

impl InferCtx<'_> {
    #[stacksafe]
    pub(super) fn infer_expr(&mut self, id: ExprId) -> Ty {
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
            // §15.8.2: `T.class` has type `Class<T>`. For a primitive type the
            // class literal denotes the *boxed* class — `int.class` is
            // `Class<Integer>`, `long.class` `Class<Long>`, `void.class`
            // `Class<Void>` ([§15.8.2]: "the type of `p.class`, where p is the
            // name of a primitive type, is `Class<B>` where B is the type of an
            // expression of type p after boxing conversion").
            ExprData::ClassLit(tyref) => {
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
                let inner = match inner.kind(self.db) {
                    TyKind::Primitive(p) => Ty::reference(self.db, boxed_type(*p), Vec::new()),
                    _ => inner,
                };
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
                // §15.9: a *qualified* class instance creation
                // (`primary.new Inner(...)`) is an instance creation of the
                // member class `Inner` of the receiver expression's
                // compile-time type ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9),
                // [§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)).
                // The receiver is inferred standalone — it is the enclosing
                // instance and has no effect on the created type's own
                // target-driven inference. Its *type* is what the member
                // class resolves against: `a.new B()` creates the `B` nested
                // in the type of `a`, which the lexical scope never names.
                let receiver_ty = match receiver {
                    Some(receiver) => {
                        let ty = self.with_target(None, |this| this.infer_expr(receiver));
                        Some(ty)
                    }
                    None => None,
                };
                self.new_expr(
                    id,
                    ty,
                    diamond,
                    &args,
                    self.target,
                    !members.is_empty(),
                    receiver_ty,
                )
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
                // §15.10.1: each dimension expression is a *standalone*
                // expression whose type is `int` — the enclosing array
                // creation's target type must not reach it, or a conditional
                // dimension with a poly arm (`new double[(c ? a.size() : 0) +
                // 1]`) would be typed against the `double[]` target and
                // degrade its arms.
                self.with_target(None, |this| {
                    for &dim in &dims {
                        let _ = this.infer_expr(dim);
                    }
                    if let Some(elems) = initializer {
                        for elem in elems {
                            let _ = this.infer_expr(elem);
                        }
                    }
                });
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
                    // §15.16: a cast whose operand is not a lambda or method
                    // reference is a *standalone* expression — the cast type
                    // does not reach the operand, and neither does the
                    // enclosing context's target. `(R) Arrays.copyOf((Object[])
                    // object, …)` inside `R x = …` must infer `copyOf` as
                    // `Object[]` (T := Object) and then perform the (unchecked)
                    // cast to `R`, not constrain `copyOf`'s `T[]` return
                    // against the type variable `R`, which no invocation type
                    // can satisfy.
                    let operand = self.with_target(None, |this| this.infer_expr(expr));
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

    /// receiver position must not be mistaken for the assigned target.
    pub(super) fn infer_read_expr(&mut self, id: ExprId) -> Ty {
        let saved_mutating = self.mutating;
        let saved_writing = self.writing;
        self.mutating = false;
        self.writing = false;
        let ty = self.infer_expr(id);
        self.mutating = saved_mutating;
        self.writing = saved_writing;
        ty
    }

    /// field of the implicit receiver ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)).
    pub(super) fn var(&mut self, expr: ExprId, name: Name) -> Ty {
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

    /// makes the simple name `FIELD` a static member access (§15.11.1).
    pub(super) fn static_import_field(&self, simple: &str) -> Option<Ty> {
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

    /// to a field of the implicit receiver.
    pub(super) fn name_path(&mut self, expr: ExprId, name: Name) -> Ty {
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

    /// variable or does not resolve to a type.
    pub(super) fn type_name_ty(&self, name: &Name) -> Option<Ty> {
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

    /// so ordinary instance field chains keep their expression treatment.
    pub(super) fn dotted_type_name(&self, id: ExprId) -> Option<Ty> {
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

    pub(super) fn field_access(&mut self, expr: ExprId, target: Option<ExprId>, name: Name) -> Ty {
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

    /// ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)).
    pub(super) fn receiver_has_nested_type(&self, receiver: &Ty, name: &str) -> bool {
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

    /// `None` for a non-reference (array, type variable, primitive, error).
    pub(super) fn receiver_fqn(&self, receiver: &Ty) -> Option<&str> {
        let TyKind::Reference { name, .. } = receiver.kind(self.db) else {
            return None;
        };
        Some(name.as_str())
    }

    /// genuine §8.9.2/§15.11 error rather than a partial-record artifact.
    pub(super) fn receiver_is_library_enum(&self, receiver: &Ty) -> bool {
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

    pub(super) fn pick_field_of(&mut self, receiver: Option<Ty>, name: &str) -> Option<FieldData> {
        let receiver = receiver?;
        pick_field(self.db, &self.scope, &receiver, name, &self.access)
    }
}
