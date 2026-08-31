//! The syntactic scan behind the effectively-final capture check
//! ([JLS §6.5.6.1], [§15.27.2]): a local captured by a lambda that is also
//! mutated anywhere in its scope is not effectively final.

use hir_expand::{
    body::{BodyId, BodyTree, ExprData, ExprId, StmtId},
    name::Name,
};
use rustc_hash::FxHashSet;

/// §6.5.6.1/[§15.27.2]: a body-tree scan — independent of inference — that
/// finds (a) every local variable that is *mutated* anywhere in the body
/// (`mutated`, keyed by name), and (b) every reference to an enclosing local
/// inside a lambda expression (`captures`), with the reference's expression.
/// A local that is both captured by a lambda and mutated anywhere in its scope
/// is not *effectively final*, and every capture of it is an error.
pub(super) fn effective_final_scan(
    bodies: &BodyTree,
    body_id: BodyId,
) -> (FxHashSet<Name>, Vec<(Name, ExprId)>) {
    fn resolve<'a>(scopes: &'a [FxHashSet<Name>], name: &Name) -> Option<usize> {
        scopes.iter().rposition(|scope| scope.contains(name))
    }
    struct Scan<'a> {
        bodies: &'a BodyTree,
        scopes: Vec<FxHashSet<Name>>,
        lambda_entries: Vec<usize>,
        mutated: FxHashSet<Name>,
        captures: Vec<(Name, ExprId)>,
    }
    impl Scan<'_> {
        fn in_lambda(&self) -> bool {
            !self.lambda_entries.is_empty()
        }
        fn walk_stmt(&mut self, stmt: StmtId) {
            use hir_expand::body::StmtData;
            let data = self.bodies.stmt(stmt);
            match data.clone() {
                StmtData::Empty | StmtData::Missing => {}
                StmtData::Block(inner) => {
                    self.scopes.push(FxHashSet::default());
                    for s in inner {
                        self.walk_stmt(s);
                    }
                    self.scopes.pop();
                }
                StmtData::Decl { local, initializer } => {
                    let name = self.bodies.local(local).name.clone();
                    self.scopes.last_mut().expect("scope").insert(name);
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
                    self.walk_stmt(then);
                    if let Some(els) = els {
                        self.walk_stmt(els);
                    }
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
                    self.scopes.push(FxHashSet::default());
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
                    self.scopes.push(FxHashSet::default());
                    let name = self.bodies.local(var).name.clone();
                    self.scopes.last_mut().expect("scope").insert(name);
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
                        let name = self.bodies.local(resource.local).name.clone();
                        self.scopes.last_mut().expect("scope").insert(name);
                        if let Some(init) = resource.initializer {
                            self.walk_expr(init);
                        }
                    }
                    self.walk_stmt(body);
                    for clause in catches {
                        self.scopes.push(FxHashSet::default());
                        let name = self.bodies.local(clause.param).name.clone();
                        self.scopes.last_mut().expect("scope").insert(name);
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
                        && resolve(&self.scopes, &name).is_some_and(|frame| {
                            self.lambda_entries
                                .last()
                                // §15.27.2/[§6.3]: the lambda's own parameter
                                // frame is the last pushed before the entry
                                // marker — index `entry - 1`. A reference that
                                // resolves to it (or deeper, to a local of the
                                // lambda body) is *not* a capture of an
                                // enclosing variable; only a frame strictly
                                // before it is.
                                .is_some_and(|&entry| frame + 1 < entry)
                        })
                    {
                        self.captures.push((name, expr));
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
                    self.mark_mutation(lhs);
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
                    self.scopes.push(FxHashSet::default());
                    for (name, _, _) in &params {
                        self.scopes.last_mut().expect("scope").insert(name.clone());
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
        fn mark_mutation(&mut self, expr: ExprId) {
            if let ExprData::Var(name) = self.bodies.expr(expr).clone() {
                self.mutated.insert(name);
            }
        }
    }
    let mut scan = Scan {
        bodies,
        scopes: vec![FxHashSet::default()],
        lambda_entries: Vec::new(),
        mutated: FxHashSet::default(),
        captures: Vec::new(),
    };
    for &param in &bodies.body(body_id).params {
        let name = bodies.local(param).name.clone();
        scan.scopes.last_mut().expect("scope").insert(name);
    }
    for &stmt in &bodies.body(body_id).stmts {
        scan.walk_stmt(stmt);
    }
    (scan.mutated, scan.captures)
}
