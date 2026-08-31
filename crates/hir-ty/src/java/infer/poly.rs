//! The poly-expression and argument-shape helpers of method invocation
//! inference ([JLS §15.2], [§18.5.2.4]): what makes an expression poly, the
//! argument kinds contributed to a candidate's constraint table, and the
//! recovery that re-infers poly arguments standalone.

use hir_expand::body::{BodyTree, ExprData, ExprId, LambdaBody, StmtData, StmtId};

use crate::java::{method::MethodData, ty::Ty};

use super::InferCtx;

use crate::java::method::Access;

/// Whether `expr` is a poly expression ([JLS §15.2]): a lambda or method
/// reference, or a parenthesized or conditional expression whose arms are
/// poly. Such an expression has no standalone type; its type is the target
/// functional interface ([JLS §15.27.3]).
pub(super) fn expr_is_poly(tree: &BodyTree, id: ExprId) -> bool {
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
pub(super) fn expr_is_poly_ext(tree: &BodyTree, id: ExprId) -> bool {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. } | ExprData::MethodRef { .. } | ExprData::MethodCall { .. } => true,
        // §15.9.3: a diamond class instance creation is a poly expression in
        // an invocation or assignment context.
        ExprData::New { diamond: true, .. } => true,
        ExprData::Paren(inner) => expr_is_poly_ext(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            expr_is_poly_ext(tree, then) && expr_is_poly_ext(tree, els)
        }
        _ => false,
    }
}

/// Whether `expr` is (possibly parenthesized) a method invocation, used to
/// recognize conditional expressions whose arms are poly invocations.
pub(super) fn expr_is_call(tree: &BodyTree, id: ExprId) -> bool {
    match tree.expr(id).clone() {
        ExprData::MethodCall { .. } => true,
        ExprData::Paren(inner) => expr_is_call(tree, inner),
        _ => false,
    }
}

/// The form of a method reference ([JLS §15.13.1]): how its target's single
/// abstract method's parameters map onto the referenced method's. A *static*
/// reference (`Type::m` naming a static member) and a *bound* reference
/// (`expr::m` — the qualifier value is the receiver) take the SAM's parameters
/// as the method's own; an *unbound* instance reference (`Type::m` naming an
/// instance member) takes the SAM's first parameter as the receiver
/// ([§15.13.3]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodRefKind {
    Static,
    Unbound,
    Bound,
}

/// The kinds of the actual arguments of a method invocation for the joint
/// inference of §18.5.2.4: a concrete argument has a standalone type; a poly
/// argument is a lambda or method reference deferred to its target formal
/// ([JLS §18.5.2.2], [§15.27.3], [§15.13.2]), a nested method invocation
/// whose inference shares the enclosing invocation's table
/// ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4)),
/// or a diamond class instance creation, which is a poly expression in an
/// invocation context ([JLS §15.9.3]).
#[derive(Clone)]
pub(super) enum ArgKind {
    /// An argument with a concrete standalone type.
    Concrete(Ty),
    /// A lambda or method reference; the arity check of §15.12.2.2/§15.12.2.3
    /// is run against the target formal's single abstract method. A method
    /// reference is not arity-checkable without resolving the referenced
    /// method, so its arity is `None`. The lambda's body additionally
    /// constrains the SAM return type's instantiation ([JLS §18.5.2.2]).
    Lambda { id: ExprId, arity: Option<usize> },
    /// A nested method invocation, resolved against the target formal by
    /// contributing its constraints to the enclosing invocation's table.
    Invocation { id: ExprId },
    /// A diamond `new Foo<>()` in an invocation context
    /// ([JLS §15.9.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.3)):
    /// its type arguments are inferred from the enclosing invocation's formal
    /// — `synchronizedList(new ArrayList<>())` against a `List<String>`
    /// target. The created class's type variables are registered in the
    /// shared table and constrained by the formal.
    DiamondNew { id: ExprId },
}

/// One actual argument of an invocation: its poly leaves — each contributing a
/// constraint to the candidate's inference — and whether the argument itself is
/// a poly expression whose type is the target formal and so must be re-inferred
/// against it after resolution ([JLS §18.5.2.4]).
pub(super) struct ArgInfo {
    /// The argument expression, re-inferred against the resolved formal.
    pub(super) id: ExprId,
    /// Whether the argument is a poly expression: its type is the target
    /// formal, so it is deferred to the post-resolution re-inference.
    pub(super) poly: bool,
    /// The poly leaves of the argument ([JLS §15.2]), each contributed against
    /// the formal during candidate probing. A concrete argument has a single
    /// `Concrete` leaf.
    pub(super) leaves: Vec<ArgKind>,
}

/// An applicable candidate in [`InferCtx::choose_candidate`]: the declared
/// method, its inferred invocation type, and the deferred poly arguments to
/// re-infer against the resolved formal parameters ([JLS §18.5.2.4]).
pub(super) type ApplicableCandidate = (MethodData, MethodData, Vec<(ExprId, usize)>);

