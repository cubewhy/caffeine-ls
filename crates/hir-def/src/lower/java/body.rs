//! Java CST → body IR.
//!
//! Walks the `BLOCK`, statement and expression subtrees of a declaration into
//! the per-file [`hir_expand::body::BodyTree`] arena, mirroring the statement
//! forms of [JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html)
//! and the expression forms of [JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html).
//! The operator nodes follow the Pratt-style grammar in `expr.rs`: unary,
//! postfix, compound assignment, binary and conditional expressions wrap
//! their operands in a single node, so the lowerer pulls the operand
//! expression children out in source order.

use java_syntax::{Lang, SyntaxKind as J};
use rowan::{NodeOrToken, SyntaxNode, SyntaxToken};
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use hir_expand::{
    body::{
        AssignOp, BinaryOp, Body, BodyId, CatchClause, ExprData, ExprId, Label, LabelId,
        LambdaBody, Literal, Local, LocalId, PostfixOp, StmtData, StmtId, SwitchArm, UnaryOp,
    },
    item_tree::ItemId,
    name::Name,
};

use crate::lower::LowerCtx;

use super::{token_is, token_text, trimmed_text};

/// Lowers the `BLOCK` of a method or constructor as a [`Body`], binding the
/// formal parameters (`None` for compact constructors).
pub(super) fn lower_method_body(
    ctx: &mut LowerCtx,
    owner: ItemId,
    block: &SyntaxNode<Lang>,
    params: Option<&SyntaxNode<Lang>>,
) -> BodyId {
    let params = params
        .map(|params| local_params(ctx, params))
        .unwrap_or_default();
    let stmts = lower_statement_list(ctx, owner, block);
    alloc_body(ctx, owner, params, stmts)
}

