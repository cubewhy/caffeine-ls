//! Presentation of the body-inference diagnostics: their diagnostic code and
//! their user-facing message and secondary detail.
//!
//! The type layer detects the conditions and records them as structured
//! [`TypeError`]s; the wording, the code mapping and the rendering of a type
//! at display time live here, one place per diagnostic.

use hir_expand::body::BodyTree;
use hir_ty::java::diagnostics::{IllegalAccessKind, NonStaticThisKind};
use hir_ty::java::ty::Ty;
use hir_ty::{TyDatabase, TypeError};
use rowan::TextRange;
use syntax::{DiagnosticCode, JavaDiagnosticCode};

pub fn code(diag: &TypeError) -> DiagnosticCode {
    use JavaDiagnosticCode::*;
    match diag {
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
        TypeError::AbstractSuperAccess { .. } => DiagnosticCode::Java(AbstractSuperAccess),
        TypeError::QualifiedSuperNotEnclosing { .. } => {
            DiagnosticCode::Java(QualifiedSuperNotEnclosing)
        }
        TypeError::LambdaParameterCountMismatch { .. } => {
            DiagnosticCode::Java(LambdaParameterCountMismatch)
        }
        TypeError::LambdaBadReturn { .. } => DiagnosticCode::Java(LambdaBadReturn),
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
        TypeError::NotAFunctionalInterface { .. } => DiagnosticCode::Java(NotAFunctionalInterface),
        TypeError::IllegalForwardReference { .. } => DiagnosticCode::Java(IllegalForwardReference),
        TypeError::NotDefinitelyAssigned { .. } => {
            DiagnosticCode::Java(VariableMightNotHaveBeenInitialized)
        }
        TypeError::VariableAlreadyAssigned { .. } => DiagnosticCode::Java(VariableAlreadyAssigned),
        TypeError::NotExhaustive { .. } => DiagnosticCode::Java(NotExhaustive),
        TypeError::NonConstantCaseLabel { .. } => DiagnosticCode::Java(NonConstantCaseLabel),
        TypeError::DuplicateCaseLabel { .. } => DiagnosticCode::Java(DuplicateCaseLabel),
        TypeError::RawTypeUse { .. } => DiagnosticCode::Java(RawTypeUse),
        TypeError::UncheckedConversion { .. } => DiagnosticCode::Java(UncheckedConversion),
        TypeError::UncheckedInvocation { .. } => DiagnosticCode::Java(UncheckedInvocation),
        TypeError::UncheckedArgument { .. } => DiagnosticCode::Java(UncheckedInvocation),
        TypeError::UncheckedCast { .. } => DiagnosticCode::Java(UncheckedCast),
        TypeError::UnreachableStatement { .. } => DiagnosticCode::Java(UnreachableStatement),
        TypeError::MissingReturnValue { .. } => DiagnosticCode::Java(MissingReturnValue),
        TypeError::CatchNeverThrown { .. } => DiagnosticCode::Java(CatchNeverThrown),
        TypeError::IllegalAccess { .. } => DiagnosticCode::Java(IllegalAccess),
        TypeError::RecursiveConstructorInvocation { .. } => {
            DiagnosticCode::Java(RecursiveConstructorInvocation)
        }
        TypeError::ConstructorCallNotFirst { .. } => DiagnosticCode::Java(ConstructorCallNotFirst),
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
        TypeError::LambdaParameterAlreadyDefined { .. } => {
            DiagnosticCode::Java(DuplicateDeclaration)
        }
        TypeError::TypeArgumentOutOfBounds { .. } => DiagnosticCode::Java(TypeArgumentOutOfBounds),
        TypeError::WrongTypeArgumentCount { .. } => DiagnosticCode::Java(WrongTypeArgumentCount),
        TypeError::CannotInstantiateWildcard { .. } => {
            DiagnosticCode::Java(CannotInstantiateWildcard)
        }
        TypeError::CannotCatchTypeVariable { .. } => DiagnosticCode::Java(CannotCatchTypeVariable),
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
        TypeError::PatternDominated { .. } => DiagnosticCode::Java(PatternDominated),
        TypeError::CannotUseDiamondWithNonGeneric { .. } => {
            DiagnosticCode::Java(CannotUseDiamondWithNonGeneric)
        }
        TypeError::NotSupportedInRelease { .. } => DiagnosticCode::Java(ApiNotSupportedInRelease),
    }
}

