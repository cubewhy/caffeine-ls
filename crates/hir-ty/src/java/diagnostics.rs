//! Type errors reported during body inference.
//!
//! The inference layer degrades broken source to [`crate::Ty::error`] so
//! downstream resolution never panics; every such degradation that is a
//! *language* error (not a compiler invariant) is additionally reported here
//! as a structured [`TypeError`] with a typed [`DiagnosticCode`] and a
//! human-readable `message`, keyed to the body IR it occurred in. The
//! diagnostics layer can collect them per file by running
//! [`crate::body_types`] for each body-carrying item and flattening the
//! [`crate::BodyTypes::diagnostics`].

use hir_expand::body::{BodyTree, ExprId, LocalId, PatternId, StmtId};
use hir_expand::name::Name;
use rowan::TextRange;
use syntax::{DiagnosticCode, JavaDiagnosticCode};

use crate::java::db::TyDatabase;
use crate::java::ty::Ty;

/// Where a reported type error occurred, in the currency of the body IR: the
/// stable-per-file arena ids the inference layer works with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagLocation {
    /// An expression of the enclosing body.
    Expr(ExprId),
    /// A local (parameter or declared local) of the enclosing body.
    Local(LocalId),
    /// A pattern of the enclosing body ([JLS §14.30]).
    Pattern(PatternId),
    /// A statement of the enclosing body.
    Stmt(StmtId),
    /// The enclosing declaration itself (no finer location recorded).
    Method,
}

impl DiagLocation {
    /// The source range of the location within its file, when the construct
    /// was lowered from a syntax node (synthetic `Missing` constructs have
    /// none). Offsets are file-relative; convert to line/column with the
    /// diagnostics layer's `LineIndex`.
    pub fn range(&self, tree: &BodyTree) -> Option<TextRange> {
        match self {
            DiagLocation::Expr(id) => tree.expr_range(*id),
            DiagLocation::Local(id) => tree.local_range(*id),
            DiagLocation::Pattern(id) => tree.pattern_range(*id),
            DiagLocation::Stmt(id) => tree.stmt_range(*id),
            DiagLocation::Method => None,
        }
    }
}

