//! Lint policy: the `@SuppressWarnings` vocabulary and scopes
//! ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)),
//! the client's enabled lint set, and the severity/lint key of every
//! diagnostic the type layer reports.
//!
//! `@SuppressWarnings` gives the programmer control over the lint-like
//! warnings a compiler would otherwise report. §9.6.4.5 fixes both the scope
//! and the vocabulary:
//!
//! * *Scope*: "If a declaration is annotated with
//!   `@SuppressWarnings(value = {S₁, ..., Sₖ})`, then a Java compiler must
//!   suppress ... any warning specified by one of `S₁ ... Sₖ` if that warning
//!   would have been generated as a result of the annotated declaration **or
//!   any of its parts**." The scope is therefore the annotated declaration
//!   and everything lexically inside it, and the warnings suppressed at a
//!   position are the *union* over every enclosing annotated declaration
//!   (the `java.lang.SuppressWarnings` contract states the same: "The set of
//!   warnings suppressed in a given element is a union of the warnings
//!   suppressed in all containing elements").
//! * *Vocabulary*: four strings are mandated — `"unchecked"`,
//!   `"deprecation"`, `"removal"` and `"preview"`. "Any other string
//!   specifies a non-standard warning. A Java compiler **must ignore any such
//!   string that it does not recognize**." This analyzer reports the
//!   unchecked warnings of §4.8/§5.1.9/§8.4.1/§15.12.4.2 as `"unchecked"`,
//!   the deprecation warnings of §9.6.4.6 as `"deprecation"`, and — following
//!   the reference implementation's documented `-Xlint` names — the raw-type
//!   warnings of §4.8/§4.12.2 as `"rawtypes"`. Every other string, including
//!   the frequently-misremembered `"all"`, names nothing here and is ignored
//!   as §9.6.4.5 requires: a string the compiler does not recognize must not
//!   suppress anything.
//!
//! The rule is lexical, so it is evaluated on the syntax tree rather than on
//! the item tree: an annotation on a *local variable declaration*
//! ([§14.4.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.4.2))
//! suppresses within that declaration exactly as one on a method does, and a
//! local is not an item.
//!
//! Detection stays in `hir-ty` — it produces the structured diagnostics and
//! nothing else. This module owns what happens to them: which are warnings,
//! which string names them, whether a client enabled them, and whether an
//! enclosing declaration suppresses them.

use hir_expand::body::BodyTree;
use rowan::{SyntaxNode, TextRange};
use rustc_hash::FxHashSet;
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J};
use triomphe::Arc;
use vfs::FileId;

use hir_ty::{DeclDiagnostic, TyDatabase, TypeError};
use ide_db::Severity;
use ide_db::base_db::FileText;

/// A warning kind this analyzer reports that `@SuppressWarnings` can name.
///
/// The spelling is the one §9.6.4.5 mandates for the unchecked and
/// deprecation warnings; `rawtypes` is the reference implementation's
/// documented non-standard name for the raw-type warning of
/// [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2),
/// which javac emits as `[rawtypes]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintKey {
    /// Unchecked warnings (JLS §4.8, §5.1.6, §5.1.9, §8.4.1, §8.4.8.3,
    /// §15.12.4.2, §15.13.2, §15.27.3) — the string `"unchecked"`.
    Unchecked,
    /// Raw-type use (JLS §4.8, §4.12.2) — the string `"rawtypes"`.
    RawTypes,
    /// Deprecation (JLS §9.6.4.6) — the string `"deprecation"`.
    Deprecation,
    /// Terminal deprecation (JLS §9.6.4.6), `@Deprecated(forRemoval = true)`
    /// — the string `"removal"`.
    Removal,
}

impl LintKey {
    /// Every key this build knows, for the `all` shorthand of the client
    /// configuration.
    pub const ALL: [LintKey; 4] = [
        LintKey::Unchecked,
        LintKey::RawTypes,
        LintKey::Deprecation,
        LintKey::Removal,
    ];

