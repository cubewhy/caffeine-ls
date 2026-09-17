//! The lowered per-file body IR ("body tree", after rust-analyzer).
//!
//! Where the item tree (`hir_def::java::item_tree::ItemTree`) carries
//! declarations, the body tree carries the executed *statements* and
//! *expressions* of method bodies, initializers, field initializers, enum
//! constant arguments and annotation element defaults, mirroring
//! [JLS §14](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html)
//! (statements) and [JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html)
//! (expressions). It is lowered alongside the item tree (one salsa query, one
//! parse per file) and embedded in `LoweredFile`, so edits invalidate bodies
//! together with declarations.
//!
//! The IR is an arena of [`ExprData`] and [`StmtData`] plus the [`Body`]
//! records that group the statements (and parameter bindings) of a single
//! method or initializer. [`ExprId`]s, [`StmtId`]s and [`LocalId`]s are
//! stable per file; a [`TypeRef`] appears where a declared type exists, so
//! [`crate::Ty`]-level resolution happens later against the file's resolver.

use rowan::TextRange;

use crate::{
    arena::{Arena, ArenaId},
    ids::ItemId,
    name::Name,
    span::{AnnotationRef, SpannedTypeRef},
};

/// The id of an expression within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub ArenaId);

/// The id of a statement within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StmtId(pub ArenaId);

/// The id of a local variable binding (parameter, declared local, catch
/// parameter, for variable, pattern variable) within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub ArenaId);

/// The id of a pattern within its owning [`BodyTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PatternId(pub ArenaId);

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

impl std::fmt::Display for PatternId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p{}", self.0.0)
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
    pub patterns: Arena<PatternData>,
    pub labels: Arena<Label>,
    pub bodies: Arena<Body>,
    /// The source range of every expression, parallel to [`BodyTree::exprs`]:
    /// index `n` is the range of the expression with arena id `n`.
    pub expr_ranges: Vec<TextRange>,
    /// The source range of the *name* of every expression, parallel to
    /// [`BodyTree::exprs`]: for a member access, invocation or method
    /// reference this is the range of the member/method identifier (so a
    /// diagnostic underlines `missing` in `b.missing`, not the whole
    /// receiver chain); for other expressions it equals [`BodyTree::expr_ranges`].
    pub expr_name_ranges: Vec<TextRange>,
    /// The source range of every local (parameter or declared local), parallel
    /// to [`BodyTree::locals`].
    pub local_ranges: Vec<TextRange>,
    /// The source range of the *name* of every local, parallel to
    /// [`BodyTree::locals`]: the identifier the declaration's name was read
    /// from — `b` of a `Base b` parameter, `x` of an `int x = 0` declarator,
    /// the binding of a type pattern. A navigation target covers the variable
    /// itself, not the declarator it was written in; for a binding with no
    /// identifier of its own it equals [`BodyTree::local_ranges`].
    pub local_name_ranges: Vec<TextRange>,
    /// The source range of every pattern, parallel to [`BodyTree::patterns`].
    pub pattern_ranges: Vec<TextRange>,
    /// The source range of every statement, parallel to [`BodyTree::stmts`]:
    /// index `n` is the range of the statement with arena id `n`.
    pub stmt_ranges: Vec<TextRange>,
}

impl BodyTree {
    pub fn expr(&self, id: ExprId) -> &ExprData {
        self.exprs.get(id.0)
    }

    /// The source range of the expression, when the expression was lowered
    /// from a syntax node (synthetic `Missing` expressions have none).
    pub fn expr_range(&self, id: ExprId) -> Option<TextRange> {
        self.expr_ranges.get(id.0.0 as usize).copied()
    }

    /// The source range of the *name* of the expression, when the expression
    /// was lowered from a syntax node: the member/method identifier of a
    /// field access, invocation or method reference, else the whole
    /// expression range. See [`BodyTree::expr_name_ranges`].
    pub fn expr_name_range(&self, id: ExprId) -> Option<TextRange> {
        self.expr_name_ranges.get(id.0.0 as usize).copied()
    }

