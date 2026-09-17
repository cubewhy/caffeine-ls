//! Kotlin CST → body IR.
//!
//! Walks a declaration's `BLOCK`s, statements and expressions into the
//! per-file [`hir_expand::body::BodyTree`], mirroring the statement forms of
//! [KLS `expressions.html`](https://kotlinlang.org/spec/expressions.html) and
//! the declaration forms of
//! [KLS `declarations.html`](https://kotlinlang.org/spec/declarations.html).
//!
//! Two structural differences from the Java walker ([`crate::java::lower::body`])
//! shape this one:
//!
//! * **Everything is an expression.** `if`, `when`, `try`, `throw` and
//!   `return` all produce a value ([KLS
//!   `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions):
//!   `return`/`throw` have type `Nothing`), so they lower to [`ExprData`] and
//!   are wrapped in [`StmtData::Expr`] when they stand as statements;
//! * **a block is a scope with local declarations.** A local `val`, a local
//!   function and a local class are declarations of the block, so the walker
//!   binds locals and records the local types ([`super::lower_kotlin_source`]
//!   collects them).
//!
//! The operators follow the Pratt-style grammar in `expr.rs`: unary, postfix,
//! binary and the named binary forms (`elvis`, `range`, `as`, `is`) each wrap
//! their operands in one node, so the walker pulls the operand children out in
//! source order.

use rowan::{NodeOrToken, SyntaxNode, SyntaxToken, TextRange};
use stacksafe::stacksafe;
use syntax::kotlin::{Lang, SyntaxKind as K};

use hir_expand::{
    body::{
        AssignOp, BinaryOp, Body, BodyId, CatchClause, ExprData, ExprId, JumpKind, LambdaBody,
        LambdaParam, Literal, Local, LocalId, PatternData, PatternId, PostfixOp, StmtData, StmtId,
        UnaryOp, WhenArm, WhenCondition,
    },
    ids::ItemId,
    name::Name,
    span::{NameRef, SpannedTypeRef},
};

use super::LowerCtx;
use super::walk::{LoweredType, lower_type_node};

/// Lowers the body of a function declaration: its `BLOCK`, its expression body
/// (`fun f() = expr`) or nothing for a declaration without one.
pub(super) fn lower_function_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<BodyId> {
    let params = lower_params(ctx, node);
    lower_body(ctx, owner, node, params)
}

/// Lowers the body of an accessor (`get() = expr`, `set(value) { … }`).
pub(super) fn lower_accessor_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<BodyId> {
    let params = lower_params(ctx, node);
    lower_body(ctx, owner, node, params)
}

/// Lowers the body of an `init` block ([KLS
/// `declarations.html#classifier-initialization`](https://kotlinlang.org/spec/declarations.html#classifier-initialization)).
pub(super) fn lower_init_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<BodyId> {
    let block = node.children().find(|child| is(child, K::BLOCK))?;
    Some(lower_block_body(ctx, owner, &block, Vec::new()))
}

/// Lowers the body of a secondary constructor ([KLS
/// `declarations.html#secondary-constructor`](https://kotlinlang.org/spec/declarations.html#secondary-constructor)):
/// its block, an expression body, or nothing.
pub(super) fn lower_constructor_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<BodyId> {
    let params = lower_params(ctx, node);
    lower_body(ctx, owner, node, params)
}

