//! Java *source-level* gating: every construct newer than the level its source
//! set is compiled at is reported, while still being typed at the newest level
//! so the IDE keeps working inside it.
//!
//! javac gates language features on a source level, not on API availability
//! (`com.sun.tools.javac.code.Source.Feature`), and reports
//! `compiler.err.feature.not.supported.in.source` — or
//! `compiler.err.preview.feature.disabled` for a preview feature used without
//! `--enable-preview` ([`Source.Preview.checkSourceLevel`]). No `@since` is
//! involved anywhere in that table, which is why this walk reads the *source*
//! level rather than the javadoc.
//!
//! The gate is purely syntactic — [`JavaLanguageLevel`] plus the parse tree —
//! because the lowered IR cannot see these constructs: `hir-def` maps a string
//! literal and a text block to the same `Literal::Str`, `non-sealed` lexes as
//! three tokens, and `yield` is a statement only in a switch expression. So
//! this module walks the file's syntax tree, keyed on [`syntax::SyntaxKind`].
//! Note that `sealed`, `record`, `permits`, `when`, `module`, `var` and
//! `yield` are contextual keywords that lex as `IDENTIFIER`; the rules below
//! are textual where that matters.
//!
//! Two of javac's rows are deliberately narrowed or absent, because deciding
//! them syntactically would produce false errors:
//!
//! - `unconditional patterns in instanceof` is, in javac, a *subtype* test
//!   (`Attr.visitTypeTest`: `types.isSubtype(exprtype, clazztype)`), so a
//!   walk cannot decide it. Only the sound half is implemented: a binding
//!   pattern whose type is exactly `Object` is unconditional for every
//!   reference expression type. Patterns that are unconditional for another
//!   reason (`String s; s instanceof String t`) stay unreported rather than
//!   risk reporting a conditional pattern.
//! - `primitive patterns` is not reported for a nested component of a record
//!   pattern (`case Point(int x, int y)`), which javac accepts even below 23
//!   whenever the component type is the same primitive
//!   (`Attr.checkCastablePattern` requires the pattern and expression types to
//!   differ). Nested primitive patterns that *do* need 23 stay unreported.
//!
//! The report is a plain diagnostic; the construct is still inferred and
//! resolved at the newest level, so hover, completion and go-to-definition
//! keep working inside offending code.
//!
//! One construct yields one diagnostic. javac can report several for a single
//! construct, because its parser and its attribution each check a layer of the
//! same syntax (`case 1, 2 ->` at 13 is both "multiple case labels" and "switch
//! rules"); the outer-most report is kept, which is the more specific row. Two
//! *distinct* constructs nested in one another — a `record` inside a `sealed`
//! class at 15 — are both reported, as javac does.

use rowan::{SyntaxElement, SyntaxNode, SyntaxToken, TextRange};
use syntax::java::{Lang, SyntaxKind as J};
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::java::decl_check::DeclDiagnostic;
use crate::java::range_ctx::range_ctx;
use hir::JavaLanguageLevel;

/// How a construct violates its source set's level, in
/// `Source.Preview.checkSourceLevel` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    /// A preview feature used without `--enable-preview`.
    PreviewDisabled,
    /// A standard construct whose release is newer than the source level.
    TooNewSource,
}

/// One row of javac's `Source.Feature` table, restricted to the rows this walk
/// can decide. `source_level` is javac's `min`; `preview_only` is whether the
/// feature is a preview feature of the newest release (javac's
/// `Preview.isPreview`), which requires `--enable-preview` regardless of the
/// source level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaFeature {
    Modules,
    EffectivelyFinalVariablesInTryWithResources,
    PrivateInterfaceMethods,
    DiamondWithAnonymousClass,
    LocalVariableTypeInference,
    VarSyntaxInImplicitLambdas,
    SwitchMultipleCaseLabels,
    SwitchRule,
    SwitchExpression,
    TextBlocks,
    PatternMatchingInInstanceof,
    ReifiableTypesInInstanceof,
    Records,
    SealedClasses,
    CaseNull,
    PatternSwitch,
    UnconditionalPatternsInInstanceof,
    RecordPatterns,
    UnnamedVariables,
    PrimitivePatterns,
}