    pub fn stmt(&self, id: StmtId) -> &StmtData {
        self.stmts.get(id.0)
    }

    /// The source range of the statement, when the statement was lowered from
    /// a syntax node (synthetic `Missing` statements have none — their empty
    /// placeholder range reads as `None`).
    pub fn stmt_range(&self, id: StmtId) -> Option<TextRange> {
        self.stmt_ranges
            .get(id.0.0 as usize)
            .copied()
            .filter(|range| !range.is_empty())
    }

    pub fn local(&self, id: LocalId) -> &Local {
        self.locals.get(id.0)
    }

    /// The source range of the local, when it was lowered from a syntax node.
    pub fn local_range(&self, id: LocalId) -> Option<TextRange> {
        self.local_ranges.get(id.0.0 as usize).copied()
    }

    /// The source range of the local's *name*, when it was lowered from a
    /// syntax node. See [`BodyTree::local_name_ranges`].
    pub fn local_name_range(&self, id: LocalId) -> Option<TextRange> {
        self.local_name_ranges.get(id.0.0 as usize).copied()
    }

    pub fn pattern(&self, id: PatternId) -> &PatternData {
        self.patterns.get(id.0)
    }

    /// The source range of the pattern, when it was lowered from a syntax node.
    pub fn pattern_range(&self, id: PatternId) -> Option<TextRange> {
        self.pattern_ranges.get(id.0.0 as usize).copied()
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

/// A local variable binding: a parameter, a declared local, a catch parameter,
/// a for-loop variable or a pattern variable. The declared type is `None` for
/// var/`var`-less parameters and patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct Local {
    pub name: Name,
    pub ty: Option<SpannedTypeRef>,
    /// The annotations written as modifiers of the declaration
    /// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)),
    /// in source order — the *declaration* annotations of this variable, as
    /// opposed to the type annotations its type carries. A method or
    /// constructor parameter's declaration annotations are lowered with the
    /// signature instead ([`hir_def::jvm::decl::Param::annotations`]);
    /// every other variable declaration — a local, a for-loop or enhanced-for
    /// variable, a resource, a catch parameter, a pattern variable and a
    /// lambda parameter ([`LambdaParam`]) — carries them here.
    pub annotations: Vec<AnnotationRef>,
    /// Whether the declaration carries the `final` modifier ([§4.12.4]):
    /// a `final` local whose initializer is a constant expression is a
    /// *constant variable*, and reads of it are constant expressions
    /// ([§15.28]).
    pub is_final: bool,
    /// Whether the binding is Kotlin's `var` — the one binding form that may be
    /// reassigned ([KLS
    /// `declarations.html#read-only-property-declaration`](https://kotlinlang.org/spec/declarations.html#read-only-property-declaration)).
    ///
    /// The opposite polarity of [`Self::is_final`], which is Java's: a Java
    /// local is neither, so the Java lowering leaves this `false` and ignores
    /// it — Java's definite-assignment and final-field rules are what refuse a
    /// reassignment there ([JLS §16]), and they are checked elsewhere.
    pub is_mutable: bool,
}

/// A type pattern `Foo f` ([JLS §14.30.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.1)):
/// a type and an optional binding. The binding is a [`LocalId`] so the
/// pattern variable participates in the local arena (and so in name
/// resolution and the type layer); `Foo _` and the match-all `_` bind nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct TypePattern {
    pub ty: SpannedTypeRef,
    /// The pattern variable binding — `Foo f`; `Foo _` and `_` have none.
    pub binding: Option<LocalId>,
}

/// A record pattern `Point(int x, int y)`
/// ([JLS §14.30.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.2)):
/// a reference type and the component patterns, each of which may bind its own
/// variable.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordPattern {
    pub ty: SpannedTypeRef,
    pub components: Vec<PatternId>,
}

