//! Source-side type name resolution.
//!
//! Resolves the names inside a lowered item tree to canonical fully qualified
//! names, following the declaration rules for simple type names
//! ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1))
//! and qualified type names
//! ([JLS §6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)),
//! with the import machinery of
//! [JLS §7.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.1)
//! (single-type imports),
//! [JLS §7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)
//! (on-demand imports) and the implicit `java.lang` fallback. Type parameters
//! in scope ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3))
//! become [`TyKind::TypeVar`].
//!
//! A [`Resolver`] captures the per-file name context of one item: the
//! compilation unit's package and imports, plus the type parameters in scope
//! at that item. Resolution itself runs against a [`hir::ResolutionScope`]:
//! the candidates are probed with [`hir::fqn_resolve`] against that scope's
//! classpath, and the first one that exists wins.

use rowan::{SyntaxNode, TextRange};
use rustc_hash::FxHashMap;
use stacksafe::stacksafe;
use syntax::SourceFile;
use syntax::java::Lang;
use vfs::FileId;

use hir_def::java::item_tree::{ImportItem, ItemData, ItemId, ItemTree, TypeParam};
use hir_def::java::ranges;
use hir_expand::ast_id_map::AstIdMap;
use hir_expand::body::{BodyId, BodyTree, LocalId, StmtData, StmtId};
use hir_expand::name::Name;
use syntax::stub::{TypeBound, TypeRef};

use crate::{
    java::db::TyDatabase,
    java::range_ctx::range_ctx,
    java::ty::{BoundKind, Ty, TyKind, TypeVarScope, WildcardBound, ty_from_type_ref},
};

/// A type parameter in scope at some item, together with the declaration that
/// introduces it ([JLS §4.4], [§6.3]). The *name* drives lexical lookup
/// ([§6.4.1]: the innermost declaration with a given name wins); the
/// [`TypeVarScope`] is the identity the resolved variable interns and
/// substitutes by, so a method parameter shadowing a same-named class
/// parameter stays a distinct type.
#[derive(Debug, Clone)]
pub struct ScopedTypeParam {
    pub param: TypeParam,
    pub scope: TypeVarScope,
}

impl PartialEq for ScopedTypeParam {
    /// Two in-scope parameters are the same when their declaring scopes are —
    /// the scope is the parameter's identity ([JLS §4.4], [§6.3]), and it
    /// determines the declaration the parameter (and hence its bounds) came
    /// from. Used by the per-file query's return-value equality.
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
    }
}

impl Eq for ScopedTypeParam {}

impl std::ops::Deref for ScopedTypeParam {
    type Target = TypeParam;
    fn deref(&self) -> &TypeParam {
        &self.param
    }
}

/// A *local* class-like declaration in scope ([JLS §14.3]): its simple name —
/// a local class has neither a fully qualified nor a canonical name ([§6.7]),
/// so a use is written with this name — and the declaration it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedLocalType {
    pub name: Name,
    pub class: hir::SourceClass,
}

/// The local declarations in scope at one item of a file
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)):
/// what a name written inside the item may denote beyond the compilation unit's
/// types and the item's own type parameters. Computed per file by
/// [`local_decl_sites`] and memoized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalDeclSite {
    /// The local class-like declarations in scope, outermost first — the
    /// order [`Resolver::type_param`] uses too, so the innermost declaration of
    /// a name wins ([§6.4.1]).
    pub local_types: Vec<ScopedLocalType>,
    /// The local variables in scope
    /// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)):
    /// the enclosing body's parameters (and the captured ones of the
    /// declarations enclosing it) plus every variable declared before this
    /// point in the enclosing blocks. A member body of a *local* class may use
    /// them ([§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)):
    /// they are the variables it captures.
    pub locals: Vec<LocalId>,
}

impl LocalDeclSite {
    /// The local declaration named `name` in scope ([§6.4.1]: the innermost
    /// declaration wins, so the *last* match — the scopes are held outermost
    /// first).
    pub fn local_type(&self, name: &Name) -> Option<&ScopedLocalType> {
        self.local_types.iter().rfind(|local| &local.name == name)
    }
}

/// The classfile `Signature`-lowering context ([JVMS §4.7.9.1]): the class
/// whose signature is being lowered and, for a member signature, the member,
/// so every type variable in it is attributed to its declaring parameter
/// ([JLS §4.4], [§6.3]).
#[derive(Clone, Copy)]
pub struct LibrarySignature<'a> {
    pub owner: &'a Name,
    pub method: Option<(&'a Name, &'a [Name])>,
}

impl<'a> LibrarySignature<'a> {
    /// The signature of the class `owner` itself (its supertypes, its own
    /// type-parameter bounds, a field's type).
    pub fn class(owner: &'a Name) -> Self {
        Self {
            owner,
            method: None,
        }
    }

    /// The scope of the type variable `name` within this signature
    /// ([§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1)):
    /// the member's own parameter when the member declares that name,
    /// otherwise the class's. A name declared by neither is an enclosing
    /// class's parameter, which this signature does not carry an argument for
    /// — it keeps the class identity so no binding of this class captures it.
    fn scope_of(&self, name: &Name) -> TypeVarScope {
        TypeVarScope::library(self.owner, self.method, name)
    }
}

/// The per-file name context of a single item: its package, the compilation
/// unit's imports, the type parameters in scope at the item, the *local*
/// class-like declarations in scope at the item and the fully qualified names
/// of every enclosing class-like declaration.
#[derive(Debug, Clone)]
pub struct Resolver {
    package: Option<Name>,
    imports: Vec<ImportItem>,
    type_params: Vec<ScopedTypeParam>,
    /// The local class-like declarations in scope at the item, outermost
    /// first ([JLS §6.3], [§6.4.1]) — the order `type_params` uses, so the
    /// innermost declaration of a name wins.
    local_types: Vec<ScopedLocalType>,
    /// The enclosing class-like declarations, innermost first, as canonical
    /// FQNs ([JLS §6.7]): their *member types* are in scope by simple name
    /// ([JLS §6.5.5.1]) ahead of any import.
    enclosing: Vec<Name>,
}

impl Resolver {
    /// The resolver for `item` of `file` ([`Self::new`] with the file's
    /// per-file maps looked up).
    pub fn for_item(db: &dyn TyDatabase, file: FileId, tree: &ItemTree, item: ItemId) -> Self {
        let text = db.file_text(file);
        Self::new(
            tree,
            crate::java::db::type_params_map_query(db, text),
            crate::java::db::local_decl_sites_query(db, text),
            item,
        )
    }

    /// Builds the resolver for `item_id` within `tree`, looking the type
    /// parameters in scope up in the per-file map computed by
    /// [`type_params_map`] and the local declarations in scope up in the map
    /// computed by [`local_decl_sites`].
    pub fn new(
        tree: &ItemTree,
        type_params: &FxHashMap<ItemId, Vec<ScopedTypeParam>>,
        local_sites: &FxHashMap<ItemId, LocalDeclSite>,
        item_id: ItemId,
    ) -> Self {
        Self {
            package: tree.package.clone(),
            imports: tree.imports.clone(),
            type_params: type_params.get(&item_id).cloned().unwrap_or_default(),
            local_types: local_sites
                .get(&item_id)
                .map(|site| site.local_types.clone())
                .unwrap_or_default(),
            enclosing: enclosing_type_chain(tree, item_id),
        }
    }

    pub fn package(&self) -> Option<&Name> {
        self.package.as_ref()
    }

    /// The resolver of a construct that belongs to no item — the annotations
    /// of a package declaration, say: the compilation unit's package and
    /// imports, with no declaration's type parameters or enclosing types
    /// around it.
    pub fn for_file(tree: &ItemTree) -> Self {
        Self {
            package: tree.package.clone(),
            imports: tree.imports.clone(),
            type_params: Vec::new(),
            local_types: Vec::new(),
            enclosing: Vec::new(),
        }
    }

    pub fn imports(&self) -> &[ImportItem] {
        &self.imports
    }

    /// For a static import ([JLS §7.5.4]) that names `simple` as a member —
    /// `import static pkg.Type.MEMBER` or `import static pkg.Type.*` — the
    /// declaring type's FQN and the member's simple name, in declaration
    /// order (the first matching import wins, [JLS §7.5.4]).
    pub fn static_import_owner(&self, simple: &str) -> Option<(Name, String)> {
        self.static_import_owners(simple).into_iter().next()
    }

    /// Every static import that could name `simple` as a member, in
    /// declaration order. On-demand imports (`import static pkg.Type.*`)
    /// contribute all their members to the scope ([JLS §7.5.4]), so several
    /// may name the same member and each must be probed until one resolves.
    pub fn static_import_owners(&self, simple: &str) -> Vec<(Name, String)> {
        let mut out = Vec::new();
        for import in &self.imports {
            if !import.is_static {
                continue;
            }
            let text = import.name.as_str();
            if import.is_asterisk {
                out.push((import.name.clone(), simple.to_owned()));
            } else if let Some((owner, member)) = text.rsplit_once('.')
                && member == simple
            {
                out.push((Name::new(owner), member.to_owned()));
            }
        }
        out
    }

    pub fn type_params(&self) -> &[ScopedTypeParam] {
        &self.type_params
    }

    /// The type parameter named `name` in scope ([JLS §6.4.1]): the innermost
    /// declaration wins, so a method's own parameter shadows an enclosing
    /// class's parameter of the same name. [`ScopedTypeParam`]s are held with
    /// the enclosing declarations *first*, so the last match is the innermost.
    pub fn type_param(&self, name: &Name) -> Option<&ScopedTypeParam> {
        self.type_params.iter().rfind(|param| param.name == *name)
    }

