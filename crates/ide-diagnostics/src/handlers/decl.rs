//! Presentation of the declaration diagnostics: their diagnostic code and
//! their user-facing message.
//!
//! The declaration checks in the type layer collect structured
//! [`DeclDiagnostic`]s; the code each maps to and the sentence each renders to
//! live here.

use hir_ty::TyDatabase;
use hir_ty::java::decl_check::{DeclDiagnostic, SafeVarargsRejection};
use hir_ty::java::deprecation::Deprecation;
use hir_ty::java::ty::Ty;
use syntax::{DiagnosticCode, JavaDiagnosticCode};

pub fn code(diag: &DeclDiagnostic) -> DiagnosticCode {
    match diag {
        DeclDiagnostic::IncompatibleOverride { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::IncompatibleOverride)
        }
        DeclDiagnostic::ConflictingDefaults { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ConflictingDefaults)
        }
        DeclDiagnostic::MethodDoesNotOverride { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::MethodDoesNotOverride)
        }
        DeclDiagnostic::MethodDoesNotOverrideStatic { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::MethodDoesNotOverrideStatic)
        }
        DeclDiagnostic::CannotResolveType { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CannotResolveType)
        }
        DeclDiagnostic::AmbiguousName { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AmbiguousName)
        }
        DeclDiagnostic::UnresolvedImport { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImport)
        }
        DeclDiagnostic::UnresolvedImportPackage { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImportPackage)
        }
        DeclDiagnostic::UnresolvedStaticImport { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedStaticImport)
        }
        DeclDiagnostic::ConflictingImport { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ConflictingImport)
        }
        DeclDiagnostic::ModuleNotAccessible { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ModuleNotAccessible)
        }
        DeclDiagnostic::RawTypeUse { .. } => DiagnosticCode::Java(JavaDiagnosticCode::RawTypeUse),
        DeclDiagnostic::WrongTypeArgumentCount { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::WrongTypeArgumentCount)
        }
        DeclDiagnostic::InvalidSafeVarargs { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::InvalidSafeVarargs)
        }
        DeclDiagnostic::NotAFunctionalInterfaceAnnotation { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::NotAFunctionalInterface)
        }
        DeclDiagnostic::UnexpectedPackagePath { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnexpectedPackagePath)
        }
        DeclDiagnostic::DuplicatePackage { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DuplicatePackage)
        }
        DeclDiagnostic::DuplicateClass { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DuplicateClass)
        }
        DeclDiagnostic::ClassPublicShouldBeInFile { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ClassPublicShouldBeInFile)
        }
        DeclDiagnostic::AnnotationNotApplicable { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotationNotApplicable)
        }
        DeclDiagnostic::AnnotationNotApplicableToType { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotationNotApplicableToType)
        }
        DeclDiagnostic::AnnotatedVar { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotatedVar)
        }
        DeclDiagnostic::UnknownAnnotationMember { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnknownAnnotationMember)
        }
        DeclDiagnostic::UnresolvedAnnotationMember { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedAnnotationMember)
        }
        DeclDiagnostic::DuplicateAnnotationMemberValue { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DuplicateAnnotationMemberValue)
        }
        DeclDiagnostic::AnnotationElementTypeMismatch { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotationElementTypeMismatch)
        }
        DeclDiagnostic::UnknownAnnotationElementConstant { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnknownAnnotationElementConstant)
        }
        DeclDiagnostic::MissingAnnotationElement { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::MissingAnnotationElement)
        }
        DeclDiagnostic::NonConstantAnnotationElement { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::NonConstantAnnotationElement)
        }
        DeclDiagnostic::AnnotationElementNotClassLiteral { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotationElementNotClassLiteral)
        }
        DeclDiagnostic::AnnotationElementNotEnumConstant { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AnnotationElementNotEnumConstant)
        }
        DeclDiagnostic::ConstructorNameMismatch { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ConstructorNameMismatch)
        }
        DeclDiagnostic::IllegalModifierCombination { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::IllegalModifierCombination)
        }
        DeclDiagnostic::CannotInheritFromFinalClass { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CannotInheritFromFinalClass)
        }
        DeclDiagnostic::InterfaceExpectedHere { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::InterfaceExpectedHere)
        }
        DeclDiagnostic::NoInterfaceExpectedHere { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::NoInterfaceExpectedHere)
        }
        DeclDiagnostic::CannotOverrideFinalMethod { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CannotOverrideFinalMethod)
        }
        DeclDiagnostic::WeakerAccessPrivileges { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::WeakerAccessPrivileges)
        }
        DeclDiagnostic::StaticInstanceClash { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::StaticInstanceClash)
        }
        DeclDiagnostic::IncompatibleThrows { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::IncompatibleThrows)
        }
        DeclDiagnostic::CannotOverrideObjectMethod { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CannotOverrideObjectMethod)
        }
        DeclDiagnostic::DuplicateMethod { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DuplicateMethod)
        }
        DeclDiagnostic::CannotDeclareBothVarargsAndArray { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CannotDeclareBothVarargsAndArray)
        }
        DeclDiagnostic::AbstractOrNativeMethodWithBody { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::AbstractOrNativeMethodWithBody)
        }
        DeclDiagnostic::DefaultCtorUnreportedException { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DefaultCtorUnreportedException)
        }
        DeclDiagnostic::CtorUnreportedException { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnreportedException)
        }
        DeclDiagnostic::EnumCtorSuperCall { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::EnumCtorSuperCall)
        }
        DeclDiagnostic::RecordCtorParamNameMismatch { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::RecordCtorParamNameMismatch)
        }
        DeclDiagnostic::EnumMemberBeforeConstants { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::EnumMemberBeforeConstants)
        }
        DeclDiagnostic::EnumConstantNotExpected { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::EnumConstantNotExpected)
        }
        DeclDiagnostic::UnimplementedAbstractMethod { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::UnimplementedAbstractMethod)
        }
        DeclDiagnostic::CyclicInheritance { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CyclicInheritance)
        }
        DeclDiagnostic::NoDefaultConstructor { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::NoDefaultConstructor)
        }
        DeclDiagnostic::RecursiveConstructorInvocation { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::RecursiveConstructorInvocation)
        }
        DeclDiagnostic::DuplicateDeclaration { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::DuplicateDeclaration)
        }
        DeclDiagnostic::FinalFieldNotInitialized { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::FinalFieldNotInitialized)
        }
        DeclDiagnostic::NameClashSameErasure { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::NameClashSameErasure)
        }
        DeclDiagnostic::GenericCannotExtendThrowable { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::GenericCannotExtendThrowable)
        }
        DeclDiagnostic::CantInheritFromSealed { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::CantInheritFromSealed)
        }
        DeclDiagnostic::SealedSealedOrFinalExpected { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::SealedSealedOrFinalExpected)
        }
        DeclDiagnostic::SealedClassMustHaveSubclasses { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::SealedClassMustHaveSubclasses)
        }
        DeclDiagnostic::FeatureRequiresNewerSourceLevel {
            preview_disabled, ..
        } => DiagnosticCode::Java(if *preview_disabled {
            JavaDiagnosticCode::PreviewFeatureDisabled
        } else {
            JavaDiagnosticCode::FeatureNotSupportedInSourceLevel
        }),
        DeclDiagnostic::ModifierNotAllowedHere { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ModifierNotAllowedHere)
        }
        DeclDiagnostic::MissingMethodBodyOrDeclareAbstract { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::MissingMethodBodyOrDeclareAbstract)
        }
        DeclDiagnostic::ModuleNotFound { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ModuleNotFound)
        }
        DeclDiagnostic::PackageEmptyOrNotFound { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::PackageEmptyOrNotFound)
        }
        DeclDiagnostic::ServiceImplementationNotSubtype { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ServiceImplementationNotSubtype)
        }
        DeclDiagnostic::NotSupportedInRelease { .. } => {
            DiagnosticCode::Java(JavaDiagnosticCode::ApiNotSupportedInRelease)
        }
        DeclDiagnostic::DeprecatedUse { deprecation, .. } => match deprecation {
            Deprecation::Ordinary => DiagnosticCode::Java(JavaDiagnosticCode::DeprecatedUse),
            Deprecation::Terminal => DiagnosticCode::Java(JavaDiagnosticCode::DeprecatedForRemoval),
        },
    }
}