/// A pattern ([JLS §14.30](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30)):
/// a type pattern, a record pattern, or the match-all pattern of
/// [§14.30.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.3).
/// Patterns appear in `instanceof` expressions ([§15.20.2]) and as `case`
/// labels ([§14.30.2], [§14.30.3]); the variables they bind are scoped by
/// flow scoping ([§14.30.3]) in the type layer.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternData {
    /// A type pattern `Foo f` ([§14.30.1]).
    Type(TypePattern),
    /// A record pattern `Point(int x, int y)` ([§14.30.2]).
    Record(RecordPattern),
    /// The match-all pattern `_` ([§14.30.3]): matches everything and binds
    /// nothing.
    MatchAll,
    /// A Kotlin destructuring declaration `val (a, b) = pair` ([KLS
    /// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)):
    /// one bound name per component, in source order, each the local it binds
    /// (the type layer resolves them through the initializer's
    /// `component1()`/`component2()`).
    Destructuring { parts: Vec<LocalId> },
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
    /// A Kotlin local property declaration bound to a delegate — `val x by
    /// lazy { … }` ([KLS
    /// `declarations.html#delegated-property-declaration`](https://kotlinlang.org/spec/declarations.html#delegated-property-declaration)):
    /// the local is bound to the `by` expression, whose `getValue` result is
    /// its value. The Java statement forms never produce this variant.
    DeclDelegated { local: LocalId, delegate: ExprId },
    /// A multi-declarator local declaration `int a = 1, b = 2;` ([§14.4]):
    /// the declarators of one declaration statement, lowered in order. Unlike
    /// a [`StmtData::Block`], this is *not* a lexical scope — every declarator
    /// of a single declaration statement is declared in the enclosing scope
    /// ([§6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)),
    /// so each inner statement must be inferred without pushing a scope.
    DeclGroup(Vec<StmtId>),
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
        /// The destructuring pattern of a Kotlin `for ((k, v) in xs)` ([KLS
        /// `expressions.html#destructuring-declarations`](https://kotlinlang.org/spec/expressions.html#destructuring-declarations)):
        /// one bound local per component, of which `var` is the first. `None`
        /// for a loop that binds one name ([§14.14.2]) and for a Java
        /// enhanced `for`, which never writes one.
        pattern: Option<PatternId>,
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
        resources: Vec<Resource>,
        body: StmtId,
        catches: Vec<CatchClause>,
        finally: Option<StmtId>,
    },
    /// An `assert` statement ([§14.10]).
    Assert { cond: ExprId, msg: Option<ExprId> },
    /// A local class, interface, record or enum declaration
    /// ([§14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)):
    /// the declaration is an item of the file's item tree (the tree's
    /// `local_types` list), and its simple name is in scope for the rest of
    /// the immediately enclosing block. The item is carried rather than a name
    /// because §6.7 gives a local class neither a fully qualified nor a
    /// canonical name: its identity is its declaration.
    LocalClass { item: ItemId },
    /// An expression or statement that could not be lowered.
    Missing,
    /// A Kotlin destructuring declaration `val (a, b) = pair` ([KLS
    /// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)):
    /// one statement binding every name the pattern declares from the
    /// initializer's `component1()`/`component2()`, so the names are *not*
    /// each equal to the initializer.
    Destructuring {
        pattern: PatternId,
        initializer: ExprId,
    },
    /// A Kotlin local function declaration ([KLS
    /// `declarations.html#local-function-declaration`](https://kotlinlang.org/spec/declarations.html#local-function-declaration)):
    /// the declaration is an item of the file's item tree (the tree's
    /// `local_types` list), carried rather than a name for the same reason as
    /// [`StmtData::LocalClass`].
    LocalFunction { item: ItemId },
}

/// One `catch (Param) Block` of a `try` statement ([§14.20]).
#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: LocalId,
    /// The declared types of the catch parameter ([§14.20]): a *multi-catch*
    /// `catch (A | B e)` lists every alternative, an ordinary catch exactly
    /// one — whose resolution also types the parameter itself (the first
    /// entry). Each alternative independently gates whether the clause may
    /// name a checked exception and which thrown exceptions it discharges.
    pub param_types: Vec<SpannedTypeRef>,
    pub body: StmtId,
}