/// Lowers the `BLOCK` of a static or instance initializer as a [`Body`].
pub(super) fn lower_initializer_body(
    ctx: &mut LowerCtx,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<BodyId> {
    if node.kind() != J::BLOCK {
        return None;
    }
    let stmts = lower_statement_list(ctx, owner, node);
    Some(alloc_body(ctx, owner, Vec::new(), stmts))
}

/// Lowers a single expression into the arena. Used for field initializers,
/// annotation element defaults and enum constant arguments.
pub(super) fn lower_expr(
    ctx: &mut LowerCtx,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<ExprId> {
    Some(expr(ctx, owner, node))
}

/// The first expression-kind child of `node`, if any.
pub(super) fn find_expression_child(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children().find(|c| is_expr_kind(c.kind()))
}

fn alloc_body(
    ctx: &mut LowerCtx,
    owner: ItemId,
    params: Vec<LocalId>,
    stmts: Vec<StmtId>,
) -> BodyId {
    BodyId(ctx.bodies.bodies.alloc(Body {
        owner: Some(owner),
        params,
        stmts,
    }))
}

/// The local bindings of a `FORMAL_PARAMETERS` node.
fn local_params(ctx: &mut LowerCtx, params: &SyntaxNode<Lang>) -> Vec<LocalId> {
    params
        .children()
        .filter(|child| child.kind() == J::FORMAL_PARAMETER || child.kind() == J::SPREAD_PARAMETER)
        .map(|child| {
            let name = first_identifier(&child).unwrap_or_else(missing_name);
            let ty = child
                .children()
                .find(|c| c.kind() == J::TYPE)
                .map(|t| super::type_from(&t))
                .unwrap_or(TypeRef::Error);
            LocalId(ctx.bodies.locals.alloc(Local { name, ty: Some(ty) }))
        })
        .collect()
}

/// `true` for a node whose kind is a statement kind ([JLS §14]).
fn is_stmt_kind(kind: J) -> bool {
    matches!(
        kind,
        J::BLOCK
            | J::EMPTY_STMT
            | J::LOCAL_VARIABLE_DECLARATION_STMT
            | J::EXPRESSION_STMT
            | J::IF_STMT
            | J::WHILE_STMT
            | J::DO_STMT
            | J::FOR_STMT
            | J::ENHANCED_FOR_STMT
            | J::SWITCH_STMT
            | J::RETURN_STMT
            | J::THROW_STMT
            | J::BREAK_STMT
            | J::CONTINUE_STMT
            | J::YIELD_STMT
            | J::LABELED_STMT
            | J::SYNCHRONIZED_STMT
            | J::TRY_STMT
            | J::ASSERT_STMT
    )
}

/// Lower the statement children of a block.
fn lower_statement_list(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> Vec<StmtId> {
    let mut out = Vec::new();
    for child in node.children() {
        if is_stmt_kind(child.kind()) {
            out.push(stmt(ctx, owner, &child));
        }
    }
    out
}

fn alloc_stmt(ctx: &mut LowerCtx, data: StmtData) -> StmtId {
    StmtId(ctx.bodies.stmts.alloc(data))
}

/// Lowers a block node into a single [`StmtData::Block`] statement.
fn block_stmt(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtId {
    let inner = lower_statement_list(ctx, owner, node);
    alloc_stmt(ctx, StmtData::Block(inner))
}

fn stmt(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtId {
    let data = stmt_data(ctx, owner, node);
    alloc_stmt(ctx, data)
}

fn stmt_data(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    use J::*;
    match node.kind() {
        EMPTY_STMT => StmtData::Empty,
        BLOCK => StmtData::Block(lower_statement_list(ctx, owner, node)),
        LOCAL_VARIABLE_DECLARATION_STMT => local_declaration(ctx, owner, node),
        EXPRESSION_STMT => StmtData::Expr(first_expr(ctx, owner, node)),
        RETURN_STMT => StmtData::Return(expr_child_opt(ctx, owner, node)),
        THROW_STMT => StmtData::Throw(first_expr(ctx, owner, node)),
        IF_STMT => {
            let mut exprs = node.children().filter(|c| is_expr_kind(c.kind()));
            let cond = exprs
                .next()
                .map(|c| expr(ctx, owner, &c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let body: Vec<StmtId> = node
                .children()
                .filter(|c| is_stmt_kind(c.kind()))
                .map(|c| stmt(ctx, owner, &c))
                .collect();
            let then = *body.first().unwrap_or(&alloc_stmt(ctx, StmtData::Missing));
            let els = body.get(1).copied();
            StmtData::If { cond, then, els }
        }
        WHILE_STMT => {
            let cond = first_expr(ctx, owner, node);
            let body = first_stmt(ctx, owner, node);
            StmtData::While { cond, body }
        }
        DO_STMT => {
            let body = first_stmt(ctx, owner, node);
            let cond = first_expr(ctx, owner, node);
            StmtData::DoWhile { body, cond }
        }
        FOR_STMT => {
            // The initializer may be a `LOCAL_VARIABLE_DECLARATION` (a
            // declaration without the trailing `;` — there is no `_STMT`
            // wrapper in the header), or a statement.
            let init: Vec<StmtId> = node
                .children()
                .filter(|c| {
                    (is_stmt_kind(c.kind()) && c.kind() != BLOCK)
                        || c.kind() == LOCAL_VARIABLE_DECLARATION
                })
                .map(|c| {
                    if c.kind() == LOCAL_VARIABLE_DECLARATION {
                        let data = local_declaration(ctx, owner, &c);
                        alloc_stmt(ctx, data)
                    } else {
                        stmt(ctx, owner, &c)
                    }
                })
                .collect();
            let exprs: Vec<ExprId> = node
                .children()
                .filter(|c| is_expr_kind(c.kind()))
                .map(|c| expr(ctx, owner, &c))
                .collect();
            // The header is `init; cond; step`: the first expression belongs
            // to the initializer, the last to the step.
            let (cond, step) = match exprs.as_slice() {
                [rest @ .., last] if !rest.is_empty() => (Some(rest[0]), vec![*last]),
                [single] => (None, vec![*single]),
                _ => (None, Vec::new()),
            };
            let body = node
                .children()
                .find(|c| c.kind() == BLOCK)
                .map(|c| block_stmt(ctx, owner, &c))
                .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing));
            StmtData::For {
                init,
                cond,
                step,
                body,
            }
        }
        ENHANCED_FOR_STMT => {
            let var = node
                .children()
                .find(|c| c.kind() == J::TYPE)
                .map(|ty| {
                    let ty = super::type_from(&ty);
                    let name = node
                        .children()
                        .find(|c| c.kind() == J::VARIABLE_DECLARATOR)
                        .and_then(|d| first_identifier(&d))
                        .or_else(|| first_identifier(node))
                        .unwrap_or_else(missing_name);
                    LocalId(ctx.bodies.locals.alloc(Local { name, ty: Some(ty) }))
                })
                .unwrap_or_else(|| alloc_local_missing(ctx));
            let iterable = first_expr(ctx, owner, node);
            let body = first_stmt_or_block(ctx, owner, node);
            StmtData::ForEach {
                var,
                iterable,
                body,
            }
        }
        SWITCH_STMT => switch(ctx, owner, node),
        LABELED_STMT => {
            let label = node
                .children_with_tokens()
                .filter_map(|e| e.as_token().cloned())
                .find(|t| token_is(t, J::IDENTIFIER))
                .map(|t| LabelId(ctx.bodies.labels.alloc(Label(Name::new(t.text())))))
                .unwrap_or_else(|| alloc_label_missing(ctx));
            ctx.labels.push((ctx.bodies.label(label).0.clone(), label));
            let stmt = node
                .children()
                .find(|c| is_stmt_kind(c.kind()))
                .map(|c| stmt(ctx, owner, &c))
                .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing));
            ctx.labels.pop();
            StmtData::Labeled { label, stmt }
        }
        BREAK_STMT | CONTINUE_STMT => {
            let label = node
                .children_with_tokens()
                .filter_map(|e| e.as_token().cloned())
                .find(|t| token_is(t, J::IDENTIFIER))
                .map(|t| {
                    let name = Name::new(t.text());
                    ctx.labels
                        .iter()
                        .rev()
                        .find(|(n, _)| *n == name)
                        .map(|(_, id)| *id)
                        .unwrap_or_else(|| LabelId(ctx.bodies.labels.alloc(Label(name))))
                });
            if node.kind() == BREAK_STMT {
                StmtData::Break(label)
            } else {
                StmtData::Continue(label)
            }
        }
        YIELD_STMT => StmtData::Yield(first_expr(ctx, owner, node)),
        SYNCHRONIZED_STMT => {
            let expr_ = first_expr(ctx, owner, node);
            let body = first_stmt_or_block(ctx, owner, node);
            StmtData::Synchronized { expr: expr_, body }
        }
        TRY_STMT => try_stmt(ctx, owner, node),
        ASSERT_STMT => {
            let mut exprs = node.children().filter(|c| is_expr_kind(c.kind()));
            let cond = exprs
                .next()
                .map(|c| expr(ctx, owner, &c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let msg = exprs.next().map(|c| expr(ctx, owner, &c));
            StmtData::Assert { cond, msg }
        }
        _ => StmtData::Missing,
    }
}

/// A `LOCAL_VARIABLE_DECLARATION_STMT`: a type plus one or more
/// `VARIABLE_DECLARATOR`s, each lowered to its own [`StmtData::Decl`].
fn local_declaration(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    use J::*;
    let decl = node
        .children()
        .find(|c| c.kind() == LOCAL_VARIABLE_DECLARATION)
        .unwrap_or_else(|| node.clone());
    let type_ref = decl
        .children()
        .find(|c| c.kind() == TYPE)
        .map(|t| super::type_from(&t))
        .unwrap_or(TypeRef::Error);
    let declarators: Vec<_> = decl
        .children()
        .find(|c| c.kind() == VARIABLE_DECLARATOR_LIST)
        .map(|list| {
            list.children()
                .filter(|c| c.kind() == VARIABLE_DECLARATOR)
                .collect()
        })
        .unwrap_or_default();
    if declarators.is_empty() {
        return StmtData::Missing;
    }
    let mut decls = Vec::with_capacity(declarators.len());
    for declarator in &declarators {
        let name = first_identifier(declarator).unwrap_or_else(missing_name);
        let local = LocalId(ctx.bodies.locals.alloc(Local {
            name,
            ty: Some(type_ref.clone()),
        }));
        let initializer = declarator
            .children()
            .find(|c| is_expr_kind(c.kind()))
            .map(|c| expr(ctx, owner, &c));
        decls.push(StmtData::Decl { local, initializer });
    }
    if decls.len() == 1 {
        return decls.pop().unwrap();
    }
    StmtData::Block(decls.into_iter().map(|d| alloc_stmt(ctx, d)).collect())
}

fn try_stmt(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    use J::*;
    let body = node
        .children()
        .find(|c| c.kind() == BLOCK)
        .map(|c| block_stmt(ctx, owner, &c))
        .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing));
    let catches: Vec<CatchClause> = node
        .children()
        .filter(|c| c.kind() == CATCH_CLAUSE)
        .map(|c| {
            let param = c
                .children()
                .find(|p| p.kind() == CATCH_FORMAL_PARAMETER)
                .map(|p| {
                    let name = first_identifier(&p).unwrap_or_else(missing_name);
                    let ty = p
                        .children()
                        .find(|t| t.kind() == CATCH_TYPE)
                        .and_then(|ct| ct.children().find(|t| t.kind() == TYPE))
                        .map(|t| super::type_from(&t))
                        .unwrap_or(TypeRef::Error);
                    LocalId(ctx.bodies.locals.alloc(Local { name, ty: Some(ty) }))
                })
                .unwrap_or_else(|| alloc_local_missing(ctx));
            let body = c
                .children()
                .find(|b| b.kind() == BLOCK)
                .map(|b| block_stmt(ctx, owner, &b))
                .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing));
            CatchClause { param, body }
        })
        .collect();
    let resources: Vec<LocalId> = node
        .children()
        .find(|c| c.kind() == RESOURCE_SPECIFICATION)
        .map(|spec| {
            spec.children()
                .filter(|r| r.kind() == RESOURCE)
                .map(|_| alloc_local_missing(ctx))
                .collect()
        })
        .unwrap_or_default();
    let finally = node
        .children()
        .find(|c| c.kind() == FINALLY_CLAUSE)
        .and_then(|f| f.children().find(|b| b.kind() == BLOCK))
        .map(|b| block_stmt(ctx, owner, &b));
    StmtData::Try {
        resources,
        body,
        catches,
        finally,
    }
}

fn switch(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    if node.kind() != J::SWITCH_STMT {
        return StmtData::Missing;
    }
    let (scrutinee, arms) = switch_parts(ctx, owner, node);
    StmtData::Switch { scrutinee, arms }
}

/// The scrutinee and arms of a `switch` statement or expression.
fn switch_parts(
    ctx: &mut LowerCtx,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> (ExprId, Vec<SwitchArm>) {
    let scrutinee = first_expr(ctx, owner, node);
    let arms = switch_arms(ctx, owner, node);
    (scrutinee, arms)
}

fn switch_arms(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> Vec<SwitchArm> {
    use J::*;
    let mut arms = Vec::new();
    for group in node
        .children()
        .filter(|c| c.kind() == SWITCH_BLOCK_STATEMENT_GROUP || c.kind() == SWITCH_RULE)
    {
        let mut labels = Vec::new();
        for label in group.children().filter(|c| c.kind() == SWITCH_LABEL) {
            let mut seen = false;
            for sub in label.children() {
                if is_expr_kind(sub.kind()) {
                    labels.push(expr(ctx, owner, &sub));
                    seen = true;
                }
            }
            if !seen {
                // `default`
                labels.push(alloc_expr(ctx, ExprData::Missing));
            }
        }
        let mut body = Vec::new();
        for c in group.children() {
            if is_stmt_kind(c.kind()) {
                body.push(stmt(ctx, owner, &c));
            } else if is_expr_kind(c.kind()) {
                // `case ... -> expression;`
                let e = expr(ctx, owner, &c);
                body.push(alloc_stmt(ctx, StmtData::Expr(e)));
            }
        }
        arms.push(SwitchArm { labels, body });
    }
    arms
}

// --- expressions ------------------------------------------------------------

/// `true` for a node whose kind is an expression kind ([JLS §15]).
pub(super) fn is_expr_kind(kind: J) -> bool {
    matches!(
        kind,
        J::LITERAL
            | J::PREFIX_EXPR
            | J::UNARY_EXPR
            | J::POSTFIX_EXPR
            | J::ASSIGN_EXPR
            | J::BINARY_EXPR
            | J::COND_EXPR
            | J::INSTANCEOF_EXPR
            | J::CAST_EXPR
            | J::PAREN_EXPR
            | J::NEW_EXPR
            | J::METHOD_CALL
            | J::FIELD_ACCESS
            | J::ARRAY_ACCESS
            | J::CLASS_LITERAL
            | J::THIS_EXPR
            | J::SUPER_EXPR
            | J::PRIMITIVE_TYPE_EXPR
            | J::METHOD_REFERENCE
            | J::LAMBDA_EXPR
            | J::SWITCH_EXPR
            | J::TEMPLATE_EXPR
            | J::ARRAY_INITIALIZER
    )
}

/// The direct-child expression nodes of `node`, in source order.
fn expr_children(node: &SyntaxNode<Lang>) -> Vec<SyntaxNode<Lang>> {
    node.children().filter(|c| is_expr_kind(c.kind())).collect()
}

fn alloc_expr(ctx: &mut LowerCtx, data: ExprData) -> ExprId {
    ExprId(ctx.bodies.exprs.alloc(data))
}

fn expr(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    let data = expr_data(ctx, owner, node);
    alloc_expr(ctx, data)
}

fn expr_data(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    use J::*;
    match node.kind() {
        LITERAL => literal(node),
        TEMPLATE_EXPR => ExprData::Literal(Literal::Str),
        PRIMITIVE_TYPE_EXPR => {
            let prim = node
                .children_with_tokens()
                .find_map(|e| e.as_token().and_then(primitive_from_token))
                .unwrap_or(PrimitiveType::Void);
            ExprData::ClassLit(TypeRef::Primitive(prim))
        }
        CLASS_LITERAL => class_literal(node),
        THIS_EXPR => ExprData::This { qualifier: None },
        SUPER_EXPR => ExprData::Super,
        PREFIX_EXPR => {
            let op = first_op_token(node)
                .map(|t| {
                    if t.text() == "--" {
                        UnaryOp::Dec
                    } else {
                        UnaryOp::Inc
                    }
                })
                .unwrap_or(UnaryOp::Inc);
            let expr_ = first_expr(ctx, owner, node);
            ExprData::Unary { op, expr: expr_ }
        }
        UNARY_EXPR => {
            let op = first_op_token(node)
                .map(|t| match t.text() {
                    "-" => UnaryOp::Minus,
                    "~" => UnaryOp::BitNot,
                    "!" => UnaryOp::Not,
                    _ => UnaryOp::Plus,
                })
                .unwrap_or(UnaryOp::Plus);
            let expr_ = first_expr(ctx, owner, node);
            ExprData::Unary { op, expr: expr_ }
        }
        POSTFIX_EXPR => {
            let op = last_op_token(node)
                .map(|t| {
                    if t.text() == "--" {
                        PostfixOp::Dec
                    } else {
                        PostfixOp::Inc
                    }
                })
                .unwrap_or(PostfixOp::Inc);
            let expr_ = first_expr(ctx, owner, node);
            ExprData::Postfix { op, expr: expr_ }
        }
        BINARY_EXPR => {
            let op = first_op_token(node)
                .map(|t| op_from_token(&t))
                .unwrap_or(BinaryOp::Add);
            let kids = expr_children(node);
            let lhs = kids
                .first()
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let rhs = kids
                .get(1)
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            ExprData::Binary { op, lhs, rhs }
        }
        INSTANCEOF_EXPR => {
            let expr_ = first_expr(ctx, owner, node);
            let ty = node
                .children()
                .find(|c| c.kind() == TYPE)
                .map(|t| super::type_from(&t))
                .unwrap_or(TypeRef::Error);
            ExprData::InstanceOf { expr: expr_, ty }
        }
        ASSIGN_EXPR => {
            let op = first_op_token(node)
                .map(|t| assign_op_from_token(&t))
                .unwrap_or(AssignOp::Assign);
            let kids = expr_children(node);
            let lhs = kids
                .first()
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let rhs = kids
                .get(1)
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            ExprData::Assign { op, lhs, rhs }
        }
        COND_EXPR => {
            let kids = expr_children(node);
            let cond = kids
                .first()
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let then = kids
                .get(1)
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let els = kids
                .get(2)
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            ExprData::Conditional { cond, then, els }
        }
        CAST_EXPR => {
            let ty = node
                .children()
                .find(|c| c.kind() == TYPE)
                .map(|t| super::type_from(&t))
                .unwrap_or(TypeRef::Error);
            let expr_ = first_expr(ctx, owner, node);
            ExprData::Cast { ty, expr: expr_ }
        }
        PAREN_EXPR => ExprData::Paren(first_expr(ctx, owner, node)),
        ARRAY_ACCESS => {
            let kids = expr_children(node);
            let array = kids
                .first()
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            let index = kids
                .get(1)
                .map(|c| expr(ctx, owner, c))
                .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing));
            ExprData::ArrayAccess { array, index }
        }
        METHOD_CALL => method_call(ctx, owner, node),
        FIELD_ACCESS => {
            let name = identifier_of(node).unwrap_or_else(missing_name);
            let target = node.children().find(|c| is_expr_kind(c.kind()));
            match target {
                Some(target) => ExprData::FieldAccess {
                    target: Some(expr(ctx, owner, &target)),
                    name,
                },
                None => ExprData::NamePath(join_dotted(node)),
            }
        }
        NEW_EXPR => new_expr(ctx, owner, node),
        ARRAY_INITIALIZER => {
            let elems = node
                .children()
                .filter(|c| is_expr_kind(c.kind()))
                .map(|c| expr(ctx, owner, &c))
                .collect();
            ExprData::ArrayInit(elems)
        }
        LAMBDA_EXPR => lambda(ctx, owner, node),
        METHOD_REFERENCE => {
            let name = join_identifier_token(node).unwrap_or_else(missing_name);
            let type_name = node
                .children()
                .find(|c| c.kind() == TYPE)
                .map(|t| super::type_from(&t));
            let qualifier = node.children().find(|c| is_expr_kind(c.kind()));
            ExprData::MethodRef {
                qualifier: qualifier.map(|c| expr(ctx, owner, &c)),
                type_name,
                name,
            }
        }
        SWITCH_EXPR => {
            let (scrutinee, arms) = switch_parts(ctx, owner, node);
            ExprData::Switch { scrutinee, arms }
        }
        QUALIFIED_NAME => ExprData::NamePath(Name::new(&trimmed_text(node))),
        _ => {
            // A bare identifier name reference.
            if let Some(name) = first_identifier(node) {
                ExprData::Var(name)
            } else {
                ExprData::Missing
            }
        }
    }
}

fn literal(node: &SyntaxNode<Lang>) -> ExprData {
    let token = node
        .children_with_tokens()
        .filter_map(|e| e.as_token().cloned())
        .find(|token| !token.kind().is_trivia());
    let Some(token) = token else {
        return ExprData::Missing;
    };
    match token.kind() {
        J::INTEGER_LITERAL => {
            if token.text().ends_with('L') || token.text().ends_with('l') {
                ExprData::Literal(Literal::Long)
            } else {
                ExprData::Literal(Literal::Int)
            }
        }
        J::FLOAT_LITERAL => {
            if token.text().ends_with('f') || token.text().ends_with('F') {
                ExprData::Literal(Literal::Float)
            } else {
                ExprData::Literal(Literal::Double)
            }
        }
        J::STRING_LITERAL | J::TEXT_BLOCK => ExprData::Literal(Literal::Str),
        J::CHAR_LITERAL => ExprData::Literal(Literal::Char),
        J::TRUE_LITERAL | J::FALSE_LITERAL => ExprData::Literal(Literal::Boolean),
        J::NULL_LITERAL => ExprData::Null,
        J::IDENTIFIER | J::UNDERSCORE => ExprData::Var(Name::new(token.text())),
        J::THIS_KW => ExprData::This { qualifier: None },
        J::SUPER_KW => ExprData::Super,
        _ => ExprData::Missing,
    }
}

fn class_literal(node: &SyntaxNode<Lang>) -> ExprData {
    if let Some(ty) = node.children().find(|c| c.kind() == J::TYPE) {
        return ExprData::ClassLit(super::type_from(&ty));
    }
    if let Some(prim) = node.children().find(|c| c.kind() == J::PRIMITIVE_TYPE_EXPR) {
        let p = prim
            .children_with_tokens()
            .find_map(|e| e.as_token().and_then(primitive_from_token))
            .unwrap_or(PrimitiveType::Void);
        return ExprData::ClassLit(TypeRef::Primitive(p));
    }
    // `String.class`, `Foo.Bar.class`: rebuild the qualified name.
    ExprData::ClassLit(TypeRef::Reference {
        name: join_identifiers(node),
        generic_args: Vec::new(),
    })
}

fn method_call(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let args: Vec<ExprId> = node
        .children()
        .find(|c| c.kind() == J::ARGUMENT_LIST)
        .map(|list| {
            list.children()
                .filter(|c| is_expr_kind(c.kind()))
                .map(|c| expr(ctx, owner, &c))
                .collect()
        })
        .unwrap_or_default();
    let type_args: Vec<TypeRef<Name>> = node
        .children()
        .find(|c| c.kind() == J::TYPE_ARGUMENTS)
        .map(|t| type_arguments_from(&t))
        .unwrap_or_default();

    // `receiver.method(args)`: the receiver chain is a FIELD_ACCESS whose
    // name is the method name and whose target is the actual receiver.
    if let Some(field) = node.children().find(|c| c.kind() == J::FIELD_ACCESS) {
        let name = identifier_of(&field).unwrap_or_else(missing_name);
        let receiver = field
            .children()
            .find(|c| is_expr_kind(c.kind()))
            .map(|c| expr(ctx, owner, &c));
        return ExprData::MethodCall {
            receiver,
            name,
            type_args,
            args,
        };
    }

    // `foo(args)`: the receiver slot is a single name reference; the method
    // name is that reference (an implicit `this` receiver).
    if let Some(recv) = node.children().find(|c| c.kind() == J::LITERAL)
        && let ExprData::Var(name) = literal(&recv)
    {
        return ExprData::MethodCall {
            receiver: None,
            name,
            type_args,
            args,
        };
    }
    // `Type.method(args)` without a declared type child is impossible in the
    // CST; everything else falls through to the field-access form above.
    ExprData::MethodCall {
        receiver: None,
        name: missing_name(),
        type_args,
        args,
    }
}

fn new_expr(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    use J::*;
    let base = node
        .children()
        .find(|c| c.kind() == TYPE)
        .map(|t| super::type_from(&t))
        .or_else(|| {
            // `new Type<A>(...)` / `new int[3]` have no `TYPE` child: the base
            // type is the primitive keyword or the `QUALIFIED_NAME`, with the
            // `TYPE_ARGUMENTS` of the `new` itself (joining all identifiers
            // would swallow the arguments).
            if let Some(prim) = node
                .children_with_tokens()
                .find_map(|e| e.as_token().and_then(primitive_from_token))
            {
                return Some(TypeRef::Primitive(prim));
            }
            let name = node
                .children()
                .find(|c| c.kind() == QUALIFIED_NAME)
                .map(|q| trimmed_text(&q))
                .unwrap_or_else(|| "<missing>".to_owned());
            let generic_args = node
                .children()
                .find(|c| c.kind() == TYPE_ARGUMENTS)
                .map(|t| type_arguments_from(&t))
                .unwrap_or_default();
            (name.as_str() != "<missing>").then(|| TypeRef::Reference {
                name: Name::new(&name),
                generic_args,
            })
        })
        .unwrap_or(TypeRef::Error);
    if let Some(args) = node.children().find(|c| c.kind() == ARGUMENT_LIST) {
        let args: Vec<ExprId> = args
            .children()
            .filter(|c| is_expr_kind(c.kind()))
            .map(|c| expr(ctx, owner, &c))
            .collect();
        return ExprData::New { ty: base, args };
    }
    // Array creation ([§15.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10)):
    // the `DIMENSIONS` node holds one `DIMENSION` per bracket pair. A
    // `DIMENSION` with a size is a DimExpr (`dims`); an empty one is part of
    // the array type and wraps the base type.
    let dimensions: Vec<SyntaxNode<Lang>> = node
        .children()
        .find(|c| c.kind() == DIMENSIONS)
        .map(|dims| dims.children().filter(|c| c.kind() == DIMENSION).collect())
        .unwrap_or_default();
    let mut ty = base;
    let mut dims = Vec::new();
    for dimension in &dimensions {
        if let Some(size) = dimension.children().find(|c| is_expr_kind(c.kind())) {
            dims.push(expr(ctx, owner, &size));
        } else {
            ty = TypeRef::Array(Box::new(ty));
        }
    }
    // An array creation initializer ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6),
    // [§15.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10.1)):
    // `new Type[] { a, b }` carries its element expressions. Array creation
    // expressions with dims have no initializer.
    let initializer = node
        .children()
        .find(|c| c.kind() == ARRAY_INITIALIZER)
        .map(|init| {
            init.children()
                .filter(|c| is_expr_kind(c.kind()))
                .map(|c| expr(ctx, owner, &c))
                .collect()
        });
    ExprData::NewArray {
        ty,
        dims,
        initializer,
    }
}

fn lambda(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    use J::*;
    let mut params: Vec<(Name, Option<TypeRef<Name>>)> = Vec::new();
    // A single-parameter lambda `x -> body` lowers the parameter as a bare
    // identifier token ([JLS §15.27.1]); the other forms are parenthesized
    // node children (`FORMAL_PARAMETERS` for typed parameters,
    // `INFERRED_PARAMETERS` for `(a, b)`, each parameter nested one level
    // deep).
    fn param_of(c: &SyntaxNode<Lang>) -> (Name, Option<TypeRef<Name>>) {
        let name = first_identifier(c).unwrap_or_else(missing_name);
        let ty = c
            .children()
            .find(|t| t.kind() == TYPE)
            .map(|t| super::type_from(&t));
        (name, ty)
    }
    fn inferred_params(c: &SyntaxNode<Lang>, out: &mut Vec<(Name, Option<TypeRef<Name>>)>) {
        for t in c.children_with_tokens() {
            match t {
                rowan::NodeOrToken::Token(token) => {
                    if token_is(&token, J::IDENTIFIER) || token_is(&token, J::UNDERSCORE) {
                        out.push((Name::new(token.text()), None));
                    }
                }
                rowan::NodeOrToken::Node(node) => inferred_params(&node, out),
            }
        }
    }
    for c in node.children_with_tokens() {
        match c {
            rowan::NodeOrToken::Token(token) => {
                if token.kind() == IDENTIFIER || token.kind() == UNDERSCORE {
                    params.push((Name::new(token.text()), None));
                }
            }
            rowan::NodeOrToken::Node(c) => match c.kind() {
                FORMAL_PARAMETERS => {
                    for p in c.children() {
                        if p.kind() == FORMAL_PARAMETER || p.kind() == SPREAD_PARAMETER {
                            params.push(param_of(&p));
                        }
                    }
                }
                INFERRED_PARAMETERS => inferred_params(&c, &mut params),
                _ => {}
            },
        }
    }
    let body = if let Some(b) = node.children().find(|c| c.kind() == BLOCK) {
        LambdaBody::Block(block_stmt(ctx, owner, &b))
    } else if let Some(e) = node.children().find(|c| is_expr_kind(c.kind())) {
        LambdaBody::Expr(expr(ctx, owner, &e))
    } else {
        LambdaBody::Expr(alloc_expr(ctx, ExprData::Missing))
    };
    ExprData::Lambda { params, body }
}

// --- small helpers ----------------------------------------------------------

fn expr_child_opt(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> Option<ExprId> {
    node.children()
        .find(|c| is_expr_kind(c.kind()))
        .map(|c| expr(ctx, owner, &c))
}

fn first_expr(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    expr_child_opt(ctx, owner, node).unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing))
}

