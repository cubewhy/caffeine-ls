//! Navigation over the HIR: goto-definition and hover at an offset of a
//! source file.
//!
//! A reference names a declaration, and navigation answers with that
//! declaration. A *body* reference — a local, a field or method access, an
//! invocation, a constructor call, a method reference — is answered from the
//! resolution the type layer recorded while inferring the body
//! ([`hir_ty::BodyTypes::resolved`]): the exact declaration the reference
//! denotes, overload selection ([JLS §15.12]) included. A class instance
//! creation ([§15.9]) names the *constructor* it selected — the declaration
//! the classfile calls `<init>`
//! ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
//! and the class writes under its own name — and the class itself when the
//! class declares no constructor of its own. A
//! *declaration-side* reference — an `extends`/`implements` clause, a field or
//! parameter or return type, a `throws`, a generic argument, an annotation, an
//! `import` — resolves the written name in the scope of the declaration that
//! carries it ([§6.5.5.1], [§7.5]).
//!
//! Only a reference the type layer did not resolve falls back to the
//! name-based classpath walk ([`resolve_at`]): a body whose inference recorded
//! nothing, an overload probe that produced no candidate, a type variable.
//! Lambda parameters and the enum constants of a `case` label are not part of
//! the recorded table and keep their own resolution.
//!
//! An offset on a *declaration's own name* — `m` in `Main m`, `Main` in
//! `class Main`, `local` in `int local = 0` — resolves to nothing as a
//! reference ([JLS §6.3] scopes a local from its own declarator on; a type's or
//! member's name is written in its declaration, not read). Once every
//! reference step above has found nothing, the *self* step ([`self_target`])
//! answers such an offset with the declaration it names, so goto-definition is
//! available on a declaration itself as it is on a use.
//!
//! When a reference resolves into a library declaration whose source is not
//! loaded yet, it is reported as pending rather than being answered: the LSP
//! layer reads the archive entry into the database and re-runs the request.

use std::collections::VecDeque;

use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId};

use hir::JvmDatabase;
use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::{
    arena::ArenaId,
    body::{BodyTree, ExprData, ExprId, LocalId, StmtData, StmtId, SwitchLabel},
    name::Name,
};
use hir_ty::Ty;

use crate::RootDatabase;
use ide_db::base_db::{self, LanguageKind};

mod kotlin;

/// The source range of a declaration item, resolved on demand from the file's
/// parse (the item tree carries no offsets).
fn item_range(db: &RootDatabase, file: FileId, tree: &ItemTree, item: ItemId) -> Option<TextRange> {
    let language = tree.language;
    if language == LanguageKind::Unknown {
        return None;
    }
    let source = base_db::parse(db, file, language).syntax_node(language);
    let map = hir::hir_def::db::ast_id_map(db, file, language);
    hir::hir_def::java::ranges::item_range(map, &source, tree, item)
}

/// The source range of a declaration item's own *name* token, resolved on
/// demand from the file's parse: the identifier a go-to-definition selects,
/// not the whole declaration it names.
fn item_name_range(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    item: ItemId,
) -> Option<TextRange> {
    let language = tree.language;
    if language == LanguageKind::Unknown {
        return None;
    }
    let source = base_db::parse(db, file, language).syntax_node(language);
    let map = hir::hir_def::db::ast_id_map(db, file, language);
    hir::hir_def::java::ranges::item_name_range(map, &source, tree, item)
}

/// The declaration a reference resolves to: a file and the source range of
/// the declaring construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: FileId,
    /// The source range of the declaration's own *name* token — the identifier
    /// an editor jumps to and selects. Deliberately not the whole declaration:
    /// a class's range contains every reference to it, so a definition that
    /// covered the whole class would leave the cursor inside its own target,
    /// and a client that treats "already inside the definition" as a no-op
    /// would never move — the usual case for the JDK's sources, where `String`
    /// is written inside `String`.
    pub range: TextRange,
    pub name: String,
}

/// A hover result: a rendered signature or type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub value: String,
}