/// A type error reported during body inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    /// §14.4.1: a `var` declaration must have an initializer — the initializer
    /// is what the local's type is inferred from, so without one the local
    /// has no type.
    VarWithoutInitializer { local: LocalId },
    /// §14.4.1: a `var` declaration whose initializer is an array initializer
    /// (a `var x = { 1, 2 }` has no standalone type to infer).
    VarArrayInitializer { local: LocalId },
    /// §6.5: a simple name resolves to nothing — no local variable, no
    /// statically imported member, no field of the implicit receiver.
    CannotResolveName { expr: ExprId, name: Name },
    /// §6.5.5.1: a reference type name in *body* position — a local's
    /// declared type, a class-instance creation, a cast, an array creation, a
    /// class literal, a method reference or lambda parameter type — resolves
    /// to nothing on the classpath.
    CannotResolveType {
        location: DiagLocation,
        name: Name,
        range: Option<TextRange>,
    },
    /// §6.5.5.1/[§7.5.2]: a simple type name is available through two or more
    /// on-demand imports that denote different types — the use is ambiguous.
    AmbiguousName {
        location: DiagLocation,
        name: Name,
        range: Option<TextRange>,
    },
    /// §7.4.3/[§7.7.2]: a class exists on the classpath, but its package is
    /// not visible from the resolving source set's module.
    ModuleNotAccessible {
        location: DiagLocation,
        name: Name,
        range: Option<TextRange>,
    },
    /// §15.11: no (accessible) field of the name on the receiver expression's
    /// type.
    NoSuchField { expr: ExprId, name: Name },
    /// §15.12.1: no method of the name on the receiver's type.
    NoSuchMethod { expr: ExprId, name: Name },
    /// §15.9/[§8.8.7.1]: a class instance creation, `this(...)` or `super(...)`
    /// invocation for which the class declares no constructor of the name.
    NoSuchConstructor { expr: ExprId, name: Name },
    /// §15.12.3/[§8.1.3]: the form is a simple name (`MethodName`) and the
    /// chosen compile-time declaration is an instance method, but the
    /// invocation occurs in a static context — a static method body, a static
    /// field initializer or a static initializer, where `this` is unavailable.
    NonStaticMethodFromStaticContext { expr: ExprId, name: Name },
    /// §15.8.3/[§15.8.4]/[§8.1.3]: the `this` or `super` keyword (bare or
    /// qualified `TypeName.this`/`I.super`) is used in a static context, where
    /// no enclosing instance exists. Both keywords map to javac's
    /// `compiler.err.non-static.cant.be.ref`, but the message quotes the
    /// actual keyword, so which form was flagged is recorded in [`NonStaticThisKind`].
    NonStaticThisFromStaticContext {
        expr: ExprId,
        keyword: NonStaticThisKind,
    },
    /// §15.11/[§8.1.3]: a simple-name read or write of an instance field of
    /// the implicit receiver in a static context — the field is reachable only
    /// through `this`, which does not exist there. A *qualified* access
    /// (`obj.x`) or a static field stays legal.
    NonStaticFieldFromStaticContext { expr: ExprId, name: Name },
    /// §15.12.2: members of the name exist but none is applicable to the
    /// actual arguments. `required` carries the parameter types of the
    /// closest candidate and `found_tys` the actual argument types (each a
    /// [`Ty`], or `None` for a poly argument that has no standalone type), so
    /// the detail (see [`TypeError::related`]) can render a `required:`/`found:`
    /// block; both empty means only the arity numbers are known.
    /// `arg_ranges` holds the source range of every actual argument, in
    /// order, and `bad_args` the argument-to-formal mismatches against the
    /// closest candidate when the arities *match*: each `(argument index,
    /// found type, formal type)` of a concrete argument that does not
    /// convert. They give the diagnostic its IntelliJ-style *bad arguments*
    /// range (see [`TypeError::range`]): when the arities match the
    /// diagnostic underlines the first incompatible argument, and each
    /// mismatch also surfaces as its own `related_information` entry.
    /// `surplus` carries the indices of the *extra* arguments when the
    /// invocation supplies more arguments than the closest candidate takes,
    /// and the diagnostic then underlines the first of those instead. The
    /// range always covers a single offending argument, never a merged span.
    /// When the call is too short, the closest candidate is ambiguous, or no
    /// argument is at fault, the diagnostic falls back to the member name.
    /// The `reason:` text is the argument-list-length message whenever the
    /// arities differ. When `owner` is `Some` the invocation is a
    /// *constructor* (`new`, `this(...)`, `super(...)`) of that class and the
    /// message reads `Constructor {owner}() cannot be applied to given types`.
    /// Types are stored unresolved (the canonical FQN), rendered simple only
    /// in [`TypeError::message`], so future quickfixes keep the full type.
    WrongArity {
        expr: ExprId,
        name: Name,
        owner: Option<Name>,
        found: usize,
        expected: usize,
        required: Vec<Ty>,
        found_tys: Vec<Option<Ty>>,
        arg_ranges: Vec<TextRange>,
        bad_args: Vec<(usize, Ty, Ty)>,
        surplus: Vec<usize>,
    },
    /// §14.18: the operand of a `throw` statement is not assignable to
    /// `Throwable` ([§5.2]).
    IncompatibleTypes {
        expr: ExprId,
        found: Ty,
        expected: Ty,
    },
    /// §14.9, §14.11, §14.12.1, §14.16, §15.25.1: the condition of an
    /// `if`/`while`/`do`/`for`/`assert`/`? :`/`&&`/`||`/`!` is not `boolean`.
    NonBooleanCondition { expr: ExprId, found: Ty },
    /// §15.15, §15.17, §15.18, §15.19, §15.22: a unary, binary or shift
    /// operator applied to a non-numeric operand.
    IncompatibleOperand {
        expr: ExprId,
        op: &'static str,
        found: Ty,
        other: Option<Ty>,
    },
    /// §15.20/§15.21: an equality or relational operator between operands that
    /// are not comparable.
    IncomparableTypes {
        expr: ExprId,
        op: &'static str,
        found: Ty,
        other: Ty,
    },
    /// §14.14.2: the iterable of a for-each loop is not an array or an
    /// `Iterable`.
    NonIterableForEach { expr: ExprId, found: Ty },
    /// §5.5/§15.16: a cast to a type the operand cannot be cast to.
    BadCast { expr: ExprId, found: Ty, target: Ty },
    /// §15.10.2: array creation with a non-reifiable component type
    /// (`new List<String>[3]`).
    GenericArrayCreation { expr: ExprId, ty: Ty },
    /// §15.9: a `new` of a type variable, interface, abstract class or enum.
    CannotInstantiateTypeVar { expr: ExprId, ty: Ty },
    /// §14.11.1: the selector of a `switch` is not one of the types a switch
    /// supports (`char`, `byte`, `short`, `int` or their boxes, `String`, an
    /// enum).
    SwitchSelectorType { expr: ExprId, found: Ty },
    /// §11.2: a checked exception is thrown at `expr` but neither caught by
    /// an enclosing `catch` nor declared by the enclosing method's `throws`.
    UnreportedException { expr: ExprId, thrown: Ty },
    /// §11.2.3/§14.20: a catch clause's parameter is shadowed by an earlier
    /// clause whose type is a superclass — the clause is unreachable. The
    /// clause's alternatives (a multi-catch) are kept individually.
    AlreadyCaught { local: LocalId, caught: Vec<Ty> },
    /// §9.8/§15.27.3: a lambda or method reference target is not a functional
    /// interface.
    NotAFunctionalInterface { expr: ExprId, target: Ty },
    /// §8.3.3: a field initializer reads a same-class field declared
    /// textually later, by simple name and of the same static/instance kind.
    IllegalForwardReference { expr: ExprId, name: Name },
    /// §16 (definite assignment): the value of a blank or not-yet-assigned
    /// local is read before an assignment reaches the read.
    NotDefinitelyAssigned { expr: ExprId, name: Name },
    /// §14.11.1/§15.28: a switch expression is not exhaustive — some selector
    /// values have no matching arm and there is no `default`.
    NotExhaustive { expr: ExprId },
    /// §14.11.1: a `case` label of a primitive- or `String`-selector switch
    /// is not a constant expression ([§15.28]).
    NonConstantCaseLabel { expr: ExprId },
    /// §14.11.1: two `case` labels of one `switch` declare the same constant
    /// value — the second arm is unreachable.
    DuplicateCaseLabel { expr: ExprId, value: String },
    /// §4.12.2: a declared type names a generic class without type arguments
    /// — a *raw type* use. A warning, not an error.
    RawTypeUse { local: LocalId, ty: Ty },
    /// §5.1.9/§5.2: a raw-typed expression is assigned to a parameterized
    /// target; the conversion succeeds but carries no static element-type
    /// guarantee. A warning, not an error.
    UncheckedConversion { expr: ExprId, from: Ty, to: Ty },
    /// §14.22: a statement is unreachable — the statement before it cannot
    /// complete normally (`return`, `throw`, `break`, `continue`).
    UnreachableStatement { stmt: StmtId },
    /// §8.4.7: a method with a non-`void` return type can complete normally
    /// without executing a `return`. Reported against the method's
    /// declaration range.
    MissingReturnValue { range: Option<TextRange> },
    /// §11.2.3: a `catch` clause names a checked exception that the `try`
    /// block cannot throw.
    CatchNeverThrown { local: LocalId, caught: Ty },
    /// §6.6: a field or method exists on the receiver type with the referenced
    /// name but is not accessible from the enclosing class — its access is
    /// more restrictive than [§6.6.1]/[§6.6.2] allows at the access site.
    /// javac: `{member} has {access} access in {owner}`; the message here is
    /// IntelliJ-style. `kind` selects the field/method wording, `name` the
    /// referenced member's simple name, `owner` the simple name of its
    /// declaring class and `access` the keyword (`private`/`protected`/
    /// `package-private`) that fails.
    IllegalAccess {
        expr: ExprId,
        kind: IllegalAccessKind,
        name: Name,
        owner: Name,
        access: &'static str,
    },
    /// §8.8.7.1: the class's constructor delegation graph — `this(...)` calls
    /// between its own constructors — contains a cycle, so no path reaches
    /// `super(...)`/`Object()`. javac: `recursive constructor invocation`.
    RecursiveConstructorInvocation { expr: ExprId },
    /// §8.8.7.1: an explicit constructor invocation `this(...)`/`super(...)`
    /// that is not the first statement of the constructor body. javac: `call
    /// to {this|super} must be first statement in constructor`.
    ConstructorCallNotFirst { expr: ExprId },
    /// §8.8.7.1: a reference to `this`, `super` or an instance member of the
    /// class being constructed — a bare `this`/`super`, `this.x`, `super.x`,
    /// a simple-name instance field read or write, an unqualified instance
    /// method invocation — before the supertype constructor has been called.
    /// javac: `cannot reference {x} before supertype constructor has been
    /// called`.
    CannotReferenceBeforeSuper { expr: ExprId, name: Name },
    /// §8.3.1.2/[§16]: an assignment (simple, compound or increment) whose
    /// target is a `final` local variable or `final` field — a final variable
    /// can be assigned only by its initializer. javac: `cannot assign a value
    /// to final variable {x}`.
    CannotAssignToFinalVariable { expr: ExprId, name: Name },
    /// §6.5.6.1/[§15.27.2]: a local variable captured by a lambda expression
    /// (or a method-reference receiver) that is not *effectively final* — it
    /// is reassigned after its initializer, so the capture may observe a
    /// changing value. Reported at the capturing reference.
    /// javac: `local variables referenced from a lambda expression must be
    /// final or effectively final`.
    VariableMustBeEffectivelyFinal { expr: ExprId, name: Name },
    /// §6.4: a local variable or parameter declaration re-declares a name
    /// already in scope as a local variable, formal parameter, exception
    /// parameter, for-loop variable or resource variable — a local may not
    /// shadow an enclosing local. javac: `{x} is already defined in {y}`.
    /// `local` is the later binding and `name` the duplicated name.
    VariableAlreadyDefined { local: LocalId, name: Name },
    /// §4.5.1: a type argument of a parameterized type is not within the
    /// bounds of the type parameter it fills — `class M<T extends Number>`
    /// used as `M<String>`. javac: `type argument {a} is not within bounds of
    /// type-variable {T}`; the message is IntelliJ's `Type argument 'String'
    /// is not within bounds of type-variable 'T'`. `name` is the type
    /// parameter, `arg` the offending argument and `bound` the violated bound;
    /// `location` anchors the diagnostic (the local, expression or pattern the
    /// type reference belongs to) and `range` the type reference's own name
    /// span (matching javac's caret).
    TypeArgumentOutOfBounds {
        location: DiagLocation,
        name: Name,
        arg: Ty,
        bound: Ty,
        range: Option<TextRange>,
    },
    /// §15.9: a class instance creation whose type argument is a wildcard
    /// (`new ArrayList<?>()`) — a wildcard never names a concrete type, so
    /// nothing is created. javac: `unexpected type; required: exact type,
    /// found: ?`; the message is IntelliJ's `Cannot instantiate wildcard
    /// parameterized type 'ArrayList<?>'`.
    CannotInstantiateWildcard { expr: ExprId, ty: Ty },
    /// §14.20: a catch clause parameter declared with a type variable — an
    /// exception type must be a concrete class, not a type parameter. javac:
    /// `unexpected type; required: class, found: type parameter T`; the
    /// message is javac's `Cannot catch type variables` (IntelliJ-style,
    /// reported at the clause parameter).
    CannotCatchTypeVariable { local: LocalId },
    /// §4.7/[§15.20.2]: the reference type of an `instanceof` check must be
    /// reifiable — a type variable, or a parameterized type with a type
    /// variable, concrete or bounded-wildcard argument (like `List<String>` or
    /// `List<? extends Number>`), is not. javac: `inconvertible types; cannot
    /// cast {o} to {T}`; the message is IntelliJ's `Cannot perform instanceof
    /// check against non-reifiable type 'List<String>'`.
    IllegalGenericInstanceOf { expr: ExprId, ty: Ty },
    /// §14.16: a `continue` statement outside any `while`, `do`, `for` or
    /// enhanced `for` statement. javac: `continue outside of loop`; the message
    /// is javac's, IntelliJ-style, reported at the statement.
    ContinueOutsideLoop { stmt: StmtId },
    /// §14.15: an unlabeled `break` statement outside any `switch` or loop —
    /// there is no enclosing statement for it to exit. javac: `break outside
    /// switch or loop`; the message is javac's, IntelliJ-style, reported at the
    /// statement.
    BreakOutsideSwitchOrLoop { stmt: StmtId },
    /// §14.15/[§14.16]: a labeled `break` or `continue` whose label is not in
    /// scope — no enclosing labeled statement declares it. javac: `undefined
    /// label: {label}`; the message is javac's, IntelliJ-style, reported at the
    /// statement.
    UndefinedLabel { stmt: StmtId, label: String },
    /// §14.16: a labeled `continue` whose label names an enclosing statement
    /// that is not a loop — only a labeled loop is a legal `continue` target.
    /// javac: `not a loop label: {label}`; the message is javac's,
    /// IntelliJ-style, reported at the statement.
    NotALoopLabel { stmt: StmtId, label: String },
    /// §14.30.2: a record pattern `Point(int x, int y)` declares a different
    /// number of nested patterns than the record has components — the pattern
    /// can never match. javac: `incorrect number of nested patterns`; the
    /// message is javac's, IntelliJ-style, reported at the record pattern.
    IncorrectNumberOfPatternComponents {
        pattern: PatternId,
        expected: usize,
        found: usize,
    },
}