    /// The *local* class-like declaration named `name` in scope
    /// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)):
    /// the innermost declaration wins ([§6.4.1]), and a local declaration
    /// shadows every other type of that name, type parameters included. The
    /// scopes are held outermost first, so the last match is the innermost.
    pub fn local_type(&self, name: &Name) -> Option<&ScopedLocalType> {
        self.local_types.iter().rfind(|local| &local.name == name)
    }

    /// The local class-like declarations in scope at the item, outermost
    /// first.
    pub fn local_types(&self) -> &[ScopedLocalType] {
        &self.local_types
    }

    /// Replaces the local class-like declarations in scope with those
    /// declaring `items`, in order ([JLS §6.3]): the positional scope of a
    /// reference inside a body, whose declarations are `(file, item)` pairs.
    pub(crate) fn set_local_types_from(&mut self, items: &[ItemId], file: FileId, tree: &ItemTree) {
        self.local_types.clear();
        self.local_types.extend(items.iter().filter_map(|item| {
            let name = class_like_name(tree.data(*item))?;
            Some(ScopedLocalType {
                name: name.clone(),
                class: hir::SourceClass { file, item: *item },
            })
        }));
    }

    /// Empties the local scope: after a body has been walked, no declaration of
    /// it is in scope any more.
    pub(crate) fn clear_local_types(&mut self) {
        self.local_types.clear();
    }

    /// Extends the scope with a local class-like declaration
    /// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)):
    /// it is in scope in its own body and for the rest of the enclosing block,
    /// so a body walk pushes it where it is declared.
    pub(crate) fn push_local_type(&mut self, local: ScopedLocalType) {
        self.local_types.push(local);
    }

    /// Drops every declaration pushed since the scope had `len` entries — the
    /// exit of the block the declarations were made in ([§6.3]: a declaration
    /// is scoped to the rest of its own block).
    pub(crate) fn truncate_local_types(&mut self, len: usize) {
        self.local_types.truncate(len);
    }

    /// The enclosing class-like declarations, innermost first, as FQNs.
    pub fn enclosing(&self) -> &[Name] {
        &self.enclosing
    }
}

/// The enclosing class-like declarations of `item_id`, innermost first, as
/// canonical fully qualified names ([JLS §6.7]): the package followed by the
/// chain of nested type names. Member types of these declarations are in
/// scope by simple name ([JLS §6.5.5.1], [§8.1], [§9.1]).
///
/// A declaration with no canonical name — a *local* declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// or a member type of one — contributes nothing here: it has no FQN to be
/// probed with, and its member types reach the resolver through the *local*
/// scopes instead ([`LocalDeclSite`]).
pub(crate) fn enclosing_type_chain(tree: &ItemTree, item_id: ItemId) -> Vec<Name> {
    // The simple names of the class-like ancestors, outermost last.
    let mut names = Vec::new();
    let mut current = tree.parent_of(item_id);
    while let Some(id) = current {
        if let Some(name) = class_like_name(tree.data(id))
            && canonical_class_fqn(tree, id).is_some()
        {
            names.push(name.clone());
        }
        current = tree.parent_of(id);
    }

    // Accumulate FQNs from the outside in; the result is innermost first.
    let mut acc = tree.package.clone();
    let mut out = Vec::with_capacity(names.len());
    for name in names.iter().rev() {
        acc = Some(match &acc {
            Some(prefix) => join(prefix, name.as_str()),
            None => name.clone(),
        });
        let fqn = acc.clone().unwrap();
        out.push(fqn);
    }
    out
}

/// The canonical fully qualified name ([JLS §6.7]) of the class-like
/// declaration `item` of `tree`: the package followed by the chain of
/// enclosing type names, or `None` when the declaration has no canonical name
/// — a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)),
/// or a member type of one, which §6.7 leaves unnamed too. Mirrors the name
/// [`hir::source_class_fqn`] derives from the file's symbol set: a
/// declaration is named exactly when no local declaration encloses it.
pub(crate) fn canonical_class_fqn(tree: &ItemTree, item: ItemId) -> Option<Name> {
    let mut names = Vec::new();
    let mut current = Some(item);
    while let Some(id) = current {
        if tree.is_local_type(id) {
            return None;
        }
        if let Some(name) = class_like_name(tree.data(id)) {
            names.push(name.clone());
        }
        current = tree.parent_of(id);
    }

    match names.pop() {
        Some(outermost) => {
            let mut acc = match &tree.package {
                Some(package) => join(package, outermost.as_str()),
                None => outermost,
            };
            for name in names.iter().rev() {
                acc = join(&acc, name.as_str());
            }
            Some(acc)
        }
        None => None,
    }
}

/// The declared name of a class-like declaration, `None` for every other item.
fn class_like_name(data: &ItemData) -> Option<&Name> {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => Some(&d.name),
        ItemData::Enum(d) => Some(&d.name),
        ItemData::Record(d) => Some(&d.name),
        ItemData::Annotation(d) => Some(&d.name),
        _ => None,
    }
}

/// The type parameters in scope at every item of `tree` ([JLS §6.3]):
/// those of every enclosing type declaration plus, for methods, the method's
/// own parameters, with their declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)).
/// Computed in a single tree walk so each item's scope is a map lookup.
pub(crate) fn type_params_map(
    tree: &ItemTree,
    file: FileId,
) -> FxHashMap<ItemId, Vec<ScopedTypeParam>> {
    fn collect(
        tree: &ItemTree,
        file: FileId,
        id: ItemId,
        outer: &[ScopedTypeParam],
        map: &mut FxHashMap<ItemId, Vec<ScopedTypeParam>>,
    ) {
        let data = tree.data(id);
        let mut own = outer.to_vec();
        // §4.4/§6.3: each parameter is introduced by the declaration that
        // lists it, and its variable is identified by that declaration. A
        // class declares class-scoped parameters, a method or constructor
        // method-scoped ones; the two namespaces are distinct ([§6.4.1]), so
        // a method parameter shadows a same-named class parameter rather than
        // aliasing it.
        let declare = |params: &[TypeParam], method: bool| -> Vec<ScopedTypeParam> {
            params
                .iter()
                .map(|param| ScopedTypeParam {
                    param: param.clone(),
                    scope: if method {
                        TypeVarScope::Method {
                            file,
                            item: id,
                            name: param.name.clone(),
                        }
                    } else {
                        TypeVarScope::Class {
                            file,
                            item: id,
                            name: param.name.clone(),
                        }
                    },
                })
                .collect()
        };
        match data {
            ItemData::Class(d) | ItemData::Interface(d) => {
                own.extend(declare(&d.type_params, false))
            }
            ItemData::Record(d) => own.extend(declare(&d.type_params, false)),
            ItemData::Method(m) => own.extend(declare(&m.sig.type_params, true)),
            _ => {}
        }
        map.insert(id, own.clone());
        for &child in data.body() {
            collect(tree, file, child, &own, map);
        }
        // A local class-like declaration of the item's body
        // ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
        // is not a member, so it is not in any `body()`: the parameters in
        // scope at it are those of the body that declares it, and its own are
        // added below.
        for local in tree.local_types_of(id) {
            collect(tree, file, local, &own, map);
        }
    }

    let mut map = FxHashMap::default();
    for &top in &tree.top {
        collect(tree, file, top, &[], &mut map);
    }
    map
}