/// The declarations the reference at `offset` resolves to ([JLS §6.5] in a Java
/// file, nothing at all in a Kotlin one).
pub fn definition(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<NavigationTarget> {
    match hir::file_item_tree(db, file).language {
        LanguageKind::Kotlin | LanguageKind::KotlinScript => kotlin::definition(db, file, offset),
        // `Unknown` is a file with no source root yet (opened before the
        // workspace loaded) or a non-JVM file; it lowers to an empty item tree,
        // so the Java path finds nothing.
        _ => java_definition(db, file, offset),
    }
}

/// The Java declarations the reference at `offset` resolves to ([JLS §6.5]).
fn java_definition(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<NavigationTarget> {
    // §15.12: the type layer resolved this reference exactly — a local, a
    // field, an overload-selected method or constructor, a method reference.
    let recorded = recorded_reference(db, file, offset);
    if !recorded.is_empty() {
        return targets(db, recorded);
    }

    let bodies = hir::file_body_tree(db, file);
    // A reference the recorded table does not cover: a lambda parameter (the
    // body IR carries it as a name/range pair, not a local), a local of a body
    // the type layer could not infer, or the enum constant of a `case` label
    // ([§14.11.1] labels are checked before inference).
    for expr in exprs_at(&bodies, offset) {
        let ExprData::Var(name) = bodies.expr(expr).clone() else {
            continue;
        };
        // §6.4/[§15.27.2]: a lambda parameter shadows every enclosing local of
        // the same name throughout its body, so it is looked up first.
        if let Some(resolution) = lambda_param_resolution(file, &bodies, offset, &name) {
            return targets(db, vec![resolution]);
        }
        if let Some(local) = resolve_local(&bodies, name.as_str(), offset) {
            return targets(
                db,
                vec![Resolution::Variable {
                    file,
                    range: bodies.local_name_range(local).unwrap_or_default(),
                    name: name.as_str().to_owned(),
                }],
            );
        }
        let resolution = switch_label_resolution(db, file, offset, &name, expr);
        if !resolution.is_empty() {
            return targets(db, resolution);
        }
    }

    // No recorded resolution: the reference is resolved through the classpath
    // by name and arity. A library declaration that is not materialized yet
    // cannot be answered here — the LSP layer reads it into the database and
    // re-runs the request (see [`pending_library_sources`]).
    let declaration = declaration_reference(db, file, offset);
    if !declaration.is_empty() {
        return targets(db, declaration);
    }
    let resolved = resolve_at(db, file, offset);
    if !resolved.is_empty() {
        return targets(db, resolved);
    }
    // Nothing above resolved: the offset is on a declaration's own name, which
    // is not a reference to itself (§6.3) but still has a definition — the
    // declaration it names. Self-navigation answers it.
    self_target(db, file, offset).into_iter().collect()
}

/// Goto-definition on a declaration's own name answers with the declaration
/// itself: `m` in `Main m`, `Main` in `class Main`, `local` in
/// `int local = 0`. These names are declarations, not references ([JLS §6.3]
/// scopes a local from its own declarator on; a type's or member's name is
/// written in its declaration), so no step above resolves them.
///
/// Only consulted once every *reference* step found nothing, so a name that is
/// also read as a reference — the `Main` of `Main m` — is still answered by
/// the reference (the class `Main`), never by a self-target.
fn self_target(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<NavigationTarget> {
    let tree = hir::file_item_tree(db, file);
    if tree.language == LanguageKind::Unknown {
        return None;
    }
    let source = base_db::parse(db, file, tree.language).syntax_node(tree.language);
    let map = hir::hir_def::db::ast_id_map(db, file, tree.language);

    // A declared source item: its own identifier carries the offset.
    if let Some(symbol) = hir::file_symbols(db, file)
        .iter()
        .filter(|symbol| {
            hir::hir_def::java::ranges::item_name_range(map, &source, &tree, symbol.item)
                .is_some_and(|range| range.contains(offset))
        })
        .min_by_key(|symbol| {
            hir::hir_def::java::ranges::item_name_range(map, &source, &tree, symbol.item)
                .map_or(u32::MAX, |range| u32::from(range.len()))
        })
        && let Some(target) = decl_target(db, file, symbol.item, symbol.name.simple_name())
    {
        return Some(target);
    }

    // A type parameter the offset is written as: `T` in `class Box<T>`, the
    // `T` of `<T> T id(T v)`.
    if let Some(target) = type_param_target(db, file, &tree, offset) {
        return Some(target);
    }

    // A variable carried without an item of its own: a local, parameter,
    // pattern binding or lambda parameter, at the identifier it was named by.
    variable_target(&hir::file_body_tree(db, file), file, offset)
}

/// The declaration of the type parameter whose own name token carries `offset`,
/// among the type parameters the items enclosing the offset declare.
fn type_param_target(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Option<NavigationTarget> {
    for item in items_at(db, file, tree, offset) {
        let declared: &[hir::hir_def::java::item_tree::TypeParam] = match tree.data(item) {
            ItemData::Class(data) | ItemData::Interface(data) => &data.type_params,
            ItemData::Record(data) => &data.type_params,
            ItemData::Method(data) => &data.sig.type_params,
            _ => continue,
        };
        for param in declared {
            let Some(declaration) = hir_ty::type_param_declaration(db, file, item, &param.name)
            else {
                continue;
            };
            if declaration.range.contains(offset) {
                return Some(NavigationTarget {
                    file: declaration.file,
                    range: declaration.range,
                    name: param.name.simple_name().to_owned(),
                });
            }
        }
    }
    None
}

/// The variable a *declaration-side* name offset denotes: a local, parameter or
/// pattern binding (a [`LocalId`]), or a lambda parameter (carried by the body
/// IR as a name/range pair, not as a local). The target is the identifier the
/// variable was named by, matching a use's target.
fn variable_target(bodies: &BodyTree, file: FileId, offset: TextSize) -> Option<NavigationTarget> {
    let mut best: Option<(TextRange, String)> = None;
    let mut consider = |range: TextRange, name: &Name| {
        if range.contains(offset)
            && best
                .as_ref()
                .is_none_or(|(best, _)| range.len() < best.len())
        {
            best = Some((range, name.as_str().to_owned()));
        }
    };
    for (id, local) in bodies.locals.iter() {
        if let Some(range) = bodies.local_name_range(LocalId(id)) {
            consider(range, &local.name);
        }
    }
    for index in 0..bodies.expr_ranges.len() {
        if let ExprData::Lambda { params, .. } = bodies.expr(ExprId(ArenaId(index as u32))) {
            for param in params {
                consider(param.range, &param.name);
            }
        }
    }
    best.map(|(range, name)| NavigationTarget { file, range, name })
}

/// The resolutions of a *declaration-side* reference: the type reference or
/// annotation whose written name carries the offset, or the import the offset
/// falls in. A declaration's own name carries no such reference, so an offset
/// on a declaration stays unresolved — as does a name that resolves to
/// nothing (a type variable, a package segment).
fn declaration_reference(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let declared = declaration_type_ref_targets(db, file, offset);
    if !declared.is_empty() {
        return declared;
    }
    import_targets(db, file, offset)
}

/// The declarations the type reference at `offset` denotes: the reference whose
/// own name range contains the offset, resolved in the scope of the item that
/// carries it ([JLS §6.5.5.1]).
///
/// The enclosing items are consulted innermost first, then the rest of the
/// file: a member's annotation and declared type lie *before* the declarator
/// node a field item is anchored to (`@Anno Base f;`), so the reference the
/// offset falls on may lie outside every item whose range contains the offset.
fn declaration_type_ref_targets(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<Resolution> {
    let tree = hir::file_item_tree(db, file);
    let enclosed = items_at(db, file, &tree, offset);
    let mut rest: Vec<(TextRange, ItemId)> = all_items(db, file, &tree)
        .into_iter()
        .filter(|(range, _)| !range.contains(offset))
        .collect();
    rest.sort_by_key(|(range, _)| range.len());
    for item in enclosed
        .into_iter()
        .chain(rest.into_iter().map(|(_, item)| item))
    {
        for (name, range) in hir_ty::item_type_references(db, file, item) {
            if range.is_some_and(|range| range.contains(offset)) {
                let resolved = type_resolution(db, file, Some(item), &name);
                if !resolved.is_empty() {
                    return resolved;
                }
                // §4.4/[§6.5.5.1]: a type parameter is a declaration of its own
                // — the `T` of `class Box<T>` or of `<T> T id(T v)` — so a
                // written type *variable* denotes that parameter, not a class
                // of the same spelling.
                let Some(param) = hir_ty::type_param_declaration(db, file, item, &name) else {
                    return Vec::new();
                };
                return vec![Resolution::Variable {
                    file: param.file,
                    range: param.range,
                    name: name.simple_name().to_owned(),
                }];
            }
        }
    }
    Vec::new()
}

/// The declaration an import declaration's name denotes ([JLS §7.5]): the type
/// of a single-type import, or — for a static import — the member its last
/// segment names, with the type of the preceding segments. An on-demand
/// import's `*` is no identifier and names no declaration, and a leading
/// package segment names no type, so neither is answered.
fn import_targets(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let tree = hir::file_item_tree(db, file);
    if tree.language == LanguageKind::Unknown {
        return Vec::new();
    }
    let source = base_db::parse(db, file, tree.language).syntax_node(tree.language);
    let map = hir::hir_def::db::ast_id_map(db, file, tree.language);
    for import in &tree.imports {
        let segments = hir::hir_def::java::ranges::import_segments(map, &source, import);
        let Some(index) = segments
            .iter()
            .position(|(_, range)| range.contains(offset))
        else {
            continue;
        };
        let written = |last: usize| {
            segments[..=last]
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join(".")
        };
        // `import static Type.member;` — the last segment is the member, the
        // segments before it name its (possibly nested) declaring type. A
        // malformed `import static member;` names no owner and resolves to
        // nothing.
        if import.is_static && index + 1 == segments.len() && index > 0 {
            let owner = Name::new(&written(index - 1));
            let member = import.name.simple_name();
            for use_kind in [Use::Field, Use::Method] {
                match member_of_named_owner(db, file, None, &owner, &member, use_kind, None) {
                    MemberLookup::Found(resolution) => return vec![resolution],
                    MemberLookup::PendingSource(source) => {
                        return vec![Resolution::Pending(source)];
                    }
                    MemberLookup::Absent => {}
                }
            }
            return Vec::new();
        }
        return type_resolution(db, file, None, &Name::new(&written(index)));
    }
    Vec::new()
}

/// The navigation target of a resolution: a declaration item, or a variable's
/// declarator range. `LibraryMember` (hover's merged-signature path) and
/// `Pending` have no target — a pending one is materialized and the request
/// re-run instead.
fn targets(db: &RootDatabase, resolutions: Vec<Resolution>) -> Vec<NavigationTarget> {
    resolutions
        .into_iter()
        .filter_map(|resolution| match resolution {
            Resolution::Decl {
                file: decl_file,
                item,
                name,
            } => decl_target(db, decl_file, item, &name),
            Resolution::Variable { file, range, name } => {
                Some(NavigationTarget { file, range, name })
            }
            Resolution::LibraryMember {
                decl:
                    hir::LibrarySourceDecl::Loaded {
                        file: decl_file,
                        item,
                    },
                name,
                ..
            } => decl_target(db, decl_file, item, &name),
            Resolution::LibraryMember { .. } | Resolution::Pending(_) => None,
        })
        .collect()
}

/// The resolutions inference recorded for the reference at `offset`, from the
/// innermost expression that names a reference. See
/// [`hir_ty::BodyTypes::resolved`]: an expression inference never resolved has
/// no entry, and then no *outer* expression has one for this reference either —
/// the enclosing invocation or field access names a different declaration.
fn recorded_reference(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let tree = hir::file_item_tree(db, file);
    let items = body_items_at(db, file, &tree, offset);
    for expr in exprs_at(&bodies, offset) {
        if !names_a_reference(&bodies, expr) {
            continue;
        }
        // §15.9: the resolution of a class instance creation is the
        // *constructor* it selected — a member the classfile names `<init>`
        // ([JVMS §4.6]) and the class declares under its own name, never a
        // method that happens to carry that name.
        let reference = reference_at(&bodies, expr);
        for &item in &items {
            let Some(types) = hir_ty::body_types(db, file, item) else {
                continue;
            };
            let Some(member) = types.resolved.get(&expr) else {
                continue;
            };
            return match member {
                hir_ty::ResolvedMember::Local(local) => match bodies.local_name_range(*local) {
                    Some(range) => vec![Resolution::Variable {
                        file,
                        range,
                        name: bodies.local(*local).name.as_str().to_owned(),
                    }],
                    None => Vec::new(),
                },
                member => member_resolution(db, file, member, reference),
            };
        }
        return Vec::new();
    }
    Vec::new()
}

/// Whether the expression at `expr` names a declaration — the expressions the
/// type layer records a [`hir_ty::ResolvedMember`] for.
fn names_a_reference(bodies: &BodyTree, expr: ExprId) -> bool {
    matches!(
        bodies.expr(expr),
        ExprData::Var(_)
            | ExprData::FieldAccess { .. }
            | ExprData::MethodCall { .. }
            | ExprData::New { .. }
            | ExprData::ClassLit(_)
            | ExprData::InstanceOf { .. }
            | ExprData::NamePath(_)
            | ExprData::MethodRef { .. }
    )
}

/// The reference a recorded resolution was recorded for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reference {
    /// Any other reference the table carries: a field access, an invocation, a
    /// method reference.
    Member,
    /// A class instance creation ([JLS §15.9]): `new C(...)` resolves to the
    /// constructor it selected.
    ClassInstanceCreation,
}

/// The reference the navigable expression at `expr` is.
fn reference_at(bodies: &BodyTree, expr: ExprId) -> Reference {
    match bodies.expr(expr) {
        ExprData::New { .. } => Reference::ClassInstanceCreation,
        _ => Reference::Member,
    }
}

/// Whether the item is a constructor *declaration* ([JLS §8.8]). A class may
/// declare a method carrying its own name (`void C(int)`, legal with the
/// required return type), so being named like the class does not make a
/// declaration a constructor.
fn is_constructor_decl(db: &RootDatabase, file: FileId, item: ItemId) -> bool {
    matches!(
        hir::file_item_tree(db, file).data(item),
        ItemData::Method(method) if method.is_constructor()
    )
}

/// The kind of member a recorded *method* resolution is looked up with: a
/// class instance creation resolves a constructor, every other reference the
/// method itself.
fn member_use_kind(reference: Reference) -> Use {
    match reference {
        Reference::ClassInstanceCreation => Use::Constructor,
        Reference::Member => Use::Method,
    }
}

/// The name the resolved member is *declared* under. A source constructor is
/// declared under the class's own simple name; a classfile one under `<init>`
/// ([JVMS §4.6]), which is no declaration's name and has to be read back as
/// the class the owner names.
fn member_decl_name(method: &hir_ty::MethodData, reference: Reference) -> String {
    if reference != Reference::ClassInstanceCreation || method.name != "<init>" {
        return method.name.clone();
    }
    // A library owner is spelled with binary names ([JVMS §4.2]): nesting is
    // `$` there and `.` in the source declaration, the same normalization
    // [`hir::library_source_decl`] applies before it looks a type up in its
    // archive. `$` stays an ordinary identifier character in a source name
    // ([JLS §3.8]), which is why the rewrite only touches a classfile name.
    Name::new(&method.owner.replace('$', "."))
        .simple_name()
        .to_owned()
}

/// The declaration a recorded body resolution names, through the classpath
/// (mirrors [`members_of_owner`]). A class instance creation ([§15.9]) is
/// looked up as the constructor it resolved to instead.
fn member_resolution(
    db: &RootDatabase,
    file: FileId,
    member: &hir_ty::ResolvedMember,
    reference: Reference,
) -> Vec<Resolution> {
    // The member's *declaration* form: the declaring class FQN, the parameter
    // count of a method, and the workspace item when the declaration is a
    // source one (a library member carries no file and no item).
    let (name, use_kind, arity, source_decl, owner) = match member {
        hir_ty::ResolvedMember::Method(method) => (
            member_decl_name(method, reference),
            member_use_kind(reference),
            // §15.9/[§15.12.2]: the constructor a creation selects is the one
            // whose parameter list accepted the arguments — the same
            // parameter count the invocation was resolved with.
            Some(method.params.len()),
            // Only a *constructor declaration* answers a creation: the
            // recorded item of an inference fallback (a method named like the
            // class) is not one, so the lookup below finds the constructor —
            // or the class, when it declares none.
            method
                .owner_file
                .zip(method.decl_item)
                .filter(|(decl_file, item)| {
                    reference != Reference::ClassInstanceCreation
                        || is_constructor_decl(db, *decl_file, *item)
                }),
            method.owner.clone(),
        ),
        hir_ty::ResolvedMember::Field(field) => (
            field.name.clone(),
            Use::Field,
            None,
            field.owner_file.zip(field.decl_item),
            field.owner.clone(),
        ),
        hir_ty::ResolvedMember::Local(_) => return Vec::new(),
    };
    if let Some((decl_file, item)) = source_decl {
        return vec![Resolution::Decl {
            file: decl_file,
            item,
            name,
        }];
    }
    // A library member: `owner` is the *binary* FQN of the declaring class,
    // which is the key the classfile index and the source index are looked up
    // by.
    match owner_lookup(db, file, &owner) {
        OwnerLookup::Source(class) => vec![Resolution::Decl {
            item: member_or_owner(db, class.file, class.item, &name, use_kind, arity),
            file: class.file,
            name,
        }],
        OwnerLookup::Library {
            decl: hir::LibrarySourceDecl::Loaded { file, item },
            ..
        } => vec![Resolution::Decl {
            item: member_or_owner(db, file, item, &name, use_kind, arity),
            file,
            name,
        }],
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Pending { entry, path },
            ..
        } => match hir::library_sources(db, library) {
            Some(sources) => vec![Resolution::Pending(LibrarySourceRef {
                library,
                archive: sources.archive,
                entry,
                path,
            })],
            None => Vec::new(),
        },
        OwnerLookup::Unresolved => Vec::new(),
    }
}

/// The declared member `name` of the owner class declared in `decl_file`,
/// falling back to the owner declaration itself when the member has no source
/// item of its own — an implicit constructor, a record accessor.
fn member_or_owner(
    db: &RootDatabase,
    decl_file: FileId,
    owner_item: ItemId,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> ItemId {
    let tree = hir::file_item_tree(db, decl_file);
    member_item(db, decl_file, &tree, name, use_kind, arity).unwrap_or(owner_item)
}

/// The lambda parameter the `Var` at `expr` names: the innermost enclosing
/// lambda expression declaring a parameter of the name ([JLS §6.4], [§15.27.2]).
/// The body IR keeps a lambda parameter as a name/range pair
/// ([`hir_expand::body::LambdaParam`]) rather than a local, so no inference
/// resolution exists for it.
fn lambda_param_resolution(
    file: FileId,
    bodies: &BodyTree,
    offset: TextSize,
    name: &Name,
) -> Option<Resolution> {
    for expr in exprs_at(bodies, offset) {
        if let ExprData::Lambda { params, .. } = bodies.expr(expr)
            && let Some(param) = params.iter().find(|param| &param.name == name)
        {
            return Some(Resolution::Variable {
                file,
                range: param.range,
                name: name.as_str().to_owned(),
            });
        }
    }
    None
}

/// The enum constant an unqualified `case NAME:` label at `label` names
/// ([JLS §14.11.1]). A switch over an enum lowers its label as a bare `Var`,
/// and `infer_switch_label` types it against the selector's constants *before*
/// inference runs on it, so the recorded table has no entry.
fn switch_label_resolution(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    name: &Name,
    label: ExprId,
) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let Some(scrutinee) = switch_scrutinee_of(&bodies, offset, label) else {
        return Vec::new();
    };
    let tree = hir::file_item_tree(db, file);
    let items = body_items_at(db, file, &tree, offset);
    let Some(selector) = items.iter().find_map(|&item| {
        hir_ty::body_types(db, file, item)?
            .exprs
            .get(&scrutinee)
            .copied()
    }) else {
        return Vec::new();
    };
    let Some(&item) = items.first() else {
        return Vec::new();
    };
    let scope = hir_ty::scope_for_file(db, file);
    // `case NAME` is an unqualified access to a *static* enum constant.
    let access = hir_ty::access_context(db, file, item).with_mode(hir_ty::InvocationMode::Static);
    let Some(field) = hir_ty::pick_field(db, &scope, &selector, name.as_str(), &access) else {
        return Vec::new();
    };
    member_resolution(
        db,
        file,
        &hir_ty::ResolvedMember::Field(field),
        Reference::Member,
    )
}

/// The scrutinee of the innermost switch containing `offset` whose labels
/// include the label expression `label`. A switch is a statement
/// ([`StmtData::Switch`], §14.11) or a switch expression
/// ([`ExprData::Switch`], §15.28); both carry the same arms.
fn switch_scrutinee_of(bodies: &BodyTree, offset: TextSize, label: ExprId) -> Option<ExprId> {
    let labels_include = |arms: &[hir_expand::body::SwitchArm]| {
        arms.iter().any(|arm| {
            arm.labels
                .iter()
                .any(|candidate| matches!(candidate, SwitchLabel::Expr(expr) if *expr == label))
        })
    };
    let mut candidates: Vec<(u32, ExprId)> = Vec::new();
    for (index, range) in bodies.stmt_ranges.iter().enumerate() {
        if !range.contains(offset) {
            continue;
        }
        if let StmtData::Switch { scrutinee, arms } = bodies.stmt(StmtId(ArenaId(index as u32)))
            && labels_include(arms)
        {
            candidates.push((u32::from(range.len()), *scrutinee));
        }
    }
    for expr in exprs_at(bodies, offset) {
        if let ExprData::Switch { scrutinee, arms } = bodies.expr(expr)
            && labels_include(arms)
        {
            let len = bodies
                .expr_range(expr)
                .map_or(0, |range| u32::from(range.len()));
            candidates.push((len, *scrutinee));
        }
    }
    candidates.sort_by_key(|(len, _)| *len);
    candidates.first().map(|(_, scrutinee)| *scrutinee)
}

/// The library source files a reference resolves into but which are not loaded
/// into the database yet, in resolution order. The LSP layer reads each
/// `entry` out of `archive` into `path` and re-runs the request.
pub fn pending_library_sources(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<LibrarySourceRef> {
    if matches!(
        hir::file_item_tree(db, file).language,
        LanguageKind::Kotlin | LanguageKind::KotlinScript
    ) {
        return kotlin::pending_library_sources(db, file, offset);
    }
    // The recorded resolution names the declaring source; a declaration-side
    // reference names its own; and the classpath walk names every unloaded
    // owner along a member's hierarchy, so a hover — which still resolves
    // through [`resolve_at`] — materializes everything it needs in one round.
    let mut seen: FxHashSet<(hir::LibraryId, Arc<str>)> = FxHashSet::default();
    let mut out = Vec::new();
    for resolution in recorded_reference(db, file, offset)
        .into_iter()
        .chain(declaration_reference(db, file, offset))
        .chain(resolve_at(db, file, offset))
    {
        if let Resolution::Pending(source) = resolution
            && seen.insert((source.library, source.entry.clone()))
        {
            out.push(source);
        }
    }
    out
}

/// A library source file a reference resolves into but which is not loaded
/// into the database yet: the LSP layer reads `entry` out of `archive` into
/// `path` and re-runs the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySourceRef {
    pub library: hir::LibraryId,
    pub archive: AbsPathBuf,
    pub entry: Arc<str>,
    pub path: AbsPathBuf,
}

/// What the reference at an offset resolves to through the classpath.
enum Resolution {
    /// A declaration in the database: a workspace or already-loaded library
    /// declaration.
    Decl {
        file: FileId,
        item: ItemId,
        name: String,
    },
    /// A declaration carried without an item of its own — a local variable, a
    /// parameter, a pattern binding, a lambda parameter or a type parameter —
    /// at the range of its own name, not of the declaration it was written in
    /// (`Base b` and `int x = 0` target `b` and `x`).
    Variable {
        file: FileId,
        range: TextRange,
        name: String,
    },
    /// A resolved library member: everything the signature renderer needs plus
    /// where its declaring source is, if it is loaded.
    LibraryMember {
        library: hir::LibraryId,
        owner_fqn: Name,
        name: String,
        use_kind: Use,
        arity: Option<usize>,
        decl: hir::LibrarySourceDecl,
    },
    /// A library source entry that has to be materialized before the reference
    /// can be answered.
    Pending(LibrarySourceRef),
}

/// The classpath resolutions of the innermost navigable expression at
/// `offset`, innermost first: the first expression with a resolution wins.
fn resolve_at(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let tree = hir::file_item_tree(db, file);
    let symbols = hir::file_symbols(db, file);
    let items = body_items_at(db, file, &tree, offset);
    let item = items.first().copied();

    for expr_id in exprs_at(&bodies, offset) {
        let resolution = match bodies.expr(expr_id).clone() {
            ExprData::New { ty, .. } | ExprData::ClassLit(ty) => type_ref_name(&ty)
                .map_or_else(Vec::new, |name| {
                    type_resolution(db, file, item, &Name::new(&name))
                }),
            ExprData::InstanceOf { ty, .. } => ty
                .as_ref()
                .and_then(|t| type_ref_name(t))
                .map_or_else(Vec::new, |name| {
                    type_resolution(db, file, item, &Name::new(&name))
                }),
            // A method invocation, with an implicit `this` receiver when
            // `receiver` is empty ([JLS §15.12.1]).
            ExprData::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                let arity = Some(args.len());
                match receiver {
                    Some(receiver) => receiver_ty(db, file, &items, receiver)
                        .map_or_else(Vec::new, |ty| {
                            member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, arity)
                        }),
                    None => enclosing_class_receiver(db, file, &tree, &symbols, offset)
                        .map_or_else(Vec::new, |ty| {
                            member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, arity)
                        }),
                }
            }
            // A field access, with an implicit receiver when `target` is empty.
            ExprData::FieldAccess { target, name } => match target {
                Some(target) => receiver_ty(db, file, &items, target).map_or_else(Vec::new, |ty| {
                    member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, None)
                }),
                None => enclosing_class_receiver(db, file, &tree, &symbols, offset)
                    .map_or_else(Vec::new, |ty| {
                        member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, None)
                    }),
            },
            // A statically imported member ([JLS §7.5.4]): `import static
            // pkg.Type.MEMBER` (or `.*`) puts the member itself in scope.
            ExprData::Var(name) => {
                let resolver = hir_ty::Resolver::for_file(&tree);
                let mut found = Vec::new();
                let mut pending = Vec::new();
                for (owner, member) in resolver.static_import_owners(name.as_str()) {
                    match member_of_named_owner(db, file, item, &owner, &member, Use::Field, None) {
                        MemberLookup::Found(resolution) => {
                            found = vec![resolution];
                            break;
                        }
                        // A pending owner is only reported after every owner
                        // was probed.
                        MemberLookup::PendingSource(source) => pending.push(source),
                        MemberLookup::Absent => {}
                    }
                }
                if !found.is_empty() {
                    found
                } else {
                    pending.into_iter().map(Resolution::Pending).collect()
                }
            }
            // A qualified name in expression position: `Outer.Inner`,
            // `Type.field`. [JLS §6.5.2] reclassifies an ambiguous name as a
            // type first and only then as an expression name, so the whole
            // text is tried as a type reference before its last segment is
            // read as a member of the class its prefix denotes.
            ExprData::NamePath(name) => {
                let as_type = type_resolution(db, file, item, &name);
                if !as_type.is_empty() {
                    as_type
                } else {
                    match name.as_str().rsplit_once('.') {
                        Some((prefix, member)) => {
                            match member_of_named_owner(
                                db,
                                file,
                                item,
                                &Name::new(prefix),
                                member,
                                Use::Field,
                                None,
                            ) {
                                MemberLookup::Found(resolution) => vec![resolution],
                                MemberLookup::PendingSource(source) => {
                                    vec![Resolution::Pending(source)]
                                }
                                MemberLookup::Absent => Vec::new(),
                            }
                        }
                        None => Vec::new(),
                    }
                }
            }
            _ => Vec::new(),
        };
        if !resolution.is_empty() {
            return resolution;
        }
    }
    Vec::new()
}