    /// The string that names this warning in `@SuppressWarnings`
    /// ([JLS §9.6.4.5]).
    pub fn as_str(self) -> &'static str {
        match self {
            LintKey::Unchecked => "unchecked",
            LintKey::RawTypes => "rawtypes",
            LintKey::Deprecation => "deprecation",
            LintKey::Removal => "removal",
        }
    }

    /// The key a `@SuppressWarnings` string names, or `None` for a string
    /// this analyzer does not recognize — which §9.6.4.5 requires it to
    /// ignore.
    fn from_str(text: &str) -> Option<LintKey> {
        match text {
            "unchecked" => Some(LintKey::Unchecked),
            "rawtypes" => Some(LintKey::RawTypes),
            "deprecation" => Some(LintKey::Deprecation),
            "removal" => Some(LintKey::Removal),
            _ => None,
        }
    }
}

/// The lint keys in effect for a client run: the keys the client enabled plus
/// the keys javac reports without any flag.
///
/// The set is a set of *enabled* keys only — a key the client does not name is
/// simply not enabled, never explicitly turned off — because javac's lint
/// configuration is additive in the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintConfig {
    enabled: FxHashSet<LintKey>,
}

impl LintConfig {
    /// Every key this build knows: the configuration of the memoized report,
    /// which must contain every diagnostic that survives `@SuppressWarnings`
    /// regardless of what the client asked for.
    pub fn all() -> Self {
        Self {
            enabled: LintKey::ALL.into_iter().collect(),
        }
    }

    /// The keys named by `keys`, plus the default-on ones. The `all` shorthand
    /// means every key this build knows; unknown strings are ignored
    /// ([JLS §9.6.4.5]).
    pub fn from_keys(keys: &[String]) -> Self {
        let mut enabled = Self::default_enabled();
        for key in keys {
            if key == "all" {
                enabled.extend(LintKey::ALL);
                continue;
            }
            if let Some(key) = LintKey::from_str(key) {
                enabled.insert(key);
            }
        }
        Self { enabled }
    }

    /// The keys reported without any client configuration: javac reports the
    /// raw-type and unchecked-conversion warnings only when `-Xlint` names
    /// them, but it reports *terminal* deprecation
    /// (`@Deprecated(forRemoval = true)`) unconditionally — ordinary
    /// deprecation needs `-Xlint:deprecation`, terminal only needs `-Xlint`
    /// not to be `none`. So `removal` alone is on by default
    /// ([JLS §9.6.4.6]).
    fn default_enabled() -> FxHashSet<LintKey> {
        FxHashSet::from_iter([LintKey::Removal])
    }

    pub fn enables(&self, key: LintKey) -> bool {
        self.enabled.contains(&key)
    }
}

/// One `@SuppressWarnings` scope: the annotated declaration's source range and
/// the warning keys in effect for it — its own plus every enclosing
/// declaration's ([JLS §9.6.4.5]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SuppressionScope {
    pub range: TextRange,
    pub keys: FxHashSet<LintKey>,
}

/// Every `@SuppressWarnings` scope of `file_id`, in source order.
///
/// A warning at `range` is suppressed when some returned scope both contains
/// `range` and carries its key; [`is_suppressed`] performs that check.
pub(crate) fn suppression_scopes(db: &dyn TyDatabase, file_id: FileId) -> Vec<SuppressionScope> {
    let tree = hir::file_item_tree(db, file_id);
    let Some((_map, source)) = hir_ty::java::range_ctx::range_ctx(db, file_id, tree.language)
    else {
        return Vec::new();
    };
    let SourceFile::Java(file) = &source else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(&file.syntax_node, &FxHashSet::default(), &mut out);
    out
}

/// Whether a warning of `key` reported at `range` is suppressed by any scope
/// in `scopes` ([JLS §9.6.4.5]).
pub(crate) fn is_suppressed(scopes: &[SuppressionScope], range: TextRange, key: LintKey) -> bool {
    scopes
        .iter()
        .any(|scope| scope.keys.contains(&key) && scope.range.contains_range(range))
}