impl TypeError {
    /// The typed code of this error ([`DiagnosticCode`]).
    pub fn code(&self) -> DiagnosticCode {
        use JavaDiagnosticCode::*;
        match self {
            TypeError::VarWithoutInitializer { .. } => DiagnosticCode::Java(VarWithoutInitializer),
            TypeError::VarArrayInitializer { .. } => DiagnosticCode::Java(VarArrayInitializer),
            TypeError::CannotResolveName { .. } => DiagnosticCode::Java(CannotResolveName),
            TypeError::CannotResolveType { .. } => DiagnosticCode::Java(CannotResolveType),
            TypeError::AmbiguousName { .. } => DiagnosticCode::Java(AmbiguousName),
            TypeError::ModuleNotAccessible { .. } => DiagnosticCode::Java(ModuleNotAccessible),
            TypeError::NoSuchField { .. } => DiagnosticCode::Java(NoSuchField),
            TypeError::NoSuchMethod { .. } => DiagnosticCode::Java(NoSuchMethod),
            TypeError::NoSuchConstructor { .. } => DiagnosticCode::Java(NoSuchConstructor),
            TypeError::NonStaticMethodFromStaticContext { .. } => {
                DiagnosticCode::Java(NonStaticMethodFromStaticContext)
            }
            TypeError::NonStaticThisFromStaticContext { .. } => {
                DiagnosticCode::Java(NonStaticThisFromStaticContext)
            }
            TypeError::NonStaticFieldFromStaticContext { .. } => {
                DiagnosticCode::Java(NonStaticFieldFromStaticContext)
            }
            TypeError::WrongArity { .. } => DiagnosticCode::Java(WrongArity),
            TypeError::IncompatibleTypes { .. } => DiagnosticCode::Java(IncompatibleTypes),
            TypeError::NonBooleanCondition { .. } => DiagnosticCode::Java(NonBooleanCondition),
            TypeError::IncompatibleOperand { .. } => DiagnosticCode::Java(IncompatibleOperand),
            TypeError::IncomparableTypes { .. } => DiagnosticCode::Java(IncomparableTypes),
            TypeError::NonIterableForEach { .. } => DiagnosticCode::Java(NonIterableForEach),
            TypeError::BadCast { .. } => DiagnosticCode::Java(BadCast),
            TypeError::GenericArrayCreation { .. } => DiagnosticCode::Java(GenericArrayCreation),
            TypeError::CannotInstantiateTypeVar { .. } => {
                DiagnosticCode::Java(CannotInstantiateTypeVar)
            }
            TypeError::SwitchSelectorType { .. } => DiagnosticCode::Java(SwitchSelectorType),
            TypeError::UnreportedException { .. } => DiagnosticCode::Java(UnreportedException),
            TypeError::AlreadyCaught { .. } => DiagnosticCode::Java(AlreadyCaught),
            TypeError::NotAFunctionalInterface { .. } => {
                DiagnosticCode::Java(NotAFunctionalInterface)
            }
            TypeError::IllegalForwardReference { .. } => {
                DiagnosticCode::Java(IllegalForwardReference)
            }
            TypeError::NotDefinitelyAssigned { .. } => {
                DiagnosticCode::Java(VariableMightNotHaveBeenInitialized)
            }
            TypeError::NotExhaustive { .. } => DiagnosticCode::Java(NotExhaustive),
            TypeError::NonConstantCaseLabel { .. } => DiagnosticCode::Java(NonConstantCaseLabel),
            TypeError::DuplicateCaseLabel { .. } => DiagnosticCode::Java(DuplicateCaseLabel),
            TypeError::RawTypeUse { .. } => DiagnosticCode::Java(RawTypeUse),
            TypeError::UncheckedConversion { .. } => DiagnosticCode::Java(UncheckedConversion),
            TypeError::UnreachableStatement { .. } => DiagnosticCode::Java(UnreachableStatement),
            TypeError::MissingReturnValue { .. } => DiagnosticCode::Java(MissingReturnValue),
            TypeError::CatchNeverThrown { .. } => DiagnosticCode::Java(CatchNeverThrown),
            TypeError::IllegalAccess { .. } => DiagnosticCode::Java(IllegalAccess),
            TypeError::RecursiveConstructorInvocation { .. } => {
                DiagnosticCode::Java(RecursiveConstructorInvocation)
            }
            TypeError::ConstructorCallNotFirst { .. } => {
                DiagnosticCode::Java(ConstructorCallNotFirst)
            }
            TypeError::CannotReferenceBeforeSuper { .. } => {
                DiagnosticCode::Java(CannotReferenceBeforeSuper)
            }
            TypeError::CannotAssignToFinalVariable { .. } => {
                DiagnosticCode::Java(CannotAssignToFinalVariable)
            }
            TypeError::VariableMustBeEffectivelyFinal { .. } => {
                DiagnosticCode::Java(VariableMustBeEffectivelyFinal)
            }
            TypeError::VariableAlreadyDefined { .. } => DiagnosticCode::Java(DuplicateDeclaration),
            TypeError::TypeArgumentOutOfBounds { .. } => {
                DiagnosticCode::Java(TypeArgumentOutOfBounds)
            }
            TypeError::CannotInstantiateWildcard { .. } => {
                DiagnosticCode::Java(CannotInstantiateWildcard)
            }
            TypeError::CannotCatchTypeVariable { .. } => {
                DiagnosticCode::Java(CannotCatchTypeVariable)
            }
            TypeError::IllegalGenericInstanceOf { .. } => {
                DiagnosticCode::Java(IllegalGenericInstanceOf)
            }
            TypeError::ContinueOutsideLoop { .. } => DiagnosticCode::Java(ContinueOutsideLoop),
            TypeError::BreakOutsideSwitchOrLoop { .. } => {
                DiagnosticCode::Java(BreakOutsideSwitchOrLoop)
            }
            TypeError::UndefinedLabel { .. } => DiagnosticCode::Java(UndefinedLabel),
            TypeError::NotALoopLabel { .. } => DiagnosticCode::Java(NotALoopLabel),
            TypeError::IncorrectNumberOfPatternComponents { .. } => {
                DiagnosticCode::Java(IncorrectNumberOfPatternComponents)
            }
        }
    }