/// The type of the expression `receiver`, read from the body that owns it.
fn receiver_ty(db: &RootDatabase, file: FileId, items: &[ItemId], receiver: ExprId) -> Option<Ty> {
    items.iter().find_map(|&item| {
        hir_ty::body_types(db, file, item).and_then(|body| body.exprs.get(&receiver).cloned())
    })
}

/// The receiver type of an implicit-`this` member access: the enclosing
/// class-like declaration of the offset.
fn enclosing_class_receiver(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    symbols: &[hir::SourceSymbol],
    offset: TextSize,
) -> Option<Ty> {
    let fqn = symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                hir::SourceSymbolKind::Class
                    | hir::SourceSymbolKind::Interface
                    | hir::SourceSymbolKind::Enum
                    | hir::SourceSymbolKind::Record
                    | hir::SourceSymbolKind::Annotation
            )
        })
        .filter(|symbol| {
            item_range(db, file, tree, symbol.item).is_some_and(|range| range.contains(offset))
        })
        .min_by_key(|symbol| {
            let range = item_range(db, file, tree, symbol.item).unwrap_or_default();
            range.end() - range.start()
        })
        .and_then(|symbol| hir::source_class_fqn(db, file, symbol.item))?;
    Some(Ty::reference(db, fqn, Vec::new()))
}

