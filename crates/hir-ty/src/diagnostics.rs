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

use crate::db::TyDatabase;
use crate::ty::Ty;

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
    /// §15.12.3/[§8.1.3]: the form is a simple name (`MethodName`) and the
    /// chosen compile-time declaration is an instance method, but the
    /// invocation occurs in a static context — a static method body, a static
    /// field initializer or a static initializer, where `this` is unavailable.
    NonStaticMethodFromStaticContext { expr: ExprId, name: Name },
    /// §15.12.2: members of the name exist but none is applicable to the
    /// actual arguments. `required` carries the parameter types of the
    /// closest candidate and `found_tys` the actual argument types (each a
    /// [`Ty`], or `None` for a poly argument that has no standalone type), so
    /// the message can render javac's `required:/found:` block; both empty
    /// means only the arity numbers are known. `incompatible` carries the
    /// first argument-to-formal mismatch against the closest candidate when
    /// the arities *match* — javac then renders
    /// `reason: incompatible types: … cannot be converted to …` instead of
    /// the argument-list-length text. Types are stored unresolved (the
    /// canonical FQN), rendered simple only in [`TypeError::message`], so
    /// future quickfixes keep the full type.
    WrongArity {
        expr: ExprId,
        name: Name,
        found: usize,
        expected: usize,
        required: Vec<Ty>,
        found_tys: Vec<Option<Ty>>,
        incompatible: Option<(Ty, Ty)>,
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
            TypeError::NonStaticMethodFromStaticContext { .. } => {
                DiagnosticCode::Java(NonStaticMethodFromStaticContext)
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
            | NonStaticMethodFromStaticContext { expr, .. }
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
            MissingReturnValue { .. } => DiagLocation::Method,
            CatchNeverThrown { local, .. } => DiagLocation::Local(*local),
            CannotResolveType { location, .. }
            | AmbiguousName { location, .. }
            | ModuleNotAccessible { location, .. } => location.clone(),
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
            | TypeError::MissingReturnValue { range } => *range,
            _ => self.location().range(tree),
        }
    }

    /// The human-readable message, rendered against the body IR the error
    /// occurred in (for the local's name). Wherever javac has a 1:1 message
    /// for the construct (see `compiler.properties` / the `-XDrawDiagnostics`
    /// probe harness in `crates/hir-ty/tests/javac_parity.rs`), this text
    /// mirrors it verbatim, using javac's *simple* class-name rendering
    /// ([`Ty::display_simple`]). The structured fields keep the canonical
    /// FQN; the simple rendering happens only here, at display time.
    pub fn message(&self, db: &dyn TyDatabase, tree: &BodyTree) -> String {
        use TypeError::*;
        match self {
            VarWithoutInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!(
                    "cannot infer type for local variable {name}\n  (cannot use 'var' on variable without initializer)"
                )
            }
            VarArrayInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!(
                    "cannot infer type for local variable {name}\n  (array initializer needs an explicit target-type)"
                )
            }
            CannotResolveName { name, .. } => {
                format!("cannot find symbol\n  symbol:   variable {}", name.as_str())
            }
            CannotResolveType { name, .. } => {
                format!("cannot find symbol\n  symbol:   class {}", name.as_str())
            }
            AmbiguousName { name, .. } => {
                format!("reference to '{}' is ambiguous", name.as_str())
            }
            ModuleNotAccessible { name, .. } => {
                format!(
                    "package in which '{}' is declared is not visible from the current module",
                    name.as_str()
                )
            }
            NoSuchField { name, .. } => {
                format!("cannot find symbol\n  symbol:   variable {}", name.as_str())
            }
            NoSuchMethod { name, .. } => {
                format!("cannot find symbol\n  symbol:   method {}()", name.as_str())
            }
            NonStaticMethodFromStaticContext { name, .. } => {
                format!(
                    "non-static method {}() cannot be referenced from a static context",
                    name.as_str()
                )
            }
            WrongArity {
                name,
                required,
                found_tys,
                incompatible,
                ..
            } => {
                // javac's default-mode message block for
                // `compiler.err.cant.apply.symbol`. The reason line mirrors
                // javac's two shapes: the argument-list-length text when the
                // arities differ, otherwise the first argument-to-formal
                // conversion failure against the closest candidate.
                if required.is_empty() {
                    return format!(
                        "method '{}' cannot be applied to given types",
                        name.as_str()
                    );
                }
                let reason = match incompatible {
                    Some((found, expected)) => format!(
                        "reason: incompatible types: {} cannot be converted to {}",
                        render_simple(db, *found),
                        render_simple(db, *expected)
                    ),
                    None => "reason: actual and formal argument lists differ in length".to_owned(),
                };
                format!(
                    "method {} cannot be applied to given types;\n  required: {}\n  found: {}\n  {}",
                    name.as_str(),
                    required
                        .iter()
                        .map(|ty| render_simple(db, *ty))
                        .collect::<Vec<_>>()
                        .join(","),
                    found_tys
                        .iter()
                        .map(|ty| match ty {
                            Some(ty) => render_simple(db, *ty),
                            None => "<poly>".to_owned(),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    reason
                )
            }
            IncompatibleTypes {
                found, expected, ..
            } => {
                format!(
                    "incompatible types: {} cannot be converted to {}",
                    render_simple(db, *found),
                    render_simple(db, *expected)
                )
            }
            NonBooleanCondition { found, .. } => {
                format!(
                    "incompatible types: {} cannot be converted to boolean",
                    render_simple(db, *found)
                )
            }
            IncompatibleOperand {
                op, found, other, ..
            } => match other {
                Some(other) => format!(
                    "bad operand types for binary operator '{op}'\n  first type:  {}\n  second type: {}",
                    render_simple(db, *found),
                    render_simple(db, *other)
                ),
                None => format!(
                    "bad operand type {} for unary operator '{op}'",
                    render_simple(db, *found)
                ),
            },
            IncomparableTypes {
                op, found, other, ..
            } => format!(
                "bad operand types for binary operator '{op}'\n  first type:  {}\n  second type: {}",
                render_simple(db, *found),
                render_simple(db, *other)
            ),
            NonIterableForEach { found, .. } => format!(
                "for-each not applicable to expression type\n  required: array or java.lang.Iterable\n  found:    {}",
                render_simple(db, *found)
            ),
            BadCast { found, target, .. } => {
                format!(
                    "inconvertible types: {} cannot be cast to {}",
                    render_simple(db, *found),
                    render_simple(db, *target)
                )
            }
            GenericArrayCreation { .. } => "generic array creation".to_owned(),
            CannotInstantiateTypeVar { ty, .. } => {
                format!(
                    "{} is abstract; cannot be instantiated",
                    render_simple(db, *ty)
                )
            }
            SwitchSelectorType { found, .. } => {
                format!("switch selector type {}", render_simple(db, *found))
            }
            UnreportedException { thrown, .. } => {
                format!(
                    "unreported exception {}; must be caught or declared to be thrown",
                    render_simple(db, *thrown)
                )
            }
            AlreadyCaught { caught, .. } => {
                format!(
                    "exception {} has already been caught",
                    caught
                        .iter()
                        .map(|ty| render_simple(db, *ty))
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            }
            NotAFunctionalInterface { target, .. } => format!(
                "incompatible types: {} is not a functional interface",
                render_simple(db, *target)
            ),
            IllegalForwardReference { .. } => "illegal forward reference".to_owned(),
            NotDefinitelyAssigned { name, .. } => {
                let name = name.as_str();
                format!("variable '{name}' might not have been initialized")
            }
            NotExhaustive { .. } => {
                "the switch expression does not cover all possible input values".to_owned()
            }
            NonConstantCaseLabel { .. } => "constant expression required".to_owned(),
            DuplicateCaseLabel { .. } => "duplicate case label".to_owned(),
            RawTypeUse { ty, .. } => {
                format!(
                    "raw type '{}' is used without type arguments",
                    render_simple(db, *ty)
                )
            }
            UncheckedConversion { from, to, .. } => {
                format!(
                    "unchecked conversion: {} converted to {}",
                    render_simple(db, *from),
                    render_simple(db, *to)
                )
            }
            UnreachableStatement { .. } => "unreachable statement".to_owned(),
            MissingReturnValue { .. } => "missing return statement".to_owned(),
            CatchNeverThrown { caught, .. } => format!(
                "exception {} is never thrown in the corresponding try block",
                render_simple(db, *caught)
            ),
        }
    }
}

/// The simple-name rendering of a [`Ty`] for a diagnostic message.
fn render_simple(db: &dyn TyDatabase, ty: Ty) -> String {
    ty.display_simple(db).to_string()
}