/// The local declarations in scope at every item of `tree`
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3)):
/// a local class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// is in scope in its own body and for the rest of the immediately enclosing
/// block, and every enclosing declaration is in scope inside it. Computed in
/// one walk of the declaration tree and the body IR, so each item's scope is a
/// map lookup — and, because the walk threads the scope through the *body*
/// statements, a declaration is in scope only after the statement that
/// declares it ([§6.3]).
///
/// An entry is recorded only for items that have at least one declaration in
/// scope: a top-level declaration (and everything under it, until a local
/// declaration appears) has none.
pub(crate) fn local_decl_sites(
    tree: &ItemTree,
    bodies: &BodyTree,
    file: FileId,
) -> FxHashMap<ItemId, LocalDeclSite> {
    struct Walker<'a> {
        tree: &'a ItemTree,
        bodies: &'a BodyTree,
        file: FileId,
        map: FxHashMap<ItemId, LocalDeclSite>,
    }

    impl Walker<'_> {
        /// Records the scope in force at `id` and walks the declaration's
        /// contents with that scope.
        fn item(&mut self, id: ItemId, scope: &[ScopedLocalType], locals: &[LocalId]) {
            if !scope.is_empty() || !locals.is_empty() {
                self.map.insert(
                    id,
                    LocalDeclSite {
                        local_types: scope.to_vec(),
                        locals: locals.to_vec(),
                    },
                );
            }
            let tree = self.tree;
            match tree.data(id) {
                ItemData::Class(_)
                | ItemData::Interface(_)
                | ItemData::Enum(_)
                | ItemData::Record(_)
                | ItemData::Annotation(_) => {
                    // §6.5.5.1: the member types of the declaration are in
                    // scope throughout its body. A member type of a
                    // *canonically named* class is reachable by its own
                    // canonical name and needs no scope entry; a member type
                    // of a declaration that has no canonical name ([§6.7] —
                    // a local declaration, or a member of one) has none, so
                    // it is carried as a local declaration of its own.
                    let mut inner = scope.to_vec();
                    if canonical_class_fqn(tree, id).is_none() {
                        inner.extend(
                            tree.data(id)
                                .body()
                                .iter()
                                .filter_map(|member| self.local_entry(*member)),
                        );
                    }
                    for member in tree.data(id).body().to_vec() {
                        self.item(member, &inner, locals);
                    }
                }
                ItemData::Method(data) => {
                    if let Some(body) = data.body() {
                        self.body(body, scope, locals);
                    }
                }
                ItemData::StaticInit(data) => {
                    if let Some(body) = data.body {
                        self.body(body, scope, locals);
                    }
                }
                ItemData::InstanceInit(data) => {
                    if let Some(body) = data.body {
                        self.body(body, scope, locals);
                    }
                }
                // A field's initializer and an enum constant's arguments are
                // expression forests, and an expression cannot declare a local
                // class; the member types of an enum are its constants, which
                // are not types.
                ItemData::Field(_) | ItemData::EnumConstant(_) | ItemData::Module(_) => {}
            }
        }

        /// The scope entry of a class-like member item, `None` for anything
        /// else.
        fn local_entry(&self, id: ItemId) -> Option<ScopedLocalType> {
            let name = class_like_name(self.tree.data(id))?;
            Some(ScopedLocalType {
                name: name.clone(),
                class: hir::SourceClass {
                    file: self.file,
                    item: id,
                },
            })
        }

        /// Walks a body's statements: its statement list is the body's own
        /// block, so a declaration made in it is in scope for the rest of the
        /// body.
        /// Walks a body's statements: its statement list is the body's own
        /// block, so a declaration made in it is in scope for the rest of the
        /// body, and the body's parameters are in scope throughout.
        fn body(&mut self, body: BodyId, scope: &[ScopedLocalType], locals: &[LocalId]) {
            let mut current_scope = scope.to_vec();
            let mut current_locals = locals.to_vec();
            current_locals.extend(self.bodies.body(body).params.iter().copied());
            let stmts = self.bodies.body(body).stmts.clone();
            self.stmts(&stmts, &mut current_scope, &mut current_locals);
        }

        fn stmts(
            &mut self,
            stmts: &[StmtId],
            current: &mut Vec<ScopedLocalType>,
            locals: &mut Vec<LocalId>,
        ) {
            for stmt in stmts {
                self.stmt(*stmt, current, locals);
            }
        }

        #[stacksafe]
        fn stmt(
            &mut self,
            stmt: StmtId,
            current: &mut Vec<ScopedLocalType>,
            locals: &mut Vec<LocalId>,
        ) {
            let bodies = self.bodies;
            match bodies.stmt(stmt) {
                StmtData::LocalClass { item } => {
                    // §6.3: the declaration is in scope in its own body
                    // (`class Cyclic { Cyclic c; }` is legal) and for the rest
                    // of the enclosing block.
                    let Some(entry) = self.local_entry(*item) else {
                        return;
                    };
                    current.push(entry);
                    let scope = current.clone();
                    let enclosing = locals.clone();
                    self.map.insert(
                        *item,
                        LocalDeclSite {
                            local_types: scope.clone(),
                            locals: enclosing.clone(),
                        },
                    );
                    self.item(*item, &scope, &enclosing);
                }
                StmtData::Block(inner) => {
                    // A nested block is a scope of its own: a declaration made
                    // inside it is not in scope after it, while every
                    // enclosing declaration is ([§6.3]).
                    let mut inner_scope = current.clone();
                    let mut inner_locals = locals.clone();
                    let inner = inner.clone();
                    self.stmts(&inner, &mut inner_scope, &mut inner_locals);
                }
                StmtData::DeclGroup(inner) => {
                    let inner = inner.clone();
                    self.stmts(&inner, current, locals);
                }
                StmtData::Decl { local, .. } => locals.push(*local),
                StmtData::Labeled { stmt, .. } => self.stmt(*stmt, current, locals),
                StmtData::If { then, els, .. } => {
                    self.stmt(*then, current, locals);
                    if let Some(els) = els {
                        self.stmt(*els, current, locals);
                    }
                }
                StmtData::While { body, .. }
                | StmtData::DoWhile { body, .. }
                | StmtData::Synchronized { body, .. } => self.stmt(*body, current, locals),
                // §14.14: a loop's own variable — the basic loop's declared
                // ones and the enhanced loop's — is scoped to the loop, so it
                // joins the scope only inside it.
                StmtData::ForEach { var, body, .. } => {
                    let mut inner_locals = locals.clone();
                    inner_locals.push(*var);
                    self.stmt(*body, current, &mut inner_locals);
                }
                StmtData::For { init, body, .. } => {
                    let init = init.clone();
                    let mut inner_locals = locals.clone();
                    self.stmts(&init, current, &mut inner_locals);
                    self.stmt(*body, current, &mut inner_locals);
                }
                StmtData::Switch { arms, .. } => {
                    // Every arm belongs to the switch *block*, so a
                    // declaration of one arm is in scope in the later ones.
                    let arms: Vec<Vec<StmtId>> = arms.iter().map(|arm| arm.body.clone()).collect();
                    for arm in &arms {
                        self.stmts(arm, current, locals);
                    }
                }
                StmtData::Try {
                    resources,
                    body,
                    catches,
                    finally,
                } => {
                    // §14.20.3: a resource is scoped to the try statement; a
                    // catch parameter to its own clause ([§14.20]).
                    let mut try_locals = locals.clone();
                    try_locals.extend(resources.iter().map(|resource| resource.local));
                    self.stmt(*body, current, &mut try_locals);
                    let catches: Vec<(LocalId, StmtId)> = catches
                        .iter()
                        .map(|catch| (catch.param, catch.body))
                        .collect();
                    for (param, catch) in catches {
                        let mut clause_locals = locals.clone();
                        clause_locals.push(param);
                        self.stmt(catch, current, &mut clause_locals);
                    }
                    if let Some(finally) = finally {
                        self.stmt(*finally, current, locals);
                    }
                }
                // Every other statement form contains no block statement
                // list, so it cannot declare a local class: a local
                // declaration is a *block statement*
                // ([§14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)).
                StmtData::Empty
                | StmtData::Expr(_)
                | StmtData::Return(_)
                | StmtData::Throw(_)
                | StmtData::Break(_)
                | StmtData::Continue(_)
                | StmtData::Yield(_)
                | StmtData::Assert { .. }
                | StmtData::Missing => {}
            }
        }
    }
    let mut walker = Walker {
        tree,
        bodies,
        file,
        map: FxHashMap::default(),
    };
    for &top in &tree.top {
        walker.item(top, &[], &[]);
    }
    walker.map
}

/// The *local* declaration a written reference name denotes, if any: a local
/// class-like declaration in scope
/// ([JLS §6.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.3),
/// [§14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// for a simple name, or a member type of one for a qualified name. The name
/// returned is the declaration's simple name, which is what a local type is
/// rendered as.
///
/// [§6.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4.1)
/// makes a local declaration shadow every other type of the same name in
/// scope — a type parameter and an imported or same-package class alike — so
/// every caller tries this *first*.
pub(crate) fn local_reference(
    db: &dyn TyDatabase,
    resolver: &Resolver,
    name: &Name,
) -> Option<(hir::SourceClass, Name)> {
    let text = name.as_str();
    match text.split_once('.') {
        Some((prefix, rest)) => {
            let prefix = resolver.local_type(&Name::new(prefix))?;
            let member = local_member_type(db, prefix.class, rest)?;
            Some((member, Name::new(simple_segment(rest))))
        }
        None => resolver
            .local_type(name)
            .map(|local| (local.class, local.name.clone())),
    }
}

/// The last `.`-separated segment of a written name — the simple name a type
/// reference is rendered as.
fn simple_segment(text: &str) -> &str {
    text.rsplit('.').next().unwrap_or(text)
}

/// The member type of the *local* declaration `class` named by the rest of a
/// written reference, resolved segment by segment through the item tree: a
/// member type of a declaration without a canonical name has none either
/// ([JLS §6.7]), so it cannot be probed through [`hir::fqn_resolve`] and is
/// identified by its declaration like its owner.
fn local_member_type(
    db: &dyn TyDatabase,
    class: hir::SourceClass,
    rest: &str,
) -> Option<hir::SourceClass> {
    let tree = hir::java_item_tree(db, class.file);
    let mut current = class.item;
    for segment in rest.split('.') {
        current = tree.data(current).body().iter().copied().find(|member| {
            class_like_name(tree.data(*member)).is_some_and(|name| name.as_str() == segment)
        })?;
    }
    Some(hir::SourceClass {
        file: class.file,
        item: current,
    })
}

/// The declaration a reference type denotes: its declaration for a *local*
/// class-like type ([JLS §14.3], [§6.7] — it has no canonical name to resolve),
/// otherwise the class [`hir::fqn_resolve`] finds for its canonical name
/// against `scope`'s classpath.
pub fn reference_class(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    ty: &Ty,
) -> Option<hir::Resolved> {
    let TyKind::Reference { name, local, .. } = ty.kind(db) else {
        return None;
    };
    match local {
        Some(class) => Some(hir::Resolved::Source(*class)),
        None => hir::fqn_resolve(db, scope, name.as_str()),
    }
}

/// Resolves a source [`TypeRef<Name>`] to a [`Ty`]. Reference names are
/// resolved per
/// [JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5)
/// against `scope`'s classpath; names that resolve to nothing degrade to the
/// most qualified candidate so the [`Ty`] stays displayable.
pub fn resolve_type_ref(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    tyref: &TypeRef<Name>,
) -> Ty {
    resolve_type_ref_impl(db, scope, resolver, tyref, &mut Vec::new())
}