/// A resource of a try-with-resources statement
/// ([JLS §14.20.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.20.3)):
/// each resource is a local variable declaration `Type var = init` (or a
/// `var` declaration) — a bare `VariableAccess` resource names an existing
/// variable and declares nothing, so it produces no [`Resource`] entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    /// The declared resource variable.
    pub local: LocalId,
    /// The resource initializer.
    pub initializer: Option<ExprId>,
}

/// One arm of a `switch` statement or expression: `case` labels (an empty
/// label list for `default`), followed by the statements of a block group or
/// the single consequent of `case ... ->`.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchArm {
    pub labels: Vec<SwitchLabel>,
    pub body: Vec<StmtId>,
    /// Whether this arm is a `case ... ->` rule
    /// ([JLS §14.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.1),
    /// [§14.11.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.3)):
    /// a rule's consequent is the whole arm and never falls through to the
    /// next group. A `case ...:` *block statement group* on the other hand
    /// falls through to the following group when it completes normally
    /// ([§14.11.3]).
    pub rule: bool,
}

/// One `case` label of a switch
/// ([JLS §14.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.11.1),
/// [§15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28)):
/// a constant expression (or `null`), or a pattern
/// ([§14.30.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.2),
/// [§14.30.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30.3)).
/// The `default` label is lowered as [`SwitchLabel::Expr`] of a `Missing`
/// expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SwitchLabel {
    /// A constant expression label `case 1, 2` or `null`.
    Expr(ExprId),
    /// A pattern label `case Foo f`, `case Point(int x, int y)`, `case _`.
    Pattern(PatternId),
    /// A guarded pattern label's condition — the `when cond` of
    /// `case Foo f when cond` ([§14.11.1]). It is a boolean expression
    /// evaluated when its pattern matched, not a case label of its own.
    Guard(ExprId),
}

/// The target of an explicit constructor invocation
/// ([JLS §8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
/// `this(args)` delegates to a constructor of the same class, `super(args)`
/// invokes a constructor of the direct superclass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtorCallTarget {
    This,
    Super,
}