fn first_stmt(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtId {
    node.children()
        .find(|c| is_stmt_kind(c.kind()))
        .map(|c| stmt(ctx, owner, &c))
        .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing))
}

fn first_stmt_or_block(ctx: &mut LowerCtx, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtId {
    if let Some(block) = node.children().find(|c| c.kind() == J::BLOCK) {
        return block_stmt(ctx, owner, &block);
    }
    first_stmt(ctx, owner, node)
}

fn first_identifier(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(|e| e.as_token().cloned())
        .find(|t| token_is(t, J::IDENTIFIER))
        .map(|t| Name::new(t.text()))
}

fn identifier_of(node: &SyntaxNode<Lang>) -> Option<Name> {
    first_identifier(node)
}

/// The last direct IDENTIFIER token of the node.
fn join_identifier_token(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(|e| e.as_token().cloned())
        .filter(|t| token_is(t, J::IDENTIFIER))
        .last()
        .map(|t| Name::new(t.text()))
}

/// The qualified name from the identifiers of a subtree, dot-joined.
fn join_identifiers(node: &SyntaxNode<Lang>) -> Name {
    let mut parts = Vec::new();
    collect_identifiers(node, &mut parts);
    Name::new(&parts.join("."))
}

fn collect_identifiers(node: &SyntaxNode<Lang>, parts: &mut Vec<String>) {
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) => collect_identifiers(&child, parts),
            NodeOrToken::Token(token) => {
                if token_is(&token, J::IDENTIFIER) && token.text() != "new" {
                    parts.push(token.text().to_owned());
                }
            }
        }
    }
}