/// Resolves the *member type* of a qualified class instance creation
/// (`primary.new Inner<...>(args)`) against the receiver expression's
/// compile-time type ([JLS §15.9], [§8.1.3]): the created class is the
/// member class `Inner` of the receiver's type — `a.new B()` where `a: A`
/// creates `A.B` — not a type named `Inner` in the lexical scope. The name is
/// therefore resolved *by the receiver type's canonical FQN*, never through
/// the file's imports or package.
///
/// Returns the inner type reference with its reference name rewritten to the
/// receiver-qualified FQN (`A.B`), ready for [`resolve_type_ref`]; `None`
/// when the receiver is not a class-like type or the written inner type is
/// not a bare reference, so the caller falls back to the lexical resolution.
pub fn qualify_member_type_of(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver_ty: &Ty,
    tyref: &TypeRef<Name>,
) -> Option<TypeRef<Name>> {
    let TypeRef::Reference { name, generic_args } = tyref else {
        return None;
    };
    // A member class of a parameterized type is looked up through the
    // *erasure*; the enclosing type arguments bind the member's own
    // (implicit) enclosing-instance type variables, which no separate
    // written argument here targets ([JLS §8.1.3], §15.9.3).
    let (receiver_name, _) = receiver_ty.erasure(db).as_reference(db)?;
    let member_fqn = join(receiver_name, name.as_str());
    hir::fqn_resolve(db, scope, member_fqn.as_str())?;
    Some(TypeRef::Reference {
        name: member_fqn,
        generic_args: generic_args.clone(),
    })
}

/// The recursion-guarded form of [`resolve_type_ref`]. `resolving` is the
/// stack of type parameters currently having their bounds resolved; a
/// re-entrant reference to one of them ([JLS §4.4] recursion such as
/// `T extends Comparable<T>`) yields the type variable without bounds so
/// interning terminates.
#[stacksafe]
fn resolve_type_ref_impl(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    tyref: &TypeRef<Name>,
    resolving: &mut Vec<Name>,
) -> Ty {
    match tyref {
        TypeRef::Primitive(p) => Ty::primitive(db, *p),
        TypeRef::Reference { name, generic_args } => {
            let args = generic_args
                .iter()
                .map(|arg| resolve_type_ref_impl(db, scope, resolver, arg, resolving))
                .collect();
            if let Some((class, simple)) = local_reference(db, resolver, name) {
                // §6.4.1: a local declaration shadows every other type of the
                // same name in scope, type parameters included.
                Ty::local_reference(db, class, simple, args)
            } else if let Some(tp) = resolver.type_param(name) {
                let var_scope = tp.scope.clone();
                // A type parameter in scope wins over any type named the same
                // ([JLS §6.4.1]): `Resolver::type_param` picks the innermost
                // declaration. Without this a
                // `<T extends Mapped & Copyable<T>>` method in a generic
                // `NbtEntryDecoder<T>` interface resolves its own `T` to the
                // interface's unbounded `T`, losing `copy`.
                let bounds = if resolving.iter().any(|n| n == name) {
                    Vec::new()
                } else {
                    resolving.push(name.clone());
                    let bounds = tp
                        .bounds
                        .iter()
                        .map(|bound| resolve_type_ref_impl(db, scope, resolver, bound, resolving))
                        .collect();
                    resolving.pop();
                    bounds
                };
                Ty::type_var(db, var_scope, bounds)
            } else {
                Ty::reference(db, resolve_reference_name(db, scope, resolver, name), args)
            }
        }
        TypeRef::Wildcard { bound } => Ty::wildcard(
            db,
            bound.as_deref().map(|b| match b {
                TypeBound::Upper(t) => Box::new(WildcardBound {
                    kind: BoundKind::Upper,
                    ty: resolve_type_ref_impl(db, scope, resolver, t, resolving),
                }),
                TypeBound::Lower(t) => Box::new(WildcardBound {
                    kind: BoundKind::Lower,
                    ty: resolve_type_ref_impl(db, scope, resolver, t, resolving),
                }),
            }),
        ),
        // A `TypeVariable` reference names a type parameter directly rather
        // than through a `Reference` node; resolve it against the same scope
        // list so it interns as the declaring parameter's variable. Its bounds
        // are omitted: this node shape is the recursion guard's and the
        // classfile lowering's, where re-entering the bound would not
        // terminate ([JLS §4.4]).
        TypeRef::TypeVariable(v) => match resolver.type_param(v) {
            Some(tp) => Ty::type_var(db, tp.scope.clone(), Vec::new()),
            None => Ty::unscoped_var(db, v.clone(), Vec::new()),
        },
        TypeRef::Array(inner) => Ty::array(
            db,
            resolve_type_ref_impl(db, scope, resolver, inner, resolving),
        ),
        TypeRef::Error => Ty::error(db),
    }
}

