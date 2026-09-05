//! Switch inference ([JLS §14.11], [§14.21]): the selector and label
//! checks, exhaustiveness, the constant-value narrowing of case labels, and
//! the reifiable/raw-type and checked-exception helpers the surrounding
//! checks share.

use hir_expand::{
    body::{
        ExprData, ExprId, LocalId, PatternData, PatternId, StmtData, StmtId, SwitchArm, SwitchLabel,
    },
    name::Name,
    span::SpannedTypeRef,
};
use rustc_hash::FxHashSet;
use syntax::stub::{PrimitiveType, TypeRef};

use crate::java::{
    const_eval::{Const, ConstEnv},
    diagnostics::{DiagLocation, TypeError},
    resolve::{resolve_type_ref, type_argument_bound_violation},
    ty::{Ty, TyKind},
};

use super::InferCtx;

impl InferCtx<'_> {
    /// reference — or be a `String` or an enum type.
    pub(super) fn infer_switch_selector(&mut self, scrutinee: ExprId) -> Ty {
        // JLS §15.2/§14.11: the switch selector is standalone.
        let ty = self.with_target(None, |this| this.infer_expr(scrutinee));
        if !ty.is_error(self.db) && !self.switchable(&ty) {
            self.report(TypeError::SwitchSelectorType {
                expr: scrutinee,
                found: ty,
            });
        }
        ty
    }

    /// selector requires every constant to be named by some label.
    pub(super) fn switch_is_exhaustive(&self, selector: &Ty, arms: &[SwitchArm]) -> bool {
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

    /// never selectable.
    pub(super) fn switchable(&self, ty: &Ty) -> bool {
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

    /// constant expression, which must be assignable to the selector.
    pub(super) fn infer_switch_label(&mut self, label: ExprId, selector: &Ty) {
        if let ExprData::Var(name) = self.tree.expr(label).clone()
            && let Some(constants) =
                crate::java::subtyping::enum_constants(self.db, &self.scope, selector)
            && constants.iter().any(|constant| constant == &name)
        {
            self.types.insert(label, *selector);
            return;
        }
        // JLS §15.2/§14.11.1: a case label is standalone.
        let ty = self.with_target(None, |this| this.infer_expr(label));
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
            && !self.constant_narrowable(label, ty, *selector)
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

    /// target type ([§5.1.3] narrowing of constants).
    pub(super) fn constant_narrowable(&self, expr: ExprId, src: Ty, dst: Ty) -> bool {
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

    /// expression in assignment context, [§15.28]).
    pub(super) fn switch_results_narrowable(&self, arms: &[SwitchArm], d: PrimitiveType) -> bool {
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

    /// expression, and the `yield` values of a block arm.
    pub(super) fn collect_switch_results(&self, stmts: &[StmtId], out: &mut Vec<ExprId>) {
        for &stmt in stmts {
            match self.tree.stmt(stmt) {
                StmtData::Expr(expr) => out.push(*expr),
                StmtData::Yield(expr) => out.push(*expr),
                StmtData::Block(inner) => self.collect_switch_results(inner, out),
                _ => {}
            }
        }
    }

    /// names of constant variables — evaluated by [`crate::java::const_eval`].
    pub(super) fn const_int_value(&self, id: ExprId) -> Option<i64> {
        self.const_value(id).and_then(|value| value.as_int())
    }

    /// environment of constant variables ([§4.12.4]).
    pub(super) fn const_value(&self, id: ExprId) -> Option<Const> {
        ConstEnv::new(&self.tree, &self.const_locals).eval(id)
    }

    /// ([§8.1.2]) used without its type arguments.
    pub(super) fn is_raw_type(&self, ty: &Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Reference { name, args } if args.is_empty() => {
                !ty.is_error(self.db)
                    && crate::java::resolve::class_is_generic(self.db, &self.scope, name)
            }
            _ => false,
        }
    }

    /// `List<? extends Number>`, `ArrayList<T>`), is not.
    pub(super) fn is_reifiable(&self, ty: &Ty) -> bool {
        match ty.kind(self.db) {
            TyKind::Primitive(_) => true,
            TyKind::Reference { args, .. } => args
                .iter()
                .all(|a| matches!(a.kind(self.db), TyKind::Wildcard(None))),
            TyKind::Array(inner) => self.is_reifiable(inner),
            _ => false,
        }
    }

    /// parameterized type with a non-wildcard argument cannot be tested.
    pub(super) fn check_instanceof_target(&mut self, expr: ExprId, spanned: &SpannedTypeRef) {
        self.check_type_argument_bounds(DiagLocation::Expr(expr), spanned);
        let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, spanned);
        if !ty.is_error(self.db) && !self.is_reifiable(&ty) {
            self.report(TypeError::IllegalGenericInstanceOf { expr, ty });
        }
    }

    /// type of a `TYPE_PATTERN`/`RECORD_PATTERN`, `None` for a `MatchAll`.
    pub(super) fn pattern_type_ref(&self, id: PatternId) -> Option<SpannedTypeRef> {
        match self.tree.pattern(id).clone() {
            PatternData::Type(tp) => Some(tp.ty),
            PatternData::Record(rp) => Some(rp.ty),
            PatternData::MatchAll => None,
        }
    }

    /// §4.12.2: reports a declared local whose type is a raw type.
    pub(super) fn warn_raw_declared_type(&mut self, local: LocalId) {
        let Some(ty) = self.locals.get(&local).copied() else {
            return;
        };
        if let TyKind::Reference { .. } = ty.kind(self.db)
            && self.is_raw_type(&ty)
        {
            self.report(TypeError::RawTypeUse { local, ty });
        }
    }

    /// javac's caret.
    pub(super) fn check_type_argument_bounds(
        &mut self,
        location: DiagLocation,
        spanned: &SpannedTypeRef,
    ) {
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

    /// is parameterized succeeds by *unchecked conversion*; report it.
    pub(super) fn warn_unchecked(&mut self, expr: ExprId, src: &Ty, dst: &Ty) {
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

    /// their bare-name labels are resolved as constants above.
    pub(super) fn check_case_label(&mut self, label: ExprId, selector: &Ty) {
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

    /// ([§11.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-11.html#jls-11.1.1)).
    pub(super) fn is_checked(&self, ty: &Ty) -> bool {
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

    /// type is reported and the rest of that statement's set is left alone.
    pub(super) fn check_thrown_liability(&mut self) {
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
}