pub fn message(db: &dyn TyDatabase, diag: &DeclDiagnostic) -> String {
    match diag {
        DeclDiagnostic::IncompatibleOverride {
            found,
            expected_owner,
            expected_ret,
            ..
        } => {
            format!(
                "Incompatible override: '{}' cannot override '{}' in '{}'",
                found.display_simple(db),
                expected_ret.display_simple(db),
                expected_owner.simple_name()
            )
        }
        DeclDiagnostic::ConflictingDefaults { method } => {
            let name = method.as_str();
            format!(
                "Class inherits unrelated default methods for '{}()'; it must be overridden",
                name
            )
        }
        DeclDiagnostic::MethodDoesNotOverride { method, .. } => {
            let name = method.as_str();
            format!(
                "Method '{}()' annotated @Override does not override or implement a method from a supertype",
                name
            )
        }
        DeclDiagnostic::MethodDoesNotOverrideStatic { method, .. } => {
            format!(
                "Static method '{}()' cannot be annotated with @Override",
                method.as_str()
            )
        }
        DeclDiagnostic::CannotResolveType { name, .. } => {
            format!("Cannot resolve symbol '{}'", name.as_str())
        }
        DeclDiagnostic::AmbiguousName { name, .. } => {
            format!(
                "Reference to '{}' is ambiguous; it is imported on demand from more than one type",
                name.as_str()
            )
        }
        DeclDiagnostic::UnresolvedImport { name, .. } => {
            format!(
                "Cannot resolve symbol '{}' in the single-type import",
                name.as_str()
            )
        }
        DeclDiagnostic::UnresolvedImportPackage { name, .. } => {
            format!("Package '{}' does not exist", name.as_str())
        }
        DeclDiagnostic::UnresolvedStaticImport { name, .. } => {
            format!("Cannot resolve symbol '{}'", name.simple_name())
        }
        DeclDiagnostic::ConflictingImport { name, .. } => {
            format!(
                "Import conflicts with another declaration of '{}'",
                name.as_str()
            )
        }
        DeclDiagnostic::ModuleNotAccessible { name, .. } => {
            format!(
                "Package in which '{}' is declared is not visible from the current module",
                name.as_str()
            )
        }
        DeclDiagnostic::RawTypeUse { ty, .. } => {
            format!("Raw use of parameterized class '{}'", ty.display_simple(db))
        }
        DeclDiagnostic::InvalidSafeVarargs { reason, .. } => match reason {
            SafeVarargsRejection::NotAMethod => {
                "Invalid @SafeVarargs annotation: not a method or constructor".to_owned()
            }
            SafeVarargsRejection::NotVarargs => {
                "Invalid @SafeVarargs annotation: method is not variable arity".to_owned()
            }
            SafeVarargsRejection::Instance => {
                "Invalid @SafeVarargs annotation: instance method is neither final nor private"
                    .to_owned()
            }
        },
        DeclDiagnostic::NotAFunctionalInterfaceAnnotation { .. } => {
            "Not a functional interface".to_owned()
        }
        DeclDiagnostic::WrongTypeArgumentCount { ty, expected, .. } => {
            if *expected == 0 {
                format!("Type '{}' does not take parameters", ty.display_simple(db))
            } else {
                format!(
                    "Wrong number of type arguments for '{}': required {}",
                    ty.display_simple(db),
                    expected
                )
            }
        }
        DeclDiagnostic::UnexpectedPackagePath { expected, dir, .. } => format!(
            "Package name '{}' does not correspond to the file path '{}'",
            expected.as_str(),
            dir
        ),
        DeclDiagnostic::DuplicatePackage { package, .. } => {
            format!("Duplicate package declaration '{}'", package.as_str())
        }
        DeclDiagnostic::DuplicateClass { fqn, .. } => {
            format!("Duplicate class: {fqn}")
        }
        DeclDiagnostic::ClassPublicShouldBeInFile { name, .. } => {
            let simple = name.simple_name();
            format!(
                "Class '{simple}' is public; it should be declared in a file named '{simple}.java'"
            )
        }
        DeclDiagnostic::AnnotationNotApplicable {
            name, element_type, ..
        } => format!(
            "'@{}' not applicable to {}",
            name.as_str(),
            element_type_display(element_type)
        ),
        DeclDiagnostic::AnnotationNotApplicableToType { name, .. } => {
            format!("'@{}' not applicable to type use", name.as_str())
        }
        DeclDiagnostic::AnnotatedVar { .. } => "'var' type may not be annotated".to_owned(),
        DeclDiagnostic::UnknownAnnotationMember { name, .. } => {
            format!("No annotation member named '{}'", name.as_str())
        }
        // IntelliJ reports one message for the whole "there is no such
        // element" rule; javac's two keys ([`JavaDiagnosticCode`]) are its
        // two resolution phases, not two sentences.
        DeclDiagnostic::UnresolvedAnnotationMember { name, .. } => {
            format!("No annotation member named '{}'", name.as_str())
        }
        DeclDiagnostic::DuplicateAnnotationMemberValue { name, .. } => {
            format!("Duplicate annotation member '{}'", name.as_str())
        }
        DeclDiagnostic::AnnotationElementTypeMismatch {
            found, expected, ..
        } => format!(
            "Incompatible types. Found: '{}', required: '{}'",
            found.display_simple(db),
            expected.display_simple(db)
        ),
        DeclDiagnostic::UnknownAnnotationElementConstant { member, .. } => {
            format!("Cannot resolve symbol '{}'", member.simple_name())
        }
        DeclDiagnostic::MissingAnnotationElement { names, .. } => format!(
            "{} missing but required",
            names
                .iter()
                .map(|name| format!("'{}'", name.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DeclDiagnostic::NonConstantAnnotationElement { .. } => {
            "Attribute value must be constant".to_owned()
        }
        DeclDiagnostic::AnnotationElementNotClassLiteral { .. } => {
            "Attribute value must be a class literal".to_owned()
        }
        DeclDiagnostic::AnnotationElementNotEnumConstant { .. } => {
            "Attribute value must be an enum constant".to_owned()
        }
        DeclDiagnostic::ConstructorNameMismatch { name, class, .. } => {
            format!(
                "Constructor name '{}' is different from the class name '{}'",
                name.as_str(),
                class.as_str()
            )
        }
        DeclDiagnostic::IllegalModifierCombination { first, second, .. } => {
            format!("Illegal combination of modifiers: '{first}' and '{second}'")
        }
        DeclDiagnostic::CannotInheritFromFinalClass { super_owner, .. } => {
            format!("Cannot inherit from '{}'", super_owner.simple_name())
        }
        DeclDiagnostic::InterfaceExpectedHere { .. } => "Interface expected here".to_owned(),
        DeclDiagnostic::NoInterfaceExpectedHere { .. } => "No interface expected here".to_owned(),
        DeclDiagnostic::CannotOverrideFinalMethod {
            method,
            super_owner,
            ..
        } => {
            format!(
                "Cannot override final method '{}()' in '{}'",
                method.as_str(),
                super_owner.simple_name()
            )
        }
        DeclDiagnostic::WeakerAccessPrivileges {
            method,
            super_owner,
            ..
        } => {
            format!(
                "Overrides '{}()' in '{}' with weaker access privilege",
                method.as_str(),
                super_owner.simple_name()
            )
        }
        DeclDiagnostic::StaticInstanceClash {
            method,
            super_owner,
            overriding_is_static,
        } => {
            let (overridden, clashing) = if *overriding_is_static {
                ("instance", "static")
            } else {
                ("static", "instance")
            };
            format!(
                "Cannot declare {clashing} method '{}()': it clashes with the {overridden} method '{}()' inherited from '{}'",
                method.as_str(),
                method.as_str(),
                super_owner.simple_name()
            )
        }
        DeclDiagnostic::IncompatibleThrows {
            method,
            super_owner,
            thrown,
        } => {
            format!(
                "Overridden method '{}()' in '{}' does not throw '{}'",
                method.as_str(),
                super_owner.simple_name(),
                thrown.display_simple(db)
            )
        }
        DeclDiagnostic::CannotOverrideObjectMethod { method, is_static } => {
            if *is_static {
                format!(
                    "Static method '{}()' cannot override a member of java.lang.Object",
                    method.as_str()
                )
            } else {
                format!(
                    "Default method '{}()' overrides a member of java.lang.Object",
                    method.as_str()
                )
            }
        }
        DeclDiagnostic::DuplicateMethod {
            method,
            is_constructor,
            ..
        } => {
            let kind = if *is_constructor {
                "Constructor"
            } else {
                "Method"
            };
            format!("{kind} '{}()' is already defined", method.as_str())
        }
        DeclDiagnostic::CannotDeclareBothVarargsAndArray { method, array } => {
            let rendered = array.display_simple(db).to_string();
            let element = rendered.strip_suffix("[]").unwrap_or(&rendered);
            format!(
                "Cannot declare both '{}({rendered})' and '{}({element}...)'",
                method.as_str(),
                method.as_str()
            )
        }
        DeclDiagnostic::AbstractOrNativeMethodWithBody {
            method, abstract_, ..
        } => {
            let kind = if *abstract_ { "abstract" } else { "native" };
            format!("{kind} method '{}()' cannot have a body", method.as_str())
        }
        DeclDiagnostic::DefaultCtorUnreportedException {
            super_owner,
            thrown,
            ..
        } => {
            format!(
                "Unreported exception '{}' in the default constructor of '{}'",
                thrown.display_simple(db),
                super_owner.simple_name()
            )
        }
        DeclDiagnostic::CtorUnreportedException { thrown, .. } => {
            format!("Unhandled exception: {}", thrown.display_simple(db))
        }
        DeclDiagnostic::EnumCtorSuperCall { .. } => {
            "Call to 'super' is not allowed in an enum constructor".to_owned()
        }
        DeclDiagnostic::RecordCtorParamNameMismatch { record, .. } => {
            format!(
                "Invalid canonical constructor of record '{}': parameter names differ from the record components",
                record.as_str()
            )
        }
        DeclDiagnostic::EnumMemberBeforeConstants { .. } => {
            "Enum constant expected here".to_owned()
        }
        DeclDiagnostic::EnumConstantNotExpected { .. } => {
            "Enum constant not expected here".to_owned()
        }
        DeclDiagnostic::UnimplementedAbstractMethod {
            class,
            method,
            owner,
            ..
        } => {
            format!(
                "Class '{}' must either be declared abstract or implement abstract method '{}()' in '{}'",
                class.simple_name(),
                method.as_str(),
                owner.simple_name()
            )
        }
        DeclDiagnostic::CyclicInheritance { class, .. } => {
            format!("Cyclic inheritance involving '{}'", class.simple_name())
        }
        DeclDiagnostic::NoDefaultConstructor { super_owner, .. } => {
            format!(
                "There is no default constructor available in '{}'",
                super_owner.simple_name()
            )
        }
        DeclDiagnostic::RecursiveConstructorInvocation { .. } => {
            "Recursive constructor invocation".to_owned()
        }
        DeclDiagnostic::DuplicateDeclaration { name, .. } => {
            format!(
                "Variable '{}' is already defined in the scope",
                name.as_str()
            )
        }
        DeclDiagnostic::FinalFieldNotInitialized { field, .. } => {
            format!(
                "Variable '{}' might not have been initialized",
                field.as_str()
            )
        }
        DeclDiagnostic::NameClashSameErasure {
            method,
            params,
            other_params,
        } => {
            let render = |ty: &Ty| ty.display_simple(db).to_string();
            let sig = |name: &str, params: &[Ty]| {
                format!(
                    "{}({})",
                    name,
                    params.iter().map(render).collect::<Vec<_>>().join(", ")
                )
            };
            format!(
                "'{}' clashes with '{}'; both methods have same erasure",
                sig(method.as_str(), params),
                sig(method.as_str(), other_params)
            )
        }
        DeclDiagnostic::GenericCannotExtendThrowable { class, .. } => {
            format!(
                "Generic class '{}' may not subclass java.lang.Throwable",
                class.simple_name()
            )
        }
        DeclDiagnostic::CantInheritFromSealed { super_owner, .. } => {
            format!("Cannot inherit from sealed '{}'", super_owner.simple_name())
        }
        DeclDiagnostic::SealedSealedOrFinalExpected { .. } => {
            "Sealed, non-sealed or final expected".to_owned()
        }
        DeclDiagnostic::SealedClassMustHaveSubclasses { .. } => {
            "Sealed class must have subclasses".to_owned()
        }
        DeclDiagnostic::FeatureRequiresNewerSourceLevel {
            feature,
            found,
            required,
            preview_disabled,
            ..
        } => {
            let (verb, are) = if super::level::feature_is_plural(*feature) {
                ("are", "are")
            } else {
                ("is", "is")
            };
            let feature = super::level::feature_display(*feature);
            if *preview_disabled {
                format!(
                    "{feature} {are} a preview feature and {are} disabled by default (use --enable-preview to enable {feature})"
                )
            } else {
                format!(
                    "{feature} {verb} not supported in source level {found} (use source level {required} or higher to enable {feature})"
                )
            }
        }
        DeclDiagnostic::ModifierNotAllowedHere { modifier, .. } => {
            format!("Modifier '{modifier}' is not allowed here")
        }
        DeclDiagnostic::MissingMethodBodyOrDeclareAbstract { method, .. } => {
            format!(
                "Missing method body, or declare abstract for '{}()'",
                method.as_str()
            )
        }
        DeclDiagnostic::ModuleNotFound { module, .. } => {
            format!("Module not found: '{}'", module.as_str())
        }
        DeclDiagnostic::PackageEmptyOrNotFound { package, .. } => {
            format!("Package '{}' is empty or does not exist", package.as_str())
        }
        DeclDiagnostic::ServiceImplementationNotSubtype {
            service,
            implementation,
            ..
        } => format!(
            "Service implementation '{}' is not a subtype of service interface '{}'",
            implementation.display_simple(db),
            service.display_simple(db)
        ),
        DeclDiagnostic::NotSupportedInRelease {
            api, found, added, ..
        } => super::release::render(db, api, *found, *added),
        DeclDiagnostic::DeprecatedUse {
            api, deprecation, ..
        } => super::deprecation::message(db, api, *deprecation),
    }
}

/// The noun IntelliJ names an `ElementType` by in its
/// `'@X' not applicable to {0}` message (`annotation.target.*` in
/// `JavaPsiBundle`), for an `ElementType` constant as it is spelled in
/// `java.lang.annotation` ([JLS §9.6.4.1] Table 9.7-1). The fallback keeps
/// the constant itself, so a new element type never renders as nothing.
fn element_type_display(element_type: &'static str) -> &'static str {
    match element_type {
        "ANNOTATION_TYPE" => "annotation type",
        "CONSTRUCTOR" => "constructor",
        "FIELD" => "field",
        "LOCAL_VARIABLE" => "local variable",
        "METHOD" => "method",
        "MODULE" => "module",
        "PACKAGE" => "package",
        "PARAMETER" => "parameter",
        "RECORD_COMPONENT" => "record component",
        "TYPE" => "type",
        "TYPE_PARAMETER" => "type parameter",
        "TYPE_USE" => "type use",
        other => other,
    }
}