/// Resolves `name` to a canonical fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)).
///
/// The candidate order follows
/// [JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)
/// and [JLS §7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5):
/// single-type imports, the current package, `java.lang`, on-demand imports,
/// then the unnamed package. The first candidate that exists on the classpath
/// wins; if none does, the most qualified candidate is kept so the name
/// remains usable for display and later resolution.
fn resolve_reference_name(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> Name {
    let text = name.as_str();
    if let Some((prefix, rest)) = text.split_once('.') {
        // §6.5.5.2: a qualified name is tried as-is, then with each
        // simple-name resolution of its prefix. The on-demand accessibility
        // filter ([§7.5.2]) applies to the simple *prefix* step through
        // [`simple_candidates_with_kind`] below; the qualified probe itself
        // is shared with the checked path.
        let mut candidates = vec![name.clone()];
        for candidate in simple_candidates(resolver, prefix) {
            candidates.push(join(&candidate, rest));
        }
        for candidate in &candidates {
            if let Some(canonical) = canonical_type_name(db, scope, candidate) {
                return canonical;
            }
        }
    } else {
        // §6.5.5.1: a simple name walks the candidate steps in order; an
        // on-demand import ([§7.5.2]) contributes only *accessible* types,
        // so an inaccessible candidate does not shadow the later steps.
        for (step, candidate) in simple_candidates_with_kind(resolver, text) {
            if step == CandidateStep::OnDemand
                && !on_demand_candidate_accessible(db, scope, resolver, &candidate)
            {
                continue;
            }
            if let Some(canonical) = canonical_type_name(db, scope, &candidate) {
                return canonical;
            }
        }
    }
    // §6.5.5.1: a member type *inherited* by an enclosing declaration is in
    // scope by simple name too — `Sub` may name `Super.Nested`. Walk each
    // enclosing type's supertype chain and offer `{super}.{simple}`
    // candidates (and `{super-prefix}.{rest}` for qualified names).
    for candidate in inherited_member_candidates(db, scope, resolver, text) {
        if let Some(canonical) = canonical_type_name(db, scope, &candidate) {
            return canonical;
        }
    }
    // Nothing resolved (pre-workspace silence): degrade to the most
    // qualified *non-member* candidate — an enclosing-member prefix would
    // invent a nested type that was never declared.
    let candidates = candidate_fqns(resolver, name);
    let degraded = candidates.iter().find(|candidate| {
        !resolver
            .enclosing()
            .iter()
            .any(|enclosing| candidate.as_str().starts_with(&format!("{enclosing}.")))
    });
    degraded
        .or_else(|| candidates.first())
        .cloned()
        .unwrap_or_else(|| name.clone())
}

/// The *canonical* name of the type `candidate` resolves to, or `None` when
/// it does not resolve. A library class is keyed by its binary name whose
/// nested segments join with `$` ([JVMS §4.2]) — returning that spelling
/// makes a type reached through a single-type import (`picocli.CommandLine
/// .ParseResult`) identical to the same class substituted from a library
/// signature (`picocli.CommandLine$ParseResult`). Source-side declarations
/// keep their dotted spelling.
fn canonical_type_name(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidate: &Name,
) -> Option<Name> {
    match hir::fqn_resolve(db, scope, candidate.as_str())? {
        hir::Resolved::Library(class) => {
            let interner = &db.hir_state().interner;
            Some(Name::new(interner.resolve(&class.entry.fqn)))
        }
        hir::Resolved::Source(_) => Some(candidate.clone()),
    }
}

/// The candidates for a type name that a member type *inherited* by an
/// enclosing declaration puts in scope
/// ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)):
/// each enclosing type's transitive supertype chain joined with the simple
/// name — or, for a qualified name, with its remainder after the prefix.
fn inherited_member_candidates(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    text: &str,
) -> Vec<Name> {
    let mut out = Vec::new();
    let mut seen: FxHashMap<Name, ()> = FxHashMap::default();
    for enclosing in resolver.enclosing() {
        let mut queue = vec![Ty::reference(db, enclosing.clone(), Vec::new())];
        while let Some(ty) = queue.pop() {
            for parent in crate::java::subtyping::supertypes_impl(db, scope, &ty) {
                if parent.is_error(db) {
                    continue;
                }
                let TyKind::Reference {
                    name: super_name, ..
                } = parent.kind(db)
                else {
                    continue;
                };
                if seen.insert(super_name.clone(), ()).is_some() {
                    continue;
                }
                queue.push(parent);
                let suffix = match text.split_once('.') {
                    Some((prefix, rest)) => {
                        // The inherited chain qualifies the *prefix*; keep any
                        // deeper nesting after it (`Mode.CLOSE` against a
                        // superclass that inherits `Mode` yields
                        // `Super.Mode.CLOSE`).
                        match super_name.as_str().ends_with(prefix)
                            && super_name.as_str().len() > prefix.len()
                        {
                            true => continue,
                            false => rest,
                        }
                    }
                    None => text,
                };
                out.push(join(&super_name.clone(), suffix));
            }
        }
    }
    out
}

/// The candidate FQNs for a (possibly qualified) type name, most specific
/// first. A qualified name is tried as-is first (it may already be fully
/// qualified), then with each simple-name resolution of its prefix
/// ([JLS §6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)).
pub(crate) fn candidate_fqns(resolver: &Resolver, name: &Name) -> Vec<Name> {
    let text = name.as_str();
    if let Some((prefix, rest)) = text.split_once('.') {
        let mut out = vec![name.clone()];
        for candidate in simple_candidates(resolver, prefix) {
            out.push(join(&candidate, rest));
        }
        out
    } else {
        simple_candidates(resolver, text)
    }
}

/// The candidate FQNs for a simple type name, in
/// [JLS §7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5)
/// precedence order.
fn simple_candidates(resolver: &Resolver, simple: &str) -> Vec<Name> {
    simple_candidates_with_kind(resolver, simple)
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

/// The step ([JLS §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1),
/// [§7.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5))
/// a simple-name candidate belongs to. The step drives the *checked*
/// resolution ([`resolve_name_checked`]): whether a name may fall through to
/// a later step, and whether two on-demand imports make it ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateStep {
    /// A member type of an enclosing type declaration
    /// ([§6.5.5.1], [§8.1]): the innermost declaration wins.
    EnclosingMember,
    /// A nested type named by a *single-static* import
    /// ([§7.5.4]): `import static p.Outer.Nested` puts the simple name
    /// `Nested` in scope as a type wherever it is used.
    StaticImportType,
    /// A single-type import whose simple name matches ([§7.5.1]).
    SingleImport,
    /// A type in the current package ([§7.4.2]).
    CurrentPackage,
    /// A type in `java.lang` (implicitly imported, [§7.3]).
    JavaLang,
    /// A type reachable through an on-demand import ([§7.5.2]).
    OnDemand,
    /// A type in the unnamed package ([§7.4.2]).
    UnnamedPackage,
}

fn simple_candidates_with_kind(resolver: &Resolver, simple: &str) -> Vec<(CandidateStep, Name)> {
    let mut out = Vec::new();

    // 0. a member type of an enclosing class-like declaration (§6.5.5.1):
    // innermost first; these shadow single-type imports ([§6.4.1]).
    for enclosing in &resolver.enclosing {
        out.push((CandidateStep::EnclosingMember, join(enclosing, simple)));
    }

    // 0.5. a nested type imported by a *single-static* import ([§7.5.4]):
    // `import static p.Outer.Nested` makes the type usable by simple name.
    for import in resolver.imports.iter().filter(|import| {
        import.is_static
            && !import.is_asterisk
            && import.name.as_str().rsplit('.').next() == Some(simple)
    }) {
        out.push((CandidateStep::StaticImportType, import.name.clone()));
    }

    // 1. a single-type import whose simple name matches (§7.5.1)
    if let Some(import) = resolver.imports.iter().find(|import| {
        !import.is_static
            && !import.is_asterisk
            && import.name.as_str().rsplit('.').next() == Some(simple)
    }) {
        out.push((CandidateStep::SingleImport, import.name.clone()));
    }

    // 2. a type in the current package (§7.4.2)
    if let Some(package) = &resolver.package {
        out.push((CandidateStep::CurrentPackage, join(package, simple)));
    }

    // 3. a type in `java.lang`
    out.push((
        CandidateStep::JavaLang,
        Name::new(&format!("java.lang.{simple}")),
    ));

    // 4. a type reachable through an on-demand import (§7.5.2)
    for import in resolver
        .imports
        .iter()
        .filter(|import| !import.is_static && import.is_asterisk)
    {
        out.push((CandidateStep::OnDemand, join(&import.name, simple)));
    }

    // 5. a type in the unnamed package
    out.push((CandidateStep::UnnamedPackage, Name::new(simple)));
    out
}

/// §7.5.2: whether the type `fqn` — reached through a *type-import-on-demand*
/// (`import pkg.*;`) — is imported by that import from the current compilation
/// unit. An on-demand import imports only *accessible* types
/// ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// a top-level class of another package is imported only when it is `public`
/// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1),
/// `ACC_PUBLIC = 0x0001`). A package-private class of the *current* package
/// would be reached at the current-package step instead ([§7.4.2]), so it is
/// not a candidate of the on-demand step either. Without this filter, two
/// on-demand imports of different packages — `org.objectweb.asm.*` and
/// `org.objectweb.asm.tree.analysis.*` — would both "provide" the simple name
/// `Frame` when the former's `Frame` is package-private, and javac resolves
/// to the only accessible one instead of reporting an ambiguity.
fn on_demand_candidate_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    fqn: &Name,
) -> bool {
    let same_package = match resolver.package() {
        Some(pkg) => match fqn.as_str().rsplit_once('.') {
            Some((p, _)) => p == pkg.as_str(),
            None => true,
        },
        // The unnamed package: only a same-unnamed-package class is
        // accessible by package rule, and it is a current-package candidate,
        // not an on-demand one.
        None => fqn.as_str().find('.').is_none(),
    };
    if same_package {
        return true;
    }
    match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Library(class)) => class.entry.flags & 0x0001 != 0,
        Some(hir::Resolved::Source(source)) => {
            let tree = hir::java_item_tree(db, source.file);
            let Some(data) = crate::java::resolve::item_data(&tree, source.item) else {
                return false;
            };
            match data {
                hir_def::java::item_tree::ItemData::Class(d) => d.modifiers.is_public(),
                hir_def::java::item_tree::ItemData::Interface(d) => d.modifiers.is_public(),
                hir_def::java::item_tree::ItemData::Enum(d) => d.modifiers.is_public(),
                hir_def::java::item_tree::ItemData::Record(d) => d.modifiers.is_public(),
                hir_def::java::item_tree::ItemData::Annotation(d) => d.modifiers.is_public(),
                _ => false,
            }
        }
        None => false,
    }
}

/// The outcome of a *checked* name resolution ([JLS §6.5.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5))
/// — the primitive the unknown-type diagnostics are built from. Unlike
/// [`resolve_reference_name`], which degrades an unresolvable name to its
/// most-qualified candidate so the [`Ty`] stays displayable, this reports
/// *whether* the name resolved, and the exact divergences from the JLS rules
/// ([§6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1),
/// [§7.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.2)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolution {
    /// The name denotes a type variable of the enclosing scope
    /// ([§6.5.5.1] step 1).
    TypeVar,
    /// Resolved to this canonical fully qualified name ([§6.7]).
    Resolved(Name),
    /// The name denotes a *local* class-like declaration ([JLS §14.3]) or a
    /// member type of one: a declaration with no canonical name ([§6.7]),
    /// identified by its declaration instead.
    ResolvedLocal(hir::SourceClass),
    /// The simple name is accessible through two or more on-demand imports
    /// that denote different types — a compile-time error ([§6.5.5.1],
    /// [§7.5.2]).
    Ambiguous(Vec<Name>),
    /// A candidate class exists on the classpath, but its package is not
    /// visible from the resolving source set's module
    /// ([§7.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.3),
    /// [§7.7.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7.2)) —
    /// the package is *observable* but not *visible*.
    NotAccessible(Name),
    /// No candidate resolves — either the name is shadowed by a broken
    /// single-type import whose imported type does not exist ([§7.5.1],
    /// in which case the name cannot fall through), or nothing on the
    /// classpath provides it.
    Unresolved,
}

/// Whether `fqn`'s package is visible from `module_ctx` ([§7.4.3], [§7.7.2]).
/// Types in the unnamed package are never module-hidden.
fn fqn_visible(db: &dyn TyDatabase, module_ctx: &hir::ModuleCtx, fqn: &str) -> bool {
    match fqn.rsplit_once('.') {
        Some((package, _)) => module_ctx.package_visible(&db.hir_state().interner, package),
        None => true,
    }
}

/// The canonical class a *written* reference name denotes at the position of
/// `node` in `file` ([JLS
/// §6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1)):
/// `name` resolved in the context of the innermost declaration containing
/// `node` — that declaration's type parameters, the member types of the
/// classes enclosing it, the compilation unit's imports and its package — or in
/// the compilation unit's own context when no declaration encloses it (the
/// annotations of a package declaration).
///
/// The resolver is the one the declaration itself would use, so a written name
/// is answered by the *symbol* it denotes: a class of the same spelling
/// declared in the file's package, imported, or nested in an enclosing class
/// is that class, not the platform type the spelling resembles.
pub fn resolve_written_name(
    db: &dyn TyDatabase,
    file: FileId,
    node: &SyntaxNode<Lang>,
    name: &Name,
) -> NameResolution {
    let tree = hir::java_item_tree(db, file);
    let resolver = resolver_at(db, file, &tree, node);
    resolve_name_checked(db, &scope_for_file(db, file), &resolver, name)
}

/// The checked resolution of the type name `name` written inside `item` of
/// `file` ([JLS §6.5.5.1]), using the resolver the item itself would use — or
/// the compilation unit's own resolver when the reference is not inside a
/// body-carrying item ([`Resolver::for_file`]) — and the file's scope (which
/// for a loaded library source file is its own library).
pub fn resolve_type_name_at(
    db: &dyn TyDatabase,
    file: FileId,
    item: Option<ItemId>,
    name: &Name,
) -> NameResolution {
    let tree = hir::java_item_tree(db, file);
    let resolver = match item {
        Some(item) => Resolver::for_item(db, file, &tree, item),
        None => Resolver::for_file(&tree),
    };
    resolve_name_checked(db, &scope_for_file(db, file), &resolver, name)
}