/// Infers the *poly* arguments standalone — the recovery when an invocation
/// has no applicable method ([§15.12.2]): a lambda or method reference keeps
/// its error type and a nested invocation resolves in isolation, so every
/// argument expression still carries a recorded type. The inference is truly
/// standalone ([§15.12.2.6]): the enclosing context's target does not reach
/// an *argument* position — only its invocation formal constrains it — so it
/// is cleared here. The concrete arguments were already inferred while
/// collecting [`ArgInfo`] and are left untouched.
pub(super) fn reinfer_poly_standalone(ctx: &mut InferCtx<'_>, arg_kinds: &[ArgInfo]) {
    for info in arg_kinds.iter().filter(|info| info.poly) {
        let _ = ctx.with_target(None, |this| this.infer_expr(info.id));
    }
}

/// The poly leaves of an argument ([JLS §15.2]): a lambda, method reference or
/// method invocation, or the leaves of a parenthesized or conditional
/// expression whose arms are poly ([JLS §18.5.2.4]). An argument that is not a
/// poly expression has no leaves — it is inferred standalone.
pub(super) fn poly_leaves(tree: &BodyTree, id: ExprId) -> Vec<ExprId> {
    match tree.expr(id).clone() {
        ExprData::Lambda { .. }
        | ExprData::MethodRef { .. }
        | ExprData::MethodCall { .. }
        // §15.9.3: a diamond class instance creation is a poly expression in
        // an invocation context.
        | ExprData::New { diamond: true, .. } => vec![id],
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
pub(super) fn poly_arity(tree: &BodyTree, id: ExprId) -> Option<usize> {
    match tree.expr(id).clone() {
        ExprData::Lambda { params, .. } => Some(params.len()),
        ExprData::Paren(inner) => poly_arity(tree, inner),
        ExprData::Conditional { then, els, .. } => {
            poly_arity(tree, then).filter(|n| poly_arity(tree, els) == Some(*n))
        }
        _ => None,
    }
}

impl InferCtx<'_> {
    /// Whether a block lambda contains a `return` statement carrying a value —
    /// the syntactic core of value compatibility
    /// ([JLS §15.27.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27.3),
    /// [§14.17](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.17)):
    /// every path must return a value or throw. A block without any valued
    /// `return` is only void-compatible, so it cannot target a functional
    /// interface whose function type produces a result.
    pub(super) fn lambda_block_has_value(&self, body: &LambdaBody) -> bool {
        let LambdaBody::Block(stmt) = *body else {
            // An expression lambda's value compatibility is decided against
            // its inferred result, not syntactically.
            return true;
        };
        self.stmt_has_valued_return(stmt)
    }

    pub(super) fn stmt_has_valued_return(&self, stmt: StmtId) -> bool {
        match self.tree.stmt(stmt).clone() {
            StmtData::Return(Some(_)) | StmtData::Yield(_) => true,
            StmtData::Return(None)
            | StmtData::Empty
            | StmtData::Decl { .. }
            | StmtData::Expr(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::Throw(_)
            | StmtData::Assert { .. }
            | StmtData::LocalClass { .. }
            | StmtData::Missing => false,
            StmtData::Block(stmts) | StmtData::DeclGroup(stmts) => {
                stmts.iter().any(|stmt| self.stmt_has_valued_return(*stmt))
            }
            StmtData::Labeled { stmt, .. }
            | StmtData::While { body: stmt, .. }
            | StmtData::DoWhile { body: stmt, .. }
            | StmtData::ForEach { body: stmt, .. }
            | StmtData::Synchronized { body: stmt, .. } => self.stmt_has_valued_return(stmt),
            StmtData::If { then, els, .. } => {
                self.stmt_has_valued_return(then)
                    || els.is_some_and(|els| self.stmt_has_valued_return(els))
            }
            StmtData::For { body: stmt, .. } => self.stmt_has_valued_return(stmt),
            StmtData::Switch { arms, .. } => arms.iter().any(|arm| {
                arm.body
                    .iter()
                    .any(|stmt| self.stmt_has_valued_return(*stmt))
            }),
            StmtData::Try {
                body,
                catches,
                finally,
                ..
            } => {
                self.stmt_has_valued_return(body)
                    || catches
                        .iter()
                        .any(|catch| self.stmt_has_valued_return(catch.body))
                    || finally.is_some_and(|finally| self.stmt_has_valued_return(finally))
            }
        }
    }
}

/// The keyword naming the access of a member
/// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)),
/// for the `has {access} access` wording of an [`IllegalAccess`]
/// (e.g. javac's `secret has private access in p.Priv`).
pub(super) fn access_keyword(access: Access) -> &'static str {
    match access {
        Access::Private => "private",
        Access::Protected => "protected",
        Access::Package => "package-private",
        Access::Public => "public",
    }
}