/// An expression, mirroring the expression forms of
/// [JLS §15](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html).
#[derive(Debug, Clone, PartialEq)]
pub enum ExprData {
    /// A literal ([§3.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10)).
    Literal(Literal),
    /// The null literal ([§3.10.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10.8)).
    Null,
    /// A string template `STR."\{expr}"` ([JLS §15.8.6]; a preview feature
    /// removed in JLS 23). The processor is not modelled — the expression
    /// types as `String` — and each `args` element is an embedded expression,
    /// inferred by the type layer.
    Template { args: Vec<ExprId> },
    /// `this` or `TypeName.this` ([§15.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.3)).
    This { qualifier: Option<SpannedTypeRef> },
    /// `super` or `TypeName.super` — the latter only as the receiver of a
    /// method invocation ([§15.8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.4),
    /// [§15.11.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.2)):
    /// the qualified form invokes the default method of the named interface.
    Super { qualifier: Option<SpannedTypeRef> },
    /// A class literal `Foo.class` ([§15.8.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2)).
    ClassLit(SpannedTypeRef),
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
        type_args: Vec<SpannedTypeRef>,
        args: Vec<ExprId>,
        /// The name each written argument carries, parallel to `args` — the
        /// `a` of `f(a = 1)` ([KLS
        /// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
        /// A Java call writes none, so its lowering leaves this empty.
        arg_names: Vec<Option<Name>>,
        /// The index of the call's *trailing* lambda, when it has one: the
        /// lambda written after the argument list, which binds to the **last**
        /// parameter rather than positionally
        /// (<https://kotlinlang.org/docs/lambdas.html#passing-trailing-lambdas>).
        /// A lambda written inside the parentheses is a positional argument and
        /// carries no index.
        trailing: Option<usize>,
    },
    /// A class instance creation `new Type(args)` ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)).
    New {
        ty: SpannedTypeRef,
        args: Vec<ExprId>,
        /// `new Foo<>()` — the diamond operator
        /// ([§15.9.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.2)):
        /// the type arguments are inferred from the target type by the type
        /// layer. `false` for a raw type and an explicit argument list.
        diamond: bool,
        /// The methods declared in an anonymous class body
        /// ([§15.9.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.5)):
        /// each member's name and parameter count, so the body is not
        /// dropped. Empty for a plain instance creation.
        members: Vec<AnonymousMethod>,
        /// Whether the creation carries an anonymous class body at all
        /// ([§15.9.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.5)):
        /// `new C(args) { ... }` — even an empty `{}`. The anonymous class is
        /// a subclass of `C`, which widens the access of the constructor
        /// invocation ([§6.6.2]), so this must not be inferred from
        /// [`Self::New::members`] being non-empty.
        anonymous: bool,
        /// §15.9: the enclosing instance of a *qualified* class instance
        /// creation — `primary.new Inner(args)`. `None` for the unqualified
        /// form.
        receiver: Option<ExprId>,
    },
    /// An explicit constructor invocation in a constructor body
    /// ([§8.8.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8.7.1)):
    /// `this(args)` delegates to another constructor of the same class,
    /// `super(args)` invokes a direct superclass constructor; as a statement
    /// form it has no value.
    CtorCall {
        args: Vec<ExprId>,
        target: CtorCallTarget,
    },
    /// An array creation `new Type[dims]` ([§15.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.10)),
    /// or — with empty `dims` — `new Type[] { ... }`, whose `initializer`
    /// ([§10.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-10.html#jls-10.6))
    /// carries the element expressions of an array initializer when present.
    NewArray {
        ty: SpannedTypeRef,
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
    /// A cast `(Type) expr` ([§15.16](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.16)),
    /// or Kotlin's `expr as T` / `expr as? T` ([KLS
    /// `expressions.html#cast-expressions`](https://kotlinlang.org/spec/expressions.html#cast-expressions)).
    /// `safe` is Kotlin's `as?`, which yields `null` where the cast cannot
    /// succeed — and is *not* the same as `as T?`, which throws (kotlinc
    /// 2.4.20: `1 as? String` is `null`, `1 as String?` fails with
    /// `ClassCastException`). Always `false` for a Java cast.
    Cast {
        ty: SpannedTypeRef,
        expr: ExprId,
        safe: bool,
    },
    /// An `instanceof` test `expr instanceof Type` or, with a pattern,
    /// `expr instanceof Pattern` ([§15.20.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.20.2)).
    /// Exactly one of `ty` (a plain type test) and `pattern` (a pattern test,
    /// [§14.30](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.30))
    /// is set; a pattern test binds its pattern variables by flow scoping
    /// ([§14.30.3]).
    InstanceOf {
        expr: ExprId,
        ty: Option<SpannedTypeRef>,
        pattern: Option<PatternId>,
    },
    /// A conditional expression `cond ? then : els` ([§15.25](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25)).
    Conditional {
        cond: ExprId,
        then: ExprId,
        els: ExprId,
    },
    /// A lambda expression ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27)).
    Lambda {
        params: Vec<LambdaParam>,
        body: LambdaBody,
    },
    /// A method reference `Type::name` / `expr::name` / `Type::new`
    /// ([§15.13](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.13)).
    MethodRef {
        qualifier: Option<ExprId>,
        type_name: Option<SpannedTypeRef>,
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
    /// A Kotlin block in *expression* position ([KLS
    /// `expressions.html#expressions`](https://kotlinlang.org/spec/expressions.html#expressions)):
    /// a branch of an `if` or a `when` (`if (c) { a; b } else { d }`), whose
    /// value is the last expression of its statement list. The statements are
    /// the same `StmtData::Block` the block lowers to as a statement, so the
    /// block is not lowered twice.
    Block(StmtId),
    /// A Kotlin `when` expression ([KLS
    /// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)),
    /// with the subject it scrutinizes (`None` for a subject-less `when`).
    /// Every Kotlin `when` lowers to this form; used as a *statement* it is
    /// wrapped in [`StmtData::Expr`], and needs no `else` arm
    /// (kotlinc 2.4.20: a `when` statement that is not exhaustive compiles,
    /// while `val x: Int = when (1) { 1 -> 2 }` fails with *"'when' expression
    /// must be exhaustive. Add an 'else' branch."*).
    When {
        subject: Option<ExprId>,
        arms: Vec<WhenArm>,
    },
    /// A Kotlin `try` expression ([KLS
    /// `expressions.html#try-expressions`](https://kotlinlang.org/spec/expressions.html#try-expressions)).
    /// Unlike Java's — a statement ([`StmtData::Try`]) — a Kotlin `try` is an
    /// expression whose value is the last expression of the block that ran.
    /// Kotlin has no `try`-with-resources.
    Try {
        body: StmtId,
        catches: Vec<CatchClause>,
        finally: Option<StmtId>,
    },
    /// A Kotlin elvis expression `lhs ?: rhs` ([KLS
    /// `expressions.html#elvis-operator-expressions`](https://kotlinlang.org/spec/expressions.html#elvis-operator-expressions)).
    Elvis { lhs: ExprId, rhs: ExprId },
    /// A Kotlin safe navigation `receiver?.member` ([KLS
    /// `expressions.html#navigation-operators`](https://kotlinlang.org/spec/expressions.html#navigation-operators)).
    /// The member is the lowered [`ExprData::FieldAccess`] or
    /// [`ExprData::MethodCall`] the suffix names; the access is performed only
    /// when the receiver is not null, and its result is nullable.
    SafeAccess { receiver: ExprId, member: ExprId },
    /// A Kotlin not-null assertion `expr!!` ([KLS
    /// `expressions.html#not-null-assertion-expressions`](https://kotlinlang.org/spec/expressions.html#not-null-assertion-expressions)),
    /// the one place a nullable value is used as a non-null one.
    NullAssert { expr: ExprId },
    /// A Kotlin range expression `lhs..rhs` / `lhs..<rhs` ([KLS
    /// `expressions.html#range-expressions`](https://kotlinlang.org/spec/expressions.html#range-expressions)).
    /// `inclusive` is `false` for the `..<` form.
    Range {
        lhs: ExprId,
        rhs: ExprId,
        inclusive: bool,
    },
    /// A Kotlin infix function call `receiver name arg` ([KLS
    /// `declarations.html#infix-functions`](https://kotlinlang.org/spec/declarations.html#infix-functions)):
    /// an invocation written without parentheses, resolved like any other call.
    InfixCall {
        receiver: ExprId,
        name: Name,
        arg: ExprId,
    },
    /// A Kotlin object literal `object : Base() { … }` ([KLS
    /// `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals)):
    /// the members are the item tree's declaration (`item`), and the expression's
    /// type is the anonymous class it denotes.
    ObjectLiteral { item: ItemId },
    /// A Kotlin callable reference `receiver::name` / `::name` ([KLS
    /// `expressions.html#callable-references`](https://kotlinlang.org/spec/expressions.html#callable-references)).
    CallableReference {
        receiver: Option<ExprId>,
        name: Name,
    },
    /// A Kotlin spread argument `*array` ([KLS
    /// `expressions.html#spread-operator-expressions`](https://kotlinlang.org/spec/expressions.html#spread-operator-expressions)).
    Spread { expr: ExprId },
    /// A Kotlin jump expression ([KLS
    /// `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)):
    /// `return`, `break`, `continue` and `throw` are *expressions* of type
    /// `kotlin.Nothing`, so they can be written where a value is expected
    /// (`val x = if (c) 1 else throw E()`). In statement position the
    /// expression is wrapped in [`StmtData::Expr`], and the Java statements
    /// [`StmtData::Return`]/[`StmtData::Throw`]/[`StmtData::Break`]/[`StmtData::Continue`]
    /// stay Java-only.
    Jump {
        kind: JumpKind,
        value: Option<ExprId>,
        label: Option<LabelId>,
    },
}

/// The kind of a Kotlin jump expression ([KLS
/// `expressions.html#jump-expressions`](https://kotlinlang.org/spec/expressions.html#jump-expressions)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpKind {
    Return,
    Throw,
    Break,
    Continue,
}