fn join_dotted(node: &SyntaxNode<Lang>) -> Name {
    join_identifiers(node)
}

/// The first non-trivia direct token of the node — the operator.
fn first_op_token(node: &SyntaxNode<Lang>) -> Option<SyntaxToken<Lang>> {
    node.children_with_tokens()
        .filter_map(|e| e.as_token().cloned())
        .find(|t| !matches!(t.kind(), J::WHITESPACE | J::LINE_COMMENT | J::BLOCK_COMMENT))
}

fn last_op_token(node: &SyntaxNode<Lang>) -> Option<SyntaxToken<Lang>> {
    node.children_with_tokens()
        .filter_map(|e| e.as_token().cloned())
        .find(|t| !matches!(t.kind(), J::WHITESPACE | J::LINE_COMMENT | J::BLOCK_COMMENT))
}

fn op_from_token(token: &SyntaxToken<Lang>) -> BinaryOp {
    use BinaryOp::*;
    match token.kind() {
        J::STAR => Mul,
        J::SLASH => Div,
        J::MODULO => Rem,
        J::PLUS => Add,
        J::MINUS => Sub,
        J::LEFT_SHIFT => Shl,
        J::RIGHT_SHIFT => Shr,
        J::UNSIGNED_RIGHT_SHIFT => UShr,
        J::LESS => Lt,
        J::GREATER => Gt,
        J::LESS_EQUAL => Le,
        J::GREATER_EQUAL => Ge,
        J::EQUAL_EQUAL => Eq,
        J::NOT_EQUAL => Ne,
        J::BIT_AND => BitAnd,
        J::CARET => BitXor,
        J::BIT_OR => BitOr,
        J::AND => And,
        J::OR => Or,
        _ => Add,
    }
}

