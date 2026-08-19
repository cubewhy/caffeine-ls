//! The lowered per-file body IR ("body tree", after rust-analyzer).
//!
//! Where [`crate::item_tree::ItemTree`] carries declarations, the body
//! tree carries the executed *statements* and *expressions* of method bodies,
//! initializers, field initializers, enum constant arguments and annotation
//! element defaults, mirroring [JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html)
//! (statements) and [JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html)
//! (expressions). It is lowered alongside the item tree (one salsa query, one
//! parse per file) and embedded in [`ItemTree`], so edits invalidate bodies
//! together with declarations.
//!
//! The IR is an arena of [`ExprData`] and [`StmtData`] plus the [`Body`]
//! records that group the statements (and parameter bindings) of a single
//! method or initializer. [`ExprId`]s, [`StmtId`]s and [`LocalId`]s are
//! stable per file; a [`TypeRef`] appears where a declared type exists, so
//! [`crate::Ty`]-level resolution happens later against the file's resolver.

use syntax::stub::TypeRef;

use crate::{
    arena::{Arena, ArenaId},
    item_tree::ItemId,
    name::Name,
};

/// The id of an expression within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub ArenaId);

/// The id of a statement within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StmtId(pub ArenaId);

/// The id of a local variable binding (parameter, declared local, catch
/// parameter, for variable) within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub ArenaId);

/// The id of a label within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LabelId(pub ArenaId);

/// The id of a [`Body`] within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BodyId(pub ArenaId);

impl std::fmt::Display for ExprId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "e{}", self.0.0)
    }
}

impl std::fmt::Display for StmtId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s{}", self.0.0)
    }
}

impl std::fmt::Display for LocalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "l{}", self.0.0)
    }
}

impl std::fmt::Display for LabelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "label{}", self.0.0)
    }
}

impl std::fmt::Display for BodyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.0.0)
    }
}

/// The statement and expression arenas of one file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BodyTree {
    pub exprs: Arena<ExprData>,
    pub stmts: Arena<StmtData>,
    pub locals: Arena<Local>,
    pub labels: Arena<Label>,
    pub bodies: Arena<Body>,
}

impl BodyTree {
    pub fn expr(&self, id: ExprId) -> &ExprData {
        self.exprs.get(id.0)
    }

    pub fn stmt(&self, id: StmtId) -> &StmtData {
        self.stmts.get(id.0)
    }

    pub fn local(&self, id: LocalId) -> &Local {
        self.locals.get(id.0)
    }

    pub fn label(&self, id: LabelId) -> &Label {
        self.labels.get(id.0)
    }

    pub fn body(&self, id: BodyId) -> &Body {
        self.bodies.get(id.0)
    }
}

/// The statement list and parameter bindings of one method, constructor,
/// initializer block, field initializer, enum constant argument list or
/// annotation element default.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    /// The owning declaration, when the body belongs to a method,
    /// constructor, initializer or annotation element.
    pub owner: Option<ItemId>,
    /// The parameters of the owning method or constructor.
    pub params: Vec<LocalId>,
    /// The statements of the body.
    pub stmts: Vec<StmtId>,
}

/// A local variable binding: a parameter, a declared local, a catch parameter
/// or a for-loop variable. The declared type is `None` for var/`var`-less
/// parameters and patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct Local {
    pub name: Name,
    pub ty: Option<TypeRef<Name>>,
}

/// A label name for [`StmtData::Break`], [`StmtData::Continue`],
/// [`StmtData::Labeled`] and `yield`.
#[derive(Debug, Clone, PartialEq)]
pub struct Label(pub Name);

/// A statement, mirroring the statement forms of
/// [JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html).
#[derive(Debug, Clone, PartialEq)]
pub enum StmtData {
    /// An empty statement `;` ([§14.6]).
    Empty,
    /// A block ([§14.5]).
    Block(Vec<StmtId>),
    /// A local variable declaration with an optional initializer ([§14.4]).
    Decl {
        local: LocalId,
        initializer: Option<ExprId>,
    },
    /// An expression statement ([§14.8]).
    Expr(ExprId),
    /// A labeled statement ([§14.7]).
    Labeled { label: LabelId, stmt: StmtId },
    /// An `if` statement ([§14.9]).
    If {
        cond: ExprId,
        then: StmtId,
        els: Option<StmtId>,
    },
    /// A `while` statement ([§14.12]).
    While { cond: ExprId, body: StmtId },
    /// A `do` statement ([§14.13]).
    DoWhile { body: StmtId, cond: ExprId },
    /// A basic `for` statement ([§14.14.1]).
    For {
        init: Vec<StmtId>,
        cond: Option<ExprId>,
        step: Vec<ExprId>,
        body: StmtId,
    },
    /// An enhanced `for` statement ([§14.14.2]).
    ForEach {
        var: LocalId,
        iterable: ExprId,
        body: StmtId,
    },
    /// A `switch` statement ([§14.11]).
    Switch {
        scrutinee: ExprId,
        arms: Vec<SwitchArm>,
    },
    /// A `return` statement ([§14.17]).
    Return(Option<ExprId>),
    /// A `throw` statement ([§14.18]).
    Throw(ExprId),
    /// A `break` statement ([§14.15]).
    Break(Option<LabelId>),
    /// A `continue` statement ([§14.16]).
    Continue(Option<LabelId>),
    /// A `yield` statement ([§14.21]).
    Yield(ExprId),
    /// A `synchronized` statement ([§14.19]).
    Synchronized { expr: ExprId, body: StmtId },
    /// A `try` statement, `try`-catch and `try` with resources ([§14.20]).
    Try {
        resources: Vec<LocalId>,
        body: StmtId,
        catches: Vec<CatchClause>,
        finally: Option<StmtId>,
    },
    /// An `assert` statement ([§14.10]).
    Assert { cond: ExprId, msg: Option<ExprId> },
    /// An expression or statement that could not be lowered.
    Missing,
}