/// The resolution of a member of `receiver`, walking the receiver's own class
/// and its supertypes breadth-first, so the most-derived declaration wins
/// ([JLS §8.4.8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.1),
/// [§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1)).
///
/// The walk collects **every** unloaded owner along the hierarchy in one pass
/// (typically the class plus its supertypes, 3-6 files), so a request needs
/// exactly one materialization round instead of one per supertype level. Those
/// pending refs are only reported once the hierarchy was walked with nothing
/// found — that is what keeps it to one round.
fn member_in_hierarchy(
    db: &RootDatabase,
    file: FileId,
    receiver: Ty,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> Vec<Resolution> {
    let scope = hir_ty::scope_for_file(db, file);
    let mut queue = VecDeque::from([receiver]);
    let mut seen: FxHashSet<Name> = FxHashSet::default();
    let mut pending: Vec<LibrarySourceRef> = Vec::new();
    while let Some(ty) = queue.pop_front() {
        // A primitive or array receiver has no reference type to search.
        let Some((fqn, _)) = ty.as_reference(db) else {
            continue;
        };
        if !seen.insert(fqn.clone()) {
            continue;
        }
        match members_of_owner(db, file, fqn.as_str(), name, use_kind, arity) {
            MemberLookup::Found(resolution) => return vec![resolution],
            MemberLookup::PendingSource(source) => pending.push(source),
            MemberLookup::Absent => {}
        }
        queue.extend(hir_ty::supertypes(db, &scope, &ty));
    }
    pending.into_iter().map(Resolution::Pending).collect()
}

/// An owner class of a member access, resolved through the classpath.
enum OwnerLookup {
    /// A workspace source class and its declaration item — the fallback target
    /// of a member the class declares without an item (an implicit
    /// constructor, a record accessor).
    Source(hir::SourceClass),
    /// A library class and the location of its source declaration.
    Library {
        library: hir::LibraryId,
        fqn: Name,
        decl: hir::LibrarySourceDecl,
    },
    /// Nothing on the classpath carries the name.
    Unresolved,
}

/// The declared member named `name` of one owner class.
enum MemberLookup {
    Found(Resolution),
    PendingSource(LibrarySourceRef),
    Absent,
}

/// The declared member named `name` of the owner class `owner_fqn`, which must
/// already be canonical ([JLS §6.7]).
fn members_of_owner(
    db: &RootDatabase,
    file: FileId,
    owner_fqn: &str,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> MemberLookup {
    match owner_lookup(db, file, owner_fqn) {
        OwnerLookup::Source(class) => {
            let owner_file = class.file;
            let tree = hir::file_item_tree(db, owner_file);
            match member_item(db, owner_file, &tree, name, use_kind, arity) {
                Some(item) => MemberLookup::Found(Resolution::Decl {
                    file: owner_file,
                    item,
                    name: name.to_owned(),
                }),
                // A *loaded* owner's member set is conclusive.
                None => MemberLookup::Absent,
            }
        }
        OwnerLookup::Library {
            library,
            fqn,
            decl: hir::LibrarySourceDecl::Loaded {
                file: decl_file, ..
            },
        } => {
            let tree = hir::file_item_tree(db, decl_file);
            match member_item(db, decl_file, &tree, name, use_kind, arity) {
                Some(item) => MemberLookup::Found(Resolution::LibraryMember {
                    library,
                    owner_fqn: fqn,
                    name: name.to_owned(),
                    use_kind,
                    arity,
                    decl: hir::LibrarySourceDecl::Loaded {
                        file: decl_file,
                        item,
                    },
                }),
                None => MemberLookup::Absent,
            }
        }
        // The owning source is not loaded, so nothing about its members is
        // known: the file has to be read before the member set can be.
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Pending { entry, path },
            ..
        } => match hir::library_sources(db, library) {
            Some(sources) => MemberLookup::PendingSource(LibrarySourceRef {
                library,
                archive: sources.archive,
                entry,
                path,
            }),
            None => MemberLookup::Absent,
        },
        OwnerLookup::Unresolved => MemberLookup::Absent,
    }
}

/// The declared member named `name` of the class the written type name
/// `owner_name` denotes ([JLS §6.5.5.1]).
fn member_of_named_owner(
    db: &RootDatabase,
    file: FileId,
    item: Option<ItemId>,
    owner_name: &Name,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> MemberLookup {
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, owner_name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return MemberLookup::Absent,
    };
    members_of_owner(db, file, fqn.as_str(), name, use_kind, arity)
}

/// The class `owner_fqn` denotes in `file`'s scope, and where its source is.
fn owner_lookup(db: &RootDatabase, file: FileId, owner_fqn: &str) -> OwnerLookup {
    let scope = hir_ty::scope_for_file(db, file);
    let Some(resolved) = hir::fqn_resolve(db, &scope, owner_fqn) else {
        return OwnerLookup::Unresolved;
    };
    match &resolved {
        hir::Resolved::Source(class) => OwnerLookup::Source(*class),
        hir::Resolved::Library(class) => {
            let library = class.library;
            let fqn = resolved.fqn(db);
            match hir::library_source_decl(db, library, fqn.as_str()) {
                Some(decl) => OwnerLookup::Library {
                    library,
                    fqn: fqn.as_name().clone(),
                    decl,
                },
                // A library class with no source layout has no members to
                // navigate into.
                None => OwnerLookup::Unresolved,
            }
        }
    }
}

/// The symbol of the member `name` declared in `file`, preferring the one that
/// takes `arity` parameters and falling back to a name-only match — mirroring
/// the file-local `member_targets` shape.
fn member_item(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
) -> Option<ItemId> {
    let symbols = hir::file_symbols(db, file);
    let candidates: Vec<&hir::SourceSymbol> = symbols
        .iter()
        .filter(|symbol| {
            let kind_matches = match use_kind {
                Use::Field => matches!(
                    symbol.kind,
                    hir::SourceSymbolKind::Field | hir::SourceSymbolKind::EnumConstant
                ),
                Use::Method => symbol.kind == hir::SourceSymbolKind::Method,
                // §8.8: a constructor is a method of its class by name and
                // parameter list, but no other declaration is one — `void C()`
                // in `class C` is a method that carries the class's name.
                Use::Constructor => {
                    symbol.kind == hir::SourceSymbolKind::Method
                        && matches!(
                            tree.data(symbol.item),
                            ItemData::Method(method) if method.is_constructor()
                        )
                }
            };
            kind_matches && symbol.name.simple_name() == name
        })
        .collect();
    let by_arity = arity.and_then(|arity| {
        candidates
            .iter()
            .find(|symbol| parameter_count(tree, symbol.item) == Some(arity))
    });
    by_arity
        .or_else(|| candidates.first())
        .map(|symbol| symbol.item)
}

/// The classpath resolution of the type name written at `item` in `file`.
fn type_resolution(
    db: &RootDatabase,
    file: FileId,
    item: Option<ItemId>,
    name: &Name,
) -> Vec<Resolution> {
    // §7.4.3: a name that exists on the classpath but is not visible from the
    // file's module still denotes that class — navigation is not a compile
    // check.
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return Vec::new(),
    };
    class_resolution(db, file, &fqn)
}

