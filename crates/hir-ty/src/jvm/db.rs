//! The JVM type-database substrate: the salsa database trait and the interned
//! keys the shared queries are memoized on.
//!
//! [`TyDatabase`] extends [`hir::HirDatabase`] with everything a type layer
//! needs — a language adds its own tracked queries as an extension trait
//! ([`crate::java::db`]). [`ItemKey`], [`ScopeId`] and [`ContextKey`] exist so
//! that per-item work, the per-pair subtype/supertype walks and the member set
//! of a name can key salsa queries on cheap `Copy` values instead of on the
//! values themselves.

use base_db::salsa;
use hir_def::java::item_tree::ItemId;
use hir_expand::name::Name;
use vfs::FileId;

use crate::java::method::{InvocationContext, InvocationMode};

/// The type database: [`hir::HirDatabase`] plus the type-system API of this
/// crate. Concrete databases (e.g. `ide-db`'s `RootDatabase`) implement this
/// and salsa's `#[salsa::db]` machinery wires up the tracked queries.
#[salsa::db]
pub trait TyDatabase: hir::HirDatabase {}

/// A workspace-unique item id. Interned so it can key tracked queries; the
/// underlying [`FileId`]/[`ItemId`] pair is `Copy`, so `#[returns(copy)]` keeps
/// the accessors cheap.
#[salsa::interned]
pub struct ItemKey {
    #[returns(copy)]
    pub file: FileId,
    #[returns(copy)]
    pub item: ItemId,
}

/// The set of libraries a type query may see: the interned analogue of
/// [`hir::ResolutionScope`]. Interned (rather than passed as a plain value) so
/// it can key the memoized subtype/supertype queries in [`crate::java::subtyping`].
#[salsa::interned(unsafe(no_lifetime), debug, revisions = usize::MAX)]
pub struct ScopeId {
    pub kind: ScopeKind,
}

/// The interned form of [`hir::ResolutionScope`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// A workspace source set: its ordered classpath.
    SourceSet(hir::SourceSetId),
    /// An explicit, ordered library list (tests / synthetic scopes).
    Classpath(Vec<hir::LibraryId>),
    /// Only the JDK built-ins (jimage / rt.jar).
    JdkBuiltins,
}

impl ScopeKind {
    /// The interning data for a [`hir::ResolutionScope`].
    pub fn from_scope(scope: &hir::ResolutionScope) -> Self {
        match scope {
            hir::ResolutionScope::SourceSet(source_set) => ScopeKind::SourceSet(source_set.clone()),
            hir::ResolutionScope::Classpath(libraries) => ScopeKind::Classpath(libraries.clone()),
            hir::ResolutionScope::JdkBuiltins => ScopeKind::JdkBuiltins,
        }
    }

    /// The [`hir::ResolutionScope`] this kind was interned from.
    pub fn to_scope(&self) -> hir::ResolutionScope {
        match self {
            ScopeKind::SourceSet(source_set) => hir::ResolutionScope::SourceSet(source_set.clone()),
            ScopeKind::Classpath(libraries) => hir::ResolutionScope::Classpath(libraries.clone()),
            ScopeKind::JdkBuiltins => hir::ResolutionScope::JdkBuiltins,
        }
    }
}

/// The invocation context of a method or field access, interned so it can key
/// the memoized member-set query
/// ([`crate::jvm::member_set::member_set_query`]): the invocation mode
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// plus the access-control context
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6))
/// of the access site. Interned (rather than passed as a plain value) so it can
/// key the memoized member-set query.
#[salsa::interned(unsafe(no_lifetime), debug, revisions = usize::MAX)]
pub struct ContextKey {
    pub mode: InvocationMode,
    /// The class or interface the access appears in
    /// ([`crate::jvm::member::ClassKey`]: a *local* declaration is its own
    /// class, so the key round-trips it).
    pub enclosing_class: Option<crate::jvm::member::ClassKey>,
    pub package: Option<Name>,
    pub subclass_of: Option<crate::jvm::member::ClassKey>,
}

impl ContextKey {
    /// The interning data of an [`InvocationContext`].
    pub fn from_invocation(db: &dyn TyDatabase, ctx: &InvocationContext) -> ContextKey {
        ContextKey::new(
            db,
            ctx.mode,
            ctx.enclosing_class.clone(),
            ctx.package.as_deref().map(Name::new),
            ctx.subclass_of.clone(),
        )
    }
}
