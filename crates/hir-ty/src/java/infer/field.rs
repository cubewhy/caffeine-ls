//! Field-write tracking and access reporting ([JLS §8.3.1.2], [§16],
//! [§6.6]): the blank-`final` field write verdict, the chain lookup for
//! simple-name field reads, and the illegal-access reports.

use hir_expand::{
    body::{ExprData, ExprId},
    name::Name,
};

use crate::java::{
    diagnostics::{IllegalAccessKind, TypeError},
    method::{
        FieldData, InvocationContext, member_set_ignoring_access, pick_field,
        pick_field_ignoring_access,
    },
    ty::Ty,
};

use super::{FinalFieldWrite, InferCtx, InitCtx, poly::access_keyword};

impl InferCtx<'_> {
    /// contributes nothing).
    pub(super) fn record_break_flow(&mut self, label: Option<&Name>) {
        if let Some(name) = label {
            for frame in self.loop_breaks.iter_mut().rev() {
                if frame.label.as_ref().is_some_and(|n| n == name) {
                    frame.flows.push(self.flow.clone());
                    return;
                }
            }
            return;
        }
        if let Some(innermost) = self.loop_breaks.last_mut() {
            innermost.flows.push(self.flow.clone());
        }
    }

    /// innermost first ([§6.5.5.1], [§8.3]).
    pub(super) fn pick_field_of_chain(&mut self, name: &str) -> Option<FieldData> {
        for class in std::iter::once(&self.enclosing_class)
            .flatten()
            .chain(self.enclosing_chain.iter())
        {
            if let Some(field) = pick_field(self.db, &self.scope, class, name, &self.access) {
                return Some(field);
            }
        }
        None
    }

    /// and [`Self::field_access`].
    pub(super) fn record_field_write(&mut self, lhs: ExprId) {
        let name = match self.tree.expr(lhs).clone() {
            ExprData::Var(name) => {
                // A bare `Var` names a local variable or parameter when one is
                // in scope ([§6.5.6.1]) — `mappedMember = false` assigning a
                // constructor parameter must not be mistaken for a write to a
                // same-named *field* of the enclosing class. Only when no
                // local of the name exists does a bare name denote the
                // implicit `this` field ([§6.5.6.1], [§15.26.1]).
                if self.lookup_local(&name).is_some() {
                    return;
                }
                name
            }
            ExprData::FieldAccess { target, name } => {
                // Only a bare `this.name` (the implicit receiver) can be the
                // class's own field; `Type.name` and `obj.name` assign a
                // member of another object and never touch the enclosing
                // class's blank finals.
                let Some(target) = target else {
                    return;
                };
                if !matches!(
                    self.tree.expr(target).clone(),
                    ExprData::This { qualifier: None }
                ) {
                    return;
                }
                name
            }
            _ => return,
        };
        let Some(field) = self.pick_field_of_chain(name.as_str()) else {
            return;
        };
        // §8.3.1.2/[§16]: only *blank* finals are one-shot — a final with an
        // initializer can never be assigned, and is reported elsewhere; it is
        // not "assigned here".
        if !field.is_final || !self.field_is_blank(&field) {
            return;
        }
        self.flow.field_touched.insert(name.as_str().to_owned());
    }

    /// which is implicitly blank, [§8.10.4]).
    pub(super) fn field_is_blank(&self, field: &FieldData) -> bool {
        let Some(owner_file) = field.owner_file else {
            return false;
        };
        let tree = hir::file_item_tree(self.db, owner_file);
        let mut found_blank = false;
        fn walk(
            tree: &hir_def::java::item_tree::ItemTree,
            id: hir_def::java::item_tree::ItemId,
            name: &str,
            found: &mut bool,
        ) {
            if *found {
                return;
            }
            match tree.data(id) {
                hir_def::java::item_tree::ItemData::Field(f) if f.name.as_str() == name => {
                    *found = f.initializer.is_none();
                    return;
                }
                // §8.10.1: a record's components become blank final fields.
                hir_def::java::item_tree::ItemData::Record(r)
                    if r.components.iter().any(|c| c.name.as_str() == name) =>
                {
                    *found = true;
                    return;
                }
                _ => {}
            }
            for &child in tree.data(id).body() {
                walk(tree, child, name, found);
            }
        }
        for &top in &tree.top {
            walk(&tree, top, &field.name, &mut found_blank);
        }
        found_blank
    }

    /// [`FinalFieldWrite::CannotAssign`].
    pub(super) fn final_field_write_verdict(
        &self,
        field: &FieldData,
        bare_this: bool,
    ) -> FinalFieldWrite {
        if !bare_this || self.lambda_depth > 0 {
            return FinalFieldWrite::CannotAssign;
        }
        // The context must match the field's static/instance kind.
        let ctx_ok = if field.is_static {
            self.init_ctx == Some(InitCtx::Static)
        } else {
            self.in_constructor || self.init_ctx == Some(InitCtx::Instance)
        };
        if !ctx_ok {
            return FinalFieldWrite::CannotAssign;
        }
        // The field must belong to the class being initialized.
        let same_class = self
            .enclosing_class
            .as_ref()
            .and_then(|ty| ty.as_reference(self.db))
            .is_some_and(|(fqn, _)| fqn.as_str() == field.owner);
        if !same_class {
            return FinalFieldWrite::CannotAssign;
        }
        // The field must be blank — a source field with no initializer (its
        // initializer is what makes a later assignment a double-write). A
        // record component field is implicitly blank ([§8.10.4]): the
        // canonical and compact constructors may assign it, and no component
        // ever carries an initializer.
        if !self.field_is_blank(field) {
            return FinalFieldWrite::CannotAssign;
        }
        // A blank final may be assigned only once: a write to one already
        // assigned on some path is the already-assigned error, not a fresh
        // initialization.
        if self.flow.field_touched.contains(field.name.as_str()) {
            return FinalFieldWrite::AlreadyAssigned;
        }
        FinalFieldWrite::Legal
    }

    /// so; the caller falls back to a no-such-member error otherwise.
    pub(super) fn report_illegal_field_access(
        &mut self,
        expr: ExprId,
        receiver: Ty,
        name: &str,
    ) -> bool {
        let Some(field) =
            pick_field_ignoring_access(self.db, &self.scope, &receiver, name, &self.access)
        else {
            return false;
        };
        self.report(TypeError::IllegalAccess {
            expr,
            kind: IllegalAccessKind::Field,
            name: Name::new(&field.name),
            owner: Name::new(&field.owner),
            access: access_keyword(field.access),
        });
        true
    }

    /// most-derived one as `IllegalAccess` and returns `true` when so.
    pub(super) fn report_illegal_method_access(
        &mut self,
        expr: ExprId,
        receiver: Ty,
        name: &str,
        ctx: &InvocationContext,
    ) -> bool {
        let members = member_set_ignoring_access(self.db, &self.scope, &receiver, name, ctx);
        let Some(method) = members.first() else {
            return false;
        };
        self.report(TypeError::IllegalAccess {
            expr,
            kind: IllegalAccessKind::Method,
            name: Name::new(&method.name),
            owner: Name::new(&method.owner),
            access: access_keyword(method.access),
        });
        true
    }
}