/// The resolution of the canonical class name `fqn` in `file`'s scope.
fn class_resolution(db: &RootDatabase, file: FileId, fqn: &Name) -> Vec<Resolution> {
    let scope = hir_ty::scope_for_file(db, file);
    let Some(resolved) = hir::fqn_resolve(db, &scope, fqn.as_str()) else {
        return Vec::new();
    };
    match &resolved {
        hir::Resolved::Source(class) => vec![Resolution::Decl {
            file: class.file,
            item: class.item,
            name: fqn.simple_name().to_owned(),
        }],
        hir::Resolved::Library(class) => {
            let library_fqn = resolved.fqn(db);
            library_class_resolution(db, class.library, library_fqn.as_name())
        }
    }
}

/// The resolution of a class known to live in `library`.
fn library_class_resolution(
    db: &RootDatabase,
    library: hir::LibraryId,
    fqn: &Name,
) -> Vec<Resolution> {
    match hir::library_source_decl(db, library, fqn.as_str()) {
        Some(hir::LibrarySourceDecl::Loaded { file, item }) => vec![Resolution::Decl {
            file,
            item,
            name: fqn.simple_name().to_owned(),
        }],
        Some(hir::LibrarySourceDecl::Pending { entry, path }) => {
            let Some(archive) = hir::library_sources(db, library).map(|sources| sources.archive)
            else {
                return Vec::new();
            };
            vec![Resolution::Pending(LibrarySourceRef {
                library,
                archive,
                entry,
                path,
            })]
        }
        None => Vec::new(),
    }
}