/// One `catch (Param) Block` of a `try` statement ([§14.20]).
#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: LocalId,
    pub body: StmtId,
}

/// One arm of a `switch` statement or expression: `case` labels (an empty
/// label list for `default`), followed by the statements of a block group or
/// the single consequent of `case ... ->`.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchArm {
    pub labels: Vec<ExprId>,
    pub body: Vec<StmtId>,
}

/// An expression, mirroring the expression forms of
/// [JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html).
#[derive(Debug, Clone, PartialEq)]
pub enum ExprData {
    /// A literal ([§3.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10)).
    Literal(Literal),
    /// The null literal ([§3.10.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10.8)).
    Null,
    /// `this` or `TypeName.this` ([§15.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.3)).
    This { qualifier: Option<TypeRef<Name>> },
    /// `super` ([§15.8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.4)).
    Super,
    /// A class literal `Foo.class` ([§15.8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2)).
    ClassLit(TypeRef<Name>),
    /// A single name: a local variable or parameter reference, or — after
    /// resolution — a field of an implicit receiver.
    Var(Name),
    /// A qualified name in expression position falling through to a
    /// [`crate::Resolver`]: `Outer.Inner`, `Type.field`, ... Kept as raw text.
    NamePath(Name),
    /// A field access `expr.name` ([§15.11](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11)),
    /// with an implicit receiver when `target` is empty.
    FieldAccess { target: Option<ExprId>, name: Name },
    /// An array access `array[index]` ([§15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13)).
    ArrayAccess { array: ExprId, index: ExprId },
    /// A method invocation ([§15.12](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12)),
    /// with an implicit receiver when `receiver` is empty.
    MethodCall {
        receiver: Option<ExprId>,
        name: Name,
        type_args: Vec<TypeRef<Name>>,
        args: Vec<ExprId>,
    },
    /// A class instance creation `new Type(args)` ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)).
    /// Anonymous class bodies are not modelled (a `Missing` body).
    New {
        ty: TypeRef<Name>,
        args: Vec<ExprId>,
    },
    /// An array creation `new Type[dims]` ([§15.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10)),
    /// or — with empty `dims` — `new Type[] { ... }`, whose `initializer`
    /// ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6))
    /// carries the element expressions of an array initializer when present.
    NewArray {
        ty: TypeRef<Name>,
        dims: Vec<ExprId>,
        initializer: Option<Vec<ExprId>>,
    },
    /// An array initializer `{ a, b }` ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6)).
    ArrayInit(Vec<ExprId>),
    /// A unary or prefix expression `+x`, `-x`, `~x`, `!x`, `++x`, `--x`
    /// ([§15.15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.15)).
    Unary { op: UnaryOp, expr: ExprId },
    /// A postfix expression `x++`, `x--` ([§14.14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-14.14)).
    Postfix { op: PostfixOp, expr: ExprId },
    /// A binary expression ([§15.17](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.17)–[§15.24](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.24)).
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    /// An assignment `lhs = rhs` or compound assignment ([§15.26](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.26)).
    Assign {
        op: AssignOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    /// A cast `(Type) expr` ([§15.16](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.16)).
    Cast { ty: TypeRef<Name>, expr: ExprId },
    /// An `instanceof` test `expr instanceof Type` ([§15.20.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.20.2)).
    InstanceOf { expr: ExprId, ty: TypeRef<Name> },
    /// A conditional expression `cond ? then : els` ([§15.25](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25)).
    Conditional {
        cond: ExprId,
        then: ExprId,
        els: ExprId,
    },
    /// A lambda expression ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27)).
    Lambda {
        params: Vec<(Name, Option<TypeRef<Name>>)>,
        body: LambdaBody,
    },
    /// A method reference `Type::name` / `expr::name` / `Type::new`
    /// ([§15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13)).
    MethodRef {
        qualifier: Option<ExprId>,
        type_name: Option<TypeRef<Name>>,
        name: Name,
    },
    /// A switch expression ([§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)).
    Switch {
        scrutinee: ExprId,
        arms: Vec<SwitchArm>,
    },
    /// A parenthesized expression ([§15.8.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.5)).
    Paren(ExprId),
    /// An expression that could not be lowered.
    Missing,
}

/// The kind of a literal ([JLS §3.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Literal {
    Int,
    Long,
    Char,
    Float,
    Double,
    Boolean,
    Str,
}

/// A lambda body: an expression or a block ([JLS §15.27.2]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaBody {
    Expr(ExprId),
    Block(StmtId),
}

/// A unary operator ([JLS §15.15]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    BitNot,
    Not,
    /// Prefix `++`.
    Inc,
    /// Prefix `--`.
    Dec,
}

/// A postfix operator ([JLS §14.14]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostfixOp {
    Inc,
    Dec,
}

/// A binary operator ([JLS §15.17]–[§15.24]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    UShr,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    /// `&&`
    And,
    /// `||`
    Or,
}

/// An assignment operator ([JLS §15.26]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    UShr,
    BitAnd,
    BitXor,
    BitOr,
}