/// One entry of a Kotlin `when` expression ([KLS
/// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)):
/// the conditions it matches (`None` conditions are the `else` arm) and the
/// expression it evaluates.
#[derive(Debug, Clone, PartialEq)]
pub struct WhenArm {
    /// The conditions of the entry, in source order. Empty for the `else`
    /// arm, which is the only arm without conditions.
    pub conditions: Vec<WhenCondition>,
    pub body: ExprId,
}

/// One condition of a Kotlin `when` entry ([KLS
/// `expressions.html#when-expressions`](https://kotlinlang.org/spec/expressions.html#when-expressions)):
/// a value compared for equality, `in`/`!in` containment, or an `is`/`!is`
/// type test.
#[derive(Debug, Clone, PartialEq)]
pub enum WhenCondition {
    /// A value condition: the subject is compared with it for equality
    /// ([`ExprData::Binary`] over `EQEQ` after lowering).
    Value(ExprId),
    /// An `is`/`!is` type test
    /// ([KLS `expressions.html#type-checking-expressions`](https://kotlinlang.org/spec/expressions.html#type-checking-expressions)):
    /// `negated` is `true` for `!is`. The tested type is the condition's own.
    TypeTest {
        expr: ExprId,
        ty: SpannedTypeRef,
        negated: bool,
    },
    /// An `in`/`!in` containment test ([KLS
    /// `expressions.html#containment-checking-expressions`](https://kotlinlang.org/spec/expressions.html#containment-checking-expressions)).
    Containment {
        element: ExprId,
        container: ExprId,
        negated: bool,
    },
}

