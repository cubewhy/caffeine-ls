//! Lint policy: the `@SuppressWarnings` vocabulary and scopes
//! ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5))
//! and the severity/lint key of every diagnostic the type layer reports.
//!
//! Every kind of warning this analyzer can produce is reported; §9.6.4.5 fixes
//! how a *declaration* suppresses one, not a set a client may switch off.
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
//!   warnings of §4.8/§4.12.2 as `"rawtypes"`. Every other string names
//!   nothing here and is ignored as §9.6.4.5 requires: a string the compiler
//!   does not recognize must not suppress anything.
//! * *`"all"`*: the one string this analyzer recognizes that javac does not.
//!   §9.6.4.5 obliges a compiler to ignore a string it does not *recognize*,
//!   not to refuse to recognize one, and the IDEs this server runs inside
//!   (IntelliJ, Eclipse) accept `@SuppressWarnings("all")` as "every warning
//!   written here" — it is what IntelliJ inserts. Honouring it means a
//!   suppression the editor shows is not reported again here; the cost is
//!   that the report is quieter than `javac`'s for a file javac compiles with
//!   those warnings. See [`LintKey::ALL`].
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
use hir_expand::name::Name;
use hir_ty::java::resolve::NameResolution;
use rowan::{SyntaxNode, TextRange};
use rustc_hash::FxHashSet;
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J, translate_unicode_escapes};
use triomphe::Arc;
use vfs::FileId;

use hir_ty::{DeclDiagnostic, TyDatabase, TypeError};
use ide_db::Severity;
use ide_db::base_db::FileText;

/// A warning kind this analyzer reports, named by the string
/// `@SuppressWarnings` uses for it ([JLS §9.6.4.5]).
///
/// Every kind is reported: §9.6.4.5 fixes the *scope* of a suppression and its
/// vocabulary, not a switch a client can turn off. The spelling is the one
/// §9.6.4.5 mandates for the unchecked and deprecation warnings; `rawtypes` is
/// the reference implementation's documented non-standard name for the
/// raw-type warning of
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
    /// Every key this analyzer reports, in one place — what the non-standard
    /// string `"all"` names ([`LintKey::keys_of`]).
    pub(crate) const ALL: &'static [LintKey] = &[
        LintKey::Unchecked,
        LintKey::RawTypes,
        LintKey::Deprecation,
        LintKey::Removal,
    ];

    /// The keys a `@SuppressWarnings` string names, or an empty slice for a
    /// string this analyzer does not recognize — which §9.6.4.5 requires it
    /// to ignore.
    ///
    /// The four javac names name one key each and are case-sensitive. `"all"`
    /// names every key: it is not one of §9.6.4.5's four strings and javac
    /// ignores it, but the IDEs this server runs inside honour it (see the
    /// module docs), so `@SuppressWarnings("all")` suppresses here exactly
    /// what it suppresses there.
    fn keys_of(text: &str) -> &'static [LintKey] {
        match text {
            "unchecked" => &[LintKey::Unchecked],
            "rawtypes" => &[LintKey::RawTypes],
            "deprecation" => &[LintKey::Deprecation],
            "removal" => &[LintKey::Removal],
            "all" => LintKey::ALL,
            _ => &[],
        }
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
    let tree = hir::hir_def::java::plugin::tree(db, file_id);
    let Some((_map, source)) = hir_ty::java::range_ctx::range_ctx(db, file_id, tree.language)
    else {
        return Vec::new();
    };
    let SourceFile::Java(file) = &source else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(
        db,
        file_id,
        &file.syntax_node,
        &FxHashSet::default(),
        &mut out,
    );
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
/// *declaration* that names `@SuppressWarnings` and recording its range.
///
/// The scope owner is the declaration node itself (`METHOD_DECL`,
/// `FIELD_DECL`, `CLASS_DECL`, `LOCAL_VARIABLE_DECLARATION`, `PARAMETER`,
/// `ENUM_CONSTANT`, `MODULE_DECL`, ...), because §9.6.4.5 scopes a
/// suppression to "the annotated declaration **or any of its parts**" — its
/// whole source range. [`declaration_keys`] reads the annotations off it,
/// wherever the grammar put them.
fn collect(
    db: &dyn TyDatabase,
    file_id: FileId,
    node: &SyntaxNode<Lang>,
    inherited: &FxHashSet<LintKey>,
    out: &mut Vec<SuppressionScope>,
) {
    let mut keys = inherited.clone();
    let own = declaration_keys(db, file_id, node);
    if !own.is_empty() {
        keys.extend(own);
        out.push(SuppressionScope {
            range: node.text_range(),
            keys: keys.clone(),
        });
    }
    for child in node.children() {
        collect(db, file_id, &child, &keys, out);
    }
}

/// The keys the `@SuppressWarnings` annotations written on the declaration
/// `node` name — empty for a node that is not one.
///
/// A declaration's annotations are the `ANNOTATION` children of its
/// `MODIFIER_LIST` child ([`grammar::modifiers`]), which is how every
/// declaration but one is parsed. The exception is an *enum constant*
/// ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)),
/// whose grammar has no modifier list: its annotations are children of the
/// `ENUM_CONSTANT` node itself, and they scope the constant exactly as a
/// method's scope its own body.
fn declaration_keys(
    db: &dyn TyDatabase,
    file_id: FileId,
    node: &SyntaxNode<Lang>,
) -> FxHashSet<LintKey> {
    let mut out = FxHashSet::default();
    if let Some(modifier_list) = node
        .children()
        .find(|child| child.kind() == J::MODIFIER_LIST)
    {
        for annotation in modifier_list.children() {
            if annotation.kind() == J::ANNOTATION {
                annotation_keys(db, file_id, &annotation, &mut out);
            }
        }
    }
    if node.kind() == J::ENUM_CONSTANT {
        for annotation in node.children() {
            if annotation.kind() == J::ANNOTATION {
                annotation_keys(db, file_id, &annotation, &mut out);
            }
        }
    }
    out
}