    /// Whether this diagnostic is a *warning* — a legal program reported for
    /// its unsoundness ([§4.12.2] raw types, [§5.1.9] unchecked conversion) —
    /// rather than a compile-time error.
    pub fn is_warning(&self) -> bool {
        matches!(
            self,
            TypeError::RawTypeUse { .. } | TypeError::UncheckedConversion { .. }
        )
    }

    /// The location of the error within its body.
    pub fn location(&self) -> DiagLocation {
        use TypeError::*;
        match self {
            VarWithoutInitializer { local } | VarArrayInitializer { local } => {
                DiagLocation::Local(*local)
            }
            CannotResolveName { expr, .. }
            | NoSuchField { expr, .. }
            | NoSuchMethod { expr, .. }
            | NoSuchConstructor { expr, .. }
            | NonStaticMethodFromStaticContext { expr, .. }
            | NonStaticThisFromStaticContext { expr, .. }
            | NonStaticFieldFromStaticContext { expr, .. }
            | WrongArity { expr, .. }
            | IncompatibleTypes { expr, .. }
            | NonBooleanCondition { expr, .. }
            | IncompatibleOperand { expr, .. }
            | IncomparableTypes { expr, .. }
            | NonIterableForEach { expr, .. }
            | BadCast { expr, .. }
            | GenericArrayCreation { expr, .. }
            | CannotInstantiateTypeVar { expr, .. }
            | SwitchSelectorType { expr, .. }
            | UnreportedException { expr, .. }
            | NotAFunctionalInterface { expr, .. }
            | IllegalForwardReference { expr, .. }
            | NotDefinitelyAssigned { expr, .. }
            | NotExhaustive { expr, .. }
            | NonConstantCaseLabel { expr, .. }
            | DuplicateCaseLabel { expr, .. }
            | UncheckedConversion { expr, .. } => DiagLocation::Expr(*expr),
            UnreachableStatement { stmt } => DiagLocation::Stmt(*stmt),
            ContinueOutsideLoop { stmt }
            | BreakOutsideSwitchOrLoop { stmt }
            | UndefinedLabel { stmt, .. }
            | NotALoopLabel { stmt, .. } => DiagLocation::Stmt(*stmt),
            IncorrectNumberOfPatternComponents { pattern, .. } => DiagLocation::Pattern(*pattern),
            MissingReturnValue { .. } => DiagLocation::Method,
            CatchNeverThrown { local, .. } => DiagLocation::Local(*local),
            CannotCatchTypeVariable { local } => DiagLocation::Local(*local),
            IllegalAccess { expr, .. } => DiagLocation::Expr(*expr),
            RecursiveConstructorInvocation { expr }
            | ConstructorCallNotFirst { expr }
            | CannotReferenceBeforeSuper { expr, .. }
            | CannotAssignToFinalVariable { expr, .. }
            | VariableMustBeEffectivelyFinal { expr, .. } => DiagLocation::Expr(*expr),
            VariableAlreadyDefined { local, .. } => DiagLocation::Local(*local),
            CannotInstantiateWildcard { expr, .. } => DiagLocation::Expr(*expr),
            IllegalGenericInstanceOf { expr, .. } => DiagLocation::Expr(*expr),
            CannotResolveType { location, .. }
            | AmbiguousName { location, .. }
            | ModuleNotAccessible { location, .. }
            | TypeArgumentOutOfBounds { location, .. } => location.clone(),
            AlreadyCaught { local, .. } => DiagLocation::Local(*local),
            RawTypeUse { local, .. } => DiagLocation::Local(*local),
        }
    }