/// Lowers a property's initializer expression (`val x = expr`).
pub(super) fn lower_property_initializer(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<ExprId> {
    let value = node
        .children()
        .find(|child| is_expression(child.kind()) && !is(child, K::PROPERTY_DELEGATE))?;
    Some(expr(ctx, owner, &value))
}

/// Lowers a delegated property's `by` expression (`val x by lazy { … }`).
pub(super) fn lower_property_delegate(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<ExprId> {
    let delegate = node
        .children()
        .find(|child| is(child, K::PROPERTY_DELEGATE))?;
    let value = delegate
        .children()
        .find(|child| is_expression(child.kind()))?;
    Some(expr(ctx, owner, &value))
}

/// The default value of every parameter of a declaration, in parameter order
/// ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)):
/// the `= expr` a `VALUE_PARAMETER`/`CLASS_PARAMETER` writes, lowered in the
/// declaring item's context, or `None` for a parameter that writes none.
pub(super) fn lower_defaults(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Vec<Option<ExprId>> {
    let Some(parameters) = node
        .children()
        .find(|child| matches!(child.kind(), K::VALUE_PARAMETERS | K::CLASS_PARAMETERS))
    else {
        return Vec::new();
    };
    parameters
        .children()
        .filter(|child| matches!(child.kind(), K::VALUE_PARAMETER | K::CLASS_PARAMETER))
        .map(|parameter| {
            // The parameter's one expression child is its default: the type is
            // a type node and the modifiers are annotations, so nothing else
            // in the parameter is an expression ([spec:
            // grammar-rule-functionValueParameter]).
            parameter
                .children()
                .find(|child| is_expression(child.kind()))
                .map(|value| expr(ctx, owner, &value))
        })
        .collect()
}

/// The lowered form of one expression node, in the context of the declaration
/// `owner` that carries it — the entry point the declaration walker uses for
/// the expressions *it* owns: a parameter default, a supertype's constructor
/// arguments, a delegate.
pub(super) fn lower_expr(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    expr(ctx, owner, node)
}

/// The lowered arguments of a `VALUE_ARGUMENTS` node, in the context of the
/// declaration that carries them.
pub(super) fn lower_value_arguments(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Vec<ExprId> {
    value_arguments(ctx, owner, node)
}

/// Lowers the constructor arguments of an enum entry ([KLS
/// `declarations.html#enum-class-declaration`](https://kotlinlang.org/spec/declarations.html#enum-class-declaration)).
pub(super) fn lower_enum_entry_arguments(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Vec<ExprId> {
    let Some(arguments) = node.children().find(|child| is(child, K::VALUE_ARGUMENTS)) else {
        return Vec::new();
    };
    value_arguments(ctx, owner, &arguments)
}

/// The `BLOCK`, expression body or nothing at all of a declaration that has a
/// `functionBody` ([spec: grammar-rule-functionBody]).
fn lower_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
    params: Vec<LocalId>,
) -> Option<BodyId> {
    if let Some(block) = node.children().find(|child| is(child, K::BLOCK)) {
        return Some(lower_block_body(ctx, owner, &block, params));
    }
    // An expression body (`fun f() = expr`, `get() = expr`): one
    // `StmtData::Expr` statement. The `=` is a *token* child, and the
    // expression is the first expression node after it.
    let value = expression_body(node)?;
    let id = expr(ctx, owner, &value);
    let stmt = alloc_stmt(ctx, StmtData::Expr(id), value.text_range());
    Some(alloc_body(ctx, owner, params, vec![stmt]))
}

/// Whether the body a declaration node writes is the *expression* form
/// (`fun f() = expr`, `get() = expr`) rather than a block
/// ([spec: grammar-rule-functionBody]). A block-bodied declaration that writes no
/// return type returns `Unit`; an expression-bodied one is typed by its
/// expression — the two are indistinguishable from the lowered statements
/// alone, since a one-expression block lowers to the same single
/// `StmtData::Expr`.
pub(super) fn expression_body_form(node: &SyntaxNode<Lang>) -> bool {
    expression_body(node).is_some()
}

/// The expression of an `= expr` body, if the declaration writes one
/// ([spec: grammar-rule-functionBody]).
fn expression_body(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    let mut after_equal = false;
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Token(token) if is_token(&token, K::EQUAL) => after_equal = true,
            NodeOrToken::Node(child) if after_equal && is_expression(child.kind()) => {
                return Some(child);
            }
            _ => {}
        }
    }
    None
}

/// Whether a node kind is an expression, in the shapes the expression grammar
/// produces ([spec: grammar-rule-expression]).
pub(super) fn is_expression(kind: K) -> bool {
    matches!(
        kind,
        K::PRIMARY_EXPRESSION
            | K::PARENTHESIZED_EXPRESSION
            | K::POSTFIX_UNARY_EXPRESSION
            | K::PREFIX_UNARY_EXPRESSION
            | K::BINARY_EXPRESSION
            | K::RANGE_EXPRESSION
            | K::ELVIS_EXPRESSION
            | K::AS_EXPRESSION
            | K::IS_EXPRESSION
            | K::IN_EXPRESSION
            | K::INFIX_FUNCTION_CALL
            | K::IF_EXPRESSION
            | K::WHEN_EXPRESSION
            | K::TRY_EXPRESSION
            | K::JUMP_EXPRESSION
            | K::LAMBDA_LITERAL
            | K::ANONYMOUS_FUNCTION
            | K::THIS_EXPRESSION
            | K::SUPER_EXPRESSION
            | K::OBJECT_LITERAL
            | K::CALLABLE_REFERENCE
            | K::STRING_LITERAL
            | K::COLLECTION_LITERAL
            | K::ASSIGNMENT_STATEMENT
    )
}

/// The body of a `BLOCK`: its statements, with the parameter locals bound.
fn lower_block_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    block: &SyntaxNode<Lang>,
    params: Vec<LocalId>,
) -> BodyId {
    let stmts = lower_statement_list(ctx, owner, block);
    alloc_body(ctx, owner, params, stmts)
}

fn alloc_body(
    ctx: &mut LowerCtx<'_>,
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

fn alloc_stmt(ctx: &mut LowerCtx<'_>, data: StmtData, range: TextRange) -> StmtId {
    let range_idx = ctx.bodies.stmts.len();
    let id = ctx.bodies.stmts.alloc(data);
    debug_assert_eq!(ctx.bodies.stmt_ranges.len(), range_idx);
    ctx.bodies.stmt_ranges.push(range);
    StmtId(id)
}

fn alloc_expr(ctx: &mut LowerCtx<'_>, data: ExprData, range: TextRange) -> ExprId {
    ctx.bodies.expr_ranges.push(range);
    ctx.bodies.expr_name_ranges.push(range);
    ExprId(ctx.bodies.exprs.alloc(data))
}

/// The local bindings of the declared parameters of `node`: a function's
/// `VALUE_PARAMETERS`, a constructor's, or a setter's single parameter.
fn lower_params(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<LocalId> {
    let mut params = Vec::new();
    if let Some(list) = node
        .children()
        .find(|child| matches!(child.kind(), K::VALUE_PARAMETERS | K::CLASS_PARAMETERS))
    {
        for parameter in list
            .children()
            .filter(|child| matches!(child.kind(), K::VALUE_PARAMETER | K::CLASS_PARAMETER))
        {
            params.push(lower_param(ctx, &parameter));
        }
    }
    // A setter's parameter is a direct child of the `SETTER` node
    // ([spec: grammar-rule-setter]).
    for parameter in node
        .children()
        .filter(|child| is(child, K::VALUE_PARAMETER))
    {
        params.push(lower_param(ctx, &parameter));
    }
    params
}

/// Binds one declared parameter as a [`Local`], with its declared type when it
/// writes one. A parameter is a `val`: it cannot be reassigned
/// ([KLS `declarations.html#function-declaration`](https://kotlinlang.org/spec/declarations.html#function-declaration)).
pub(super) fn lower_param(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> LocalId {
    let name = parameter_name(node).unwrap_or_else(|| Name::new("<missing>"));
    let ty = declared_type(ctx, node);
    alloc_local(ctx, name, ty, node.text_range(), name_range(node))
}

/// Binds a local with its declared type, `is_mutable` being Kotlin's `var`
/// ([KLS
/// `declarations.html#local-property-declaration`](https://kotlinlang.org/spec/declarations.html#local-property-declaration)):
/// only a `var` binding may be reassigned, and the type layer reports a write
/// to anything else.
fn alloc_local(
    ctx: &mut LowerCtx<'_>,
    name: Name,
    ty: Option<SpannedTypeRef>,
    range: TextRange,
    name_range: TextRange,
) -> LocalId {
    alloc_local_mutability(ctx, name, ty, range, name_range, false)
}

/// [`alloc_local`] with the binding's mutability: a `var` local is mutable, a
/// `val` local, a parameter, a loop variable and a pattern binding are not.
fn alloc_local_mutability(
    ctx: &mut LowerCtx<'_>,
    name: Name,
    ty: Option<SpannedTypeRef>,
    range: TextRange,
    name_range: TextRange,
    is_mutable: bool,
) -> LocalId {
    let range_idx = ctx.bodies.locals.len();
    let id = ctx.bodies.locals.alloc(Local {
        name,
        ty,
        annotations: Vec::new(),
        is_final: false,
        is_mutable,
    });
    debug_assert_eq!(ctx.bodies.local_ranges.len(), range_idx);
    ctx.bodies.local_ranges.push(range);
    ctx.bodies.local_name_ranges.push(name_range);
    LocalId(id)
}

/// The declared type of a declaration, as a body-IR type.
fn declared_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<SpannedTypeRef> {
    let ty = node.children().find(|child| is_type_node(child.kind()))?;
    Some(spanned_type(ctx, &ty))
}

/// A [`SpannedTypeRef`] from a type node, carrying the reference names and
/// their ranges (so the type layer can report unresolved names at the right
/// offsets).
fn spanned_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let LoweredType {
        ty,
        refs,
        annotations: _,
    } = lower_type_node(ctx, node);
    SpannedTypeRef::new(
        ty,
        refs.into_iter()
            .map(|(name, range)| NameRef::new(name, range))
            .collect(),
    )
}

fn is_type_node(kind: K) -> bool {
    matches!(
        kind,
        K::TYPE
            | K::NULLABLE_TYPE
            | K::DEFINITELY_NON_NULLABLE_TYPE
            | K::PARENTHESIZED_TYPE
            | K::FUNCTION_TYPE
            | K::USER_TYPE
    )
}

/// The statements of a `BLOCK`, in source order.
#[stacksafe]
fn lower_statement_list(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    block: &SyntaxNode<Lang>,
) -> Vec<StmtId> {
    block
        .children()
        .filter(|child| is_statement(child.kind()))
        .map(|child| statement(ctx, owner, &child))
        .collect()
}

/// Whether a node kind is a statement ([spec: grammar-rule-statement]) — a
/// `BLOCK` included, because it is the body a loop or a `try` wraps
/// ([spec: grammar-rule-block]).
fn is_statement(kind: K) -> bool {
    matches!(
        kind,
        K::BLOCK
            | K::PROPERTY_DECL
            | K::FUNCTION_DECL
            | K::CLASS_DECL
            | K::OBJECT_DECL
            | K::TYPE_ALIAS
            | K::ASSIGNMENT_STATEMENT
            | K::EXPRESSION_STATEMENT
            | K::FOR_STATEMENT
            | K::WHILE_STATEMENT
            | K::DO_WHILE_STATEMENT
            | K::LABEL
    )
}

#[stacksafe]
fn statement(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtId {
    let data = stmt_data(ctx, owner, node);
    alloc_stmt(ctx, data, node.text_range())
}

fn stmt_data(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    match node.kind() {
        // A block: the body of a loop, a `catch`/`finally`, or a bare `{ … }`
        // statement ([spec: grammar-rule-block]).
        K::BLOCK => StmtData::Block(lower_statement_list(ctx, owner, node)),
        // A statement is an expression ([KLS
        // `expressions.html#expressions`](https://kotlinlang.org/spec/expressions.html#expressions)):
        // the expression statement, the assignment and the loops all wrap one.
        K::EXPRESSION_STATEMENT => {
            match node.children().find(|child| is_expression(child.kind())) {
                Some(value) => StmtData::Expr(expr(ctx, owner, &value)),
                None => StmtData::Missing,
            }
        }
        K::ASSIGNMENT_STATEMENT => {
            let id = expr(ctx, owner, node);
            StmtData::Expr(id)
        }
        // `label@ statement` ([spec: grammar-rule-label]).
        K::LABEL => match node.children().find(|child| is_statement(child.kind())) {
            Some(inner) => StmtData::Labeled {
                label: alloc_label(ctx, &node),
                stmt: statement(ctx, owner, &inner),
            },
            None => StmtData::Missing,
        },
        // `for (x in xs) body` ([KLS
        // `expressions.html#for-loops`](https://kotlinlang.org/spec/expressions.html#for-loops)).
        K::FOR_STATEMENT => foreach(ctx, owner, node),
        K::WHILE_STATEMENT => {
            let Some(cond) = node.children().find(|child| is_expression(child.kind())) else {
                return StmtData::Missing;
            };
            let Some(body) = control_structure_body(ctx, owner, node) else {
                return StmtData::Missing;
            };
            StmtData::While {
                cond: expr(ctx, owner, &cond),
                body,
            }
        }
        K::DO_WHILE_STATEMENT => {
            let Some(cond) = node.children().find(|child| is_expression(child.kind())) else {
                return StmtData::Missing;
            };
            let Some(body) = control_structure_body(ctx, owner, node) else {
                return StmtData::Missing;
            };
            StmtData::DoWhile {
                body,
                cond: expr(ctx, owner, &cond),
            }
        }
        // A local function ([KLS
        // `declarations.html#local-function-declaration`](https://kotlinlang.org/spec/declarations.html#local-function-declaration)):
        // the declaration is an item of the file's item tree, lowered by the
        // declaration walker (which sees this statement's block).
        K::FUNCTION_DECL => match super::walk::lower_local_declaration(ctx, owner, node) {
            Some(item) => StmtData::LocalFunction { item },
            None => StmtData::Missing,
        },
        // A local class, object or type alias: class-like declarations are
        // local types ([KLS
        // `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
        K::CLASS_DECL | K::OBJECT_DECL | K::TYPE_ALIAS => {
            match super::walk::lower_local_declaration(ctx, owner, node) {
                Some(item) => StmtData::LocalClass { item },
                None => StmtData::Missing,
            }
        }
        // A local property declaration `val x = expr` ([KLS
        // `declarations.html#local-property-declaration`](https://kotlinlang.org/spec/declarations.html#local-property-declaration)).
        K::PROPERTY_DECL => local_property(ctx, owner, node),
        _ => StmtData::Missing,
    }
}

/// A local property declaration: one `Decl` statement per bound name, each
/// carrying the declared or initializer expression.
fn local_property(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    // `var x = …` may be reassigned, `val x = …` may not
    // ([KLS `declarations.html#local-property-declaration`](https://kotlinlang.org/spec/declarations.html#local-property-declaration)).
    let is_var = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::VAR_KW));
    let initializer = lower_property_initializer(ctx, owner, node);
    // The declared type of `val x: T` sits on the *variable declaration* child
    // ([spec: grammar-rule-variableDeclaration]), not on the property node.
    let declaration = node
        .children()
        .find(|child| is(child, K::VARIABLE_DECLARATION));
    let declared = declaration
        .as_ref()
        .and_then(|declaration| declared_type(ctx, declaration));

    // A destructuring declaration (`val (a, b) = pair`) binds one local per
    // component ([KLS
    // `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)),
    // as one statement: the pattern carries the names and the initializer is
    // the value they are destructured from.
    if let Some(multi) = node
        .children()
        .find(|child| is(child, K::MULTI_VARIABLE_DECLARATION))
    {
        let declarations: Vec<SyntaxNode<Lang>> = multi
            .children()
            .filter(|child| is(child, K::VARIABLE_DECLARATION))
            .collect();
        let parts = declarations
            .iter()
            .map(|declaration| {
                let name = variable_name(declaration).unwrap_or_else(|| Name::new("<missing>"));
                alloc_local(
                    ctx,
                    name,
                    declared.clone(),
                    declaration.text_range(),
                    name_range(declaration),
                )
            })
            .collect();
        let pattern = alloc_pattern(
            ctx,
            PatternData::Destructuring { parts },
            multi.text_range(),
        );
        return match initializer {
            Some(initializer) => StmtData::Destructuring {
                pattern,
                initializer,
            },
            None => StmtData::Missing,
        };
    }

    let Some(name) = node
        .children()
        .find(|child| is(child, K::VARIABLE_DECLARATION))
        .and_then(|declaration| variable_name(&declaration))
    else {
        return StmtData::Missing;
    };
    // The name's own range is the *variable declaration's* identifier — the
    // property node's first direct token is its `val`/`var` keyword, so the
    // declaration child is what carries it.
    let name_range = declaration
        .as_ref()
        .map(|declaration| name_range(declaration))
        .unwrap_or_else(|| name_range(node));
    let local = alloc_local_mutability(ctx, name, declared, node.text_range(), name_range, is_var);
    // `val x by lazy { … }` ([KLS
    // `declarations.html#delegated-property-declaration`](https://kotlinlang.org/spec/declarations.html#delegated-property-declaration)):
    // the local is bound to the `by` expression, not to an initializer, and
    // its value is the delegate's `getValue` result — which the type layer
    // produces once the member bridge lands, and which is the error type until
    // then.
    if let Some(delegate) = lower_property_delegate(ctx, owner, node) {
        return StmtData::DeclDelegated { local, delegate };
    }
    StmtData::Decl { local, initializer }
}

/// Allocates a pattern lowered from `node`, recording its source range.
fn alloc_pattern(ctx: &mut LowerCtx<'_>, data: PatternData, range: TextRange) -> PatternId {
    ctx.bodies.pattern_ranges.push(range);
    PatternId(ctx.bodies.patterns.alloc(data))
}

/// A `for (x in xs) body` statement ([spec: grammar-rule-forStatement]).
///
/// The loop variable is one name or a destructuring pattern: `for ((k, v) in
/// xs)` binds one local per component ([KLS
/// `expressions.html#destructuring-declarations`](https://kotlinlang.org/spec/expressions.html#destructuring-declarations)),
/// and the pattern — not the first component — is what the type layer
/// destructures the iterable's element type into. The parser writes the
/// pattern as a `MULTI_VARIABLE_DECLARATION` where a plain loop writes a
/// `VARIABLE_DECLARATION` ([spec: grammar-rule-forStatement]).
fn foreach(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> StmtData {
    let Some(variable) = node.children().find(|child| {
        matches!(
            child.kind(),
            K::VARIABLE_DECLARATION | K::MULTI_VARIABLE_DECLARATION
        )
    }) else {
        return StmtData::Missing;
    };
    let Some(iterable) = node.children().find(|child| is_expression(child.kind())) else {
        return StmtData::Missing;
    };
    let (var, pattern) = if is(&variable, K::MULTI_VARIABLE_DECLARATION) {
        let mut parts = Vec::new();
        for component in variable
            .children()
            .filter(|child| is(child, K::VARIABLE_DECLARATION))
        {
            let name = variable_name(&component).unwrap_or_else(|| Name::new("<missing>"));
            let ty = declared_type(ctx, &component);
            parts.push(alloc_local(
                ctx,
                name,
                ty,
                component.text_range(),
                name_range(&component),
            ));
        }
        let Some(&first) = parts.first() else {
            return StmtData::Missing;
        };
        let pattern = alloc_pattern(
            ctx,
            PatternData::Destructuring { parts },
            variable.text_range(),
        );
        (first, Some(pattern))
    } else {
        let name = variable_name(&variable).unwrap_or_else(|| Name::new("<missing>"));
        let ty = declared_type(ctx, &variable);
        (
            alloc_local(ctx, name, ty, variable.text_range(), name_range(&variable)),
            None,
        )
    };
    let Some(body) = control_structure_body(ctx, owner, node) else {
        return StmtData::Missing;
    };
    StmtData::ForEach {
        var,
        pattern,
        iterable: expr(ctx, owner, &iterable),
        body,
    }
}

/// The `controlStructureBody` of a loop: its `BLOCK` or its single statement
/// ([spec: grammar-rule-controlStructureBody]).
fn control_structure_body(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<StmtId> {
    let body = node.children().find(|child| is_statement(child.kind()))?;
    Some(statement(ctx, owner, &body))
}

fn alloc_label(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> hir_expand::body::LabelId {
    let name = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(|| Name::new("<missing>"));
    hir_expand::body::LabelId(ctx.bodies.labels.alloc(hir_expand::body::Label(name)))
}

#[stacksafe]
fn expr(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    let data = expr_data(ctx, owner, node);
    alloc_expr(ctx, data, node.text_range())
}

fn expr_data(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    match node.kind() {
        K::PRIMARY_EXPRESSION => primary(ctx, owner, node),
        // `this`/`this@label` and `super`/`super<T>`/`super@label` are primary
        // forms the parser completes as their own node ([KLS
        // `expressions.html#this-expressions`](https://kotlinlang.org/spec/expressions.html#this-expressions),
        // [`#super-forms`](https://kotlinlang.org/spec/expressions.html#super-forms)).
        // The label of `this@outer` and the supertype of `super<Base>` both
        // qualify *which* receiver is meant, so both are the qualifier: the
        // label as a one-segment reference, the supertype as the reference it
        // names.
        K::THIS_EXPRESSION => ExprData::This {
            qualifier: label_qualifier(node),
        },
        K::SUPER_EXPRESSION => ExprData::Super {
            qualifier: super_qualifier(ctx, node),
        },
        K::PARENTHESIZED_EXPRESSION => match child_expression(node) {
            Some(inner) => ExprData::Paren(expr(ctx, owner, &inner)),
            None => ExprData::Missing,
        },
        K::STRING_LITERAL => string_template(ctx, owner, node),
        K::COLLECTION_LITERAL => ExprData::ArrayInit(
            node.children()
                .filter(|child| is_expression(child.kind()))
                .map(|child| expr(ctx, owner, &child))
                .collect(),
        ),
        K::POSTFIX_UNARY_EXPRESSION => postfix(ctx, owner, node),
        K::PREFIX_UNARY_EXPRESSION => prefix(ctx, owner, node),
        K::BINARY_EXPRESSION => binary(ctx, owner, node),
        K::RANGE_EXPRESSION => range(ctx, owner, node),
        K::ELVIS_EXPRESSION => match (first_expression(node), last_expression(node)) {
            (Some(lhs), Some(rhs)) => ExprData::Elvis {
                lhs: expr(ctx, owner, &lhs),
                rhs: expr(ctx, owner, &rhs),
            },
            _ => ExprData::Missing,
        },
        // `expr as T` / `expr as? T` ([KLS
        // `expressions.html#cast-expressions`](https://kotlinlang.org/spec/expressions.html#cast-expressions)).
        // The two differ in *behaviour*: `as? T` yields null where the cast
        // cannot succeed, while `as T` throws (kotlinc 2.4.20: `1 as? String`
        // is `null`, `1 as String?` throws `ClassCastException`), and the IR
        // records which one was written.
        K::AS_EXPRESSION => {
            let (Some(value), Some(ty)) = (child_expression(node), cast_type(node)) else {
                return ExprData::Missing;
            };
            ExprData::Cast {
                ty: spanned_type(ctx, &ty),
                expr: expr(ctx, owner, &value),
                safe: node
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| is_token(&token, K::QUESTION)),
            }
        }
        K::IS_EXPRESSION => {
            let (Some(value), Some(ty)) = (child_expression(node), cast_type(node)) else {
                return ExprData::Missing;
            };
            // `x !is T` is the negation of the test: the production carries the
            // `!` inside the operator token (`!is`), which is a `NOT_IS`
            // ([spec: grammar-rule-infixOperation]).
            let negated = node
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| is_token(&token, K::NOT_IS));
            let test = ExprData::InstanceOf {
                expr: expr(ctx, owner, &value),
                ty: Some(spanned_type(ctx, &ty)),
                pattern: None,
            };
            match negated {
                true => ExprData::Unary {
                    op: hir_expand::body::UnaryOp::Not,
                    expr: alloc_expr(ctx, test, node.text_range()),
                },
                false => test,
            }
        }
        K::IN_EXPRESSION => {
            let (Some(element), Some(container)) = (first_expression(node), last_expression(node))
            else {
                return ExprData::Missing;
            };
            // `e !in xs` is `!xs.contains(e)`, for the same reason.
            let negated = node
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| is_token(&token, K::NOT_IN));
            let containment = ExprData::InfixCall {
                receiver: expr(ctx, owner, &container),
                name: Name::new("contains"),
                arg: expr(ctx, owner, &element),
            };
            match negated {
                true => ExprData::Unary {
                    op: hir_expand::body::UnaryOp::Not,
                    expr: alloc_expr(ctx, containment, node.text_range()),
                },
                false => containment,
            }
        }
        K::INFIX_FUNCTION_CALL => {
            let (Some(receiver), Some(arg)) = (first_expression(node), last_expression(node))
            else {
                return ExprData::Missing;
            };
            let name = node
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|token| is_token(token, K::IDENTIFIER))
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(|| Name::new("<missing>"));
            ExprData::InfixCall {
                receiver: expr(ctx, owner, &receiver),
                name,
                arg: expr(ctx, owner, &arg),
            }
        }
        K::IF_EXPRESSION => conditional(ctx, owner, node),
        K::WHEN_EXPRESSION => when(ctx, owner, node),
        K::TRY_EXPRESSION => try_expr(ctx, owner, node),
        K::JUMP_EXPRESSION => jump(ctx, owner, node),
        K::LAMBDA_LITERAL => lambda(ctx, owner, node),
        // An anonymous function `fun(x: Int) = x + 1` ([KLS
        // `expressions.html#anonymous-functions`](https://kotlinlang.org/spec/expressions.html#anonymous-functions)):
        // it declares no name, so it is not a `FUNCTION_DECL`, and it lowers
        // as the lambda literal it is — a parameter list and a body, with
        // `fun(x) = expr` an expression body and `fun(x) { … }` a block.
        K::ANONYMOUS_FUNCTION => anonymous_function(ctx, owner, node),
        K::OBJECT_LITERAL => match super::walk::lower_local_declaration(ctx, owner, node) {
            Some(item) => ExprData::ObjectLiteral { item },
            None => ExprData::Missing,
        },
        K::CALLABLE_REFERENCE => callable_reference(ctx, owner, node),
        K::ASSIGNMENT_STATEMENT => assign(ctx, owner, node),
        _ => ExprData::Missing,
    }
}

/// A `primaryExpression` ([spec: grammar-rule-primaryExpression]): a literal,
/// a simple name, `this`/`super`, or a nested expression form.
fn primary(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let _ = owner;
    let Some(inner) = node.children().next() else {
        // No child node: the primary expression is a single token — a name or
        // a literal ([spec: grammar-rule-primaryExpression]).
        let token = node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .next();
        return match token {
            Some(token) if is_token(&token, K::IDENTIFIER) => {
                ExprData::Var(Name::new(token.text()))
            }
            Some(token) => literal(ctx, &token),
            None => ExprData::Missing,
        };
    };
    if is_expression(inner.kind()) {
        return expr_data(ctx, owner, &inner);
    }
    ExprData::Missing
}

/// A literal token of a `primaryExpression` ([KLS
/// `expressions.html#constant-literals`](https://kotlinlang.org/spec/expressions.html#constant-literals)).
///
/// A string literal carries its decoded value — Kotlin escapes are the JVM
/// ones, without the Unicode-escape pass Java has — and a character literal
/// the scalar value its escapes decode to: kotlinc 2.4.20 reads `'\n'`,
/// `'\\'` and `'\u0041'` as the newline, a backslash and `A`.
///
/// The numeric suffixes are folded into the value. `L` makes the literal a
/// `Long`, `f`/`F` a `Float`, and Kotlin's *unsigned* suffix (`u`/`U`,
/// optionally followed by `L`) is stripped: `1u` is a `kotlin.UInt` *value*
/// and `1uL` a `kotlin.ULong` one, and this IR carries the signed bit pattern
/// of the corresponding width — `0xFFFFFFFFu` lowers as `Int(-1)`, the 32-bit
/// pattern the compiler gives that value. The unsigned *type* the compiler
/// gives such a literal is not modelled — a recorded deviation, since the
/// model has no `kotlin.UInt` classifier to name.
pub(super) fn literal(_ctx: &LowerCtx<'_>, token: &SyntaxToken<Lang>) -> ExprData {
    let text = token.text();
    match token.kind() {
        K::INTEGER_LITERAL => {
            // `0xFF` and `0b1010` are the radix forms
            // ([spec: grammar-rule-HexLiteral], [spec:
            // grammar-rule-BinLiteral]); everything else is decimal and is
            // parsed by the same `from_str_radix` with radix 10.
            let (radix, digits) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
            {
                Some(hex) => (16u32, hex),
                None => match text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
                    Some(binary) => (2, binary),
                    None => (10, text),
                },
            };
            // The suffix order is the long one *then* the unsigned one
            // (`1UL` = `1` + `U` + `L`, [spec:
            // grammar-rule-UnsignedLiteral]), so `L` is stripped from the end
            // first and `u`/`U` after it; `consume_int_suffixes` folds both
            // into the token.
            let (digits, is_long) = match digits.strip_suffix('L') {
                Some(rest) => (rest, true),
                None => (digits, false),
            };
            let (digits, unsigned) = match digits
                .strip_suffix('u')
                .or_else(|| digits.strip_suffix('U'))
            {
                Some(rest) => (rest, true),
                None => (digits, false),
            };
            let digits = digits.replace('_', "");
            let value = if is_long {
                unsigned
                    .then(|| u64::from_str_radix(&digits, radix).ok().map(|v| v as i64))
                    .unwrap_or_else(|| i64::from_str_radix(&digits, radix).ok())
            } else if unsigned {
                u32::from_str_radix(&digits, radix)
                    .ok()
                    .map(|value| i64::from(value as i32))
            } else {
                i64::from_str_radix(&digits, radix).ok()
            };
            let Some(value) = value else {
                return ExprData::Missing;
            };
            ExprData::Literal(if is_long {
                Literal::Long(value)
            } else {
                Literal::Int(value)
            })
        }
        K::FLOAT_LITERAL => {
            // `1.0f` is a `Float`, `1.0` a `Double` ([KLS
            // `built-in-types-and-their-semantics.html`](https://kotlinlang.org/spec/built-in-types-and-their-semantics.html)):
            // the `f`/`F` suffix is the only difference the token carries.
            ExprData::Literal(if text.ends_with(['f', 'F']) {
                Literal::Float
            } else {
                Literal::Double
            })
        }
        K::TRUE_KW | K::FALSE_KW => {
            ExprData::Literal(Literal::Boolean(is_token(token, K::TRUE_KW)))
        }
        K::CHAR_LITERAL => {
            let content = text
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
                .unwrap_or(text);
            match decode_string_content(content).chars().next() {
                Some(value) => ExprData::Literal(Literal::Char(value)),
                None => ExprData::Missing,
            }
        }
        K::STRING_LITERAL | K::STRING_CONTENT => {
            ExprData::Literal(Literal::Str(decode_string_content(text)))
        }
        // `null` is not a literal of the IR's value kinds: it is its own form.
        K::NULL_KW => ExprData::Null,
        _ => ExprData::Missing,
    }
}