/// The kind of a literal ([JLS §3.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10)).
/// Integral and character literals carry their value: a constant expression's
/// value drives the narrowing conversion of assignment context
/// ([JLS §5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2))
/// and definite assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// An integer literal ([§3.10.1]) with its parsed value (underscores and
    /// any suffix stripped).
    Int(i64),
    /// A long literal ([§3.10.1]) with its parsed value.
    Long(i64),
    /// A character literal ([§3.10.4]) with its decoded scalar value.
    Char(char),
    Float,
    Double,
    /// A boolean literal ([§3.10.3]) with its value.
    Boolean(bool),
    /// A string literal or text block ([§3.10.5], [§3.10.6]) with its decoded
    /// value — escapes resolved, text-block incidental whitespace stripped —
    /// so constant expressions over strings can be evaluated ([§15.28]).
    Str(String),
}

/// A lambda body: an expression or a block ([JLS §15.27.2]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaBody {
    Expr(ExprId),
    Block(StmtId),
}

/// One formal parameter of a lambda expression ([JLS §15.27.1]): a *normal*
/// parameter specifier (`{VariableModifier} LambdaParameterType
/// VariableDeclaratorId`) with a declared type — `(String s)`, `(@A int i)`,
/// `(var v)` — or a *concise* one, an identifier with neither type nor
/// modifiers (`(s)`). `ty` is `None` for a concise parameter and for a normal
/// one written with `var`, whose type is inferred from the functional
/// interface ([§15.27.3]); `annotations` carries the parameter's declaration
/// annotations ([§9.7.4]) — necessarily empty for a concise parameter, which
/// the grammar allows no modifiers on. `range` is the parameter name's source
/// range ([JLS §6.4] anchors the "already defined" diagnostic at the name).
#[derive(Debug, Clone, PartialEq)]
pub struct LambdaParam {
    pub name: Name,
    pub ty: Option<SpannedTypeRef>,
    pub annotations: Vec<AnnotationRef>,
    pub range: TextRange,
    /// The names a *destructuring* parameter binds — `{ (a, b) -> … }` — in
    /// component order, each the `componentN` of the parameter the function type
    /// declares ([KLS
    /// `declarations.html#destructuring-declarations`](https://kotlinlang.org/spec/declarations.html#destructuring-declarations)).
    /// Empty for a parameter that binds one name, and for a Java lambda
    /// parameter, which never writes one.
    pub destructured: Vec<Name>,
}

/// One method declared in an anonymous class body
/// ([JLS §15.9.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9.5)):
/// its name and parameter count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonymousMethod {
    pub name: Name,
    pub params: u32,
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