    /// The source range of the error within its file, when the construct was
    /// lowered from a syntax node. Unknown-type and ambiguity reports carry
    /// the exact range of the offending reference name.
    pub fn range(&self, tree: &BodyTree) -> Option<TextRange> {
        match self {
            TypeError::CannotResolveType { range, .. }
            | TypeError::AmbiguousName { range, .. }
            | TypeError::ModuleNotAccessible { range, .. }
            | TypeError::TypeArgumentOutOfBounds { range, .. }
            | TypeError::MissingReturnValue { range } => *range,
            // §15.12.2: a wrong-argument diagnostic points at a *single*
            // offending argument (IntelliJ-style), never a merged span — the
            // first incompatible argument when the arities match ([§5.3]
            // loose conversion), the first *surplus* argument when the
            // invocation is too long for the closest candidate, and the
            // member name when the call has no arguments, is too short, or
            // the closest candidate is ambiguous (e.g. `new Foo()` against
            // `Foo(int)` keeps pointing at `Foo`). Every other incompatible /
            // surplus argument surfaces as its own `related_information`
            // entry, so highlighting several bad arguments does not merge
            // back into the whole argument list.
            TypeError::WrongArity {
                expr,
                arg_ranges,
                bad_args,
                surplus,
                ..
            } => {
                let first_bad = bad_args
                    .first()
                    .and_then(|(idx, _, _)| arg_ranges.get(*idx).copied());
                let first_surplus = surplus
                    .first()
                    .and_then(|idx| arg_ranges.get(*idx).copied());
                first_bad.or(first_surplus).or_else(|| {
                    tree.expr_name_range(*expr)
                        .or_else(|| tree.expr_range(*expr))
                })
            }
            // Name-bearing diagnostics underline just the member/method/name
            // identifier (`b.missing` → `missing`), not the whole expression.
            TypeError::CannotResolveName { expr, .. }
            | TypeError::NoSuchField { expr, .. }
            | TypeError::NoSuchMethod { expr, .. }
            | TypeError::NoSuchConstructor { expr, .. }
            | TypeError::IllegalAccess { expr, .. }
            | TypeError::RecursiveConstructorInvocation { expr }
            | TypeError::ConstructorCallNotFirst { expr }
            | TypeError::CannotReferenceBeforeSuper { expr, .. }
            | TypeError::CannotAssignToFinalVariable { expr, .. }
            | TypeError::VariableMustBeEffectivelyFinal { expr, .. }
            | TypeError::NonStaticMethodFromStaticContext { expr, .. }
            | TypeError::NonStaticThisFromStaticContext { expr, .. }
            | TypeError::NonStaticFieldFromStaticContext { expr, .. }
            | TypeError::NonIterableForEach { expr, .. }
            | TypeError::GenericArrayCreation { expr, .. }
            | TypeError::CannotInstantiateTypeVar { expr, .. }
            | TypeError::CannotInstantiateWildcard { expr, .. }
            | TypeError::IllegalGenericInstanceOf { expr, .. }
            | TypeError::SwitchSelectorType { expr, .. }
            | TypeError::NotAFunctionalInterface { expr, .. }
            | TypeError::IllegalForwardReference { expr, .. }
            | TypeError::NotDefinitelyAssigned { expr, .. }
            | TypeError::DuplicateCaseLabel { expr, .. }
            | TypeError::UncheckedConversion { expr, .. } => tree
                .expr_name_range(*expr)
                .or_else(|| tree.expr_range(*expr)),
            _ => self.location().range(tree),
        }
    }