/// A navigation target for one resolved declaration: its name range inside
/// `decl_file`.
fn decl_target(
    db: &RootDatabase,
    decl_file: FileId,
    item: ItemId,
    name: &str,
) -> Option<NavigationTarget> {
    let tree = hir::file_item_tree(db, decl_file);
    // The declared *name*, not the whole declaration: see
    // [`NavigationTarget::range`]. Falls back to the whole range for an item
    // whose name token cannot be resolved.
    let range = item_name_range(db, decl_file, &tree, item)
        .or_else(|| item_range(db, decl_file, &tree, item))?;
    Some(NavigationTarget {
        file: decl_file,
        range,
        name: Name::new(name).simple_name().to_owned(),
    })
}

/// The rendered signature of a resolved library member: types, flags and the
/// return type come from the classfile stub (the authority); parameter names
/// are the classfile's `MethodParameters` names when it has them
/// ([JVMS §4.7.24](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.24))
/// and the source declaration's names at the same index otherwise, and
/// `arg{i}` when neither is available.
///
/// Renders as `ret name(T p, T p2)` for a method and `type name` for a field —
/// the shape a Java declaration reads as, rather than a classfile descriptor.
fn library_member_signature(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
    name: &str,
    use_kind: Use,
    arity: Option<usize>,
    source_file: Option<FileId>,
) -> Option<HoverInfo> {
    // The class index is keyed by binary names
    // ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)),
    // which is the spelling `Resolved::fqn` hands out for a library class.
    let interner = &db.hir_state().interner;
    let symbol = interner.get_or_intern(owner_fqn.as_str());
    let index = hir::library_name_index(db, library);
    let (entry_idx, entry) = index.lookup(symbol)?;
    let resolved = hir::ResolvedClass {
        library,
        entry_idx,
        entry: entry.clone(),
    };
    let record = hir::class_record(db, &resolved)?;
    let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
        return None;
    };

    match use_kind {
        Use::Field => {
            let field = stub
                .fields
                .iter()
                .find(|field| interner.resolve(&field.name) == name)?;
            let field_ty = hir_ty::ty_from_library(db, &field.field_type);
            let ty = field_ty.display(db);
            Some(HoverInfo {
                value: format!("{ty} {name}"),
            })
        }
        Use::Method | Use::Constructor => {
            // §8.8/[JVMS §4.6]: a constructor's signature carries no return
            // type and is rendered under the class's own name, while the
            // classfile declares it as `<init>`.
            let classfile_name = match use_kind {
                Use::Constructor => "<init>",
                _ => name,
            };
            let method = stub.methods.iter().find(|method| {
                interner.resolve(&method.name) == classfile_name
                    && arity.is_none_or(|arity| method.params.len() == arity)
            })?;
            let param_count = method.params.len();
            let head = match use_kind {
                Use::Constructor => String::new(),
                _ => {
                    let return_ty = hir_ty::ty_from_library(db, &method.return_type);
                    format!("{} ", return_ty.display(db))
                }
            };
            let params: Vec<String> = method
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    let param_ty = hir_ty::ty_from_library(db, &param.param_type);
                    let ty = param_ty.display(db);
                    let name = param
                        .name
                        .map(|symbol| interner.resolve(&symbol).to_owned())
                        .or_else(|| {
                            source_parameter_name(
                                db,
                                source_file,
                                name,
                                param_count,
                                index,
                                use_kind,
                            )
                        })
                        .unwrap_or_else(|| format!("arg{index}"));
                    format!("{ty} {name}")
                })
                .collect();
            Some(HoverInfo {
                value: format!("{head}{name}({})", params.join(", ")),
            })
        }
    }
}

