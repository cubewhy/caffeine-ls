//! The platform-release view of the API a reference names
//! ([JEP 247](https://openjdk.org/jeps/247)).
//!
//! A reference is resolved against the runtime JDK
//! ([JLS §7.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.3)),
//! so an API the runtime provides but the source set's `--release` does not is
//! otherwise invisible — javac rejects it, because it resolves the platform
//! against the release's view of the SDK's `ct.sym`
//! ([`hir::ct_sym`]). This module asks that archive the same question about the
//! API a reference *did* resolve to, and renders the report.
//!
//! The check is additive by construction: it runs only on a reference that
//! resolved (`NameResolution::Resolved` / a picked member), so a reference the
//! runtime view cannot resolve keeps reporting exactly as before
//! ([`crate::java::name_check`], [`crate::java::diagnostics`]) and the two
//! never double-report.

use hir_expand::name::Name;

use crate::java::db::TyDatabase;
use crate::java::ty::Ty;

/// The one platform API a report names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseApi {
    /// A type ([JLS §6.7]); `name` is the spelling at the reference site.
    Class { name: Name },
    /// A method or constructor
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6),
    /// [JLS §8.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4)/[§8.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8));
    /// `name` is `<init>` for a constructor.
    Method {
        owner: String,
        name: String,
        params: Vec<Ty>,
    },
    /// A field ([JLS §8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3)).
    Field { owner: String, name: String },
    /// A member whose kind the reference does not determine (a static single
    /// import, [JLS §7.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4)).
    Member { owner: String, name: String },
}

impl ReleaseApi {
    /// The report line: `<api> is not supported in release {found} (added in
    /// release {added})`, where `<api>` is `class 'X'`, `method 'm(A, B)' in
    /// 'X'`, `constructor 'X(A)' in 'X'` (the simple name of the owner),
    /// `field 'f' in 'X'` or `member 'm' in 'X'`. Parameter types render
    /// through [`Ty::display`] and join with `", "` — the same rendering a
    /// resolved invocation uses.
    pub fn render(&self, db: &dyn TyDatabase, found: u8, added: u8) -> String {
        let api = match self {
            ReleaseApi::Class { name } => format!("class '{}'", name.as_str()),
            ReleaseApi::Method {
                owner,
                name,
                params,
            } => {
                let params = params
                    .iter()
                    .map(|ty| ty.display(db).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                if name == "<init>" {
                    format!(
                        "constructor '{}({params})' in '{owner}'",
                        simple_name(owner)
                    )
                } else {
                    format!("method '{name}({params})' in '{owner}'")
                }
            }
            ReleaseApi::Field { owner, name } => format!("field '{name}' in '{owner}'"),
            ReleaseApi::Member { owner, name } => format!("member '{name}' in '{owner}'"),
        };
        format!("{api} is not supported in release {found} (added in release {added})")
    }
}

/// The simple name of a (binary) owner FQN, for the constructor rendering.
fn simple_name(owner: &str) -> &str {
    owner.rsplit('.').next().unwrap_or(owner)
}

/// The release `scope`'s files compile against
/// ([JEP 247](https://openjdk.org/jeps/247)). `Classpath` and `JdkBuiltins`
/// scopes have no source set and therefore no release.
pub(crate) fn release_of(db: &dyn TyDatabase, scope: &hir::ResolutionScope) -> Option<u8> {
    match scope {
        hir::ResolutionScope::SourceSet(source_set) => {
            hir::release_for_source_set(db, source_set.clone())
        }
        hir::ResolutionScope::Classpath(_) | hir::ResolutionScope::JdkBuiltins => None,
    }
}

/// Whether `fqn` belongs to a platform library of the workspace (the JDK
/// built-ins) rather than to a third-party jar, which the release view says
/// nothing about.
fn is_platform(db: &dyn TyDatabase, library: hir::LibraryId) -> bool {
    hir::jdk_builtin_libraries(db).contains(&library)
}

/// JEP 247: `Some((found, added))` when `name` — a name a type reference
/// resolved to — is a platform class the release view does not provide.
pub(crate) fn class_of_reference(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    name: &Name,
) -> Option<(u8, u8)> {
    let release = release_of(db, scope)?;
    let hir::Resolved::Library(class) = hir::fqn_resolve(db, scope, name.as_str())? else {
        // A source class is not platform API.
        return None;
    };
    if !is_platform(db, class.library) {
        return None;
    }
    let fqn = db.hir_state().interner.resolve(&class.entry.fqn);
    hir::ct_sym_class_not_in_release(db, class.library, release, fqn)
}

/// JEP 247 for one member of the platform class `owner` (its binary FQN,
/// [JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)):
/// `Some((found, added))` when the runtime JDK provides the member but the
/// release's platform view does not. `descriptor` is the member's classfile
/// descriptor ([JVMS §4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5)/[§4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)),
/// or `None` to match a member of that name of either kind — the form a static
/// single import needs
/// ([JLS §7.5.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4)).
pub(crate) fn member_of_owner(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    owner: &str,
    name: &str,
    descriptor: Option<&str>,
) -> Option<(u8, u8)> {
    let release = release_of(db, scope)?;
    let hir::Resolved::Library(class) = hir::fqn_resolve(db, scope, owner)? else {
        // A source class is not platform API.
        return None;
    };
    if !is_platform(db, class.library) {
        return None;
    }
    hir::ct_sym_member_not_in_release(db, class.library, release, owner, name, descriptor)
}