/// The decoded value of a Kotlin string's content: the JVM escapes
/// ([KLS `syntax-and-grammar.html`](https://kotlinlang.org/spec/syntax-and-grammar.html)
/// gives the lexical grammar; Kotlin has no Unicode-escape pass, unlike Java).
fn decode_string_content(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('$') => out.push('$'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A string literal: a plain literal, or a template over its interpolations
/// ([KLS
/// `expressions.html#string-interpolation-expressions`](https://kotlinlang.org/spec/expressions.html#string-interpolation-expressions)).
fn string_template(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let entries: Vec<SyntaxNode<Lang>> = node
        .children()
        .filter(|child| is(child, K::STRING_TEMPLATE))
        .collect();
    if entries.is_empty() {
        let text = node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|token| matches!(token.kind(), K::STRING_CONTENT | K::TEXT_BLOCK))
            .map(|token| decode_string_content(token.text()))
            .collect::<String>();
        return ExprData::Literal(Literal::Str(text));
    }
    let args = entries
        .iter()
        .map(|entry| template_entry(ctx, owner, entry))
        .collect();
    ExprData::Template { args }
}

/// One `STRING_TEMPLATE` interpolation: `${expr}`, or the short `$name` form,
/// which the parser leaves as a bare identifier token.
fn template_entry(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    if let Some(inner) = node.children().find(|child| is_expression(child.kind())) {
        return expr(ctx, owner, &inner);
    }
    let token = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| !token.kind().is_trivia());
    let data = match token {
        Some(token) if is_token(&token, K::IDENTIFIER) => ExprData::Var(Name::new(token.text())),
        Some(token) if is_token(&token, K::THIS_KW) => ExprData::This { qualifier: None },
        Some(token) if is_token(&token, K::SUPER_KW) => ExprData::Super { qualifier: None },
        _ => ExprData::Missing,
    };
    alloc_expr(ctx, data, node.text_range())
}