/// The declared name of parameter `index` of the member `name` in the loaded
/// library source file, when that declaration has the same parameter count.
fn source_parameter_name(
    db: &RootDatabase,
    source_file: Option<FileId>,
    name: &str,
    param_count: usize,
    index: usize,
    use_kind: Use,
) -> Option<String> {
    let file = source_file?;
    let tree = hir::file_item_tree(db, file);
    let item = member_item(db, file, &tree, name, use_kind, Some(param_count))?;
    match tree.data(item) {
        ItemData::Method(method) => method
            .sig
            .params
            .get(index)
            .map(|param| param.name.as_str().to_owned()),
        _ => None,
    }
}

/// The local of `name` in scope at `offset` ([JLS §6.3], [§6.4]): a
/// same-named declarator *enclosing* the reference is a shadowing inner
/// declaration and wins over every outer one; otherwise the nearest
/// declaration *before* the use wins, since a local's scope begins at its own
/// declarator and cannot reach forward. The body IR records declarator
/// ranges but not block extents ([§6.3] scopes), so a use lexically *after*
/// an inner block closed still resolves to the inner declaration — the
/// innermost-first order matches javac everywhere else. `None` when no
/// declaration is in scope: the name may denote a field or import.
fn resolve_local(bodies: &BodyTree, name: &str, offset: TextSize) -> Option<LocalId> {
    let mut enclosing: Option<(TextRange, LocalId)> = None;
    let mut preceding: Option<(TextRange, LocalId)> = None;
    for (id, local) in bodies.locals.iter() {
        if local.name.as_str() != name {
            continue;
        }
        let id = LocalId(id);
        let Some(range) = bodies.local_range(id) else {
            continue;
        };
        if range.contains(offset) {
            if enclosing.is_none_or(|(best, _)| range.len() < best.len()) {
                enclosing = Some((range, id));
            }
        } else if range.start() <= offset
            && preceding.is_none_or(|(best, _)| range.start() > best.start())
        {
            preceding = Some((range, id));
        }
    }
    enclosing.or(preceding).map(|(_, id)| id)
}

/// The plain (possibly qualified) type name of a `TypeRef`, descending
/// through array dimensions.
fn type_ref_name(tyref: &syntax::stub::TypeRef<hir_expand::name::Name>) -> Option<String> {
    match tyref {
        syntax::stub::TypeRef::Reference { name, .. } => Some(name.as_str().to_owned()),
        syntax::stub::TypeRef::Array(inner) => type_ref_name(inner),
        _ => None,
    }
}

