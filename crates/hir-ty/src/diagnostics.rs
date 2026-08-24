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

use hir_expand::body::{BodyTree, ExprId, LocalId, PatternId};
use hir_expand::name::Name;
use rowan::TextRange;
use syntax::{DiagnosticCode, JavaDiagnosticCode};

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
    /// §15.12.2: members of the name exist but none is applicable to the
    /// actual arguments.
    WrongArity {
        expr: ExprId,
        name: Name,
        found: usize,
        expected: usize,
    },
    /// §14.18: the operand of a `throw` statement is not assignable to
    /// `Throwable` ([§5.2]).
    IncompatibleTypes {
        expr: ExprId,
        found: String,
        expected: String,
    },
    /// §14.9, §14.11, §14.12.1, §14.16, §15.25.1: the condition of an
    /// `if`/`while`/`do`/`for`/`assert`/`? :`/`&&`/`||`/`!` is not `boolean`.
    NonBooleanCondition { expr: ExprId },
    /// §15.15, §15.17, §15.18, §15.19, §15.22: a unary, binary or shift
    /// operator applied to a non-numeric operand.
    IncompatibleOperand { expr: ExprId, op: &'static str },
    /// §15.20/§15.21: an equality or relational operator between operands that
    /// are not comparable.
    IncomparableTypes {
        expr: ExprId,
        found: String,
        other: String,
    },
    /// §14.14.2: the iterable of a for-each loop is not an array or an
    /// `Iterable`.
    NonIterableForEach { expr: ExprId, found: String },
    /// §5.5/§15.16: a cast to a type the operand cannot be cast to.
    BadCast {
        expr: ExprId,
        found: String,
        target: String,
    },
    /// §15.10.2: array creation with a non-reifiable component type
    /// (`new List<String>[3]`).
    GenericArrayCreation { expr: ExprId, ty: String },
    /// §15.9: a `new` of a type variable, interface, abstract class or enum.
    CannotInstantiateTypeVar { expr: ExprId, name: String },
    /// §14.11.1: the selector of a `switch` is not one of the types a switch
    /// supports (`char`, `byte`, `short`, `int` or their boxes, `String`, an
    /// enum).
    SwitchSelectorType { expr: ExprId, found: String },
    /// §11.2: a checked exception is thrown at `expr` but neither caught by
    /// an enclosing `catch` nor declared by the enclosing method's `throws`.
    UnreportedException { expr: ExprId, thrown: String },
    /// §11.2.3/§14.20: a catch clause's parameter is shadowed by an earlier
    /// clause whose type is a superclass — the clause is unreachable.
    AlreadyCaught { local: LocalId },
    /// §9.8/§15.27.3: a lambda or method reference target is not a functional
    /// interface.
    NotAFunctionalInterface { expr: ExprId, target: String },
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
    RawTypeUse { local: LocalId, name: String },
    /// §5.1.9/§5.2: a raw-typed expression is assigned to a parameterized
    /// target; the conversion succeeds but carries no static element-type
    /// guarantee. A warning, not an error.
    UncheckedConversion {
        expr: ExprId,
        from: String,
        to: String,
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
            | WrongArity { expr, .. }
            | IncompatibleTypes { expr, .. }
            | NonBooleanCondition { expr }
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
            CannotResolveType { location, .. }
            | AmbiguousName { location, .. }
            | ModuleNotAccessible { location, .. } => location.clone(),
            AlreadyCaught { local } => DiagLocation::Local(*local),
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
            | TypeError::ModuleNotAccessible { range, .. } => *range,
            _ => self.location().range(tree),
        }
    }

    /// The human-readable message, rendered against the body IR the error
    /// occurred in (for the local's name).
    pub fn message(&self, tree: &BodyTree) -> String {
        use TypeError::*;
        match self {
            VarWithoutInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!("variable '{name}' is declared with 'var' but has no initializer")
            }
            VarArrayInitializer { local } => {
                let name = tree.local(*local).name.as_str();
                format!(
                    "array initializer needs an explicit target-type, but variable '{name}' is declared with 'var'"
                )
            }
            CannotResolveName { name, .. } => {
                format!("cannot resolve symbol '{name}'")
            }
            CannotResolveType { name, .. } => {
                format!("cannot resolve symbol '{name}'")
            }
            AmbiguousName { name, .. } => {
                format!("reference to '{name}' is ambiguous; both are imported on demand")
            }
            ModuleNotAccessible { name, .. } => {
                format!(
                    "package in which '{name}' is declared is not visible from the current module"
                )
            }
            NoSuchField { name, .. } => {
                format!("cannot find field '{name}'")
            }
            NoSuchMethod { name, .. } => {
                format!("cannot find method '{name}'")
            }
            WrongArity {
                name,
                found,
                expected,
                ..
            } => {
                format!(
                    "method '{name}' is not applicable to the arguments (expected {expected}, found {found})"
                )
            }
            IncompatibleTypes {
                found, expected, ..
            } => {
                format!("incompatible types: {found} cannot be converted to {expected}")
            }
            NonBooleanCondition { .. } => {
                "incompatible types: the condition must be a boolean".to_owned()
            }
            IncompatibleOperand { op, .. } => {
                format!("bad operand type for operator '{op}'")
            }
            IncomparableTypes { found, other, .. } => {
                format!("incomparable types: {found} and {other}")
            }
            NonIterableForEach { found, .. } => {
                format!("for-each requires an array or an Iterable, found {found}")
            }
            BadCast { found, target, .. } => {
                format!("inconvertible types: {found} cannot be cast to {target}")
            }
            GenericArrayCreation { ty, .. } => {
                format!("generic array creation: cannot create array of {ty}")
            }
            CannotInstantiateTypeVar { name, .. } => {
                format!("{name} is abstract; cannot be instantiated")
            }
            SwitchSelectorType { found, .. } => {
                format!("switch selector type {found} is not convertible to int, String or an enum")
            }
            UnreportedException { thrown, .. } => {
                format!("unreported exception {thrown}; must be caught or declared to be thrown")
            }
            AlreadyCaught { local } => {
                let name = tree.local(*local).name.as_str();
                format!("exception {name} has already been caught")
            }
            NotAFunctionalInterface { target, .. } => {
                format!("{target} is not a functional interface")
            }
            IllegalForwardReference { name, .. } => {
                let name = name.as_str();
                format!("illegal forward reference: '{name}' is declared later")
            }
            NotDefinitelyAssigned { name, .. } => {
                let name = name.as_str();
                format!("variable '{name}' might not have been initialized")
            }
            NotExhaustive { .. } => {
                "the switch expression does not cover all possible input values".to_owned()
            }
            NonConstantCaseLabel { .. } => "case label must be a constant expression".to_owned(),
            DuplicateCaseLabel { value, .. } => {
                format!("duplicate case label: '{value}' has already been covered")
            }
            RawTypeUse { name, .. } => {
                format!("raw type '{name}' is used without type arguments")
            }
            UncheckedConversion { from, to, .. } => {
                format!("unchecked conversion: {from} converted to {to}")
            }
        }
    }
}