impl JavaFeature {
    /// The construct's name in javac's message, taken from the
    /// `compiler.misc.feature.*` fragment the row points at. `LocalVariableTypeInference`
    /// is the one row javac carries no fragment for (it reports a plain
    /// "cannot find symbol: class var"), so its name is this crate's.
    fn display(self) -> &'static str {
        match self {
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
            JavaFeature::UnconditionalPatternsInInstanceof => {
                "unconditional patterns in instanceof"
            }
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
    fn is_plural(self) -> bool {
        !matches!(
            self,
            JavaFeature::DiamondWithAnonymousClass
                | JavaFeature::LocalVariableTypeInference
                | JavaFeature::PatternMatchingInInstanceof
                | JavaFeature::CaseNull
        )
    }

    /// The release at which the construct became standard (javac's `min`).
    fn source_level(self) -> u8 {
        match self {
            JavaFeature::Modules => 9,
            JavaFeature::EffectivelyFinalVariablesInTryWithResources => 9,
            JavaFeature::PrivateInterfaceMethods => 9,
            JavaFeature::DiamondWithAnonymousClass => 9,
            JavaFeature::LocalVariableTypeInference => 10,
            JavaFeature::VarSyntaxInImplicitLambdas => 11,
            JavaFeature::SwitchMultipleCaseLabels => 14,
            JavaFeature::SwitchRule => 14,
            JavaFeature::SwitchExpression => 14,
            JavaFeature::TextBlocks => 15,
            JavaFeature::PatternMatchingInInstanceof => 16,
            JavaFeature::ReifiableTypesInInstanceof => 16,
            JavaFeature::Records => 16,
            JavaFeature::SealedClasses => 17,
            JavaFeature::CaseNull => 21,
            JavaFeature::PatternSwitch => 21,
            JavaFeature::UnconditionalPatternsInInstanceof => 21,
            JavaFeature::RecordPatterns => 21,
            JavaFeature::UnnamedVariables => 22,
            JavaFeature::PrimitivePatterns => 23,
        }
    }

    /// Whether the feature is a preview feature of the newest release, and so
    /// needs `--enable-preview` at *every* level (`Source.Preview.isPreview`).
    fn is_preview(self) -> bool {
        matches!(self, JavaFeature::PrimitivePatterns)
    }

    /// The violation of `level`, in javac's order: a disabled preview feature
    /// is reported before the source level is even consulted.
    fn violation(self, level: JavaLanguageLevel) -> Option<Violation> {
        if self.is_preview() && !level.preview {
            return Some(Violation::PreviewDisabled);
        }
        if level.source < self.source_level() {
            return Some(Violation::TooNewSource);
        }
        None
    }
}

/// The source-level diagnostics of `file`: every construct gated on a Java
/// source level newer than the one `file`'s source set declares. Files whose
/// source set exported no level (or an unknown one) yield an empty report — a
/// guessed level would report every file of the project.
pub(crate) fn level_diagnostics_impl(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let Some(level) = hir::language_level_for_file(db, file) else {
        return Vec::new();
    };
    let tree = hir::file_item_tree(db, file);
    let Some((_map, source)) = range_ctx(db, file, tree.language) else {
        return Vec::new();
    };
    let syntax::SourceFile::Java(root) = &source else {
        return Vec::new();
    };

    let mut out = Vec::new();
    // Ranges already reported. A reported construct swallows its descendants
    // (one diagnostic per offending construct), while a construct *allowed* at
    // this level reports nothing and suppresses nothing — so a `case Point(int
    // x, int y)` in a level-16 file still reports the deconstruction pattern
    // once the pattern switch around it is allowed.
    let mut reported: Vec<TextRange> = Vec::new();
    for element in root.syntax_node.descendants_with_tokens() {
        let range = element.text_range();
        if range.is_empty() || reported.iter().any(|r| r.contains_range(range)) {
            continue;
        }
        let Some((feature, violation)) = violation_of(&element, level) else {
            continue;
        };
        reported.push(range);
        out.push(DeclDiagnostic::FeatureRequiresNewerSourceLevel {
            feature: feature.display(),
            plural: feature.is_plural(),
            found: level.source,
            required: feature.source_level(),
            preview_disabled: matches!(violation, Violation::PreviewDisabled),
            range: Some(range),
        });
    }
    out
}

/// The first feature `element` violates at `level`, if any.
///
/// An element can be the subject of more than one row (an `instanceof` is both
/// a pattern match and possibly a non-reifiable type test; a `case` label can
/// be a pattern switch and a primitive pattern), so the candidates are tried
/// most-specific first. The order follows javac's own check order — its parser
/// checks (`JavacParser`) run before its attributed checks (`Attr`) — so the
/// feature javac names is the one reported.
fn violation_of(
    element: &SyntaxElement<Lang>,
    level: JavaLanguageLevel,
) -> Option<(JavaFeature, Violation)> {
    let mut violated = |feature: JavaFeature| feature.violation(level).map(|v| (feature, v));

    match element {
        SyntaxElement::Token(token) => match token.kind() {
            J::TEXT_BLOCK => violated(JavaFeature::TextBlocks),
            J::UNDERSCORE if in_underscore_position(token) => {
                violated(JavaFeature::UnnamedVariables)
            }
            _ => None,
        },
        SyntaxElement::Node(node) => node_violation(node, &mut violated),
    }
}

fn node_violation(
    node: &SyntaxNode<Lang>,
    violated: &mut impl FnMut(JavaFeature) -> Option<(JavaFeature, Violation)>,
) -> Option<(JavaFeature, Violation)> {
    match node.kind() {
        J::MODULE_DECL => violated(JavaFeature::Modules),
        J::RECORD_DECL => violated(JavaFeature::Records),
        J::SWITCH_RULE => violated(JavaFeature::SwitchRule),
        J::SWITCH_EXPR => violated(JavaFeature::SwitchExpression),
        J::YIELD_STMT => violated(JavaFeature::SwitchExpression),
        J::RECORD_PATTERN => violated(JavaFeature::RecordPatterns),

        // A `sealed`/`non-sealed` modifier or a `permits` clause. Both are
        // contextual keywords, so the modifier is an `IDENTIFIER` token.
        J::MODIFIER_LIST
            if node
                .children_with_tokens()
                .filter_map(|element| element.into_token())
                .any(|token| token.kind() == J::IDENTIFIER && token.text() == "sealed") =>
        {
            violated(JavaFeature::SealedClasses)
        }
        J::PERMITS_CLAUSE if !declares_sealed(node) => violated(JavaFeature::SealedClasses),

        // `try (r) {}`: a resource that is a bare (effectively final) variable
        // access rather than a declaration.
        J::RESOURCE
            if !node
                .children()
                .any(|child| child.kind() == J::LOCAL_VARIABLE_DECLARATION) =>
        {
            violated(JavaFeature::EffectivelyFinalVariablesInTryWithResources)
        }

        // `private` methods of an interface body are the one interface member
        // that needs 9. The nearest enclosing body decides whether the method
        // is in an interface at all.
        J::METHOD_DECL
            if node
                .children()
                .filter(|child| child.kind() == J::MODIFIER_LIST)
                .any(|list| {
                    list.children_with_tokens()
                        .filter_map(|element| element.into_token())
                        .any(|token| token.kind() == J::PRIVATE_KW)
                })
                && nearest_body(node) == Some(J::INTERFACE_BODY) =>
        {
            violated(JavaFeature::PrivateInterfaceMethods)
        }

        // `new Runnable<>() { ... }`: the diamond on an anonymous class, an
        // empty `TYPE_ARGUMENTS` next to a class body.
        J::NEW_EXPR
            if node.children().any(|child| {
                child.kind() == J::TYPE_ARGUMENTS
                    && !child.children().any(|arg| arg.kind() == J::TYPE_ARGUMENT)
            }) && node.children().any(|child| child.kind() == J::CLASS_BODY) =>
        {
            violated(JavaFeature::DiamondWithAnonymousClass)
        }

        // `var` as a local variable (or resource) type. The grammar bumps a
        // bare contextual `var` as an `IDENTIFIER`, not as a `TYPE`, so the
        // declaration has no type child at all.
        J::LOCAL_VARIABLE_DECLARATION if declares_var(node) => {
            violated(JavaFeature::LocalVariableTypeInference)
        }

        // `var` introduced by a `TYPE` node: either the implicit parameter of
        // a lambda (`(var x) -> x`, 11) or the variable of an enhanced `for`.
        J::TYPE if is_bare_var_type(node) => {
            if node.ancestors().any(|anc| anc.kind() == J::LAMBDA_EXPR) {
                violated(JavaFeature::VarSyntaxInImplicitLambdas)
            } else {
                violated(JavaFeature::LocalVariableTypeInference)
            }
        }

        J::SWITCH_LABEL => switch_label_violation(node, violated),

        // A pattern that is the direct subject of an `instanceof`, or of a
        // `case` label. The label is visited first, so a level that already
        // rejects the pattern switch reports the switch rather than the
        // pattern inside it (javac's order too).
        J::TYPE_PATTERN => type_pattern_violation(node, violated),
        J::MATCH_ALL_PATTERN => {
            if node.ancestors().any(|anc| anc.kind() == J::SWITCH_LABEL) {
                violated(JavaFeature::PatternSwitch)
            } else {
                None
            }
        }

        J::INSTANCEOF_EXPR => instanceof_violation(node, violated),

        // `case null -> ...`: `null` is a `NULL_LITERAL` token directly under
        // the label.
        _ => None,
    }
}

/// An `instanceof`, which can be a pattern match, a non-reifiable type test and
/// an unconditional pattern all at once.
fn instanceof_violation(
    node: &SyntaxNode<Lang>,
    violated: &mut impl FnMut(JavaFeature) -> Option<(JavaFeature, Violation)>,
) -> Option<(JavaFeature, Violation)> {
    let pattern = node
        .children()
        .find(|child| matches!(child.kind(), J::TYPE_PATTERN | J::RECORD_PATTERN));
    match pattern {
        // `o instanceof Object x`: unconditional for every reference type.
        Some(pattern) if pattern.kind() == J::TYPE_PATTERN && is_object_type(&pattern) => {
            violated(JavaFeature::UnconditionalPatternsInInstanceof)
                .or_else(|| violated(JavaFeature::PatternMatchingInInstanceof))
        }
        // Any other binding or record pattern is a pattern match (the parser
        // checks this before the attributed unconditional test).
        Some(_) => violated(JavaFeature::PatternMatchingInInstanceof),
        // `o instanceof List<String>`: no pattern, but a type that is not
        // reifiable, which is what 16 made legal.
        None if is_non_reifiable_type(&node.children().find(|c| c.kind() == J::TYPE)?) => {
            violated(JavaFeature::ReifiableTypesInInstanceof)
        }
        None => None,
    }
}

/// A pattern as the direct subject of an `instanceof` or a `case` label.
fn type_pattern_violation(
    node: &SyntaxNode<Lang>,
    violated: &mut impl FnMut(JavaFeature) -> Option<(JavaFeature, Violation)>,
) -> Option<(JavaFeature, Violation)> {
    // A primitive type test pattern (`case int i`, `o instanceof int i`).
    // Nested components of a record pattern are typed against the component
    // (see the module docs), so only a top-level pattern is gated here.
    let nested_in_record = node.ancestors().any(|anc| anc.kind() == J::RECORD_PATTERN);
    if !nested_in_record
        && first_type_token(node).is_some_and(|token| is_primitive_kw(token.kind()))
    {
        return violated(JavaFeature::PrimitivePatterns);
    }
    if node.ancestors().any(|anc| anc.kind() == J::SWITCH_LABEL) {
        violated(JavaFeature::PatternSwitch)
    } else {
        None
    }
}

/// A `case` label, whose kind can be a pattern, several case labels or `null`.
fn switch_label_violation(
    node: &SyntaxNode<Lang>,
    violated: &mut impl FnMut(JavaFeature) -> Option<(JavaFeature, Violation)>,
) -> Option<(JavaFeature, Violation)> {
    let has_pattern = node.children().any(|child| {
        matches!(
            child.kind(),
            J::TYPE_PATTERN | J::RECORD_PATTERN | J::MATCH_ALL_PATTERN
        )
    });
    if has_pattern {
        return violated(JavaFeature::PatternSwitch);
    }
    if node
        .descendants_with_tokens()
        .any(|element| element.kind() == J::NULL_LITERAL)
    {
        return violated(JavaFeature::CaseNull);
    }
    // `case 1, 2:` / `case 1, 2 ->`: several labels on one case.
    if node
        .children_with_tokens()
        .any(|element| element.kind() == J::COMMA)
    {
        return violated(JavaFeature::SwitchMultipleCaseLabels);
    }
    None
}

/// Whether the declaration enclosing a `permits` clause already carries a
/// `sealed`/`non-sealed` modifier — the same construct, so the modifier list's
/// report already covers the clause (javac reports a sealed declaration once).
fn declares_sealed(node: &SyntaxNode<Lang>) -> bool {
    let Some(decl) = node.parent() else {
        return false;
    };
    decl.children()
        .filter(|child| child.kind() == J::MODIFIER_LIST)
        .any(|list| {
            list.children_with_tokens()
                .filter_map(|element| element.into_token())
                .any(|token| token.kind() == J::IDENTIFIER && token.text() == "sealed")
        })
}

/// Whether a `LOCAL_VARIABLE_DECLARATION` is a `var` declaration: it has no
/// declared type, and its first child past the modifiers is the contextual
/// `var` identifier.
fn declares_var(node: &SyntaxNode<Lang>) -> bool {
    if node.children().any(|child| child.kind() == J::TYPE) {
        return false;
    }
    node.children_with_tokens()
        .find(|element| {
            !element
                .as_token()
                .is_some_and(|token| token.kind().is_trivia())
                && element
                    .as_node()
                    .is_none_or(|n| n.kind() != J::MODIFIER_LIST)
        })
        .and_then(|element| element.into_token())
        .is_some_and(|token| token.kind() == J::IDENTIFIER && token.text() == "var")
}

/// Whether a `TYPE` node is the bare contextual `var` (an implicit lambda
/// parameter or an enhanced-`for` variable).
fn is_bare_var_type(node: &SyntaxNode<Lang>) -> bool {
    let mut tokens = node
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().is_trivia());
    tokens
        .next()
        .is_some_and(|token| token.kind() == J::IDENTIFIER && token.text() == "var")
        && tokens.next().is_none()
}