    /// The human-readable message, rendered against the body IR the error
    /// occurred in (for the local's name). Messages are written in the
    /// IntelliJ IDEA style: a single, capitalized sentence naming the
    /// offending symbol, with types rendered simple ([`Ty::display_simple`]).
    /// The structured fields keep the canonical FQN; the simple rendering
    /// happens only here, at display time. Where a javac-style detail block
    /// (`required:`/`found:`/`reason:`) applies it is carried separately by
    /// [`TypeError::detail`] and surfaced as LSP `related_information`, not in
    /// this message.
    pub fn message(&self, db: &dyn TyDatabase, tree: &BodyTree) -> String {
        use TypeError::*;
        match self {
            VarWithoutInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!("Cannot infer type for 'var' variable '{name}'")
            }
            VarArrayInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!(
                    "Cannot infer type for 'var' variable '{name}': array initializer needs an explicit target type"
                )
            }
            CannotResolveName { name, .. } => {
                format!("Cannot resolve symbol '{}'", name.as_str())
            }
            CannotResolveType { name, .. } => {
                format!("Cannot resolve symbol '{}'", name.as_str())
            }
            AmbiguousName { name, .. } => {
                format!("Reference to '{}' is ambiguous", name.as_str())
            }
            ModuleNotAccessible { name, .. } => {
                format!(
                    "Package in which '{}' is declared is not visible from the current module",
                    name.as_str()
                )
            }
            NoSuchField { name, .. } => {
                format!("Cannot resolve symbol '{}'", name.as_str())
            }
            NoSuchMethod { name, .. } => {
                format!("Cannot resolve method '{}()'", name.as_str())
            }
            NoSuchConstructor { name, .. } => {
                format!("Cannot resolve constructor '{}()'", name.as_str())
            }
            NonStaticMethodFromStaticContext { name, .. } => {
                format!(
                    "Non-static method '{}()' cannot be referenced from a static context",
                    name.as_str()
                )
            }
            NonStaticThisFromStaticContext { keyword, .. } => match keyword {
                NonStaticThisKind::This => {
                    "Non-static variable 'this' cannot be referenced from a static context"
                        .to_owned()
                }
                NonStaticThisKind::Super => {
                    "Non-static variable 'super' cannot be referenced from a static context"
                        .to_owned()
                }
            },
            NonStaticFieldFromStaticContext { name, .. } => {
                format!(
                    "Non-static field '{}' cannot be referenced from a static context",
                    name.as_str()
                )
            }
            WrongArity { name, owner, .. } => {
                // The head sentence only; the `required:`/`found:`/`reason:`
                // block is carried separately and surfaced as LSP
                // `related_information` (see [`TypeError::related`]).
                match owner {
                    Some(owner) => {
                        format!(
                            "Constructor '{}()' cannot be applied to given types",
                            owner.as_str()
                        )
                    }
                    None => {
                        format!(
                            "Method '{}()' cannot be applied to given types",
                            name.as_str()
                        )
                    }
                }
            }
            IncompatibleTypes {
                found, expected, ..
            } => format!(
                "Incompatible types. Found: '{}', required: '{}'",
                render_simple(db, *found),
                render_simple(db, *expected)
            ),
            NonBooleanCondition { found, .. } => format!(
                "Incompatible types. Found: '{}', required: 'boolean'",
                render_simple(db, *found)
            ),
            IncompatibleOperand {
                op, found, other, ..
            } => match other {
                Some(other) => format!(
                    "Operator '{op}' cannot be applied to '{}' and '{}'",
                    render_simple(db, *found),
                    render_simple(db, *other)
                ),
                None => format!(
                    "Operator '{op}' cannot be applied to '{}'",
                    render_simple(db, *found)
                ),
            },
            IncomparableTypes {
                op, found, other, ..
            } => format!(
                "Operator '{op}' cannot be applied to '{}' and '{}'",
                render_simple(db, *found),
                render_simple(db, *other)
            ),
            NonIterableForEach { found, .. } => format!(
                "For-each is not applicable to expression of type '{}'",
                render_simple(db, *found)
            ),
            BadCast { found, target, .. } => format!(
                "Inconvertible types; cannot cast '{}' to '{}'",
                render_simple(db, *found),
                render_simple(db, *target)
            ),
            GenericArrayCreation { .. } => "Generic array creation".to_owned(),
            CannotInstantiateTypeVar { ty, .. } => {
                format!(
                    "'{}' is abstract; cannot be instantiated",
                    render_simple(db, *ty)
                )
            }
            SwitchSelectorType { found, .. } => {
                format!("Switch selector type '{}'", render_simple(db, *found))
            }
            UnreportedException { thrown, .. } => {
                format!("Unhandled exception: {}", render_simple(db, *thrown))
            }
            AlreadyCaught { caught, .. } => format!(
                "Exception {} has already been caught",
                caught
                    .iter()
                    .map(|ty| render_simple(db, *ty))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            NotAFunctionalInterface { target, .. } => format!(
                "'{}' is not a functional interface",
                render_simple(db, *target)
            ),
            IllegalForwardReference { .. } => "Illegal forward reference".to_owned(),
            NotDefinitelyAssigned { name, .. } => {
                format!(
                    "Variable '{}' might not have been initialized",
                    name.as_str()
                )
            }
            NotExhaustive { .. } => {
                "Switch expression does not cover all possible input values".to_owned()
            }
            NonConstantCaseLabel { .. } => "Constant expression required".to_owned(),
            DuplicateCaseLabel { .. } => "Duplicate case label".to_owned(),
            RawTypeUse { ty, .. } => {
                format!(
                    "Raw use of parameterized class '{}'",
                    render_simple(db, *ty)
                )
            }
            UncheckedConversion { from, to, .. } => {
                format!(
                    "Unchecked assignment: '{}' to '{}'",
                    render_simple(db, *from),
                    render_simple(db, *to)
                )
            }
            UnreachableStatement { .. } => "Unreachable statement".to_owned(),
            MissingReturnValue { .. } => "Missing return statement".to_owned(),
            CatchNeverThrown { caught, .. } => format!(
                "Exception '{}' is never thrown in the corresponding try block",
                render_simple(db, *caught)
            ),
            IllegalAccess {
                kind,
                name,
                owner,
                access,
                ..
            } => match kind {
                IllegalAccessKind::Field => format!(
                    "Variable '{}' has {} access in '{}'",
                    name.as_str(),
                    access,
                    owner.simple_name()
                ),
                IllegalAccessKind::Method => format!(
                    "'{}()' has {} access in '{}'",
                    name.as_str(),
                    access,
                    owner.simple_name()
                ),
            },
            RecursiveConstructorInvocation { .. } => "Recursive constructor invocation".to_owned(),
            ConstructorCallNotFirst { .. } => {
                "Constructor call must be the first statement in a constructor".to_owned()
            }
            CannotReferenceBeforeSuper { name, .. } => format!(
                "Cannot reference '{}' before supertype constructor has been called",
                name.as_str()
            ),
            CannotAssignToFinalVariable { name, .. } => {
                format!(
                    "Cannot assign a value to final variable '{}'",
                    name.as_str()
                )
            }
            VariableMustBeEffectivelyFinal { name, .. } => format!(
                "Variable '{}' used in lambda expression should be final or effectively final",
                name.as_str()
            ),
            VariableAlreadyDefined { name, .. } => {
                format!(
                    "Variable '{}' is already defined in the scope",
                    name.as_str()
                )
            }
            TypeArgumentOutOfBounds {
                name,
                arg,
                bound: _,
                ..
            } => format!(
                "Type argument '{}' is not within bounds of type-variable '{}'",
                render_simple(db, *arg),
                name.as_str()
            ),
            CannotInstantiateWildcard { ty, .. } => format!(
                "Cannot instantiate wildcard parameterized type '{}'",
                render_simple(db, *ty)
            ),
            CannotCatchTypeVariable { .. } => "Cannot catch type variables".to_owned(),
            IllegalGenericInstanceOf { ty, .. } => format!(
                "Cannot perform instanceof check against non-reifiable type '{}'",
                render_simple(db, *ty)
            ),
            ContinueOutsideLoop { .. } => "Continue outside of loop".to_owned(),
            BreakOutsideSwitchOrLoop { .. } => "Break outside of switch or loop".to_owned(),
            UndefinedLabel { label, .. } => format!("Undefined label: '{label}'"),
            NotALoopLabel { label, .. } => format!("Not a loop label: '{label}'"),
            IncorrectNumberOfPatternComponents {
                expected, found, ..
            } => format!("Incorrect number of nested patterns: expected {expected}, found {found}"),
        }
    }

    /// Secondary detail entries for the diagnostic, IntelliJ-style
    /// `related_information`: each `(message, range)` renders against `db`
    /// with simple type names. The `required:`/`found:`/`reason:` block of an
    /// invocation or assignment mismatch is attached to the diagnostic's own
    /// range; a wrong-argument invocation additionally carries one
    /// `reason: …` entry per incompatible *argument* at that argument's own
    /// range ([§15.12.2], [§5.3]). Empty for most diagnostics, whose message
    /// already carries everything.
    pub fn related(&self, db: &dyn TyDatabase, tree: &BodyTree) -> Vec<(String, TextRange)> {
        use TypeError::*;
        match self {
            WrongArity {
                required,
                found_tys,
                arg_ranges,
                bad_args,
                ..
            } => {
                // The `required:`/`found:`/`reason:` block anchors at the
                // diagnostic's own range — a single bad / surplus argument, or
                // the member name — never a merged whole argument list.
                let primary = self.range(tree).unwrap_or_default();
                let mut out = Vec::new();
                if required.is_empty() {
                    return out;
                }
                out.push((
                    format!(
                        "required: {}",
                        required
                            .iter()
                            .map(|ty| render_simple(db, *ty))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    primary,
                ));
                out.push((
                    format!(
                        "found: {}",
                        found_tys
                            .iter()
                            .map(|ty| match ty {
                                Some(ty) => render_simple(db, *ty),
                                None => "<poly>".to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    primary,
                ));
                // §15.12.2: the reason line. When the arities differ it is the
                // argument-list-length text; when they match, a single
                // `cannot be converted` entry for the *first* incompatible
                // argument at its own range — every further incompatible
                // argument is reported as its own diagnostic, not buried here.
                if let Some((idx, found, expected)) = bad_args.first() {
                    out.push((
                        format!(
                            "reason: '{}' cannot be converted to '{}'",
                            render_simple(db, *found),
                            render_simple(db, *expected)
                        ),
                        arg_ranges.get(*idx).copied().unwrap_or(primary),
                    ));
                } else {
                    out.push((
                        "reason: actual and formal argument lists differ in length".to_owned(),
                        primary,
                    ));
                }
                out
            }
            IncompatibleTypes {
                found, expected, ..
            } => {
                let primary = self.range(tree);
                vec![
                    (
                        format!("required: {}", render_simple(db, *expected)),
                        primary.unwrap_or_default(),
                    ),
                    (
                        format!("found: {}", render_simple(db, *found)),
                        primary.unwrap_or_default(),
                    ),
                ]
            }
            TypeArgumentOutOfBounds { arg, bound, .. } => {
                let primary = self.range(tree);
                vec![(
                    format!(
                        "reason: '{}' is not a subtype of '{}'",
                        render_simple(db, *arg),
                        render_simple(db, *bound)
                    ),
                    primary.unwrap_or_default(),
                )]
            }
            _ => Vec::new(),
        }
    }
}

/// Which keyword a [`TypeError::NonStaticThisFromStaticContext`] flags: the
/// `this` or the `super` form. Both name the enclosing instance and are
/// illegal in a static context, but the user-facing message must quote the
/// keyword that actually appears in source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonStaticThisKind {
    /// The bare or qualified `TypeName.this` form ([§15.8.3]).
    This,
    /// The bare `super` or qualified `I.super` form ([§15.8.4]).
    Super,
}

/// Which member form a [`TypeError::IllegalAccess`] flags, selecting the
/// user-facing wording: `Variable 'x' has private access…` for a field and
/// `'f()' has private access…` for a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IllegalAccessKind {
    /// A field access ([§15.11]).
    Field,
    /// A method invocation ([§15.12.1]).
    Method,
}

/// The simple-name rendering of a [`Ty`] for a diagnostic message.
fn render_simple(db: &dyn TyDatabase, ty: Ty) -> String {
    ty.display_simple(db).to_string()
}