/// Walks the tree carrying the keys in effect, extending them at each
/// `MODIFIER_LIST` that names `@SuppressWarnings` and recording the annotated
/// declaration's range.
///
/// Every declaration's annotations hang off a `MODIFIER_LIST` child, whose
/// parent is the declaration itself (`METHOD_DECL`, `FIELD_DECL`,
/// `CLASS_DECL`, `LOCAL_VARIABLE_DECLARATION`, ...), so the parent's range is
/// exactly the annotated declaration — the unit §9.6.4.5 scopes the
/// suppression to.
fn collect(
    node: &SyntaxNode<Lang>,
    inherited: &FxHashSet<LintKey>,
    out: &mut Vec<SuppressionScope>,
) {
    let mut keys = inherited.clone();
    if node.kind() == J::MODIFIER_LIST {
        let own = suppress_keys(node);
        if !own.is_empty() {
            keys.extend(own);
            if let Some(declaration) = node.parent() {
                out.push(SuppressionScope {
                    range: declaration.text_range(),
                    keys: keys.clone(),
                });
            }
        }
    }
    for child in node.children() {
        collect(&child, &keys, out);
    }
}

/// The keys named by every `@SuppressWarnings` annotation of one
/// `MODIFIER_LIST`, as a set. Unrecognized strings are dropped
/// ([JLS §9.6.4.5]).
fn suppress_keys(modifier_list: &SyntaxNode<Lang>) -> FxHashSet<LintKey> {
    let mut out = FxHashSet::default();
    for annotation in modifier_list.children() {
        if annotation.kind() != J::ANNOTATION {
            continue;
        }
        let named = annotation.children().any(|child| {
            child.kind() == J::QUALIFIED_NAME && is_suppress_warnings(&child.text().to_string())
        });
        if !named {
            continue;
        }
        let Some(args) = annotation
            .children()
            .find(|child| child.kind() == J::ANNOTATION_ARGUMENT_LIST)
        else {
            // A marker `@SuppressWarnings` has no argument list; it names no
            // warning, and `@SuppressWarnings` is not a marker annotation
            // anyway ([§9.6.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4)).
            continue;
        };
        // The element is an array of `String` ([§9.6.4.5]), so the keys are
        // the string literals of the argument list — either a lone literal
        // (`@SuppressWarnings("unchecked")`, §9.7.1's single-element form) or
        // the literals of an array initializer. No nested annotation can
        // appear in a `String[]` element, so every string literal here is a
        // key. A literal is a *token* of its `LITERAL` node, so the walk
        // descends through tokens.
        for literal in args
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| token.kind() == J::STRING_LITERAL)
        {
            if let Some(key) = literal
                .text()
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .and_then(LintKey::from_str)
            {
                out.insert(key);
            }
        }
    }
    out
}

/// The `@SuppressWarnings` scopes of `file`, computed in a single tree walk
/// per file and memoized. Invalidated together with the file's item tree when
/// the file text changes.
///
/// The scopes are independent of the client's lint configuration — the client
/// set is applied on top, by [`keeps_body_diagnostic`] and
/// [`keeps_decl_diagnostic`] — so the memoized report stays valid across a
/// lint-config change.
#[salsa::tracked(returns(ref))]
pub(crate) fn warning_scopes_query(db: &dyn TyDatabase, file: FileText) -> Arc<[SuppressionScope]> {
    let file_id = *file.file_id(db);
    suppression_scopes(db, file_id).into()
}

/// Whether a warning of `key` reported at `range` is suppressed in `file`
/// ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
///
/// The scopes are memoized per file ([`warning_scopes_query`]), so this is a
/// binary search over a short list per diagnostic.
fn warning_is_suppressed(
    db: &dyn TyDatabase,
    file_id: FileId,
    range: TextRange,
    key: LintKey,
) -> bool {
    let scopes = warning_scopes_query(db, db.file_text(file_id));
    is_suppressed(scopes, range, key)
}

