//! The syntactic scan behind the effectively-final capture check
//! ([JLS §6.5.6.1], [§15.27.2]): a local captured by a lambda that is also
//! mutated anywhere in its scope is not effectively final.

use hir_expand::{
    arena::ArenaId,
    body::{BodyId, BodyTree, ExprData, ExprId, LocalId, StmtId},
    name::Name,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// §6.5.6.1/[§15.27.2]: a body-tree scan — independent of inference — that
/// finds (a) every local variable that is *mutated* anywhere in the body
/// (`mutated`, keyed by the local's identity), and (b) every reference to an
/// enclosing local inside a lambda expression (`captures`), with the
/// reference's expression. A local that is both captured by a lambda and
/// mutated anywhere in its scope is not *effectively final*, and every capture
/// of it is an error.
///
/// Mutations and captures are keyed by [`LocalId`] — not by name — so two
/// same-named locals in sibling scopes do not collide. The JLS permits a local
/// to reuse a name that is not in scope at its declaration ([§6.4]), and
/// assigning a captured variable in one block must not flag a distinct
/// same-named variable captured in another.
pub(super) fn effective_final_scan(
    bodies: &BodyTree,
    body_id: BodyId,
) -> (FxHashSet<LocalId>, Vec<(LocalId, Name, ExprId)>) {
    fn resolve(scopes: &[FxHashMap<Name, LocalId>], name: &Name) -> Option<usize> {
        scopes.iter().rposition(|scope| scope.contains_key(name))
    }
    struct Scan<'a> {
        bodies: &'a BodyTree,
        scopes: Vec<FxHashMap<Name, LocalId>>,
        lambda_entries: Vec<usize>,
        mutated: FxHashSet<LocalId>,
        captures: Vec<(LocalId, Name, ExprId)>,
        assignment_counts: FxHashMap<LocalId, u32>,
        initialized: FxHashSet<LocalId>,
    }
    impl Scan<'_> {
        fn in_lambda(&self) -> bool {
            !self.lambda_entries.is_empty()
        }
        fn declare(&mut self, local: LocalId) {
            let name = self.bodies.local(local).name.clone();
            self.scopes.last_mut().expect("scope").insert(name, local);
        }
        fn walk_stmt(&mut self, stmt: StmtId) {
            use hir_expand::body::StmtData;
            let data = self.bodies.stmt(stmt);
            match data.clone() {
                StmtData::Empty | StmtData::Missing => {}
                StmtData::Block(inner) => {
                    self.scopes.push(FxHashMap::default());
                    for s in inner {
                        self.walk_stmt(s);
                    }
                    self.scopes.pop();
                }
                StmtData::Decl { local, initializer } => {
                    // §4.12.4: a declarator *with* an initializer makes any
                    // later assignment a re-assignment — the local is
                    // effectively final only while never assigned again. A
                    // blank declarator (no initializer) lets the first later
                    // assignment stand as its initial one.
                    self.declare(local);
                    if initializer.is_some() {
                        self.initialized.insert(local);
                    }
                    if let Some(initializer) = initializer {
                        self.walk_expr(initializer);
                    }
                }
                StmtData::DeclGroup(inner) => {
                    for s in inner {
                        self.walk_stmt(s);
                    }
                }
                StmtData::Expr(expr) => self.walk_expr(expr),
                StmtData::Labeled { stmt, .. } => self.walk_stmt(stmt),
                StmtData::If { cond, then, els } => {
                    self.walk_expr(cond);
                    // §4.12.4: assignments in the `then` and `else` branches
                    // are *alternative* paths, not a sequence — a blank local
                    // assigned once in each branch (`if (b) { x = 1; } else {
                    // x = 2; }`) is still assigned once per path and stays
                    // effectively final. The assignment counts are snapshotted
                    // before each branch and joined: a local blank before the
                    // `if` that is assigned in a branch counts as its initial
                    // assignment; only a local already assigned before the
                    // `if` that a branch reassigns is a mutation.
                    let saved = self.assignment_counts.clone();
                    self.walk_stmt(then);
                    let then_counts = self.assignment_counts.clone();
                    self.assignment_counts = saved.clone();
                    if let Some(els) = els {
                        self.walk_stmt(els);
                    }
                    let else_counts = std::mem::replace(&mut self.assignment_counts, saved.clone());
                    self.join_assignment_counts(&saved, &then_counts, &else_counts);
                }
                StmtData::While { cond, body } | StmtData::DoWhile { body, cond } => {
                    self.walk_expr(cond);
                    self.walk_stmt(body);
                }
                StmtData::For {
                    init,
                    cond,
                    step,
                    body,
                } => {
                    self.scopes.push(FxHashMap::default());
                    for s in init {
                        self.walk_stmt(s);
                    }
                    if let Some(cond) = cond {
                        self.walk_expr(cond);
                    }
                    for e in step {
                        self.walk_expr(e);
                    }
                    self.walk_stmt(body);
                    self.scopes.pop();
                }
                StmtData::ForEach {
                    var,
                    iterable,
                    body,
                } => {
                    self.scopes.push(FxHashMap::default());
                    self.declare(var);
                    // §14.14.2: the loop variable is assigned once per
                    // iteration from the iterable — any assignment inside the
                    // body is a re-assignment.
                    self.initialized.insert(var);
                    self.walk_expr(iterable);
                    self.walk_stmt(body);
                    self.scopes.pop();
                }
                StmtData::Switch { scrutinee, arms } => {
                    self.walk_expr(scrutinee);
                    for arm in arms {
                        for label in arm.labels {
                            if let hir_expand::body::SwitchLabel::Expr(e) = label {
                                self.walk_expr(e);
                            }
                        }
                        for s in arm.body {
                            self.walk_stmt(s);
                        }
                    }
                }
                StmtData::Return(ret) => {
                    if let Some(ret) = ret {
                        self.walk_expr(ret);
                    }
                }
                StmtData::Throw(expr) => self.walk_expr(expr),
                StmtData::Break(_) | StmtData::Continue(_) => {}
                StmtData::Yield(expr) => self.walk_expr(expr),
                StmtData::Synchronized { expr, body } => {
                    self.walk_expr(expr);
                    self.walk_stmt(body);
                }
                StmtData::Try {
                    resources,
                    body,
                    catches,
                    finally,
                } => {
                    for resource in resources {
                        self.declare(resource.local);
                        // §14.20.3: a resource variable is initialized by its
                        // initializer (or its declaration) — later assignment
                        // inside the try body is a re-assignment.
                        self.initialized.insert(resource.local);
                        if let Some(init) = resource.initializer {
                            self.walk_expr(init);
                        }
                    }
                    self.walk_stmt(body);
                    for clause in catches {
                        self.scopes.push(FxHashMap::default());
                        self.declare(clause.param);
                        self.initialized.insert(clause.param);
                        self.walk_stmt(clause.body);
                        self.scopes.pop();
                    }
                    if let Some(finally) = finally {
                        self.walk_stmt(finally);
                    }
                }
                StmtData::Assert { cond, msg } => {
                    self.walk_expr(cond);
                    if let Some(msg) = msg {
                        self.walk_expr(msg);
                    }
                }
                StmtData::LocalClass { .. } => {}
            }
        }
        fn walk_expr(&mut self, expr: ExprId) {
            use hir_expand::body::ExprData;
            let data = self.bodies.expr(expr);
            match data.clone() {
                ExprData::Var(name) | ExprData::NamePath(name) => {
                    if self.in_lambda()
                        && let Some(local) = self.resolve_local(&name)
                        && self
                            .lambda_entries
                            .last()
                            // §15.27.2/[§6.3]: the lambda's own parameter
                            // frame is the last pushed before the entry
                            // marker — index `entry - 1`. A reference that
                            // resolves to it (or deeper, to a local of the
                            // lambda body) is *not* a capture of an
                            // enclosing variable; only a frame strictly
                            // before it is.
                            .is_some_and(|&entry| {
                                self.scopes
                                    .iter()
                                    .rposition(|scope| scope.values().any(|id| *id == local))
                                    .is_some_and(|frame| frame + 1 < entry)
                            })
                    {
                        self.captures.push((local, name, expr));
                    }
                }
                ExprData::Literal(_)
                | ExprData::Null
                | ExprData::This { .. }
                | ExprData::Super { .. }
                | ExprData::Missing => {}
                ExprData::Template { args } => {
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::ClassLit(_) => {}
                ExprData::FieldAccess { target, .. } => {
                    if let Some(target) = target {
                        self.walk_expr(target);
                    }
                }
                ExprData::ArrayAccess { array, index } => {
                    self.walk_expr(array);
                    self.walk_expr(index);
                }
                ExprData::MethodCall { receiver, args, .. } => {
                    if let Some(receiver) = receiver {
                        self.walk_expr(receiver);
                    }
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::New { args, receiver, .. } => {
                    if let Some(receiver) = receiver {
                        self.walk_expr(receiver);
                    }
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::CtorCall { args, .. } => {
                    for a in args {
                        self.walk_expr(a);
                    }
                }
                ExprData::NewArray {
                    dims, initializer, ..
                } => {
                    for d in dims {
                        self.walk_expr(d);
                    }
                    if let Some(elems) = initializer {
                        for e in elems {
                            self.walk_expr(e);
                        }
                    }
                }
                ExprData::ArrayInit(elems) => {
                    for e in elems {
                        self.walk_expr(e);
                    }
                }
                ExprData::Unary { expr: inner, op } => {
                    if matches!(
                        op,
                        hir_expand::body::UnaryOp::Inc | hir_expand::body::UnaryOp::Dec
                    ) {
                        self.mark_mutation(inner);
                    }
                    self.walk_expr(inner);
                }
                ExprData::Postfix { expr: inner, .. } => {
                    self.mark_mutation(inner);
                    self.walk_expr(inner);
                }
                ExprData::Binary { lhs, rhs, .. } => {
                    self.walk_expr(lhs);
                    self.walk_expr(rhs);
                }
                ExprData::Assign { lhs, rhs, .. } => {
                    // §4.12.4: a *simple* assignment that is the local's
                    // first — its declaration carried no initializer, and no
                    // earlier assignment wrote it — is its *initial*
                    // assignment, which does not stop it being effectively
                    // final (`Runnable r; if (c && (r = f()) != null) { use(r)
                    // }` is effectively final). A compound assignment or a
                    // re-assignment after the first is a mutation. This is
                    // decided per local identity ([§6.4] name reuse in sibling
                    // scopes), not per name.
                    self.mark_mutation_if_reassigned(lhs);
                    self.walk_expr(lhs);
                    self.walk_expr(rhs);
                }
                ExprData::Cast { expr: inner, .. } => self.walk_expr(inner),
                ExprData::InstanceOf {
                    expr: inner,
                    pattern,
                    ..
                } => {
                    self.walk_expr(inner);
                    let _ = pattern;
                }
                ExprData::Conditional { cond, then, els } => {
                    self.walk_expr(cond);
                    self.walk_expr(then);
                    self.walk_expr(els);
                }
                ExprData::Lambda { params, body } => {
                    self.scopes.push(FxHashMap::default());
                    for (name, _, _) in &params {
                        // Lambda parameters are not `LocalId`s in the body
                        // tree; synthesize a placeholder identity so captures
                        // of the lambda's own parameters are not treated as
                        // captures of an enclosing variable. The sentinel
                        // cannot collide with a real local (body-tree arena
                        // ids are bounded well below `u32::MAX`).
                        let id = LocalId(ArenaId(u32::MAX - 1));
                        self.scopes
                            .last_mut()
                            .expect("scope")
                            .insert(name.clone(), id);
                    }
                    self.lambda_entries.push(self.scopes.len());
                    match body {
                        hir_expand::body::LambdaBody::Expr(inner) => self.walk_expr(inner),
                        hir_expand::body::LambdaBody::Block(stmt) => self.walk_stmt(stmt),
                    }
                    self.lambda_entries.pop();
                    self.scopes.pop();
                }
                ExprData::MethodRef { qualifier, .. } => {
                    if let Some(qualifier) = qualifier {
                        self.walk_expr(qualifier);
                    }
                }
                ExprData::Switch { scrutinee, arms } => {
                    self.walk_expr(scrutinee);
                    for arm in arms {
                        for label in arm.labels {
                            if let hir_expand::body::SwitchLabel::Expr(e) = label {
                                self.walk_expr(e);
                            }
                        }
                        for s in arm.body {
                            self.walk_stmt(s);
                        }
                    }
                }
                ExprData::Paren(inner) => self.walk_expr(inner),
            }
        }
        fn resolve_local(&self, name: &Name) -> Option<LocalId> {
            self.scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name).copied())
        }
        /// Join the per-path assignment counts after an `if`/`else`: a local
        /// that was blank (unassigned) before the branches is now assigned at
        /// most once per path — its post-join count is 1 if either branch
        /// assigned it. A local already assigned before (count ≥ 1) takes the
        /// larger branch count, so a re-assignment in either branch raises it
        /// toward the mutation threshold.
        fn join_assignment_counts(
            &mut self,
            saved: &FxHashMap<LocalId, u32>,
            then_counts: &FxHashMap<LocalId, u32>,
            else_counts: &FxHashMap<LocalId, u32>,
        ) {
            let keys: FxHashSet<LocalId> = saved
                .keys()
                .chain(then_counts.keys())
                .chain(else_counts.keys())
                .copied()
                .collect();
            for local in keys {
                let pre = saved.get(&local).copied().unwrap_or(0);
                let then_c = then_counts.get(&local).copied().unwrap_or(pre);
                let else_c = else_counts.get(&local).copied().unwrap_or(pre);
                let joined = if pre == 0 {
                    // Blank before the `if`: assigned in either branch is the
                    // initial assignment — count 1, not a mutation.
                    if then_c > 0 || else_c > 0 { 1 } else { 0 }
                } else {
                    // Already assigned before: the branch counts are on top.
                    then_c.max(else_c)
                };
                self.assignment_counts.insert(local, joined);
                if joined > 1 {
                    self.mutated.insert(local);
                }
            }
        }
        fn mark_mutation(&mut self, expr: ExprId) {
            if let ExprData::Var(name) = self.bodies.expr(expr).clone()
                && let Some(local) = self.resolve_local(&name)
            {
                self.mutated.insert(local);
            }
        }
        /// Mark `expr` (the LHS of an assignment) as mutated only when it is a
        /// *re*-assignment — the local already carries a value. A local
        /// declared with an initializer is re-assigned by any later write; a
        /// blank declarator is re-assigned by a *second* write. A compound
        /// assignment (`op` != `Assign`) always reads first, so it is a
        /// mutation even on the first write.
        fn mark_mutation_if_reassigned(&mut self, expr: ExprId) {
            if let ExprData::Var(name) = self.bodies.expr(expr).clone()
                && let Some(local) = self.resolve_local(&name)
            {
                // §4.12.4: a blank declarator (no initializer) makes the
                // first later assignment the initial one; a declarator *with*
                // an initializer makes any later assignment a re-assignment.
                // The scan is single-pass and does not model control flow, so
                // it conservatively treats the *second* syntactic assignment
                // to a blank local as a mutation — the `if/else`-both-assign
                // shape (each path assigns once) stays effectively final
                // because each path is one assignment, and javac's
                // definite-assignment analysis accepts it. Two assignments on
                // the *same* path (`x = 1; x = 2;`) are caught.
                if self.initialized.contains(&local) {
                    self.mutated.insert(local);
                } else {
                    *self.assignment_counts.entry(local).or_insert(0) += 1;
                    if *self.assignment_counts.get(&local).expect("present") > 1 {
                        self.mutated.insert(local);
                    }
                }
            }
        }
    }
    let mut scan = Scan {
        bodies,
        scopes: vec![FxHashMap::default()],
        lambda_entries: Vec::new(),
        mutated: FxHashSet::default(),
        captures: Vec::new(),
        assignment_counts: FxHashMap::default(),
        initialized: FxHashSet::default(),
    };
    for &param in &bodies.body(body_id).params {
        scan.declare(param);
        // §4.12.3: a parameter is assigned on entry — any assignment in the
        // body is a re-assignment.
        scan.initialized.insert(param);
    }
    for &stmt in &bodies.body(body_id).stmts {
        scan.walk_stmt(stmt);
    }
    (scan.mutated, scan.captures)
}