pub fn message(db: &dyn TyDatabase, diag: &TypeError, bodies: &BodyTree) -> String {
    use TypeError::*;
    match diag {
        VarWithoutInitializer { local } => {
            let name = bodies.local(*local).name.as_str();
            format!("Cannot infer type for 'var' variable '{name}'")
        }
        VarArrayInitializer { local } => {
            let name = bodies.local(*local).name.as_str();
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
        AbstractSuperAccess { method, owner, .. } => {
            format!(
                "Abstract method '{}()' in '{}' cannot be accessed directly",
                method.as_str(),
                owner.simple_name()
            )
        }
        QualifiedSuperNotEnclosing { qualifier, .. } => {
            format!(
                "'{}' is not an enclosing class",
                qualifier.display_simple(db)
            )
        }
        LambdaParameterCountMismatch {
            expected, found, ..
        } => {
            format!(
                "Incompatible parameter types in lambda expression: {found} parameter(s) for {expected}"
            )
        }
        LambdaBadReturn {
            found, expected, ..
        } => {
            format!(
                "Bad return type in lambda expression: '{}' cannot be converted to '{}'",
                found.display_simple(db),
                expected.display_simple(db)
            )
        }
        NonStaticThisFromStaticContext { keyword, .. } => match keyword {
            NonStaticThisKind::This => {
                "Non-static variable 'this' cannot be referenced from a static context".to_owned()
            }
            NonStaticThisKind::Super => {
                "Non-static variable 'super' cannot be referenced from a static context".to_owned()
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
        VariableAlreadyAssigned { name, .. } => {
            format!(
                "Variable '{}' might already have been assigned",
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
        UncheckedInvocation { method, owner, .. } => format!(
            "Unchecked call to '{}' as a member of the raw type '{}'",
            method.as_str(),
            owner.simple_name()
        ),
        UncheckedCast { from, to, .. } => format!(
            "Unchecked cast: '{}' to '{}'",
            render_simple(db, *from),
            render_simple(db, *to)
        ),
        UncheckedArgument { from, to, .. } => format!(
            "Unchecked method invocation: '{}' to '{}'",
            render_simple(db, *from),
            render_simple(db, *to)
        ),
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
        LambdaParameterAlreadyDefined { name, .. } => {
            format!(
                "Variable '{}' is already defined in the scope",
                name.as_str()
            )
        }
        WrongTypeArgumentCount { ty, expected, .. } => {
            if *expected == 0 {
                format!("Type '{}' does not take parameters", render_simple(db, *ty))
            } else {
                format!(
                    "Wrong number of type arguments for '{}': required {}",
                    render_simple(db, *ty),
                    expected
                )
            }
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
        PatternDominated { .. } => {
            "This case label is dominated by a preceding case label".to_owned()
        }
        CannotUseDiamondWithNonGeneric { class, .. } => {
            format!(
                "Cannot use '<>' with non-generic class '{}'",
                class.display_simple(db)
            )
        }
        NotSupportedInRelease {
            api, found, added, ..
        } => super::release::render(db, api, *found, *added),
    }
}

pub fn related(
    db: &dyn TyDatabase,
    diag: &TypeError,
    bodies: &BodyTree,
) -> Vec<(String, TextRange)> {
    use TypeError::*;
    match diag {
        WrongArity {
            required,
            varargs,
            found_tys,
            arg_ranges,
            bad_args,
            ..
        } => {
            // The `required:`/`found:`/`reason:` block anchors at the
            // diagnostic's own range — a single bad / surplus argument, or
            // the member name — never a merged whole argument list.
            let primary = diag.range(bodies).unwrap_or_default();
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
            // §15.12.2: the reason line, emitted only when it states
            // something the primary message does not. A single
            // `cannot be converted` entry for the *first* incompatible
            // argument at its own range — every further incompatible
            // argument is reported as its own diagnostic, not buried here.
            // Otherwise the reason is the argument-list length, when the
            // arities cannot be aligned at all ([§15.12.2.1]), or the
            // failing bound set of the candidate's own type parameters
            // ([§18.4] — javac's "inference variable T has incompatible
            // bounds"). When neither applies the invocation failed on
            // applicability of an argument whose standalone type the
            // message already covers, so no reason line is added.
            if let Some((idx, found, expected)) = bad_args.first() {
                out.push((
                    format!(
                        "reason: '{}' cannot be converted to '{}'",
                        render_simple(db, *found),
                        render_simple(db, *expected)
                    ),
                    arg_ranges.get(*idx).copied().unwrap_or(primary),
                ));
            } else if !arities_align(db, required, found_tys.len(), *varargs) {
                out.push((
                    "reason: actual and formal argument lists differ in length".to_owned(),
                    primary,
                ));
            } else if let Some(name) = required
                .iter()
                .find_map(|ty| declared_type_var_name(db, *ty))
            {
                out.push((
                    format!("reason: type variable '{name}' has incompatible bounds"),
                    primary,
                ));
            }
            out
        }
        IncompatibleTypes {
            found, expected, ..
        } => {
            let primary = diag.range(bodies);
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
            let primary = diag.range(bodies);
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

/// The simple-name rendering of a [`Ty`] for a diagnostic message.
fn render_simple(db: &dyn TyDatabase, ty: Ty) -> String {
    ty.display_simple(db).to_string()
}

/// Whether `found` actual arguments can be aligned with the packed `required`
/// formals ([JLS §15.12.2.1]): a fixed-arity list matches only its own length,
/// and a list whose last formal is a varargs *element* type accepts any count
/// at or above the fixed prefix.
fn arities_align(db: &dyn TyDatabase, required: &[Ty], found: usize, varargs: bool) -> bool {
    if required.len() == found {
        return true;
    }
    // `required` ends at the packed element type of a variable-arity formal
    // (§15.12.2.4), which accepts any number of trailing actuals — except when
    // the packing left the array formal itself in place: a lone array-shaped
    // actual matched the array type exactly.
    varargs && required.len() <= found && required.last().is_some_and(|ty| !ty.is_array(db))
}

/// The name of the first *declared* type variable mentioned by `ty`
/// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// used to name the variable whose bound set failed to resolve ([§18.4]).
fn declared_type_var_name(db: &dyn TyDatabase, ty: Ty) -> Option<String> {
    match ty.kind(db) {
        hir_ty::java::ty::TyKind::TypeVar { scope, .. } if !scope.is_capture() => {
            Some(scope.name().as_str().to_owned())
        }
        hir_ty::java::ty::TyKind::Reference { args, .. } => {
            args.iter().find_map(|arg| declared_type_var_name(db, *arg))
        }
        hir_ty::java::ty::TyKind::Array(inner) => declared_type_var_name(db, **inner),
        hir_ty::java::ty::TyKind::Wildcard(bound) => bound
            .as_deref()
            .and_then(|b| declared_type_var_name(db, b.ty)),
        hir_ty::java::ty::TyKind::Intersection(members) => {
            members.iter().find_map(|m| declared_type_var_name(db, *m))
        }
        _ => None,
    }
}
