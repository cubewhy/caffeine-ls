//! The wording of the deprecation warnings ([JLS §9.6.4.6]).
//!
//! Javac's `compiler.properties` carries the two literals this module renders:
//! `compiler.warn.has.been.deprecated` = `{0} in {1} has been deprecated` and
//! `compiler.warn.has.been.deprecated.for.removal` = `{0} in {1} has been
//! deprecated and marked for removal`. `{0}` is the API as javac spells it —
//! the simple name of a class, `name(params)` for a method (the constructor
//! under its class's simple name), the bare name for a field — and `{1}` is
//! the simple name of the declaring class, or the package for a top-level
//! class. An element of the unnamed package renders without the `in` clause
//! (`Top in unnamed package has been deprecated` is javac's wording for a
//! *class*; a member of a class in the unnamed package reads
//! `m() in Top has been deprecated`, whose `{1}` is the class, not the
//! package).

use hir_ty::TyDatabase;
use hir_ty::java::deprecation::{DeprecatedApi, Deprecation};
use hir_ty::java::ty::Ty;

/// Javac's deprecation sentence for `api` ([JLS §9.6.4.6]).
pub(crate) fn message(
    db: &dyn TyDatabase,
    api: &DeprecatedApi,
    deprecation: Deprecation,
) -> String {
    let text = api_text(db, api);
    match owner_operand(api).map(|owner| owner_text(api, owner)) {
        Some(owner) => match deprecation {
            Deprecation::Ordinary => format!("{text} in {owner} has been deprecated"),
            Deprecation::Terminal => {
                format!("{text} in {owner} has been deprecated and marked for removal")
            }
        },
        None => match deprecation {
            Deprecation::Ordinary => format!("{text} has been deprecated"),
            Deprecation::Terminal => {
                format!("{text} has been deprecated and marked for removal")
            }
        },
    }
}

/// Javac's `{0}` operand: how the deprecated API itself is spelled.
fn api_text(db: &dyn TyDatabase, api: &DeprecatedApi) -> String {
    match api {
        DeprecatedApi::Class { name, .. } => name.as_str().to_owned(),
        DeprecatedApi::Method {
            owner,
            name,
            params,
        } => {
            let params = params
                .iter()
                .map(|ty| display_param(db, *ty))
                .collect::<Vec<_>>()
                .join(",");
            if name.as_str() == "<init>" {
                // A constructor is named after its class, without a space
                // after the commas of its parameter list (javac's own
                // rendering: `M(String)`, `p(int,String)`).
                format!("{}({params})", simple_name(owner.as_str()))
            } else {
                format!("{}({params})", name.as_str())
            }
        }
        DeprecatedApi::Field { name, .. } => name.as_str().to_owned(),
    }
}

/// The `{1}` operand of `api`: a member's declaring class, or a class's own
/// owner operand (its package, or its enclosing class's simple name). A
/// top-level class of the unnamed package has no owner at all.
fn owner_operand(api: &DeprecatedApi) -> Option<&hir_expand::name::Name> {
    let owner = match api {
        DeprecatedApi::Class { owner, .. } => owner,
        DeprecatedApi::Method { owner, .. } | DeprecatedApi::Field { owner, .. } => owner,
    };
    (!owner.as_str().is_empty()).then_some(owner)
}

/// The rendered `{1}` operand. A member's owner is a class, printed by its
/// simple name; a class's owner is a package or an enclosing class, printed
/// as it stands.
fn owner_text(api: &DeprecatedApi, owner: &hir_expand::name::Name) -> String {
    match api {
        DeprecatedApi::Class { .. } => owner.as_str().to_owned(),
        DeprecatedApi::Method { .. } | DeprecatedApi::Field { .. } => {
            simple_name(owner.as_str()).to_owned()
        }
    }
}

/// A parameter type as javac renders it in a deprecation message: simple
/// names, no spaces after the commas (`p(int,String)`).
fn display_param(db: &dyn TyDatabase, ty: Ty) -> String {
    ty.display_simple(db).to_string()
}

/// The simple name of a class in either naming: the last segment of a source
/// FQN or of a library binary name (`p.Outer$Inner` is `Inner`).
fn simple_name(fqn: &str) -> &str {
    let class_part = fqn.rsplit('.').next().unwrap_or(fqn);
    class_part.rsplit('$').next().unwrap_or(class_part)
}
