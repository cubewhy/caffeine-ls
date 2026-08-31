//! Local variables, parameters and pattern typing ([JLS §6.3], [§14.4],
//! [§14.30]): declaration, definite-assignment scope binding, and the
//! pattern-decomposition helpers.

use hir_def::java::item_tree::ItemData;
use hir_expand::{
    body::{BinaryOp, ExprData, ExprId, LocalId, PatternData, PatternId, UnaryOp},
    name::Name,
};
use rowan::TextRange;

use crate::java::{
    diagnostics::TypeError,
    resolve::resolve_type_ref,
    ty::{Ty, TyKind},
};

use super::InferCtx;

impl InferCtx<'_> {
    pub(super) fn declare_local(&mut self, id: LocalId) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => self.error(),
        };
        self.bind_local(id, local.name, ty);
    }

    /// [§16.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-16.html#jls-16.1.9)).
    pub(super) fn declare_param(&mut self, id: LocalId) {
        self.declare_local(id);
        self.flow.definite.insert(id);
    }

    pub(super) fn declare_local_ty(&mut self, id: LocalId, fallback: Ty) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => fallback,
        };
        self.bind_local(id, local.name, ty);
    }

    pub(super) fn bind_local(&mut self, id: LocalId, name: Name, ty: Ty) {
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

    pub(super) fn lookup_local(&self, name: &Name) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&local) = scope.get(name) {
                return Some(local);
            }
        }
        None
    }

    /// parameter's own name range, matching javac.
    pub(super) fn check_lambda_param_duplicate(
        &mut self,
        expr: ExprId,
        name: &Name,
        range: TextRange,
    ) {
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

    /// Returns the pattern's type; the match-all `_` ([§14.30.3]) has none.
    pub(super) fn pattern_type(&mut self, id: PatternId) -> Ty {
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

    /// pattern-arity check ([§14.30.2]) then stays silent.
    pub(super) fn record_component_count(&self, ty: &Ty) -> Option<usize> {
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

    /// components ([JLS §14.30.1], [§14.30.2]).
    pub(super) fn pattern_bindings_of(&self, id: PatternId) -> Vec<LocalId> {
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

    /// swaps the flows, and parentheses are transparent.
    pub(super) fn pattern_flow(&self, id: ExprId) -> Option<(Vec<LocalId>, Vec<LocalId>)> {
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

    pub(super) fn pattern_binding_ids(&self, id: ExprId) -> Option<Vec<LocalId>> {
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

    /// [`Self::pattern_type`] during expression inference.
    pub(super) fn scope_binding(&mut self, id: LocalId) {
        let name = self.tree.local(id).name.clone();
        // A pattern variable is definitely assigned wherever it is in scope
        // ([§16.1.13]): it is bound exactly when the pattern matched.
        self.flow.definite.insert(id);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name, id);
    }
}
