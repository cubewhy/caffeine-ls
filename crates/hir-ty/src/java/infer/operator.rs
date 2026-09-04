//! Operator inference ([JLS §5.6.1], [§5.6.2], [§15.15]–[§15.25]): unary
//! numeric promotion, binary numeric promotion, the conditional type of
//! [§15.25], and the source symbols used in [`TypeError::IncompatibleOperand`].

use hir_expand::body::{BinaryOp, ExprId, UnaryOp};
use rustc_hash::FxHashMap;
use syntax::stub::PrimitiveType;

use crate::java::{
    diagnostics::TypeError,
    inference::least_upper_bound,
    method::pick_method,
    ty::{Ty, TyKind, capture_conversion, numeric_promotion, unboxed_primitive},
};

use super::InferCtx;

impl InferCtx<'_> {
    pub(super) fn unary(&mut self, expr: ExprId, op: UnaryOp) -> Ty {
        // §15.15.1/§15.15.2: `++`/`--` mutate their operand.
        // JLS §15.2: a unary operand is a standalone expression — the
        // enclosing poly target must not reach it.
        self.mutating = matches!(op, UnaryOp::Inc | UnaryOp::Dec);
        let inner = self.with_target(None, |this| this.infer_expr(expr));
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
    pub(super) fn conditional_type(&self, then_ty: Ty, els_ty: Ty) -> Ty {
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
        // boxed types), not the lub of the primitive and `null`. A type
        // variable, capture or intersection arm is a reference too — the
        // null type is a subtype of every reference type ([§4.10.2]), so
        // `cond ? node.value : null` with `node.value: V` keeps the type
        // `V`, whose lower bound the enclosing assignment or return checks
        // against (`V`-returning methods must not degrade to `Object`).
        let reference_like = |ty: Ty| {
            matches!(
                ty.kind(self.db),
                TyKind::Reference { .. }
                    | TyKind::Array(_)
                    | TyKind::TypeVar { .. }
                    | TyKind::Intersection(_)
            )
        };
        if then_ty.is_null(self.db) && reference_like(els_ty) {
            return els_ty;
        }
        if els_ty.is_null(self.db) && reference_like(then_ty) {
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
    pub(super) fn unboxed_operand(&self, ty: Ty) -> Ty {
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
    pub(super) fn unary_promotion(&self, ty: Ty) -> Ty {
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

    pub(super) fn binary(&mut self, op: BinaryOp, lhs: ExprId, rhs: ExprId) -> Ty {
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
        // JLS §15.2: the operands of a binary operator are standalone
        // expressions — the enclosing poly target must not reach them (JLS §15.21).
        let (lhs_ty, rhs_ty) =
            self.with_target(None, |this| (this.infer_expr(lhs), this.infer_expr(rhs)));
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
    pub(super) fn binary_numeric_promotion(&self, lhs: Ty, rhs: Ty) -> Ty {
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
    pub(super) fn iterable_element(&self, iterable: Ty) -> Option<Ty> {
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
}

/// The source symbol of a unary operator, for [`TypeError::IncompatibleOperand`].
pub(super) fn unary_op_symbol(op: UnaryOp) -> &'static str {
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
pub(super) fn binary_op_symbol(op: BinaryOp) -> &'static str {
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