/// A postfix expression: the primary expression plus its suffixes — a call, an
/// index, a navigation or a postfix operator ([spec:
/// grammar-rule-postfixUnaryExpression]).
///
/// The chain is lowered step by step: the base is an expression node of its own
/// and every intermediate step is an expression entry, while the *last* step's
/// data is what the `POSTFIX_UNARY_EXPRESSION` node itself carries (so a
/// `x.f(1)` chain allocates `x` and one `MethodCall`, not a duplicate).
fn postfix(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let Some(base) = first_expression(node) else {
        return ExprData::Missing;
    };
    let mut current = expr(ctx, owner, &base);
    // The step whose data the node will carry; allocated only once another
    // suffix needs it as a sub-expression.
    let mut pending: Option<ExprData> = None;
    let mut seen_base = false;
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) => {
                if !seen_base {
                    seen_base = true;
                    continue;
                }
                if is_type_node(child.kind()) || is(&child, K::TYPE_ARGUMENTS) {
                    continue;
                }
                if let Some(data) = pending.take() {
                    current = alloc_expr(ctx, data, child.text_range());
                }
                pending = Some(suffix_data(ctx, owner, current, &child));
            }
            NodeOrToken::Token(token) => {
                let step = match token.kind() {
                    K::PLUS_PLUS => Some(ExprData::Postfix {
                        op: PostfixOp::Inc,
                        expr: current,
                    }),
                    K::MINUS_MINUS => Some(ExprData::Postfix {
                        op: PostfixOp::Dec,
                        expr: current,
                    }),
                    K::NOT_NULL_ASSERT => Some(ExprData::NullAssert { expr: current }),
                    _ => None,
                };
                if let Some(step) = step {
                    if let Some(data) = pending.take() {
                        current = alloc_expr(ctx, data, token.text_range());
                    }
                    pending = Some(step);
                }
            }
        }
    }
    match pending {
        Some(data) => data,
        // A postfix node without a suffix: the parser abandons such a node, so
        // this is an erroneous tree — the base's value stands.
        None => ExprData::Paren(current),
    }
}