/// The first non-trivia token of a pattern's declared type.
fn first_type_token(node: &SyntaxNode<Lang>) -> Option<SyntaxToken<Lang>> {
    let ty = node.children().find(|child| child.kind() == J::TYPE)?;
    ty.descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| !token.kind().is_trivia())
}

/// The nearest enclosing body node of a declaration.
fn nearest_body(node: &SyntaxNode<Lang>) -> Option<J> {
    node.ancestors().find_map(|anc| {
        matches!(
            anc.kind(),
            J::CLASS_BODY
                | J::INTERFACE_BODY
                | J::ENUM_BODY
                | J::RECORD_BODY
                | J::ANNOTATION_TYPE_BODY
        )
        .then(|| anc.kind())
    })
}

/// Whether a binding pattern's declared type is exactly `Object`.
fn is_object_type(pattern: &SyntaxNode<Lang>) -> bool {
    let Some(ty) = pattern.children().find(|child| child.kind() == J::TYPE) else {
        return false;
    };
    let mut tokens = ty
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| !token.kind().is_trivia());
    match (tokens.next(), tokens.next()) {
        (Some(token), None) => token.kind() == J::IDENTIFIER && token.text() == "Object",
        _ => false,
    }
}

/// Whether a type is a parameterized type that is not reifiable — one with a
/// type argument that is not an unbounded wildcard (`List<String>`, not
/// `List<?>`). Such a type in `instanceof` is what 16 made legal.
fn is_non_reifiable_type(ty: &SyntaxNode<Lang>) -> bool {
    ty.descendants()
        .filter(|node| node.kind() == J::TYPE_ARGUMENTS)
        .any(|args| {
            args.children()
                .filter(|a| a.kind() == J::TYPE_ARGUMENT)
                .any(|arg| {
                    !(arg.children().any(|c| c.kind() == J::WILDCARD_TYPE)
                        && arg
                            .descendants_with_tokens()
                            .filter_map(|element| element.into_token())
                            .all(|token| {
                                token.kind().is_trivia()
                                    || matches!(
                                        token.kind(),
                                        J::QUESTION | J::EXTENDS_KW | J::SUPER_KW
                                    )
                            }))
                })
        })
}