fn assign_op_from_token(token: &SyntaxToken<Lang>) -> AssignOp {
    use AssignOp::*;
    match token.kind() {
        J::PLUS_EQUAL => Add,
        J::MINUS_EQUAL => Sub,
        J::MULTIPLE_EQUAL => Mul,
        J::DIVIDE_EQUAL => Div,
        J::MODULO_EQUAL => Rem,
        J::LEFT_SHIFT_EQUAL => Shl,
        J::RIGHT_SHIFT_EQUAL => Shr,
        J::UNSIGNED_RIGHT_SHIFT_EQUAL => UShr,
        J::AND_EQUAL => BitAnd,
        J::XOR_EQUAL => BitXor,
        J::OR_EQUAL => BitOr,
        _ => Assign,
    }
}

fn primitive_from_token(token: &SyntaxToken<Lang>) -> Option<PrimitiveType> {
    let prim = match token.kind() {
        J::INT_KW => PrimitiveType::Int,
        J::LONG_KW => PrimitiveType::Long,
        J::FLOAT_KW => PrimitiveType::Float,
        J::DOUBLE_KW => PrimitiveType::Double,
        J::BOOLEAN_KW => PrimitiveType::Boolean,
        J::BYTE_KW => PrimitiveType::Byte,
        J::CHAR_KW => PrimitiveType::Char,
        J::SHORT_KW => PrimitiveType::Short,
        J::VOID_KW => PrimitiveType::Void,
        _ => return None,
    };
    Some(prim)
}