/// Adds the keys one `@SuppressWarnings` annotation names. An annotation of
/// another type — including one that merely *spells* its name the same —
/// names nothing; unrecognized strings are dropped ([JLS §9.6.4.5]).
fn annotation_keys(
    db: &dyn TyDatabase,
    file_id: FileId,
    annotation: &SyntaxNode<Lang>,
    out: &mut FxHashSet<LintKey>,
) {
    // The annotation's written name is the first `QUALIFIED_NAME` of its
    // node — the same node the item tree lowers the name from
    // ([`hir_def::java::lower::walk::annotation_name_ref`]).
    let Some(name_node) = annotation
        .descendants()
        .find(|node| node.kind() == J::QUALIFIED_NAME)
    else {
        return;
    };
    if !is_suppress_warnings(db, file_id, &name_node) {
        return;
    }
    // The marker form names no warning, and `@SuppressWarnings` is not a
    // marker annotation anyway
    // ([§9.6.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4)).
    if !annotation
        .children()
        .any(|child| child.kind() == J::ANNOTATION_ARGUMENT_LIST)
    {
        return;
    }
    // §9.7.1 makes an element value an *expression*, so a key may be the value
    // of a constant variable rather than a literal
    // (`static final String K = "unchecked"; @SuppressWarnings(K)`), which
    // javac honours — and a concatenation names a key only as its *whole*
    // value (`"un" + "checked"` is `unchecked`, `"unchecked" + "X"` is not
    // one). The type layer reads each element value as the constant it is;
    // what it cannot read names nothing, and must not be guessed from the
    // source, which would let the second of those pass for the first.
    let Some(values) =
        hir_ty::java::annotation_value::suppress_warnings_values(db, file_id, annotation)
    else {
        return;
    };
    out.extend(
        values
            .iter()
            .flat_map(|value| LintKey::keys_of(value))
            .copied(),
    );
}

/// The `@SuppressWarnings` scopes of `file`, computed in a single tree walk
/// per file and memoized. Invalidated together with the file's item tree when
/// the file text changes.
///
/// The scopes are the only input to suppression
/// ([`keeps_body_diagnostic`], [`keeps_decl_diagnostic`]), so the memoized
/// report is the report.
#[salsa::tracked(returns(ref))]
pub(crate) fn warning_scopes_query(db: &dyn TyDatabase, file: FileText) -> Arc<[SuppressionScope]> {
    let file_id = *file.file_id(db);
    crate::lang::for_file(db, file_id)
        .map_or_else(Vec::new, |language| {
            language.suppression_scopes(db, file_id)
        })
        .into()
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

/// Whether a written annotation name denotes `java.lang.SuppressWarnings`
/// itself — the *symbol*, not a spelling ([JLS
/// §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)).
/// The name resolves in the context of the declaration it annotates, so a
/// `SuppressWarnings` type declared in the file's package, imported with that
/// name, or nested in an enclosing class is *that* annotation: it suppresses
/// nothing, as javac has it ([JLS §9.6.4.5]).
fn is_suppress_warnings(
    db: &dyn TyDatabase,
    file_id: FileId,
    name_node: &SyntaxNode<Lang>,
) -> bool {
    let name = Name::new(&translate_unicode_escapes(
        name_node.text().to_string().trim(),
    ));
    matches!(
        hir_ty::java::resolve::resolve_written_name(db, file_id, name_node, &name),
        NameResolution::Resolved(resolved) if resolved.as_str() == SUPPRESS_WARNINGS
    )
}

/// The fully qualified name of the annotation §9.6.4.5 gives its meaning to.
const SUPPRESS_WARNINGS: &str = "java.lang.SuppressWarnings";

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
/// ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
///
/// An error — a diagnostic no lint key names — is always kept; a warning is
/// kept unless an enclosing declaration names its key. Every warning kind is
/// reported, so a suppressed warning is the only one this drops.
pub fn keeps_body_diagnostic(
    db: &dyn TyDatabase,
    file_id: FileId,
    bodies: &BodyTree,
    diag: &TypeError,
) -> bool {
    let Some(key) = lint_of_body(diag) else {
        return true;
    };
    let Some(range) = diag.range(bodies) else {
        // A synthetic construct (`Missing` source) has no range, so no scope
        // contains it.
        return true;
    };
    !warning_is_suppressed(db, file_id, range, key)
}

/// Whether a declaration diagnostic survives the in-source
/// `@SuppressWarnings` scopes ([JLS §9.6.4.5]).
pub fn keeps_decl_diagnostic(db: &dyn TyDatabase, file_id: FileId, diag: &DeclDiagnostic) -> bool {
    let Some(key) = lint_of_decl(diag) else {
        return true;
    };
    let Some(range) = diag.range() else {
        return true;
    };
    !warning_is_suppressed(db, file_id, range, key)
}