fn is_primitive_kw(kind: J) -> bool {
    matches!(
        kind,
        J::BYTE_KW
            | J::SHORT_KW
            | J::CHAR_KW
            | J::INT_KW
            | J::LONG_KW
            | J::FLOAT_KW
            | J::DOUBLE_KW
            | J::BOOLEAN_KW
    )
}

/// Whether an `UNDERSCORE` token is a declaration name or an unnamed pattern,
/// which is what `UNNAMED_VARIABLES` gates, rather than the parentheses of a
/// `_`-named lambda that the grammar recovers. A `_` that is not in a name
/// position (`case _` as a constant label, `_` as an expression) is not a
/// variable and is left to the parser's own diagnostics.
///
/// The `int _` parameter, `int... _` spread parameter and `catch (Exception _)`
/// shapes have no parent here: the parser does not yet accept `_` as a formal
/// parameter name, so it recovers into `ERROR`/`LITERAL` nodes and reports its
/// own "Expected identifier". That is a parser gap, not a level-check one, and
/// those kinds are deliberately absent — a `_` in an expression position has no
/// parent here by design either.
fn in_underscore_position(token: &SyntaxToken<Lang>) -> bool {
    let Some(parent) = token.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        J::VARIABLE_DECLARATOR
            | J::LOCAL_VARIABLE_DECLARATION
            | J::FORMAL_PARAMETER
            | J::SPREAD_PARAMETER
            | J::CATCH_FORMAL_PARAMETER
            | J::INFERRED_PARAMETERS
            | J::ENHANCED_FOR_STMT
            | J::MATCH_ALL_PATTERN
            | J::TYPE_PATTERN
    )
}

/// The source-level diagnostics of `file` (see [`level_diagnostics_impl`]).
pub fn level_diagnostics(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    crate::java::db::level_diagnostics_query(db, db.file_text(file))
}