/// One postfix suffix as the expression it produces, given the value the
/// suffixes before it produced.
fn suffix_data(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    value: ExprId,
    suffix: &SyntaxNode<Lang>,
) -> ExprData {
    match suffix.kind() {
        K::NAVIGATION_SUFFIX => navigation(ctx, owner, value, suffix),
        K::CALL_EXPRESSION => call(ctx, owner, value, suffix),
        K::INDEXING_EXPRESSION => match child_expression(suffix) {
            Some(index) => ExprData::ArrayAccess {
                array: value,
                index: expr(ctx, owner, &index),
            },
            None => ExprData::Missing,
        },
        _ => ExprData::Missing,
    }
}

/// A call suffix `(args)` or a trailing lambda `{ … }`
/// ([KLS `expressions.html#function-calls-and-property-access`](https://kotlinlang.org/spec/expressions.html#function-calls-and-property-access)).
///
/// The callee decides the shape: a navigation already lowered to
/// [`ExprData::FieldAccess`] becomes a [`ExprData::MethodCall`] whose receiver
/// is the accessed target; a bare name becomes a call with an implicit
/// receiver — a *constructor* call (`Point(1)`) is the same shape, because
/// only name resolution tells the two apart (the type layer resolves the name
/// and picks the constructor when it is a classifier).
fn call(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    callee: ExprId,
    suffix: &SyntaxNode<Lang>,
) -> ExprData {
    let args = match suffix
        .children()
        .find(|child| is(child, K::VALUE_ARGUMENTS))
    {
        Some(arguments) => value_arguments(ctx, owner, &arguments),
        // A trailing lambda: the argument is the lambda literal itself.
        None => suffix
            .children()
            .filter(|child| is_expression(child.kind()))
            .map(|child| expr(ctx, owner, &child))
            .collect(),
    };
    match ctx.bodies.expr(callee).clone() {
        ExprData::FieldAccess { target, name } => ExprData::MethodCall {
            receiver: target,
            name,
            type_args: Vec::new(),
            args,
        },
        // `receiver?.member(args)`: the member call happens only when the
        // receiver is not null, so the call lives *inside* the safe access.
        ExprData::SafeAccess { receiver, member } => {
            let name = match ctx.bodies.expr(member).clone() {
                ExprData::FieldAccess { name, .. } => name,
                _ => Name::new("<missing>"),
            };
            let call = alloc_expr(
                ctx,
                ExprData::MethodCall {
                    receiver: None,
                    name,
                    type_args: Vec::new(),
                    args,
                },
                suffix.text_range(),
            );
            ExprData::SafeAccess {
                receiver,
                member: call,
            }
        }
        ExprData::Var(name) => ExprData::MethodCall {
            receiver: None,
            name,
            type_args: Vec::new(),
            args,
        },
        // A call of a *value* (`f { }` on a function-typed local, `(x)(y)`):
        // Kotlin invokes the value, which is `invoke` on it.
        _ => ExprData::MethodCall {
            receiver: Some(callee),
            name: Name::new("invoke"),
            type_args: Vec::new(),
            args,
        },
    }
}