fn type_arguments_from(node: &SyntaxNode<Lang>) -> Vec<TypeRef<Name>> {
    use J::*;
    node.children()
        .filter(|c| c.kind() == TYPE_ARGUMENT)
        .map(|c| {
            if let Some(ty) = c.children().find(|t| t.kind() == TYPE) {
                super::type_from(&ty)
            } else if let Some(wild) = c.children().find(|w| w.kind() == WILDCARD_TYPE) {
                let bound = wild
                    .children()
                    .find(|b| b.kind() == WILDCARD_BOUNDS)
                    .map(|bounds| {
                        let is_super = bounds
                            .children_with_tokens()
                            .any(|e| e.as_token().is_some_and(|t| token_text(t, "super")));
                        let ty = bounds
                            .children()
                            .find(|t| t.kind() == TYPE)
                            .map(|t| super::type_from(&t))
                            .unwrap_or(TypeRef::Error);
                        let bound = if is_super {
                            TypeBound::Lower(ty)
                        } else {
                            TypeBound::Upper(ty)
                        };
                        Box::new(bound)
                    });
                TypeRef::Wildcard { bound }
            } else {
                TypeRef::Error
            }
        })
        .collect()
}

fn alloc_local_missing(ctx: &mut LowerCtx) -> LocalId {
    LocalId(ctx.bodies.locals.alloc(Local {
        name: missing_name(),
        ty: None,
    }))
}

fn alloc_label_missing(ctx: &mut LowerCtx) -> LabelId {
    LabelId(ctx.bodies.labels.alloc(Label(missing_name())))
}

fn missing_name() -> Name {
    Name::new("<missing>")
}
