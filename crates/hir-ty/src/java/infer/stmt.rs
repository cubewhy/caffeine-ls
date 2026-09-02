//! Statement inference ([JLS §14]): local declarations, control flow,
//! loops, `switch`, `try`/`catch`/`finally`, and the definite-assignment
//! flow threading that tracks them.

use hir_expand::{
    body::{ExprData, ExprId, StmtData, StmtId, SwitchLabel},
    span::SpannedTypeRef,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::java::{
    diagnostics::{DiagLocation, TypeError},
    resolve::resolve_type_ref,
    ty::{Ty, TyKind},
};

use super::{BreakFrame, Flow, InferCtx};

impl InferCtx<'_> {
    pub(super) fn infer_block_statements(&mut self, stmts: &[StmtId]) {
        let mut reported_unreachable = false;
        let mut recovered_exit = false;
        for &stmt in stmts {
            if self.exited && !reported_unreachable {
                self.report(TypeError::UnreachableStatement { stmt });
                reported_unreachable = true;
                recovered_exit = true;
                // Keep inferring the tail (as reachable) for its own
                // diagnostics; §14.22 makes it unreachable, so this recovery
                // does not change the block's completion behavior.
                self.exited = false;
            }
            self.infer_stmt(stmt);
        }
        // An earlier abrupt exit still stands: the unreachable tail cannot
        // let the block complete normally.
        if recovered_exit {
            self.exited = true;
        }
    }

    pub(super) fn infer_stmt(&mut self, id: StmtId) {
        let stmt = self.tree.stmt(id).clone();
        self.infer_stmt_data(id, &stmt);
    }

    pub(super) fn infer_stmt_data(&mut self, id: StmtId, stmt: &StmtData) {
        match stmt {
            StmtData::Empty => {}
            StmtData::Block(stmts) => {
                self.scopes.push(FxHashMap::default());
                self.infer_block_statements(stmts);
                self.scopes.pop();
            }
            // §14.4/§6.3: the declarators of one declaration statement share
            // the *enclosing* scope, so this is inferred without pushing one
            // (unlike a [`StmtData::Block`]).
            StmtData::DeclGroup(stmts) => {
                for &stmt in stmts {
                    self.infer_stmt(stmt);
                }
            }
            StmtData::Decl { local, initializer } => {
                // §14.4.1: a `var` declaration has no written type — the
                // initializer is inferred standalone and its type becomes the
                // local's (§15.2: a `var` initializer is never poly). A `var`
                // without an initializer is a compile-time error
                // ([§14.4.1]): report it and degrade the local to `error`
                // rather than panic.
                if self.tree.local(*local).ty.is_none() {
                    let Some(initializer) = initializer else {
                        self.report(TypeError::VarWithoutInitializer { local: *local });
                        let local_data = self.tree.local(*local).clone();
                        self.bind_local(*local, local_data.name, self.error());
                        return;
                    };
                    // §14.4.1: an array initializer has no standalone type, so
                    // `var x = { 1, 2, 3 };` cannot be a `var` declaration.
                    if matches!(self.tree.expr(*initializer).clone(), ExprData::ArrayInit(_)) {
                        let local_data = self.tree.local(*local).clone();
                        self.report(TypeError::VarArrayInitializer { local: *local });
                        self.bind_local(*local, local_data.name, self.error());
                        return;
                    }
                    let ty = self.infer_expr(*initializer);
                    let local_data = self.tree.local(*local).clone();
                    self.bind_local(*local, local_data.name, ty);
                    return;
                }
                self.declare_local(*local);
                // §4.12.2: a declared type naming a generic class without its
                // arguments is a raw type — legal, reported as a warning.
                self.warn_raw_declared_type(*local);
                // §4.5.1: a declared type argument that is not within the
                // bounds of the type parameter it fills.
                if let Some(tyref) = self.tree.local(*local).ty.clone() {
                    self.check_type_argument_bounds(DiagLocation::Local(*local), &tyref);
                }
                if initializer.is_none() {
                    // §16: a declarator without an initializer is not
                    // definitely assigned until a later assignment reaches it.
                    self.flow.definite.remove(local);
                    // §4.12.4: a `final` declarator with no initializer is a
                    // *blank* final — it may still be assigned once by
                    // definite assignment, so it is not an un-assignable
                    // final yet.
                    let local_data = self.tree.local(*local).clone();
                    if local_data.is_final {
                        self.blank_finals.insert(*local);
                    }
                }
                if let Some(initializer) = initializer {
                    // The initializer is a poly expression whose target is the
                    // declared type of the local ([JLS §14.4]).
                    let target = self.locals.get(local).copied();
                    let init_ty = self.with_target(target, |this| this.infer_expr(*initializer));
                    // §4.12.4: a `final` local whose initializer is itself a
                    // constant expression is a *constant variable* — later
                    // reads of it are constant expressions ([§15.28]).
                    let local_data = self.tree.local(*local).clone();
                    if local_data.is_final
                        && let Some(value) = self.const_value(*initializer)
                    {
                        self.const_locals.insert(*local, value);
                    }
                    let (Some(target), false) = (target, init_ty.is_error(self.db)) else {
                        return;
                    };
                    if target.is_error(self.db) {
                        return;
                    }
                    // §10.6: an array-initializer expression in a declaration
                    // gets its element type from the target ([§15.2]) — an
                    // empty `{}` types as the declared array — so its
                    // standalone type (an `error[]` for empty) is not the
                    // assignability operand.
                    if matches!(self.tree.expr(*initializer).clone(), ExprData::ArrayInit(_)) {
                        return;
                    }
                    // §5.2: an int-typed constant expression whose value fits
                    // the target narrows in assignment context ([§5.1.3]).
                    if !crate::java::subtyping::is_assignable(
                        self.db,
                        &self.scope,
                        &init_ty,
                        &target,
                    ) && !self.constant_narrowable(*initializer, init_ty, target)
                    {
                        self.report(TypeError::IncompatibleTypes {
                            expr: *initializer,
                            found: init_ty,
                            expected: target,
                        });
                    }
                    // §5.1.9: a raw source assigned to a parameterized target
                    // is an unchecked conversion — report the warning.
                    self.warn_unchecked(*initializer, &init_ty, &target);
                }
            }
            StmtData::Expr(expr) => {
                let _ = self.infer_expr(*expr);
            }
            // §14.7: a labeled statement puts its label in scope for the
            // nested `break label`/`continue label`; whether it is a loop
            // decides which of the two a labeled `continue` may target.
            StmtData::Labeled { label, stmt } => {
                let name = self.tree.label(*label).0.clone();
                let is_loop = matches!(
                    self.tree.stmt(*stmt),
                    StmtData::While { .. }
                        | StmtData::DoWhile { .. }
                        | StmtData::For { .. }
                        | StmtData::ForEach { .. }
                );
                self.labels.push((name.clone(), is_loop));
                // §14.14: a labeled loop's name identifies its break frame —
                // the loop handler consumes this into the frame it pushes, so
                // `break label` (and a labeled `break outer` from a nested
                // `switch`) records on the right loop.
                let previous = if is_loop {
                    self.pending_loop_label.replace(name.clone())
                } else {
                    None
                };
                if is_loop {
                    self.infer_stmt(*stmt);
                } else {
                    // §14.7/[§14.15]: a labeled *block* (`block6: { … break
                    // block6; … }`) is also a valid `break` target. The
                    // block's `break label` completes the labeled statement
                    // *normally* ([§14.15]) — its flow joins the block's
                    // fall-through — so a break frame records it, exactly like
                    // a loop's breaks ([§16.2.9]).
                    self.loop_breaks.push(BreakFrame::new(Some(name)));
                    self.infer_stmt(*stmt);
                    let frame = self.loop_breaks.pop().expect("frame pushed above");
                    let breaks = frame.joined();
                    // The labeled block's end: the fall-through flow (when the
                    // block completes normally) joined with every break flow.
                    if self.exited {
                        self.flow = breaks.unwrap_or_else(|| self.flow.clone());
                    } else if let Some(breaks) = breaks {
                        self.flow.join_definite(&breaks);
                    }
                    self.exited = false;
                }
                if is_loop {
                    self.pending_loop_label = previous;
                }
                self.labels.pop();
            }
            StmtData::If { cond, then, els } => {
                // §16.2.7: the flow before the condition is the pre-statement
                // flow — the fallback when every branch exits and the code
                // after the `if` is unreachable.
                let before = self.flow.clone();
                let before_exited = self.exited;
                self.check_condition(*cond);
                // §16.1.2–[§16.1.5]: the condition's true flow enters the then
                // arm, its false flow the else arm — an assignment made only on
                // the condition's true flow (e.g. the right operand of a `&&`
                // that runs only when the left matched, JLS Example 16-1) is
                // definitely assigned inside the guarded then arm.
                let (cond_true_flow, cond_false_flow) = self.take_bool_outcomes();
                // §16.1.1: a condition that is a boolean *constant expression*
                // of value `true` (or `false`) can never take the other branch
                // — the impossible branch's flow is vacuous, so only the taken
                // arm constrains the code after the `if`. `final int c = 5;
                // if (c > 2)` folds (JLS Example 16-2's contrast).
                let const_bool = self.const_bool(*cond);
                // §14.30.3: the condition's true-flow pattern bindings are in
                // scope in the `then` arm; its false-flow bindings in the
                // `else` arm.
                let (true_flow, false_flow) = self.pattern_flow(*cond).unwrap_or_default();
                self.scopes.push(FxHashMap::default());
                self.flow = cond_true_flow;
                for binding in &true_flow {
                    self.scope_binding(*binding);
                }
                // §16: after the `if`, a local is definitely assigned only if
                // it is assigned on *both* paths — the then branch and (when
                // present) the else branch; a branch that exits contributes
                // no constraint. A blank `final` field is *touched* if either
                // surviving path assigned it ([§8.3.1.2]): after the `if`, a
                // later write to it is the already-assigned error.
                self.infer_stmt(*then);
                self.scopes.pop();
                let then_flow = std::mem::replace(&mut self.flow, before.clone());
                let mut then_exited = std::mem::replace(&mut self.exited, before_exited);
                // §16.1.1: a constant-`false` condition's then arm can never
                // run — its flow is vacuous, so the arm contributes nothing to
                // the join and the after-`if` state is the else arm's alone.
                if const_bool == Some(false) {
                    then_exited = true;
                }
                // The else path's end state; the else-less form's false path
                // is the condition's false flow. A constant condition makes
                // one branch impossible: its flow is vacuous and it can never
                // reach the join ([§16.1.1]).
                let mut else_flow = cond_false_flow.clone();
                let mut else_exited = before_exited;
                if let Some(els) = els {
                    self.scopes.push(FxHashMap::default());
                    self.flow = cond_false_flow;
                    for binding in &false_flow {
                        self.scope_binding(*binding);
                    }
                    self.infer_stmt(*els);
                    self.scopes.pop();
                    else_flow = std::mem::replace(&mut self.flow, before.clone());
                    else_exited = std::mem::replace(&mut self.exited, before_exited);
                } else if then_exited && !before_exited {
                    // §14.30.3/§16: when the then arm completes abruptly, the
                    // only way past this statement is the condition's false
                    // flow — its pattern bindings stay in scope after it
                    // (`if (!(x instanceof T v)) return;` makes `v` known).
                    self.flow = cond_false_flow;
                    for binding in &false_flow {
                        self.scope_binding(*binding);
                    }
                    else_flow = self.flow.clone();
                }
                // §16.1.1: a constant-`true` condition's else arm (or the
                // else-less false path) can never run — it is vacuous.
                if const_bool == Some(true) {
                    else_exited = true;
                }
                // §16.1/§8.3.1.2: join the surviving paths. A path that
                // completes abruptly never reaches the join; when no path
                // does, the following code is unreachable and `before` stands.
                let then_survives = !then_exited;
                let else_survives = !else_exited;
                match (then_survives, else_survives) {
                    (true, true) => {
                        // Both paths fall through: definite intersects,
                        // touched unions.
                        let mut then_flow = then_flow;
                        then_flow
                            .definite
                            .retain(|local| else_flow.definite.contains(local));
                        then_flow.union_touched(&else_flow);
                        self.flow = then_flow;
                    }
                    (true, false) => {
                        // Only the then path falls through.
                        self.flow = then_flow;
                    }
                    (false, true) => {
                        // Only the else path falls through — it already holds
                        // `before` plus its own writes.
                        self.flow = else_flow;
                    }
                    (false, false) => {
                        // Both exit: the following code is unreachable.
                        self.flow = before.clone();
                    }
                }
                self.exited = then_exited && else_exited;
            }
            StmtData::While { cond, body } => {
                // §16.2.10: the flow before the condition is the pre-loop
                // state — the fallback for the after-loop join.
                let before = self.flow.clone();
                self.check_condition(*cond);
                // §16.1.10: the body may run zero times, so nothing it
                // assigns is definitely assigned after the loop, and a blank
                // `final` field it touches stays *definitely unassigned* after
                // it ([§8.3.1.2]) — a later write is still a fresh
                // initialization, not an already-assigned one.
                // §16.2.10: a `while` whose condition is a constant expression
                // ([§16.1.1]) of value `true` can never complete through the
                // condition — the only way past it is a `break` — so the
                // assignments the body made before each `break` carry past the
                // loop. The body is inferred under the condition's true flow;
                // the loop's end flow is the join of the recorded break flows
                // (JLS Example 16-1).
                let const_bool = self.const_bool(*cond);
                let (cond_true_flow, cond_false_flow) = self.take_bool_outcomes();
                self.loop_depth += 1;
                self.loop_breaks
                    .push(BreakFrame::new(self.pending_loop_label.take()));
                if const_bool == Some(true) {
                    self.flow = cond_true_flow;
                }
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                self.flow = if const_bool == Some(true) {
                    // Only the break paths survive: a constant-true loop never
                    // reaches the join through its condition. When no break was
                    // recorded the loop cannot complete normally, so the
                    // pre-loop state stands (the code after is unreachable).
                    frame.joined().unwrap_or(before)
                } else {
                    // §16.2.10: a non-constant loop exits *through its
                    // condition being false* — the after-loop flow is the
                    // condition's false flow, not the pre-condition state. The
                    // condition expression is evaluated before each test, so an
                    // assignment inside it (e.g. `while (contains(x = next())
                    // || …)`) is definitely assigned after the loop even when
                    // the body never runs (JLS Example 16-1).
                    cond_false_flow
                };
                self.exited = false;
            }
            StmtData::DoWhile { body, cond } => {
                // §16.1.11: a do-loop's body runs at least once, so its
                // assignments carry past the loop when the body falls
                // through; an exiting body constrains nothing. A do-loop whose
                // condition is a constant `true` ([§16.1.1]) never exits
                // through the condition — only its `break` paths reach the
                // join ([§16.2.11]), like a constant-true `while`.
                let before = self.flow.clone();
                let const_bool = self.const_bool(*cond);
                self.loop_depth += 1;
                self.loop_breaks
                    .push(BreakFrame::new(self.pending_loop_label.take()));
                self.infer_stmt(*body);
                if const_bool == Some(true) {
                    // The body's fall-through feeds the condition (always
                    // true); only the recorded breaks escape the loop.
                } else if self.exited {
                    self.flow = before.clone();
                }
                self.check_condition(*cond);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                if const_bool == Some(true) {
                    self.flow = frame.joined().unwrap_or(before);
                }
                self.exited = false;
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
                // A missing condition is an implicit constant `true`
                // ([§16.2.12]: a condition-less `for (;;)` never completes
                // through a false value).
                let const_bool = match cond {
                    Some(c) => self.const_bool(*c),
                    None => Some(true),
                };
                if let Some(cond) = cond {
                    self.check_condition(*cond);
                }
                // §16.2.10: the body runs under the condition's *true* flow
                // when the condition is a constant `true`; capture the
                // outcomes right here — the step expressions below would
                // overwrite [`Self::bool_outcomes`].
                let (cond_true_flow, cond_false_flow) = self.take_bool_outcomes();
                for &step in step {
                    let _ = self.infer_expr(step);
                }
                // §16.1.14: like `while`, the body may run zero times — except
                // a `for` whose condition is a constant `true` **or absent**,
                // which — like a constant-true `while` ([§16.2.12]) — escapes
                // only through its `break` paths.
                let before = self.flow.clone();
                self.loop_depth += 1;
                self.loop_breaks
                    .push(BreakFrame::new(self.pending_loop_label.take()));
                // §16.2.12: the body runs *only when the condition is true*, so
                // even for a non-constant condition it is inferred under the
                // condition's true flow — an assignment made on that flow
                // (`for (…; i < n && (c = s.charAt(i)) != -1; …)` uses `c` in
                // the body) is definitely assigned there.
                self.flow = cond_true_flow;
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                let frame = self.loop_breaks.pop().expect("frame pushed above");
                self.flow = if const_bool == Some(true) {
                    frame.joined().unwrap_or(before)
                } else {
                    // §16.2.12: a non-constant loop exits through its
                    // condition being false — the condition's false flow
                    // carries assignments made while evaluating it.
                    cond_false_flow
                };
                self.exited = false;
                self.scopes.pop();
            }
            StmtData::ForEach {
                var,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr(*iterable);
                let element = if iterable_ty.is_array(self.db) {
                    iterable_ty
                        .element(self.db)
                        .copied()
                        .unwrap_or_else(|| self.error())
                } else {
                    // §14.14.2.1: the expression must be `Iterable<T>`; the
                    // loop variable takes `T`, the element type of the
                    // `Iterator<E>` that `iterator()` returns.
                    match self.iterable_element(iterable_ty) {
                        Some(element) => element,
                        None => {
                            if !iterable_ty.is_error(self.db) {
                                // §14.14.2: a for-each over a non-iterable
                                // reference type is a compile-time error.
                                self.report(TypeError::NonIterableForEach {
                                    expr: *iterable,
                                    found: iterable_ty,
                                });
                            }
                            self.error()
                        }
                    }
                };
                self.scopes.push(FxHashMap::default());
                self.declare_local_ty(*var, element);
                // §16.1.11: like `while`, the body may run zero times. A
                // labeled `break label` still needs the frame to record on
                // (a for-each is never constant-condition, so the recorded
                // flows are only used for the label bookkeeping).
                let before = self.flow.clone();
                self.loop_depth += 1;
                self.loop_breaks
                    .push(BreakFrame::new(self.pending_loop_label.take()));
                self.infer_stmt(*body);
                self.loop_depth -= 1;
                self.loop_breaks.pop();
                self.flow = before;
                self.exited = false;
                self.scopes.pop();
            }
            StmtData::Switch { scrutinee, arms } => {
                let selector = self.infer_switch_selector(*scrutinee);
                self.case_values.push(FxHashMap::default());
                self.scopes.push(FxHashMap::default());
                self.switch_depth += 1;
                // §14.22: every arm is an alternative flow path starting from
                // the pre-switch state; the switch completes normally iff at
                // least one arm completes normally, and a local is definitely
                // assigned after the switch only when it is assigned on every
                // normal-completing arm ([§16.1.9]). A blank `final` field is
                // touched if any surviving arm touched it ([§8.3.1.2]).
                //
                // §14.21: a switch *statement* with no `default` label can
                // complete normally even when *every* arm ends abruptly — the
                // selector may match no arm, so the statement is one more
                // normal-completing path ([JLS §14.21: "A switch statement
                // without a default label can complete normally only if it
                // contains at least one ... `switch` block statement group" —
                // more precisely the group or rule may be absent]). The
                // `default` label lowers as a `Missing` expression
                // ([`SwitchLabel::Expr`]), which marks the arm as the default.
                let has_default = arms.iter().any(|arm| {
                    arm.labels.iter().any(|label| {
                        matches!(
                            label,
                            SwitchLabel::Expr(e)
                                if matches!(self.tree.expr(*e).clone(), ExprData::Missing)
                        )
                    })
                });
                let before = self.flow.clone();
                let before_exited = self.exited;
                // §16.2.9: a `break` targeting the switch completes it
                // *normally* ([§14.15]) — its flow is one of the
                // normal-completing arm paths, not an abrupt exit. The frame
                // collects those flows so the arm's "exit" does not drop them.
                self.loop_breaks.push(BreakFrame::new(None));
                let mut paths: Vec<(Flow, bool)> = Vec::new();
                for arm in arms {
                    self.flow = before.clone();
                    self.exited = before_exited;
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
                        self.infer_stmt(stmt);
                    }
                    let end_state = std::mem::replace(&mut self.flow, before.clone());
                    let exits = std::mem::replace(&mut self.exited, before_exited);
                    paths.push((end_state, exits));
                    self.scopes.pop();
                }
                // The switch's break flows are normal-completing paths.
                let break_frame = self.loop_breaks.pop().expect("frame pushed above");
                let breaks = break_frame.joined();
                // The join of §16.1.9: only normal-completing arms reach the
                // statement after the switch — arms that fall through, plus
                // the break paths ([§16.2.9]); when none does, the switch
                // completes abruptly and the following code is unreachable.
                let mut live_joined: Option<Flow> = None;
                for (path, exited) in &paths {
                    if *exited {
                        continue;
                    }
                    match &mut live_joined {
                        None => live_joined = Some(path.clone()),
                        Some(acc) => acc.join_definite(path),
                    }
                }
                if let Some(ref breaks) = breaks {
                    match &mut live_joined {
                        None => live_joined = Some(breaks.clone()),
                        Some(acc) => acc.join_definite(breaks),
                    }
                }
                let any_normal = live_joined.is_some();
                self.flow = live_joined.unwrap_or_else(|| before.clone());
                // §14.21/[§16.2.9]: the switch completes normally when any
                // fall-through arm or break path reaches the join. A switch
                // *statement* without a `default` always has one more
                // normal-completing path — the empty no-match run — so it
                // completes normally regardless of how its arms exit; with a
                // `default` it completes normally exactly when some arm does.
                // A switch *expression* ([§15.28]) is exhaustive by
                // construction, so its completion is exactly the arm join.
                self.exited = if !has_default {
                    false
                } else if any_normal {
                    false
                } else {
                    paths.iter().all(|(_, exited)| *exited)
                };
                self.scopes.pop();
                self.case_values.pop();
                self.switch_depth -= 1;
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
                let ty = self.with_target(target, |this| this.infer_expr(*expr));
                // §15.27.3: inside a speculative block-lambda body the valued
                // returns are its result expressions — record them so
                // [`Self::infer_lambda_body_result`] can constrain the SAM
                // return type.
                if matches!(stmt, StmtData::Return(_))
                    && let Some(frame) = self.lambda_returns.last_mut()
                {
                    frame.push(ty);
                }
                if matches!(stmt, StmtData::Return(_)) {
                    match self.enclosing_ret {
                        // §14.17: a value returned from a `void` method or a
                        // constructor is an error.
                        None => {
                            if !ty.is_error(self.db) {
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty,
                                    expected: Ty::void(self.db),
                                });
                            }
                        }
                        Some(ret) => {
                            if !ty.is_error(self.db)
                                && !ret.is_error(self.db)
                                && !crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, &ret)
                                // §5.2: an in-range int constant narrows to a
                                // primitive return type ([§5.1.3]).
                                && !self.constant_narrowable(*expr, ty, ret)
                            {
                                // §14.17: the returned value must be assignable
                                // to the return type ([§5.2]).
                                self.report(TypeError::IncompatibleTypes {
                                    expr: *expr,
                                    found: ty,
                                    expected: ret,
                                });
                            }
                        }
                    }
                }
                // §16: a `return` exits this path; a `yield` stays inside its
                // switch expression.
                if matches!(stmt, StmtData::Return(_)) {
                    self.exited = true;
                }
            }
            StmtData::Throw(expr) => {
                // §14.18: the operand of a `throw` statement is not a poly
                // expression ([JLS §15.2]) — it is inferred standalone — and
                // must be assignable to `Throwable` ([§5.2]); a non-throwable
                // operand marks the expression as an error.
                let ty = self.infer_expr(*expr);
                let throwable = Ty::reference(self.db, "java.lang.Throwable", Vec::new());
                if !ty.is_error(self.db)
                    && !crate::java::subtyping::is_assignable(self.db, &self.scope, &ty, &throwable)
                {
                    self.types.insert(*expr, self.error());
                    // §14.18: a non-throwable operand is a compile-time error.
                    self.report(TypeError::IncompatibleTypes {
                        expr: *expr,
                        found: ty,
                        expected: throwable,
                    });
                } else if let ExprData::Var(name) = self.tree.expr(*expr).clone()
                    && let Some(local) = self.lookup_local(&name)
                    && let Some(precise) = self.rethrow_sets.get(&local).cloned()
                {
                    // §11.2.2: a `throw` of an (effectively final) catch
                    // parameter throws precisely the checked exceptions the
                    // try block can throw and the clause can catch — not the
                    // parameter's declared type. An empty set means only
                    // unchecked exceptions can reach the parameter, so the
                    // throw adds no liability.
                    for ty in precise {
                        self.thrown.push((ty, *expr));
                    }
                } else if self.is_checked(&ty) {
                    // §11.2.2/§14.18: a `throw` of a checked exception adds it
                    // to the enclosing liability.
                    self.thrown.push((ty, *expr));
                }
                // §16: control does not continue past a `throw` on this path.
                self.exited = true;
            }
            StmtData::Return(None) => {
                // §16: control does not continue past an exit on this path.
                self.exited = true;
            }
            // §14.15: `break` exits the nearest enclosing `switch` or loop
            // ([§14.11.1]), or the statement named by its label.
            StmtData::Break(label) => {
                self.exited = true;
                match label {
                    Some(label) => {
                        let name = self.tree.label(*label).0.clone();
                        // §14.15: a labeled `break` requires the label of an
                        // enclosing statement in scope.
                        if !self.labels.iter().any(|(n, _)| *n == name) {
                            self.report(TypeError::UndefinedLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            });
                        }
                        // §16.2.10: the break's flow joins the after-loop state
                        // of a constant-condition loop (JLS Example 16-1).
                        self.record_break_flow(Some(&name));
                    }
                    None => {
                        // §14.15: an unlabeled `break` needs an enclosing
                        // `switch` or loop to exit.
                        if self.loop_depth == 0 && self.switch_depth == 0 {
                            self.report(TypeError::BreakOutsideSwitchOrLoop { stmt: id });
                        }
                        self.record_break_flow(None);
                    }
                }
            }
            // §14.16: `continue` skips to the next iteration of the nearest
            // enclosing loop (or the loop named by its label).
            StmtData::Continue(label) => {
                self.exited = true;
                match label {
                    Some(label) => {
                        let name = self.tree.label(*label).0.clone();
                        // §14.16: a labeled `continue` requires a labeled
                        // *loop* in scope.
                        match self.labels.iter().rev().find(|(n, _)| *n == name) {
                            Some((_, is_loop)) if *is_loop => {}
                            Some(_) => self.report(TypeError::NotALoopLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            }),
                            None => self.report(TypeError::UndefinedLabel {
                                stmt: id,
                                label: name.as_str().to_owned(),
                            }),
                        }
                    }
                    None => {
                        // §14.16: an unlabeled `continue` needs an enclosing
                        // loop.
                        if self.loop_depth == 0 {
                            self.report(TypeError::ContinueOutsideLoop { stmt: id });
                        }
                    }
                }
            }
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
                // §11.2.3: the liability index is snapshotted before the
                // *resource initializers* as well as the body — exceptions
                // thrown while acquiring a resource gate the catch clauses
                // exactly like exceptions from the block itself.
                let thrown_before: FxHashSet<ExprId> =
                    self.thrown.iter().map(|(_, expr)| *expr).collect();
                // §14.20.3: every resource is closed at the end of the try —
                // an implicit invocation of its `close()` method whose checked
                // exceptions are thrown by the statement exactly like the
                // block's. They are collected here (the resource's declared
                // type, after `var` inference) so the catch-reachability check
                // below can see them: `catch (IOException)` around
                // `try (InputStream s = ...)` is legal because the implicit
                // close can throw it, even when the block body throws nothing.
                let mut resource_tys: Vec<Ty> = Vec::new();
                for resource in resources {
                    // §14.20.3: each declaration resource is a local variable
                    // declaration; a `var` resource infers its type from its
                    // initializer ([§14.4.1]).
                    if self.tree.local(resource.local).ty.is_none() {
                        let Some(initializer) = resource.initializer else {
                            self.declare_local(resource.local);
                            continue;
                        };
                        let ty = self.infer_expr(initializer);
                        let local = self.tree.local(resource.local).clone();
                        self.bind_local(resource.local, local.name, ty);
                        resource_tys.push(ty);
                    } else {
                        self.declare_local(resource.local);
                        if let Some(initializer) = resource.initializer {
                            // The initializer is a poly expression whose
                            // target is the resource's declared type.
                            let target = self.locals.get(&resource.local).copied();
                            let _ = self.with_target(target, |this| this.infer_expr(initializer));
                        }
                        if let Some(ty) = self.locals.get(&resource.local).copied() {
                            resource_tys.push(ty);
                        }
                    }
                    // §14.20.3: a resource's type must be a subtype of
                    // `java.lang.AutoCloseable` — an exception is thrown if it
                    // is not ([§14.20.3]). Reported at the resource
                    // initializer (javac's caret), IntelliJ-style. A resource
                    // type that does not resolve on the classpath is already
                    // reported (`CannotResolveType`), so it is skipped.
                    let auto_closeable =
                        Ty::reference(self.db, "java.lang.AutoCloseable", Vec::new());
                    if let Some(resource_ty) = self.locals.get(&resource.local).copied()
                        && let Some(initializer) = resource.initializer
                        && !resource_ty.is_error(self.db)
                        && (match resource_ty.kind(self.db) {
                            TyKind::Reference { name, .. } => {
                                hir::fqn_resolve(self.db, &self.scope, name.as_str()).is_some()
                            }
                            _ => true,
                        })
                        && !crate::java::subtyping::is_assignable(
                            self.db,
                            &self.scope,
                            &resource_ty,
                            &auto_closeable,
                        )
                    {
                        self.report(TypeError::IncompatibleTypes {
                            expr: initializer,
                            found: resource_ty,
                            expected: auto_closeable,
                        });
                    }
                }
                // §16.1.8: the try block may exit via any catch, so a local
                // is definitely assigned after the statement only if it is
                // assigned at the end of *every* path — the intersection of
                // the try block and each catch clause. A `finally` always
                // runs, so its assignments override.
                let before = self.flow.clone();
                let before_exited = self.exited;
                self.infer_stmt(*body);
                // §14.20.3: the implicit `close()` of each resource is part of
                // the try statement — it runs when the block completes and its
                // checked exceptions are thrown by the statement ([§11.2.3]),
                // even when the block itself throws nothing. Pushing them to
                // the pending liability (attributed to the resource initializer
                // expression, which is not part of this try's `thrown_before`)
                // makes both this statement's catch-reachability check and an
                // *enclosing* try/catch see them: `catch (IOException)` around
                // `try (InputStream s = ...)` is legal because the implicit
                // close can throw it.
                let mut close_thrown: Vec<(Ty, ExprId)> = Vec::new();
                for (resource_ty, resource) in resource_tys.iter().zip(resources) {
                    let members = crate::java::method::member_set(
                        self.db,
                        &self.scope,
                        resource_ty,
                        "close",
                        &self.access,
                    );
                    for close in members {
                        if close.params.is_empty() {
                            for thrown in &close.throws {
                                if self.is_checked(thrown)
                                    && let Some(expr) = resource.initializer
                                {
                                    close_thrown.push((thrown.clone(), expr));
                                }
                            }
                            break;
                        }
                    }
                }
                self.thrown.extend(close_thrown.iter().copied());
                // The end-of-body state is one path; each catch clause adds
                // another, starting from the pre-try state.
                // Each path is its end state plus whether the path reaches
                // the join at all (a clause that completed abruptly does not).
                let mut paths: Vec<(Flow, bool)> = vec![(self.flow.clone(), self.exited)];
                let mut all_exits = self.exited;
                // exceptions assignable to its declared type; a clause whose
                // type is a subtype of an *earlier* clause's type is
                // unreachable ([§14.20]).
                // §11.2.3: the checked exceptions thrown by the try block —
                // the liability it *added* while being inferred. Entries from
                // before the try may have been drained by a nested catch in
                // the meantime (a discharge removes them wherever they sit),
                // so diff by the throwing expression's id rather than by
                // index.
                let try_thrown: Vec<Ty> = self
                    .thrown
                    .iter()
                    .filter(|(_, expr)| !thrown_before.contains(expr))
                    .map(|(ty, _)| ty.clone())
                    .collect();
                let mut catch_tys: Vec<Ty> = Vec::new();
                for clause in catches {
                    // Each catch clause is an alternative path that starts
                    // from the pre-try state ([§16.1.8]).
                    self.flow = before.clone();
                    self.exited = before_exited;
                    self.scopes.push(FxHashMap::default());
                    // The entries still pending when this clause begins — the
                    // basis of its precise rethrow set ([§11.2.2]); captured
                    // before the clause discharges any of them.
                    let outstanding: Vec<(Ty, ExprId)> = self.thrown.clone();
                    // §14.20: a multi-catch parameter declares several
                    // alternative types; each gates the clause and discharges
                    // independently.
                    let clause_tys: Vec<Ty> = {
                        let declared = &self.tree.local(clause.param).ty;
                        let types: &[SpannedTypeRef] = if clause.param_types.is_empty() {
                            declared.as_slice()
                        } else {
                            &clause.param_types
                        };
                        types
                            .iter()
                            .map(|ty| resolve_type_ref(self.db, &self.scope, &self.resolver, ty))
                            .filter(|ty| !ty.is_error(self.db))
                            .collect()
                    };
                    if clause_tys.is_empty() {
                        // An unresolvable parameter type: the clause still
                        // infers its body for its own diagnostics.
                        self.declare_local(clause.param);
                        self.infer_stmt(clause.body);
                        let end_state = std::mem::replace(&mut self.flow, before.clone());
                        let exits = std::mem::replace(&mut self.exited, before_exited);
                        paths.push((end_state, exits));
                        all_exits &= exits;
                        self.scopes.pop();
                        continue;
                    }
                    // §14.20: a catch parameter type must be a class, never a
                    // type variable ([§4.4]) — a type parameter is not a class,
                    // so `catch (T t)` is rejected.
                    if clause_tys
                        .iter()
                        .any(|ty| matches!(ty.kind(self.db), TyKind::TypeVar { .. }))
                    {
                        self.report(TypeError::CannotCatchTypeVariable {
                            local: clause.param,
                        });
                    }
                    if clause_tys.iter().all(|clause_ty| {
                        catch_tys.iter().any(|earlier| {
                            crate::java::subtyping::is_subtype(
                                self.db,
                                &self.scope,
                                &clause_ty.clone(),
                                earlier,
                            )
                        })
                    }) {
                        // §11.2.3: every alternative is already caught by an
                        // earlier clause.
                        self.report(TypeError::AlreadyCaught {
                            local: clause.param,
                            caught: clause_tys.clone(),
                        });
                    } else {
                        // §11.2.3: an alternative may name a *checked*
                        // exception only when it is related by subtyping to
                        // something the try block can throw — either the
                        // block can throw a class assignable to it, or it is
                        // itself assignable to a thrown class (a defensive
                        // catch of a thrown exception's subclass). Unchecked
                        // types are always fair game, and so are `Exception`
                        // and its superclasses: a catch-all must stay legal
                        // even when the block provably throws nothing.
                        // A multi-catch with some dead and some live
                        // alternatives keeps the live ones — javac reports
                        // the dead alternatives individually.
                        let exception = Ty::reference(self.db, "java.lang.Exception", Vec::new());
                        for clause_ty in &clause_tys {
                            let covers_unchecked = crate::java::subtyping::is_assignable(
                                self.db,
                                &self.scope,
                                &exception,
                                clause_ty,
                            );
                            if self.is_checked(clause_ty)
                                && !covers_unchecked
                                && !try_thrown.iter().any(|thrown| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        thrown,
                                        clause_ty,
                                    )
                                })
                                && !try_thrown.iter().any(|thrown| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        clause_ty,
                                        thrown,
                                    )
                                })
                            {
                                self.report(TypeError::CatchNeverThrown {
                                    local: clause.param,
                                    caught: *clause_ty,
                                });
                            }
                        }
                        self.thrown.retain(|(thrown, _)| {
                            !clause_tys.iter().any(|clause_ty| {
                                crate::java::subtyping::is_assignable(
                                    self.db,
                                    &self.scope,
                                    thrown,
                                    clause_ty,
                                )
                            })
                        });
                        catch_tys.extend(clause_tys.iter().cloned());
                    }
                    // §11.2.2: the clause's precise rethrow set — the
                    // checked exceptions the try block can throw that are
                    // assignable to this clause and were *not* discharged by
                    // an earlier one (they are no longer pending). A `throw`
                    // of the parameter inside the body throws these, provided
                    // the parameter stays effectively final.
                    let precise: Vec<Ty> = outstanding
                        .iter()
                        .filter(|(thrown, expr)| {
                            !thrown_before.contains(expr)
                                && clause_tys.iter().any(|clause_ty| {
                                    crate::java::subtyping::is_assignable(
                                        self.db,
                                        &self.scope,
                                        thrown,
                                        clause_ty,
                                    )
                                })
                        })
                        .map(|(ty, _)| ty.clone())
                        .collect();
                    if !precise.is_empty() {
                        self.rethrow_sets.insert(clause.param, precise);
                    }
                    self.declare_local(clause.param);
                    self.infer_stmt(clause.body);
                    // The clause's end state joins the other paths.
                    let end_state = std::mem::replace(&mut self.flow, before.clone());
                    let exits = std::mem::replace(&mut self.exited, before_exited);
                    paths.push((end_state, exits));
                    all_exits &= exits;
                    self.scopes.pop();
                }
                // The intersection of every *live* path: a local is
                // definitely assigned after the try only if each path that
                // reaches the join assigned it ([§16.1.8]). A path whose
                // clause completed abruptly (threw or returned) never reaches
                // the join and constrains nothing; when no path reaches it,
                // the following code is unreachable and the pre-try state
                // stands.
                let mut live_joined: Option<Flow> = None;
                for (path, exited) in &paths {
                    if *exited {
                        continue;
                    }
                    match &mut live_joined {
                        None => live_joined = Some(path.clone()),
                        Some(acc) => acc.join_definite(path),
                    }
                }
                self.flow = live_joined.unwrap_or_else(|| before.clone());
                self.exited = all_exits;
                self.scopes.pop();
                if let Some(finally) = finally {
                    // §16.1.8/§14.20.2: the `finally` block *always* runs, even
                    // when the try block and every catch clause complete
                    // abruptly — so it is entered *reachable* regardless of
                    // `all_exits` (reporting its first statement as
                    // unreachable would be a false positive). Its end state is
                    // the try statement's end state: its assignments override
                    // the joined set, and the statement completes abruptly iff
                    // the finally completes abruptly, otherwise exactly as the
                    // joined paths did.
                    let before_finally_exited = self.exited;
                    self.exited = false;
                    self.infer_stmt(*finally);
                    let finally_exited = self.exited;
                    self.exited = finally_exited || before_finally_exited;
                }
            }
            StmtData::Assert { cond, msg } => {
                // §14.16: the assertion condition must be a boolean.
                self.check_condition(*cond);
                if let Some(msg) = msg {
                    let _ = self.infer_expr(*msg);
                }
            }
            // §14.3: a local class declaration declares a type, not a value —
            // it has no effect on expression typing.
            StmtData::LocalClass { .. } => {}
            StmtData::Missing => {}
        }
    }
}