/// A navigation suffix `.name`, `?.name` or `::name`
/// ([KLS `expressions.html#navigation-operators`](https://kotlinlang.org/spec/expressions.html#navigation-operators)).
fn navigation(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    receiver: ExprId,
    node: &SyntaxNode<Lang>,
) -> ExprData {
    let _ = owner;
    let safe = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::SAFE_ACCESS));
    let callable = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::COLON_COLON));
    let name = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(|| Name::new("<missing>"));
    if callable {
        return ExprData::CallableReference {
            receiver: Some(receiver),
            name,
        };
    }
    if !safe {
        return ExprData::FieldAccess {
            target: Some(receiver),
            name,
        };
    }
    // A safe access's member is the access performed on a non-null receiver:
    // it is its own expression entry so the type layer can infer it.
    let access = alloc_expr(
        ctx,
        ExprData::FieldAccess { target: None, name },
        node.text_range(),
    );
    ExprData::SafeAccess {
        receiver,
        member: access,
    }
}

/// A prefix expression `!x`, `-x`, `+x`, `++x`, `--x`
/// ([KLS `expressions.html#prefix-expressions`](https://kotlinlang.org/spec/expressions.html#prefix-expressions)).
fn prefix(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let Some(inner) = child_expression(node) else {
        return ExprData::Missing;
    };
    let op = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| {
            matches!(
                token.kind(),
                K::NOT | K::MINUS | K::PLUS | K::PLUS_PLUS | K::MINUS_MINUS
            )
        })
        .map(|token| token.kind());
    let inner = expr(ctx, owner, &inner);
    match op {
        Some(K::PLUS_PLUS) => ExprData::Unary {
            op: UnaryOp::Inc,
            expr: inner,
        },
        Some(K::MINUS_MINUS) => ExprData::Unary {
            op: UnaryOp::Dec,
            expr: inner,
        },
        // `+x` is the identity on numbers; the IR has no unary plus.
        Some(K::PLUS) => ExprData::Paren(inner),
        Some(K::NOT) => ExprData::Unary {
            op: UnaryOp::Not,
            expr: inner,
        },
        Some(K::MINUS) => ExprData::Unary {
            op: UnaryOp::Minus,
            expr: inner,
        },
        _ => ExprData::Missing,
    }
}

/// A binary expression ([KLS
/// `expressions.html#equality-expressions`](https://kotlinlang.org/spec/expressions.html#equality-expressions)
/// and the arithmetic/comparison rules that follow it). The operator token is
/// the node's child between the operands.
fn binary(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let (Some(lhs), Some(rhs)) = (first_expression(node), last_expression(node)) else {
        return ExprData::Missing;
    };
    let op = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| binary_op(token.kind()).is_some());
    let Some(op) = op.and_then(|token| binary_op(token.kind())) else {
        return ExprData::Missing;
    };
    ExprData::Binary {
        op,
        lhs: expr(ctx, owner, &lhs),
        rhs: expr(ctx, owner, &rhs),
    }
}

/// The IR operator of a Kotlin binary operator token ([KLS
/// `expressions.html#additive-expressions`](https://kotlinlang.org/spec/expressions.html#additive-expressions)
/// and the multiplicative/comparison ones). Kotlin's `==` is the *structural*
/// equality (`equals`), which the type layer applies to the same IR operator
/// Java's `==` uses for references; `===` is the referential one.
fn binary_op(kind: K) -> Option<BinaryOp> {
    Some(match kind {
        K::PLUS => BinaryOp::Add,
        K::MINUS => BinaryOp::Sub,
        K::STAR => BinaryOp::Mul,
        K::SLASH => BinaryOp::Div,
        K::MODULO => BinaryOp::Rem,
        K::AND => BinaryOp::And,
        K::OR => BinaryOp::Or,
        K::EQUAL_EQUAL => BinaryOp::Eq,
        K::NOT_EQUAL => BinaryOp::Ne,
        // `===`/`!==` are the *referential* comparisons; the IR has one
        // equality pair, and Kotlin's `==` is the structural one
        // ([KLS `expressions.html#equality-expressions`](https://kotlinlang.org/spec/expressions.html#equality-expressions)),
        // so the referential forms lower to the same pair — a recorded
        // deviation: they compare references where `==` compares values, which
        // the type layer cannot tell apart. Both are `Boolean`, which is what
        // inference needs.
        K::SHEQ => BinaryOp::Eq,
        K::SHNE => BinaryOp::Ne,
        K::LESS => BinaryOp::Lt,
        K::GREATER => BinaryOp::Gt,
        K::LESS_EQUAL => BinaryOp::Le,
        K::GREATER_EQUAL => BinaryOp::Ge,
        // Kotlin has no bitwise-operator tokens: `shl`, `shr`, `ushr`,
        // `and`, `or` and `xor` are infix *functions*, lowered as
        // [`ExprData::InfixCall`].
        _ => return None,
    })
}

/// A range expression `lhs..rhs` / `lhs..<rhs` ([KLS
/// `expressions.html#range-expressions`](https://kotlinlang.org/spec/expressions.html#range-expressions)).
fn range(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let (Some(lhs), Some(rhs)) = (first_expression(node), last_expression(node)) else {
        return ExprData::Missing;
    };
    let inclusive = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| matches!(token.kind(), K::RANGE | K::RANGE_UNTIL))
        .is_none_or(|token| is_token(&token, K::RANGE));
    ExprData::Range {
        lhs: expr(ctx, owner, &lhs),
        rhs: expr(ctx, owner, &rhs),
        inclusive,
    }
}

/// An `if` expression ([KLS
/// `expressions.html#conditional-expressions`](https://kotlinlang.org/spec/expressions.html#conditional-expressions)).
/// The IR reuses Java's [`ExprData::Conditional`]; the branches are the node's
/// statement or block bodies.
fn conditional(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let Some(cond) = first_expression(node) else {
        return ExprData::Missing;
    };
    let bodies: Vec<SyntaxNode<Lang>> = node
        .children()
        .filter(|child| is(child, K::BLOCK) || is_statement(child.kind()))
        .collect();
    let Some(then) = bodies.first() else {
        return ExprData::Missing;
    };
    let cond = expr(ctx, owner, &cond);
    let then = branch(ctx, owner, then);
    // An `if` without `else` is a statement form: it has no value, and the IR
    // carries a missing expression in the branch the source does not write
    // ([KLS
    // `expressions.html#conditional-expressions`](https://kotlinlang.org/spec/expressions.html#conditional-expressions)
    // makes `else` optional in statement position).
    let els = match bodies.get(1) {
        Some(els) => branch(ctx, owner, els),
        None => alloc_expr(ctx, ExprData::Missing, node.text_range()),
    };
    ExprData::Conditional { cond, then, els }
}

/// A branch of an `if`/`when`: its block or statement as an expression (a
/// block's value is its last expression).
fn branch(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    if is(node, K::BLOCK) {
        return block_expr(ctx, owner, node);
    }
    match node.children().find(|child| is_expression(child.kind())) {
        Some(value) => expr(ctx, owner, &value),
        None => alloc_expr(ctx, ExprData::Missing, node.text_range()),
    }
}