/// The declaration of a type parameter: the item that lists it and where its
/// name is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeParamDeclaration {
    /// The file declaring the parameter.
    pub file: FileId,
    /// The source range of the parameter's name in its declaration.
    pub range: TextRange,
}

/// The declaration of the type parameter `name` in scope at `item`
/// ([JLS §4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4),
/// [§8.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.2),
/// [§8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4)):
/// the parameter itself, never a class of the same spelling ([§6.5.5.1]), and
/// a method's own parameter over an enclosing class's ([§6.4.1]).
///
/// `None` when no *source* parameter of that name is in scope: `name` names no
/// type parameter at all, or the variable belongs to a classfile signature or
/// is a capture ([`TypeVarScope::LibraryClass`], [`TypeVarScope::LibraryMethod`],
/// [`TypeVarScope::Capture`], [`TypeVarScope::Unnamed`]) — none of which has a
/// declaration in the workspace.
pub fn type_param_declaration(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
    name: &Name,
) -> Option<TypeParamDeclaration> {
    let tree = hir::java_item_tree(db, file);
    let param = Resolver::for_item(db, file, &tree, item)
        .type_param(name)?
        .clone();
    // The scope is the parameter's identity ([`ScopedTypeParam`]): a source
    // class or method scope carries the item that declared it.
    let (decl_file, decl_item) = match param.scope {
        TypeVarScope::Class { file, item, .. } | TypeVarScope::Method { file, item, .. } => {
            (file, item)
        }
        TypeVarScope::LibraryClass { .. }
        | TypeVarScope::LibraryMethod { .. }
        | TypeVarScope::Capture { .. }
        | TypeVarScope::Unnamed { .. } => return None,
    };
    let decl_tree = hir::java_item_tree(db, decl_file);
    let (map, source) = range_ctx(db, decl_file, decl_tree.language)?;
    // The parameter's own name token, at the index it occupies in the
    // declaring item's list — the *last* match, as the scope list is read.
    let index = declared_type_params(&decl_tree, decl_item)
        .iter()
        .rposition(|candidate| candidate.name == param.name)?;
    let range = ranges::type_param_name_range(map, &source, &decl_tree, decl_item, index)?;
    Some(TypeParamDeclaration {
        file: decl_file,
        range,
    })
}

/// The type parameters the item declares itself ([JLS §4.4], [§8.1.2],
/// [§8.4.4]) — the list `type_params_map` scopes, in declaration order.
fn declared_type_params(tree: &ItemTree, item: ItemId) -> &[TypeParam] {
    match tree.data(item) {
        ItemData::Class(data) | ItemData::Interface(data) => &data.type_params,
        ItemData::Record(data) => &data.type_params,
        ItemData::Method(data) => &data.sig.type_params,
        _ => &[],
    }
}

/// The resolver in force at the declaration owning `node`: the innermost item
/// whose source range contains it, or the compilation unit's own context
/// ([`Resolver::for_file`]) when no item does.
fn resolver_at(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
    node: &SyntaxNode<Lang>,
) -> Resolver {
    let target = node
        .parent()
        .map_or_else(|| node.text_range(), |parent| parent.text_range());
    let Some(item) = range_ctx(db, file, tree.language)
        .and_then(|(map, source)| innermost_item(map, &source, tree, target))
    else {
        return Resolver::for_file(tree);
    };
    Resolver::for_item(db, file, tree, item)
}

/// The innermost item whose declaration range contains `target` — the
/// declaration an annotation or element value written at `target` belongs to
/// ([`crate::java::annotation_value::suppress_warnings_values`]).
pub(crate) fn innermost_item(
    map: &AstIdMap,
    source: &SourceFile,
    tree: &ItemTree,
    target: TextRange,
) -> Option<ItemId> {
    fn walk(
        map: &AstIdMap,
        source: &SourceFile,
        tree: &ItemTree,
        id: ItemId,
        target: TextRange,
        best: &mut Option<(TextRange, ItemId)>,
    ) {
        if let Some(range) = ranges::item_range(map, source, tree, id)
            && range.contains_range(target)
            && best.is_none_or(|(best_range, _)| range.len() < best_range.len())
        {
            *best = Some((range, id));
        }
        for &child in tree.data(id).body() {
            walk(map, source, tree, child, target, best);
        }
    }
    let mut best = None;
    for &top in &tree.top {
        walk(map, source, tree, top, target, &mut best);
    }
    best.map(|(_, id)| id)
}

/// The checked resolution of `name` against `scope`'s classpath
/// ([JLS §6.5.5.1], [§6.5.5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.2)).
///
/// A name in expression position that is *not* a type — a local or field of
/// the implicit receiver — must not be reported here; callers only pass names
/// from type-reference positions. A name shadowed by a single-type import
/// that itself names a non-existent class (§7.5.1 makes the import a
/// compile-time error) resolves to nothing rather than falling through to a
/// same-package class.
pub fn resolve_name_checked(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    name: &Name,
) -> NameResolution {
    let text = name.as_str();
    // 0. a *local* class-like declaration in scope wins over every other type
    // of the same name — a type parameter (§6.4.1) and any class the steps
    // below would find — and a qualified name whose prefix is one denotes a
    // member type of it (§6.5.5.2).
    if let Some((class, _)) = local_reference(db, resolver, name) {
        return NameResolution::ResolvedLocal(class);
    }

    // 1. a type parameter in scope wins over any type named the same (§6.5.5.1).
    if resolver.type_param(name).is_some() {
        return NameResolution::TypeVar;
    }

    // The resolving source set's module context gates each candidate's package
    // visibility ([§7.4.3], [§7.7.2]); a candidate whose package is not
    // visible is not a resolution, but a class that *exists* invisibly is
    // worth distinguishing from "nothing on the classpath" for diagnostics.
    let module_ctx = hir::module_ctx_for_scope(db, scope);

    // §6.5.5.2: a qualified name — tried as-is first, then with each
    // simple-name resolution of its prefix. (On-demand ambiguity only applies
    // to the *simple-name* step and is reported at the prefix's own use.)
    if let Some((prefix, rest)) = text.split_once('.') {
        let mut candidates = vec![name.clone()];
        for candidate in simple_candidates(resolver, prefix) {
            candidates.push(join(&candidate, rest));
        }
        let mut hidden: Option<Name> = None;
        for candidate in &candidates {
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_none() {
                continue;
            }
            if fqn_visible(db, &module_ctx, candidate.as_str()) {
                return NameResolution::Resolved(candidate.clone());
            }
            hidden.get_or_insert_with(|| candidate.clone());
        }
        // §6.5.5.1: the prefix may name a member type *inherited* by an
        // enclosing declaration.
        for prefix_fqn in inherited_member_candidates(db, scope, resolver, prefix) {
            let candidate = join(&prefix_fqn, rest);
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
                if fqn_visible(db, &module_ctx, candidate.as_str()) {
                    return NameResolution::Resolved(candidate);
                }
                hidden.get_or_insert(candidate);
            }
        }
        return match hidden {
            Some(fqn) => NameResolution::NotAccessible(fqn),
            None => NameResolution::Unresolved,
        };
    }

    let candidates = simple_candidates_with_kind(resolver, text);
    let mut hidden: Option<Name> = None;
    for (idx, (step, candidate)) in candidates.iter().enumerate() {
        // §6.5.5.1: a member type of an enclosing declaration shadows
        // everything below; the innermost enclosing class wins.
        if *step == CandidateStep::EnclosingMember {
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
                return NameResolution::Resolved(candidate.clone());
            }
            continue;
        }
        // §7.5.4: a single-static import names a member of a *type*:
        // `import static p.Type.Member` puts `Member` in scope only when `p.Type`
        // denotes a type. The prefix of `import static org.objectweb.asm.ClassWriter;`
        // is the package `org.objectweb.asm`, so the import is invalid and
        // `ClassWriter` must stay unresolved at every use. javac:
        // `compiler.err.doesnt.exist` / `.static.imp.only.classes.and.interfaces`.
        if *step == CandidateStep::StaticImportType {
            let Some((owner, _)) = candidate.as_str().rsplit_once('.') else {
                continue;
            };
            // The owner is a strictly shorter written name than the candidate,
            // so this recursion is well founded.
            if !matches!(
                resolve_name_checked(db, scope, resolver, &Name::new(owner)),
                NameResolution::Resolved(_)
            ) {
                continue;
            }
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
                return NameResolution::Resolved(candidate.clone());
            }
            continue;
        }
        // §7.5.1: a single-type import *shadows* the simple name; if it names
        // a class that cannot be found the import is an error and the name
        // does not fall through to a later step.
        if *step == CandidateStep::SingleImport {
            if hir::fqn_resolve(db, scope, candidate.as_str()).is_none() {
                return NameResolution::Unresolved;
            }
            return if fqn_visible(db, &module_ctx, candidate.as_str()) {
                NameResolution::Resolved(candidate.clone())
            } else {
                NameResolution::NotAccessible(candidate.clone())
            };
        }
        if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
            if !fqn_visible(db, &module_ctx, candidate.as_str()) {
                hidden.get_or_insert_with(|| candidate.clone());
                continue;
            }
            if *step == CandidateStep::OnDemand {
                // §7.5.2: an on-demand import imports only *accessible* types
                // ([§6.6]) — a package-private class of another package is
                // not a candidate, and the name falls through to later steps.
                if !on_demand_candidate_accessible(db, scope, resolver, candidate) {
                    continue;
                }
                // §6.5.5.1/[§7.5.2]: two or more on-demand imports that supply
                // the simple name from different types make the name
                // ambiguous — a compile-time error. Only *accessible* types
                // participate: an inaccessible one is not imported.
                let mut conflicting = Vec::new();
                for (later_step, later) in &candidates[idx + 1..] {
                    if *later_step == CandidateStep::OnDemand
                        && hir::fqn_resolve(db, scope, later.as_str()).is_some()
                        && fqn_visible(db, &module_ctx, later.as_str())
                        && on_demand_candidate_accessible(db, scope, resolver, later)
                        && later != candidate
                    {
                        conflicting.push(later.clone());
                    }
                }
                if !conflicting.is_empty() {
                    conflicting.insert(0, candidate.clone());
                    return NameResolution::Ambiguous(conflicting);
                }
            }
            return NameResolution::Resolved(candidate.clone());
        }
    }
    // §6.5.5.1: a member type *inherited* by an enclosing declaration is in
    // scope by simple name too — tried after every declared candidate.
    for candidate in inherited_member_candidates(db, scope, resolver, text) {
        if hir::fqn_resolve(db, scope, candidate.as_str()).is_some() {
            if fqn_visible(db, &module_ctx, candidate.as_str()) {
                return NameResolution::Resolved(candidate);
            }
            hidden.get_or_insert(candidate);
        }
    }
    match hidden {
        Some(fqn) => NameResolution::NotAccessible(fqn),
        None => NameResolution::Unresolved,
    }
}

