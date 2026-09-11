//! Presentation of the source-level diagnostics
//! ([JEP 182](https://openjdk.org/jeps/182) source levels): the name javac
//! gives a construct newer than the source level, and whether its message
//! pluralizes it.
//!
//! The level checks in the type layer detect the violations; the wording of
//! the report lives here, next to the enum the check records.

use hir_ty::java::level_check::JavaFeature;

/// The construct's name in javac's message, taken from the
/// `compiler.misc.feature.*` fragment the row points at. `LocalVariableTypeInference`
/// is the one row javac carries no fragment for (it reports a plain
/// "cannot find symbol: class var"), so its name is this crate's.
pub(crate) fn feature_display(feature: JavaFeature) -> &'static str {
    match feature {
        JavaFeature::Modules => "modules",
        JavaFeature::EffectivelyFinalVariablesInTryWithResources => {
            "variables in try-with-resources"
        }
        JavaFeature::PrivateInterfaceMethods => "private interface methods",
        JavaFeature::DiamondWithAnonymousClass => "'<>' with anonymous inner classes",
        JavaFeature::LocalVariableTypeInference => "local variable type inference",
        JavaFeature::VarSyntaxInImplicitLambdas => "var syntax in implicit lambdas",
        JavaFeature::SwitchMultipleCaseLabels => "multiple case labels",
        JavaFeature::SwitchRule => "switch rules",
        JavaFeature::SwitchExpression => "switch expressions",
        JavaFeature::TextBlocks => "text blocks",
        JavaFeature::PatternMatchingInInstanceof => "pattern matching in instanceof",
        JavaFeature::ReifiableTypesInInstanceof => "reifiable types in instanceof",
        JavaFeature::Records => "records",
        JavaFeature::SealedClasses => "sealed classes",
        JavaFeature::CaseNull => "null in switch cases",
        JavaFeature::PatternSwitch => "patterns in switch statements",
        JavaFeature::UnconditionalPatternsInInstanceof => "unconditional patterns in instanceof",
        JavaFeature::RecordPatterns => "deconstruction patterns",
        JavaFeature::UnnamedVariables => "unnamed variables",
        JavaFeature::PrimitivePatterns => "primitive patterns",
    }
}

/// Whether javac's message pluralizes the construct (`records are ...`).
/// javac's own `DiagKind` (`Source.java`): only the rows marked `NORMAL`
/// read singular. `LocalVariableTypeInference` carries no `DiagKind` at all
/// (javac never emits this message), so it takes the singular reading of
/// its own name.
pub(crate) fn feature_is_plural(feature: JavaFeature) -> bool {
    !matches!(
        feature,
        JavaFeature::DiamondWithAnonymousClass
            | JavaFeature::LocalVariableTypeInference
            | JavaFeature::PatternMatchingInInstanceof
            | JavaFeature::CaseNull
    )
}