/// A block used as an expression: its statement list, lowered once as a
/// [`StmtData::Block`] — the same statement the block lowers to in statement
/// position — wrapped in [`ExprData::Block`], whose value is the last
/// expression of that list ([KLS
/// `expressions.html#expressions`](https://kotlinlang.org/spec/expressions.html#expressions)).
fn block_expr(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprId {
    let stmts = lower_statement_list(ctx, owner, node);
    let block = alloc_stmt(ctx, StmtData::Block(stmts), node.text_range());
    alloc_expr(ctx, ExprData::Block(block), node.text_range())
}

/// A `when` expression ([KLS
/// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)).
fn when(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let subject = node
        .children()
        .find(|child| is(child, K::WHEN_SUBJECT))
        .and_then(|subject| when_subject(ctx, owner, &subject));
    let arms = node
        .children()
        .filter(|child| is(child, K::WHEN_ENTRY))
        .map(|entry| when_arm(ctx, owner, &entry, subject))
        .collect();
    ExprData::When { subject, arms }
}

/// The subject of a `when` ([spec: grammar-rule-whenSubject]): a bare
/// expression, or the `val x = expr` binding Kotlin 1.7 added, which the
/// parser writes as a `VARIABLE_DECLARATION` in the subject
/// ([KLS `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)).
/// The declaration binds `x`, and the subject the arms test against is the
/// initializer — `when (val x = f()) { is String -> … }` tests `f()`'s value
/// and binds it to `x`, so the two are the same expression.
fn when_subject(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> Option<ExprId> {
    if let Some(declaration) = node
        .children()
        .find(|child| is(child, K::VARIABLE_DECLARATION))
    {
        let name = variable_name(&declaration).unwrap_or_else(|| Name::new("<missing>"));
        let ty = declared_type(ctx, &declaration);
        alloc_local(
            ctx,
            name,
            ty,
            declaration.text_range(),
            name_range(&declaration),
        );
    }
    let value = node
        .children()
        .filter(|child| is_expression(child.kind()))
        .last()?;
    Some(expr(ctx, owner, &value))
}

/// One `when` entry ([spec: grammar-rule-whenEntry]): its conditions — the
/// expressions before the `->` — and its body, the `controlStructureBody`
/// after it.
///
/// An `in`/`!in` condition ([spec: grammar-rule-whenCondition] `rangeTest`) and
/// an `is`/`!is` one (`typeTest`) are tested against the `when` *subject*,
/// which the entry does not write, so the subject is what they carry.
fn when_arm(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
    subject: Option<ExprId>,
) -> WhenArm {
    let mut conditions = Vec::new();
    let mut body = None;
    for child in node.children() {
        match child.kind() {
            K::RANGE_TEST => {
                let negated = child
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| matches!(token.kind(), K::NOT_IN | K::NOT));
                let Some(container) = child_expression(&child) else {
                    continue;
                };
                let container = expr(ctx, owner, &container);
                if let Some(element) = subject {
                    conditions.push(WhenCondition::Containment {
                        element,
                        container,
                        negated,
                    });
                }
            }
            K::TYPE_TEST => {
                let negated = child
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| matches!(token.kind(), K::NOT_IS | K::NOT));
                let Some(ty) = inner_type(&child) else {
                    continue;
                };
                let expr = subject
                    .unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing, child.text_range()));
                conditions.push(WhenCondition::TypeTest {
                    expr,
                    ty: spanned_type(ctx, &ty),
                    negated,
                });
            }
            // The entry's body: the `controlStructureBody` after the arrow —
            // a block or a statement, never a bare expression node.
            _ if child.kind() == K::BLOCK || is_statement(child.kind()) => {
                body = Some(branch(ctx, owner, &child));
            }
            _ if is_expression(child.kind()) => {
                conditions.push(WhenCondition::Value(expr(ctx, owner, &child)));
            }
            _ => {}
        }
    }
    // A subject-less `when` whose conditions are *values* keeps them; an
    // entry with no written body (an erroneous tree) carries a missing one.
    WhenArm {
        conditions,
        body: body.unwrap_or_else(|| alloc_expr(ctx, ExprData::Missing, node.text_range())),
    }
}

/// A `try` expression ([KLS
/// `expressions.html#try-expressions`](https://kotlinlang.org/spec/expressions.html#try-expressions)):
/// a block, its `catch` blocks and its optional `finally` block.
fn try_expr(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let body = node
        .children()
        .find(|child| is(child, K::BLOCK))
        .map(|block| statement(ctx, owner, &block))
        .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing, node.text_range()));
    let catches = node
        .children()
        .filter(|child| is(child, K::CATCH_BLOCK))
        .map(|catch| catch_clause(ctx, owner, &catch))
        .collect();
    let finally = node
        .children()
        .find(|child| is(child, K::FINALLY_BLOCK))
        .and_then(|finally| finally.children().find(|child| is(child, K::BLOCK)))
        .map(|block| statement(ctx, owner, &block));
    ExprData::Try {
        body,
        catches,
        finally,
    }
}

/// One `catch (name: Type) { … }` of a `try` ([spec: grammar-rule-catchBlock]).
fn catch_clause(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> CatchClause {
    // The parameter is the identifier between the parentheses: the leading
    // `catch` is a *contextual* keyword, so it lexes as an identifier too and a
    // plain scan for the first identifier reads the keyword as the name.
    let mut after_paren = false;
    let param = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| {
            if is_token(token, K::L_PAREN) {
                after_paren = true;
                return false;
            }
            after_paren && matches!(token.kind(), K::IDENTIFIER | K::UNDERSCORE)
        })
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(|| Name::new("<missing>"));
    let ty = declared_type(ctx, node);
    let local = alloc_local(ctx, param, ty.clone(), node.text_range(), name_range(node));
    let body = node
        .children()
        .find(|child| is(child, K::BLOCK))
        .map(|block| statement(ctx, owner, &block))
        .unwrap_or_else(|| alloc_stmt(ctx, StmtData::Missing, node.text_range()));
    CatchClause {
        param: local,
        param_types: ty.into_iter().collect(),
        body,
    }
}

/// A jump expression ([KLS
/// `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)):
/// `return`, `break`, `continue`, `throw`, each optionally labeled.
fn jump(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let keyword = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| {
            matches!(
                token.kind(),
                K::RETURN_KW | K::THROW_KW | K::BREAK_KW | K::CONTINUE_KW
            )
        })
        .map(|token| token.kind());
    let kind = match keyword {
        Some(K::RETURN_KW) => JumpKind::Return,
        Some(K::THROW_KW) => JumpKind::Throw,
        Some(K::BREAK_KW) => JumpKind::Break,
        Some(K::CONTINUE_KW) => JumpKind::Continue,
        _ => return ExprData::Missing,
    };
    let value = child_expression(node).map(|value| expr(ctx, owner, &value));
    let label = node
        .children()
        .find(|child| is(child, K::LABEL))
        .map(|label| alloc_label(ctx, &label));
    ExprData::Jump { kind, value, label }
}

/// A lambda literal ([KLS
/// `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals)):
/// its parameters and its block. The parameter types are inferred from the
/// expected function type, so a parameter without one carries none.
fn lambda(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let params = lambda_params(ctx, node);
    // A lambda's statements are its own children — the grammar writes
    // `'{' … statements … '}'`, with no block node
    // ([spec: grammar-rule-lambdaLiteral]) — so the body is the same
    // statement-list-in-a-block shape a `BLOCK` produces.
    let stmts = node
        .children()
        .filter(|child| is_statement(child.kind()))
        .map(|child| statement(ctx, owner, &child))
        .collect::<Vec<_>>();
    let body = LambdaBody::Block(alloc_stmt(ctx, StmtData::Block(stmts), node.text_range()));
    ExprData::Lambda { params, body }
}