/// The hover at `offset`: the merged signature of a resolved library member,
/// the type of the expression or local the offset falls on, or the signature
/// of the declaration it falls inside.
pub fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let tree = hir::file_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);
    let symbols = hir::file_symbols(db, file);

    // A resolved library reference outranks everything below: its signature is
    // what the user is asking about. When its declaring source is not loaded
    // yet, hover answers `None` *without* consulting the fallbacks, so the LSP
    // layer materializes the file and the retried hover shows the merged
    // signature — answering the expression's type (or the bytecode-only
    // rendering) here would hide the merge on the first, and most likely only,
    // hover.
    match resolve_at(db, file, offset).into_iter().next() {
        Some(Resolution::Pending(_)) => return None,
        Some(Resolution::LibraryMember {
            library,
            owner_fqn,
            name,
            use_kind,
            arity,
            decl,
        }) => {
            let source_file = match decl {
                hir::LibrarySourceDecl::Loaded { file, .. } => file,
                hir::LibrarySourceDecl::Pending { .. } => return None,
            };
            return library_member_signature(
                db,
                library,
                &owner_fqn,
                &name,
                use_kind,
                arity,
                Some(source_file),
            );
        }
        // A declaration has no merged signature of its own, and a `Variable`
        // (a local, a lambda parameter, a type parameter) is not a library
        // reference: `resolve_at` never produces one.
        Some(Resolution::Decl { .. }) | Some(Resolution::Variable { .. }) | None => {}
    }

    // An expression's inferred type, from the enclosing body — walk the
    // innermost enclosing expressions first.
    for expr_id in exprs_at(&bodies, offset) {
        for item in body_items_at(db, file, &tree, offset) {
            if let Some(body) = hir_ty::body_types(db, file, item)
                && let Some(ty) = body.exprs.get(&expr_id)
            {
                return Some(HoverInfo {
                    value: ty.display(db).to_string(),
                });
            }
        }
    }

    // A local variable — the declaration's declarator contains the offset.
    for (id, local) in bodies.locals.iter() {
        if bodies
            .local_range(LocalId(id))
            .is_some_and(|range| range.contains_inclusive(offset))
        {
            for item in body_items_at(db, file, &tree, offset) {
                if let Some(body) = hir_ty::body_types(db, file, item)
                    && let Some(ty) = body.locals.get(&LocalId(id))
                {
                    return Some(HoverInfo {
                        value: format!("{}: {}", local.name.as_str(), ty.display(db)),
                    });
                }
            }
        }
    }

    // A declaration's signature.
    render_symbol_decl(db, file, &tree, &symbols, offset)
}

/// Every item whose declaration range contains `offset`, innermost (smallest
/// range) first: the enclosing class-like declarations, the member that
/// carries the offset, and the nested declarations it contains.
fn items_at(db: &RootDatabase, file: FileId, tree: &ItemTree, offset: TextSize) -> Vec<ItemId> {
    let mut found: Vec<(TextRange, ItemId)> = all_items(db, file, tree)
        .into_iter()
        .filter(|(range, _)| range.contains(offset))
        .collect();
    found.sort_by_key(|(range, _)| range.len());
    found.into_iter().map(|(_, item)| item).collect()
}

/// Every item of the file with its declaration range, in tree order.
fn all_items(db: &RootDatabase, file: FileId, tree: &ItemTree) -> Vec<(TextRange, ItemId)> {
    fn walk(
        db: &RootDatabase,
        file: FileId,
        tree: &ItemTree,
        item: ItemId,
        out: &mut Vec<(TextRange, ItemId)>,
    ) {
        if let Some(range) = item_range(db, file, tree, item) {
            out.push((range, item));
        }
        for &child in tree.data(item).body() {
            walk(db, file, tree, child, out);
        }
    }
    let mut out = Vec::new();
    for &top in &tree.top {
        walk(db, file, tree, top, &mut out);
    }
    out
}

/// The body-carrying item ids whose range contains `offset`, innermost first —
/// the owners whose [`hir_ty::BodyTypes`] may type the construct at the offset.
fn body_items_at(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Vec<ItemId> {
    items_at(db, file, tree, offset)
        .into_iter()
        .filter(|&item| {
            matches!(
                tree.data(item),
                ItemData::Method(_)
                    | ItemData::Field(_)
                    | ItemData::StaticInit(_)
                    | ItemData::InstanceInit(_)
                    | ItemData::EnumConstant(_)
            )
        })
        .collect()
}

/// The rendered signature of the declaration the offset falls inside: a
/// method's `name(params): ret`, a field's `name: ty`, a class-like
/// declaration's `kind name`.
fn render_symbol_decl(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    symbols: &[hir::SourceSymbol],
    offset: TextSize,
) -> Option<HoverInfo> {
    let symbol = symbols
        .iter()
        .filter(|s| item_range(db, file, &tree, s.item).is_some_and(|range| range.contains(offset)))
        .min_by_key(|s| {
            let range = item_range(db, file, &tree, s.item).unwrap_or_default();
            range.end() - range.start()
        })?;
    let simple = symbol.name.simple_name();
    let value = match symbol.kind {
        hir::SourceSymbolKind::Method => {
            crate::symbols::method_signature(db, file, symbol.item, simple, true)
        }
        hir::SourceSymbolKind::Field => {
            format!(
                "{simple}: {}",
                crate::symbols::item_ty(db, file, symbol.item)
            )
        }
        hir::SourceSymbolKind::EnumConstant => simple.to_string(),
        kind => format!("{} {}", kind.label(), simple),
    };
    Some(HoverInfo { value })
}

/// The kind of member or type a reference resolves to.
#[derive(Debug, Clone, Copy)]
enum Use {
    Field,
    Method,
    /// A constructor declaration ([JLS §8.8]): written under the class's own
    /// simple name in source and under `<init>` in a classfile
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)),
    /// so a lookup has to distinguish it from a method that carries the
    /// class's name.
    Constructor,
}

/// The parameter count of the method declaration `item`, from the item tree.
fn parameter_count(tree: &hir::hir_def::java::item_tree::ItemTree, item: ItemId) -> Option<usize> {
    match tree.data(item) {
        ItemData::Method(method) => Some(method.sig.params.len()),
        _ => None,
    }
}

/// The expressions whose source range contains `offset`, innermost (smallest
/// range) first. An offset on an argument or operand may fall inside several
/// nested expressions; navigation walks the innermost first.
fn exprs_at(bodies: &BodyTree, offset: TextSize) -> Vec<ExprId> {
    let mut enclosing: Vec<(u32, ExprId)> = bodies
        .expr_ranges
        .iter()
        .enumerate()
        .filter(|(_, range)| range.contains(offset))
        .map(|(idx, range)| (u32::from(range.len()), ExprId(ArenaId(idx as u32))))
        .collect();
    enclosing.sort_by_key(|(len, _)| *len);
    enclosing.into_iter().map(|(_, id)| id).collect()
}
