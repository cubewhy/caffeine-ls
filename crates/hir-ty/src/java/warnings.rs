//! Warning suppression ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
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

use rowan::{SyntaxNode, TextRange};
use rustc_hash::FxHashSet;
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J};
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::java::range_ctx::range_ctx;
use crate::java::ty::{Ty, TyKind};

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
}

impl LintKey {
    /// The string that names this warning in `@SuppressWarnings`
    /// ([JLS §9.6.4.5]).
    pub fn as_str(self) -> &'static str {
        match self {
            LintKey::Unchecked => "unchecked",
            LintKey::RawTypes => "rawtypes",
            LintKey::Deprecation => "deprecation",
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
            _ => None,
        }
    }
}

/// One `@SuppressWarnings` scope: the annotated declaration's source range and
/// the warning keys in effect for it — its own plus every enclosing
/// declaration's ([JLS §9.6.4.5]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionScope {
    pub range: TextRange,
    pub keys: FxHashSet<LintKey>,
}

/// Every `@SuppressWarnings` scope of `file_id`, in source order.
///
/// A warning at `range` is suppressed when some returned scope both contains
/// `range` and carries its key; [`is_suppressed`] performs that check.
pub fn suppression_scopes(db: &dyn TyDatabase, file_id: FileId) -> Vec<SuppressionScope> {
    let tree = hir::file_item_tree(db, file_id);
    let Some((_map, source)) = range_ctx(db, file_id, tree.language) else {
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
pub fn is_suppressed(scopes: &[SuppressionScope], range: TextRange, key: LintKey) -> bool {
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

/// Whether a warning of `key` reported at `range` is suppressed in `file`
/// ([JLS §9.6.4.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.5)).
///
/// The scopes are memoized per file
/// ([`crate::java::db::warning_scopes_query`]), so this is a binary search over
/// a short list per diagnostic — the shape both warning production points
/// (body inference and the declaration walk) call.
pub fn warning_is_suppressed(
    db: &dyn TyDatabase,
    file_id: FileId,
    range: TextRange,
    key: LintKey,
) -> bool {
    let scopes = crate::java::db::warning_scopes_query(db, db.file_text(file_id));
    is_suppressed(scopes, range, key)
}

/// Whether `ty` is a *raw* use of a generic class
/// ([JLS §4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8),
/// [§4.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.2)):
/// a reference type written without type arguments whose class declares type
/// parameters. A non-generic class (`String`) and a parameterized use
/// (`List<String>`) are not raw.
///
/// The check is on the *written* form: `List<String>`'s erasure is also named
/// `List` with no arguments, so this must be asked of the reference as it
/// appears in source (or in a classfile `Signature`), never of an erased type.
pub fn is_raw_reference(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> bool {
    let TyKind::Reference { name, args } = ty.kind(db) else {
        return false;
    };
    args.is_empty() && !ty.is_error(db) && crate::java::resolve::class_is_generic(db, scope, name)
}

/// Whether an annotation's qualified name denotes `java.lang.SuppressWarnings`
/// — the simple name, or any qualified spelling of it
/// ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5)).
fn is_suppress_warnings(name: &str) -> bool {
    let name = name.trim();
    name == "SuppressWarnings" || name.ends_with(".SuppressWarnings")
}