fn join(prefix: &Name, suffix: &str) -> Name {
    let mut text = String::with_capacity(prefix.as_str().len() + 1 + suffix.len());
    text.push_str(prefix.as_str());
    text.push('.');
    text.push_str(suffix);
    Name::new(&text)
}

/// The substitution instantiating the type parameters of a *source* class
/// declaration ([JLS §4.10.2]): each declared parameter, keyed by the scope it
/// declares ([§4.4], [§6.3]), bound to the receiver's argument at the same
/// index. A parameter list longer than the argument list leaves the extra
/// parameters unbound (a raw or partially-applied use).
pub fn source_class_binding(
    file: FileId,
    item: ItemId,
    declared: &[TypeParam],
    args: &[Ty],
) -> FxHashMap<TypeVarScope, Ty> {
    declared
        .iter()
        .zip(args.iter().copied())
        .map(|(tp, arg)| {
            (
                TypeVarScope::Class {
                    file,
                    item,
                    name: tp.name.clone(),
                },
                arg,
            )
        })
        .collect()
}

/// The substitution instantiating the type parameters of a *classfile* class
/// declaration ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1)):
/// each declared parameter, keyed by its declaring class's binary name
/// ([JLS §4.4], [§6.3]), bound to the receiver's argument at the same index.
pub fn library_class_binding(
    owner: &Name,
    params: &[Name],
    args: &[Ty],
) -> FxHashMap<TypeVarScope, Ty> {
    params
        .iter()
        .zip(args.iter().copied())
        .map(|(name, arg)| {
            (
                TypeVarScope::LibraryClass {
                    owner: owner.clone(),
                    name: name.clone(),
                },
                arg,
            )
        })
        .collect()
}

/// The canonical fully qualified name of a resolved class
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)),
/// or `None` for a source class (whose name is not a binary one — bindings
/// against it are keyed by its declaring item, not by name).
fn resolved_fqn(db: &dyn TyDatabase, resolved: &hir::Resolved) -> Option<Name> {
    match resolved {
        hir::Resolved::Library(class) => {
            let interner = &db.hir_state().interner;
            Some(Name::new(interner.resolve(&class.entry.fqn)))
        }
        hir::Resolved::Source(_) => None,
    }
}

/// JLS §4.5: the number of type arguments a *parameterized type* must carry
/// for the class named by `fqn`, or `None` when the name does not resolve to a
/// class whose parameter list is recoverable.
///
/// A raw use (no arguments at all) is legal for any generic class
/// ([§4.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.8)),
/// so the caller compares only when arguments *are* written: zero arguments
/// against zero parameters is the non-generic case and is legal too, while
/// any other mismatch is an error (`wrong number of type arguments; required
/// {n}`, or `type {C} does not take parameters` for the zero case).
pub fn type_argument_arity(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Option<usize> {
    let resolved = hir::fqn_resolve(db, scope, fqn.as_str())?;
    match &resolved {
        hir::Resolved::Library(_) => hir::class_generic_info(db, &resolved)
            .map(|info| info.type_params.len())
            // A classfile without a `Signature` attribute declares none.
            .or(Some(0)),
        hir::Resolved::Source(source) => {
            let tree = hir::java_item_tree(db, source.file);
            match tree.data(source.item) {
                ItemData::Class(d) | ItemData::Interface(d) => Some(d.type_params.len()),
                ItemData::Record(d) => Some(d.type_params.len()),
                // Enums and annotations cannot declare type parameters
                // ([§8.9], [§9.6]).
                ItemData::Enum(_) | ItemData::Annotation(_) => Some(0),
                _ => None,
            }
        }
    }
}

/// JLS §4.5: the first reference in `tyref` that is written with the wrong
/// number of type arguments, as `(reference, expected)`, or `None` when the
/// whole written type is well-formed.
///
/// The comparison is on the *written* argument count against the number the
/// named class declares. That distinction matters for a qualified member type
/// (`TreeTypeAdapter.GsonContextImpl`, a non-generic inner class of a generic
/// outer): the resolved [`Ty`] carries the outer's argument while the source
/// writes none, and the member class itself declares none, so the use is legal.
///
/// A *raw* use (no arguments at all, [§4.8]) is legal for any generic class,
/// so a reference without arguments is skipped — but a class declaring none
/// cannot take them, which is why the empty case is still checked. The walk
/// covers nested arguments (`List<Map<String>>` is wrong at the argument even
/// though the outer `List` is fine) and wildcard bounds ([§4.5.1]), each of
/// which is a type reference in its own right.
pub fn type_argument_arity_mismatch(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    tyref: &TypeRef<Name>,
) -> Option<(Ty, usize)> {
    match tyref {
        TypeRef::Reference { name, generic_args } => {
            // A *qualified member type* names its type arguments on the
            // *qualifier*: in `Outer<T>.Inner`, `T` instantiates `Outer`, and
            // `Inner` takes none of its own ([§6.5.5.2], [§4.5]). The lowered
            // reference keeps the qualifier's arguments on the whole name, so
            // the pair cannot be told apart from the reference alone — skip it
            // whenever the name's qualifier is a *type* (`Outer.Inner`, where
            // the qualifier resolves to a class) rather than a package
            // (`java.util.List`, where the arguments are `List`'s own).
            if let Some((prefix, _)) = name.as_str().rsplit_once('.') {
                let qualifier = resolve_reference_name(db, scope, resolver, &Name::new(prefix));
                if hir::fqn_resolve(db, scope, qualifier.as_str()).is_some() {
                    return None;
                }
            }
            let resolved = resolve_type_ref(db, scope, resolver, tyref);
            if let TyKind::Reference { name: fqn, .. } = resolved.kind(db)
                && let Some(expected) = type_argument_arity(db, scope, fqn)
                // §4.8: writing no arguments is the *raw* use, legal for any
                // generic class; every other count must match the declaration
                // exactly (including 0 — a non-generic class takes none).
                && expected != generic_args.len()
                && !(generic_args.is_empty() && expected != 0)
            {
                return Some((resolved, expected));
            }
            for arg in generic_args {
                if let Some(found) = type_argument_arity_mismatch(db, scope, resolver, arg) {
                    return Some(found);
                }
            }
            None
        }
        // §4.5.1: a wildcard's bound is a type reference too.
        TypeRef::Wildcard { bound } => match bound.as_deref() {
            Some(TypeBound::Upper(inner)) | Some(TypeBound::Lower(inner)) => {
                type_argument_arity_mismatch(db, scope, resolver, inner)
            }
            None => None,
        },
        TypeRef::Array(inner) => type_argument_arity_mismatch(db, scope, resolver, inner),
        _ => None,
    }
}

/// The resolution scope of a source file: its source set, its own library when
/// it is a loaded library source file, or the JDK built-ins when the file is
/// not mapped to any root.
pub fn scope_for_file(db: &dyn TyDatabase, file_id: FileId) -> hir::ResolutionScope {
    if let Some(source_set) = hir::source_set_for_file(db, file_id) {
        return hir::ResolutionScope::SourceSet(source_set);
    }
    // A loaded library source file resolves against its own library's classes,
    // then the platform built-ins. `Classpath` scope does not imply the latter,
    // hence the explicit extension.
    if let Some(library) = hir::library_source_for_file(db, file_id) {
        let mut libraries = vec![library];
        libraries.extend(hir::jdk_builtin_libraries(db));
        return hir::ResolutionScope::Classpath(libraries);
    }
    hir::ResolutionScope::JdkBuiltins
}

/// Whether `fqn` names a *generic class* — one declaring type parameters
/// ([JLS §8.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.2)).
/// A reference to it without type arguments is a raw type ([§4.8], [§4.12.2]).
pub(crate) fn class_is_generic(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> bool {
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return false;
    };
    match resolved {
        // A library class is generic when its classfile `Signature` attribute
        // ([JVMS §4.7.9.1]) declares type parameters.
        hir::Resolved::Library(_) => {
            hir::class_generic_info(db, &resolved).is_some_and(|info| !info.type_params.is_empty())
        }
        // A source class is generic when its declaration carries them.
        hir::Resolved::Source(source) => {
            let tree = hir::java_item_tree(db, source.file);
            let type_params = match tree.data(source.item) {
                ItemData::Class(d) | ItemData::Interface(d) => Some(&d.type_params),
                ItemData::Record(d) => Some(&d.type_params),
                _ => None,
            };
            type_params.is_some_and(|params| !params.is_empty())
        }
    }
}

/// §4.5.1: whether the type argument `arg` satisfies the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// of the type parameter it fills in the class named by `class_fqn`.
///
/// Returns the first violated bound as `(type parameter, argument, bound)`,
/// or `None` when the arguments are within bounds. Conservative by
/// construction: when the class's type parameters or a bound cannot be
/// recovered (a partial classpath, a raw use, an arity mismatch), or a bound
/// or argument carries an inference variable ([`TyKind::InferenceVar`], which
/// the subtype machinery cannot decide mid-inference), no violation is
/// reported. Bound references to the class's own type parameters are
/// substituted with the actual arguments first, so `class M<T extends Number>`
/// used as `M<String>` checks `String <: Number` and reports the pair.
pub fn type_argument_bound_violation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    class_fqn: &Name,
    args: &[Ty],
) -> Option<(Name, Ty, Ty)> {
    let resolved = hir::fqn_resolve(db, scope, class_fqn.as_str())?;
    // The declared type parameters as (declaring scope, resolved bounds).
    let type_params = class_param_bounds(db, &resolved, class_fqn)?;
    if type_params.len() != args.len() {
        return None;
    }
    let binding: FxHashMap<TypeVarScope, Ty> = type_params
        .iter()
        .map(|(var_scope, _)| var_scope.clone())
        .zip(args.iter().copied())
        .collect();
    for (var_scope, bounds) in type_params {
        let Some(arg) = binding.get(&var_scope).copied() else {
            continue;
        };
        // §4.5.1: a wildcard is not a concrete type argument — its own bounds
        // are checked against the type parameter, not the subtype of a bound.
        // The subtype machinery cannot decide a wildcard against a bound
        // without capture conversion, so skip it (conservative).
        if arg.is_wildcard(db) {
            continue;
        }
        for bound in bounds {
            let bound = bound.substitute(db, &binding);
            if arg.is_error(db) || bound.is_error(db) {
                continue;
            }
            if arg.contains_infer_var(db) || bound.contains_infer_var(db) {
                continue;
            }
            if !crate::java::subtyping::is_subtype(db, scope, &arg, &bound) {
                return Some((var_scope.name().clone(), arg, bound));
            }
        }
    }
    None
}

