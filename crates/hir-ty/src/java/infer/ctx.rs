//! The small inference-context helpers: error and diagnostic plumbing, the
//! expected-type threading, boolean-flow outcome recording, the primitive
//! and boxing shortcuts, and the comparability/castability tests.

use hir_expand::body::{BinaryOp, ExprData, ExprId, UnaryOp};
use syntax::stub::PrimitiveType;

use crate::java::{
    const_eval::Const,
    diagnostics::TypeError,
    ty::{Ty, TyKind, boxed_type, capture_conversion, unboxed_primitive},
};

use super::{Flow, InferCtx};

impl InferCtx<'_> {
    pub(super) fn error(&self) -> Ty {
        Ty::error(self.db)
    }

    /// total-failure path — not once per probed overload.
    pub(super) fn report(&mut self, diagnostic: TypeError) {
        if self.probing {
            return;
        }
        self.diagnostics.push(diagnostic);
    }

    /// inference ([JLS §18.5.2.4]).
    pub(super) fn with_target<T>(
        &mut self,
        target: Option<Ty>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let saved = self.target;
        self.target = target;
        let result = f(self);
        self.target = saved;
        result
    }

    /// (e.g. a condition degraded to an error).
    pub(super) fn take_bool_outcomes(&mut self) -> (Flow, Flow) {
        self.bool_outcomes
            .take()
            .unwrap_or_else(|| (self.flow.clone(), self.flow.clone()))
    }

    /// ([§4.12.4]) is captured at the current position.
    pub(super) fn const_bool(&self, id: ExprId) -> Option<bool> {
        match self.const_value(id) {
            Some(Const::Bool(b)) => Some(b),
            _ => None,
        }
    }

    /// ([§16.1.7]: `(a && b)` behaves like `a && b`).
    pub(super) fn is_bool_flow_expr(&self, id: ExprId) -> bool {
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

    /// overload resolution ([§15.12.2]).
    pub(super) fn with_probing<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.probing, true);
        let result = f(self);
        self.probing = saved;
        result
    }

    pub(super) fn primitive(&self, p: PrimitiveType) -> Ty {
        Ty::primitive(self.db, p)
    }

    /// The boxed form of a primitive type ([§5.1.7]): `int` → `Integer`.
    pub(super) fn box_primitive(&self, ty: Ty) -> Ty {
        let TyKind::Primitive(p) = ty.kind(self.db) else {
            return ty;
        };
        Ty::reference(self.db, boxed_type(*p), Vec::new())
    }

    pub(super) fn string(&self) -> Ty {
        Ty::reference(self.db, "java.lang.String", Vec::new())
    }

    pub(super) fn is_string(&self, ty: Ty) -> bool {
        matches!(ty.kind(self.db), TyKind::Reference { name, .. } if name.as_str() == "java.lang.String")
    }

    /// a primitive `boolean`, or a boxed `Boolean` after unboxing ([§5.1.8]).
    pub(super) fn is_boolean(&self, ty: Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(PrimitiveType::Boolean) => true,
            TyKind::Reference { name, .. } => {
                unboxed_primitive(name.as_str()) == Some(PrimitiveType::Boolean)
            }
            _ => false,
        }
    }

    /// a primitive other than `boolean`, or a boxed primitive after unboxing.
    pub(super) fn is_numeric_operand(&self, ty: Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(p) => !matches!(p, PrimitiveType::Boolean),
            TyKind::Reference { name, .. } => match unboxed_primitive(name.as_str()) {
                Some(p) => !matches!(p, PrimitiveType::Boolean),
                None => false,
            },
            _ => false,
        }
    }

    /// variable or intersection type ([JLS §4.3]).
    pub(super) fn is_reference_like(&self, ty: Ty) -> bool {
        matches!(
            ty.kind(self.db),
            TyKind::Reference { .. }
                | TyKind::Array(_)
                | TyKind::TypeVar { .. }
                | TyKind::Intersection(_)
                | TyKind::Null
        )
    }

    /// error type and is reported.
    pub(super) fn check_condition(&mut self, cond: ExprId) {
        // JLS §15.2: a condition (`if`/`while`/`do`/`for`/`assert`, `&&`/`||`
        // operand, conditional condition) is a standalone expression — the
        // enclosing poly target must not reach it.
        let ty = self.with_target(None, |this| this.infer_expr(cond));
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

    /// demanding a subtype link when exactly one operand unboxes.
    pub(super) fn comparable(&self, a: Ty, b: Ty) -> bool {
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

    /// reference widening/narrowing cast.
    pub(super) fn castable(&self, from: Ty, to: Ty) -> bool {
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
            // §5.5.1: a cast *from* a type variable — `R`, `R[]`, or a
            // parameterized `R` — is always a legal compile-time cast: the
            // runtime check compares the operand's runtime class against the
            // *erasure* of the target ([§4.6]), so it can never be rejected
            // statically. Only the target's erasure decides. (The `(R) o`
            // casts of a generic snapshot holder, where `R` is a fresh type
            // parameter of the enclosing method, rely on this.)
            (TyKind::TypeVar { .. }, TyKind::Array(_)) => true,
            (TyKind::TypeVar { .. }, TyKind::Reference { .. }) => true,
            (TyKind::Reference { .. }, TyKind::Reference { .. }) => {
                self.reference_castable(from, to)
            }
            _ => false,
        }
    }

    /// still fail; that is not a compile-time error per §5.5.1).
    pub(super) fn reference_castable(&self, from: Ty, to: Ty) -> bool {
        let sub = crate::java::subtyping::is_subtype(self.db, &self.scope, &from, &to);
        let sup = crate::java::subtyping::is_subtype(self.db, &self.scope, &to, &from);
        if sub || sup {
            return true;
        }
        !matches!(
            (
                crate::java::subtyping::class_like_and_final(self.db, &self.scope, &from),
                crate::java::subtyping::class_like_and_final(self.db, &self.scope, &to),
            ),
            (Some((true, _)), Some((true, _)))
        )
    }
}