/// An anonymous function `fun(x: Int) = x + 1` / `fun() { … }` ([KLS
/// `expressions.html#anonymous-functions`](https://kotlinlang.org/spec/expressions.html#anonymous-functions)):
/// the function form of a lambda — a value, not a declaration, so it lowers to
/// [`ExprData::Lambda`] — with either an expression body (the `= expr` the
/// grammar spells instead of a block) or a block one.
fn anonymous_function(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let params = lambda_params(ctx, node);
    let body = match node.children().find(|child| is(child, K::BLOCK)) {
        Some(block) => {
            let stmts = lower_statement_list(ctx, owner, &block);
            LambdaBody::Block(alloc_stmt(ctx, StmtData::Block(stmts), block.text_range()))
        }
        None => match node
            .children()
            .filter(|child| is_expression(child.kind()))
            .last()
        {
            Some(value) => LambdaBody::Expr(expr(ctx, owner, &value)),
            None => return ExprData::Missing,
        },
    };
    ExprData::Lambda { params, body }
}

/// The declared parameters of a lambda literal or of an anonymous function
/// ([KLS `expressions.html#lambda-literals`](https://kotlinlang.org/spec/expressions.html#lambda-literals),
/// [`#anonymous-functions`](https://kotlinlang.org/spec/expressions.html#anonymous-functions)):
/// a `LAMBDA_PARAMETERS` node (`{ a, b -> … }`) or a `VALUE_PARAMETERS` one
/// (`fun(a: Int) = …`), each parameter bound with the type it writes — a
/// lambda's parameters usually write none, since the expected function type
/// determines them.
fn lambda_params(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<LambdaParam> {
    let Some(parameters) = node
        .children()
        .find(|child| matches!(child.kind(), K::LAMBDA_PARAMETERS | K::VALUE_PARAMETERS))
    else {
        return Vec::new();
    };
    parameters
        .children()
        .filter(|child| {
            matches!(
                child.kind(),
                K::LAMBDA_PARAMETER | K::VARIABLE_DECLARATION | K::VALUE_PARAMETER
            )
        })
        .map(|parameter| LambdaParam {
            name: variable_name(&parameter).unwrap_or_else(|| Name::new("it")),
            ty: declared_type(ctx, &parameter),
            annotations: Vec::new(),
            range: name_range(&parameter),
            // A destructuring parameter — `(a, b) -> …` — writes one
            // `variableDeclaration` per bound name, with no wrapper of its own
            // ([spec: grammar-rule-lambdaParameter]).
            destructured: parameter
                .children()
                .filter(|child| is(child, K::VARIABLE_DECLARATION))
                .filter_map(|declaration| variable_name(&declaration))
                .collect(),
        })
        .collect()
}

/// The label that qualifies a `this` ([KLS
/// `expressions.html#this-expressions`](https://kotlinlang.org/spec/expressions.html#this-expressions)):
/// the `outer` of `this@outer`, lowered as a one-segment type reference so the
/// label's own source range is anchored. `None` for a bare `this`.
///
/// The qualifier is a *label*, not a type — `this@outer` names the receiver of
/// the declaration labeled `outer` — and the shared body IR carries a
/// [`SpannedTypeRef`] here because Java's `Outer.this` qualifies by type. The
/// classifier reference is what an unqualified label resolves through, and the
/// type layer reads only the reference's name — a recorded deviation.
fn label_qualifier(node: &SyntaxNode<Lang>) -> Option<SpannedTypeRef> {
    // The label is the identifier after the `@` of `this@outer`.
    let name = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .skip_while(|token| !is_token(token, K::AT))
        .find(|token| is_token(token, K::IDENTIFIER))?;
    Some(SpannedTypeRef::new(
        syntax::stub::TypeRef::Reference {
            name: Name::new(name.text()),
            generic_args: Vec::new(),
        },
        vec![NameRef::new(Name::new(name.text()), name.text_range())],
    ))
}

/// The qualifier of a `super<Base>`/`super@label` ([KLS
/// `expressions.html#super-forms`](https://kotlinlang.org/spec/expressions.html#super-forms)):
/// the supertype whose member is named, lowered as the type it is, or the
/// label of `super@label`, lowered as [`label_qualifier`] lowers `this@label`.
/// `None` for a bare `super`.
fn super_qualifier(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<SpannedTypeRef> {
    if let Some(ty) = node.children().find(|child| is_type_node(child.kind())) {
        return Some(spanned_type(ctx, &ty));
    }
    label_qualifier(node)
}

/// A callable reference `::name` / `receiver::name` ([spec:
/// grammar-rule-callableReference]).
fn callable_reference(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let receiver = child_expression(node).map(|receiver| expr(ctx, owner, &receiver));
    let name = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(|| Name::new("<missing>"));
    ExprData::CallableReference { receiver, name }
}

/// An assignment statement ([spec: grammar-rule-statement]): the destination,
/// the operator and the value.
fn assign(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> ExprData {
    let (Some(lhs), Some(rhs)) = (first_expression(node), last_expression(node)) else {
        return ExprData::Missing;
    };
    let op = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| assign_op(token.kind()).is_some())
        .and_then(|token| assign_op(token.kind()))
        .unwrap_or(AssignOp::Assign);
    ExprData::Assign {
        op,
        lhs: expr(ctx, owner, &lhs),
        rhs: expr(ctx, owner, &rhs),
    }
}

fn assign_op(kind: K) -> Option<AssignOp> {
    Some(match kind {
        K::EQUAL => AssignOp::Assign,
        K::PLUS_EQUAL => AssignOp::Add,
        K::MINUS_EQUAL => AssignOp::Sub,
        K::MUL_EQUAL => AssignOp::Mul,
        K::DIV_EQUAL => AssignOp::Div,
        K::MODULO_EQUAL => AssignOp::Rem,
        _ => return None,
    })
}

/// The lowered arguments of a `VALUE_ARGUMENTS` node: one expression per
/// `VALUE_ARGUMENT`, a `*` spread wrapped in [`ExprData::Spread`].
fn value_arguments(ctx: &mut LowerCtx<'_>, owner: ItemId, node: &SyntaxNode<Lang>) -> Vec<ExprId> {
    node.children()
        .filter(|child| is(child, K::VALUE_ARGUMENT))
        .filter_map(|argument| {
            let spread = argument
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| is_token(&token, K::STAR));
            let value = argument
                .children()
                .filter(|child| is_expression(child.kind()))
                .last()?;
            let value = expr(ctx, owner, &value);
            Some(if spread {
                alloc_expr(ctx, ExprData::Spread { expr: value }, argument.text_range())
            } else {
                value
            })
        })
        .collect()
}

// -- small CST helpers -------------------------------------------------------

/// The first expression-kind child of `node`.
fn child_expression(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children().find(|child| is_expression(child.kind()))
}

/// The first expression-kind child of `node`, in source order.
fn first_expression(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    child_expression(node)
}

/// The last expression-kind child of `node`, in source order.
fn last_expression(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children()
        .filter(|child| is_expression(child.kind()))
        .last()
}

/// The type node of a cast or type test: the `TYPE`-family child that follows
/// the operand.
fn inner_type(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children()
        .filter(|child| is_type_node(child.kind()))
        .last()
}

/// The type node of `expr as T` / `expr is T`: the type follows the operator
/// (and is the only type node at the end of the node).
fn cast_type(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    inner_type(node)
}

/// The declared name of a variable declaration, or of the node that carries
/// one (`PROPERTY_DECL`, `VALUE_PARAMETER`, `CLASS_PARAMETER`).
fn variable_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    if is(node, K::VARIABLE_DECLARATION) {
        return node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| matches!(token.kind(), K::IDENTIFIER | K::UNDERSCORE))
            .map(|token| Name::new(token.text()));
    }
    node.children()
        .find(|child| is(child, K::VARIABLE_DECLARATION))
        .and_then(|declaration| variable_name(&declaration))
        .or_else(|| parameter_name(node))
}

/// The declared name of a parameter node.
fn parameter_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| is_token(token, K::IDENTIFIER))
        .find(|token| !matches!(token.text(), "vararg" | "noinline" | "crossinline"))
        .map(|token| Name::new(token.text()))
}

/// The source range of a declaration's own name token.
fn name_range(node: &SyntaxNode<Lang>) -> TextRange {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| matches!(token.kind(), K::IDENTIFIER | K::UNDERSCORE))
        .map(|token| token.text_range())
        .unwrap_or_else(|| node.text_range())
}

fn is(node: &SyntaxNode<Lang>, kind: K) -> bool {
    node.kind() == kind
}

fn is_token(token: &SyntaxToken<Lang>, kind: K) -> bool {
    token.kind() == kind
}