/// The declared type parameters of `resolved` as
/// `(declaring scope, resolved bounds)` in declaration order, or `None` when
/// the declaration's parameter list cannot be recovered. `class_fqn` is the
/// name the class was resolved under, used for the classfile identity when the
/// stub carries no canonical name.
fn class_param_bounds(
    db: &dyn TyDatabase,
    resolved: &hir::Resolved,
    class_fqn: &Name,
) -> Option<Vec<(TypeVarScope, Vec<Ty>)>> {
    match resolved {
        hir::Resolved::Library(_) => {
            let info = hir::class_generic_info(db, resolved)?;
            let interner = &db.hir_state().interner;
            let owner = resolved_fqn(db, resolved).unwrap_or_else(|| class_fqn.clone());
            let names: Vec<Name> = info
                .type_params
                .iter()
                .map(|tp| Name::new(interner.resolve(&tp.name)))
                .collect();
            let ctx = LibrarySignature::class(&owner);
            Some(
                info.type_params
                    .iter()
                    .zip(names.iter())
                    .map(|(tp, name)| {
                        (
                            TypeVarScope::LibraryClass {
                                owner: owner.clone(),
                                name: name.clone(),
                            },
                            tp.bounds
                                .iter()
                                .map(|b| ty_from_library_signature(db, b, &ctx))
                                .collect(),
                        )
                    })
                    .collect(),
            )
        }
        hir::Resolved::Source(source) => {
            let tree = hir::java_item_tree(db, source.file);
            let params = match tree.data(source.item) {
                ItemData::Class(d) | ItemData::Interface(d) => Some(&d.type_params),
                ItemData::Record(d) => Some(&d.type_params),
                _ => None,
            }?;
            let file_scope = scope_for_file(db, source.file);
            let resolver = Resolver::for_item(db, source.file, &tree, source.item);
            Some(
                params
                    .iter()
                    .map(|tp| {
                        (
                            TypeVarScope::Class {
                                file: source.file,
                                item: source.item,
                                name: tp.name.clone(),
                            },
                            tp.bounds
                                .iter()
                                .map(|b| resolve_type_ref(db, &file_scope, &resolver, b))
                                .collect(),
                        )
                    })
                    .collect(),
            )
        }
    }
}

/// The declared type parameters of the class named by `fqn` as
/// `(name, [bound Ty])`, in declaration order — the source-side companion of
/// [`crate::java::ty::type_param_upper_bounds`], resolving each bound against
/// the declaring class's own scope so `?` capture conversion ([§5.1.10]) can
/// recover the true upper bound of a source class's type parameter
/// (`class Box<T extends PD>` captured from `Box<?>` gives `CAP extends PD`,
/// not `CAP extends Object`). Library classes resolve through the classfile
/// `Signature` attribute ([JVMS §4.7.9.1]); source classes through the item
/// tree.
pub fn declared_type_param_bounds(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Vec<(Name, Vec<Ty>)> {
    declared_type_param_scopes(db, scope, fqn)
        .into_iter()
        .map(|(var_scope, bounds)| (var_scope.name().clone(), bounds))
        .collect()
}

/// The declared type parameters of the class named by `fqn` as
/// `(declaring scope, [bound Ty])` in declaration order — the identity-bearing
/// companion of [`declared_type_param_bounds`], for callers that must
/// substitute into the bounds ([§4.4] capture-avoidance) rather than merely
/// read their names.
pub fn declared_type_param_scopes(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Vec<(TypeVarScope, Vec<Ty>)> {
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return Vec::new();
    };
    class_param_bounds(db, &resolved, fqn).unwrap_or_default()
}

/// The declared type of an item: the type of a field, the return type of a
/// method, or the type of a class/interface/enum/record/annotation
/// declaration. Memoized per (file, item) by the tracked query in [`crate::java::db`].
pub fn item_ty(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Ty {
    crate::java::db::item_ty_query(db, crate::java::db::ItemKey::new(db, file_id, item_id))
}

/// The parameter types of a method or constructor, in declaration order.
/// Memoized per (file, item) by the tracked query in [`crate::java::db`].
pub fn method_params(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Vec<Ty> {
    crate::java::db::method_params_query(db, crate::java::db::ItemKey::new(db, file_id, item_id))
}

/// The element types of the record components of `item` (a record
/// declaration) in `file`, in declaration order, each resolved to its element
/// type (a varargs component `String... names` resolves to `String`). The IDE
/// renders the accessor's array form (`String[]`, [§8.10.3]) and the canonical
/// constructor's ellipsis form (`String...`, [§8.10.4]) from these. Memoized
/// per (file, item) by the tracked query in [`crate::java::db`].
pub fn record_component_types(db: &dyn TyDatabase, file_id: FileId, item_id: ItemId) -> Vec<Ty> {
    crate::java::db::record_component_types_query(
        db,
        crate::java::db::ItemKey::new(db, file_id, item_id),
    )
}

/// Lowers a library [`TypeRef<Symbol>`] to a [`Ty`]. Library names are
/// already fully qualified, so only the interner lookup is needed.
pub fn ty_from_library(db: &dyn TyDatabase, tyref: &TypeRef<hir::Symbol>) -> Ty {
    let interner = &db.hir_state().interner;
    ty_from_type_ref(
        db,
        tyref,
        &mut |symbol| Name::new(interner.resolve(symbol)),
        // No declaring context: a classfile signature lowered without its
        // class cannot attribute its type variables to a parameter, so they
        // become unscoped — no declaration-scoped binding captures them.
        // Signatures that *do* carry type variables go through
        // [`ty_from_library_signature`].
        &mut |symbol| TypeVarScope::Unnamed {
            name: Name::new(interner.resolve(symbol)),
        },
    )
}

/// Lowers a classfile [`TypeRef<Symbol>`] of `ctx`'s `Signature` attribute
/// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1)),
/// attributing every type variable to its declaring parameter ([JLS §4.4],
/// [§6.3]) so a binding of that declaration instantiate exactly its own
/// variables — and never a same-named parameter of another declaration
/// ([§6.4.1]).
pub fn ty_from_library_signature(
    db: &dyn TyDatabase,
    tyref: &TypeRef<hir::Symbol>,
    ctx: &LibrarySignature<'_>,
) -> Ty {
    let interner = &db.hir_state().interner;
    ty_from_type_ref(
        db,
        tyref,
        &mut |symbol| Name::new(interner.resolve(symbol)),
        &mut |symbol| ctx.scope_of(&Name::new(interner.resolve(symbol))),
    )
}

pub(crate) fn item_data(tree: &ItemTree, item_id: ItemId) -> Option<&ItemData> {
    // `Arena::get` panics on unknown ids, so bounds-check first.
    (item_id.0.0 < tree.items.len() as u32).then(|| tree.data(item_id))
}