/// Whether an annotation's qualified name denotes `java.lang.SuppressWarnings`
/// — the simple name, or any qualified spelling of it
/// ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5)).
fn is_suppress_warnings(name: &str) -> bool {
    let name = name.trim();
    name == "SuppressWarnings" || name.ends_with(".SuppressWarnings")
}

/// The lint key of a deprecation: `removal` for a terminally deprecated
/// element, `deprecation` otherwise ([JLS §9.6.4.6]).
fn lint_of_deprecation(deprecation: hir_ty::java::deprecation::Deprecation) -> LintKey {
    match deprecation {
        hir_ty::java::deprecation::Deprecation::Ordinary => LintKey::Deprecation,
        hir_ty::java::deprecation::Deprecation::Terminal => LintKey::Removal,
    }
}

/// The lint key of a body diagnostic, or `None` for an error (no string
/// suppresses it, and a client lint set cannot disable it).
pub(crate) fn lint_of_body(diag: &TypeError) -> Option<LintKey> {
    match diag {
        TypeError::RawTypeUse { .. } => Some(LintKey::RawTypes),
        TypeError::UncheckedConversion { .. }
        | TypeError::UncheckedInvocation { .. }
        | TypeError::UncheckedCast { .. }
        | TypeError::UncheckedArgument { .. } => Some(LintKey::Unchecked),
        TypeError::DeprecatedUse { deprecation, .. } => Some(lint_of_deprecation(*deprecation)),
        _ => None,
    }
}

/// The lint key of a declaration diagnostic, or `None` for an error.
pub(crate) fn lint_of_decl(diag: &DeclDiagnostic) -> Option<LintKey> {
    match diag {
        DeclDiagnostic::RawTypeUse { .. } => Some(LintKey::RawTypes),
        DeclDiagnostic::DeprecatedUse { deprecation, .. } => {
            Some(lint_of_deprecation(*deprecation))
        }
        _ => None,
    }
}

/// Whether the type layer reports `diag` as a *warning* — a legal program
/// reported for its unsoundness ([§4.12.2] raw types, [§5.1.9] unchecked
/// conversion) — rather than as a compile-time error. Exactly the diagnostics
/// a lint key names.
pub(crate) fn severity_of_body(diag: &TypeError) -> Severity {
    if lint_of_body(diag).is_some() {
        Severity::Warning
    } else {
        Severity::Error
    }
}

/// Whether the declaration layer reports `diag` as a warning rather than an
/// error ([§4.12.2]).
pub(crate) fn severity_of_decl(diag: &DeclDiagnostic) -> Severity {
    if lint_of_decl(diag).is_some() {
        Severity::Warning
    } else {
        Severity::Error
    }
}

/// Whether a body diagnostic survives the in-source `@SuppressWarnings` scopes
/// and the client's lint set ([JLS §9.6.4.5]).
///
/// An error — a diagnostic no lint key names — is always kept; a warning is
/// kept when the client enabled its key and no enclosing declaration names it.
pub fn keeps_body_diagnostic(
    db: &dyn TyDatabase,
    file_id: FileId,
    bodies: &BodyTree,
    diag: &TypeError,
    lints: &LintConfig,
) -> bool {
    let Some(key) = lint_of_body(diag) else {
        return true;
    };
    if !lints.enables(key) {
        return false;
    }
    let Some(range) = diag.range(bodies) else {
        // A synthetic construct (`Missing` source) has no range, so no scope
        // contains it.
        return true;
    };
    !warning_is_suppressed(db, file_id, range, key)
}

/// Whether a declaration diagnostic survives the in-source `@SuppressWarnings`
/// scopes and the client's lint set ([JLS §9.6.4.5]).
pub fn keeps_decl_diagnostic(
    db: &dyn TyDatabase,
    file_id: FileId,
    diag: &DeclDiagnostic,
    lints: &LintConfig,
) -> bool {
    let Some(key) = lint_of_decl(diag) else {
        return true;
    };
    if !lints.enables(key) {
        return false;
    }
    let Some(range) = diag.range() else {
        return true;
    };
    !warning_is_suppressed(db, file_id, range, key)
}
