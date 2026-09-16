//! Java navigation over the HIR: goto-definition, references and hover at
//! an offset of a Java source file.
//!
//! A reference names a declaration, and navigation answers with that
//! declaration. A *body* reference — a local, a field or method access, an
//! invocation, a constructor call, a method reference — is answered from the
//! resolution the type layer recorded while inferring the body
//! ([`hir_ty::BodyTypes::resolved`]): the exact declaration the reference
//! denotes, overload selection ([JLS §15.12]) included. A declaration outside
//! the workspace has no item of its own to quote, so it is found by the
//! signature that resolution selected: a library member by the classfile
//! descriptor it recorded ([JVMS §4.6]), a source declaration by the
//! parameter types compared *erased* ([§4.6]) — no two members of one class
//! share an erasure ([§8.4.2]), and the classfile writes the same erasure the
//! declaration does. A class instance
//! creation ([§15.9]) names the *constructor* it selected — the declaration
//! the classfile calls `<init>`
//! ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
//! and the class writes under its own name — and the class itself when the
//! class declares no constructor of its own. An explicit constructor
//! invocation ([§8.8.7.1]) — `this(...)` and `super(...)` — is the same
//! selection: the constructor of the enclosing class, or of its direct
//! superclass, that it delegates to.
//!
//! The recorded selection answers with *several* declarations when the type
//! layer selected none of them: an invocation whose applicable overloads are
//! tied ([§15.12.2.5]), or one no overload is applicable to ([§15.12.2]),
//! denotes every declaration of the name the member set found
//! ([`hir_ty::ResolvedMember::Unresolved`]), and navigation answers each of
//! them — the same way an unresolved *type* name still denotes the class it
//! writes ([§7.4.3]), because navigation is not a compile check. A name
//! nothing declares stays unanswered.
//!
//! A `this` or `super` *keyword* ([§15.8.3], [§15.8.4]) names no member but a
//! type: the enclosing class, the direct superclass ([§8.1.4]) — never the
//! member an enclosing `super.m()`/`this.f` reads, whose recorded resolution
//! lies in a different declaration — or, qualified, the class or interface a
//! `TypeName.this`/`TypeName.super` writes ([§15.11.2]).
//!
//! A *declaration-side* reference — an `extends`/`implements` clause, a field or
//! parameter or return type, a `throws`, a generic argument, an annotation, an
//! `import` — resolves the written name in the scope of the declaration that
//! carries it ([§6.5.5.1], [§7.5]).
//!
//! A *record component* ([JLS §8.10.1]) is a declaration the HIR carries
//! without an item of its own: the record's declaration holds the component
//! list. Its private final field ([§8.10.1]) and its public accessor
//! ([§8.10.3]) are synthesized members an unqualified read, a `this.x` and a
//! `p.x()` respectively name, and each of them is declared by the component —
//! so they all navigate to the component, and the component's own name in the
//! declaration header is a self-target ([`self_target`]). An accessor the
//! record's body declares *itself* is a method with an item of its own, and
//! keeps that declaration ([§8.10.3]). A component is reachable from every
//! file through its accessor, so [`references`] sweeps the whole workspace for
//! it ([`is_workspace_visible`]).
//!
//! An annotation's *element-value pairs* ([§9.7.1]) are their own kind: a
//! pair's name denotes the annotation interface's element ([§9.6.1]), and a
//! name inside its value denotes what the same name denotes in the carrying
//! declaration's scope — a class literal's type ([§15.8.2]), a nested
//! annotation's interface, or a field ([§6.5.6]). The lowering keeps those
//! values as structured forms rather than expressions, so
//! [`annotation_reference`] reads them from the syntax tree.
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
//! layer reads the archive entry — or decompiles the class, when the library
//! ships no sources at all ([`LibraryFileRef`]) — into the database and re-runs
//! the request.

use std::collections::VecDeque;

use rayon::prelude::*;
use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;
use smol_str::SmolStr;
use triomphe::Arc;
use vfs::FileId;

use hir::JvmDatabase;
use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_expand::{
    arena::ArenaId,
    body::{BodyTree, ExprData, ExprId, LocalId, StmtData, StmtId, SwitchLabel},
    name::Name,
};
use hir_ty::Ty;

use ide_db::base_db::{self, LanguageKind};
use syntax::java::{SyntaxKind as J, translate_unicode_escapes};

use super::{HoverInfo, LibraryFileRef, NavigationTarget, ReferenceTarget, RootDatabase};

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

/// The resolutions of the reference at `offset`: the recorded inference
/// resolution, then the lambda-parameter / local / `case`-label fallbacks, the
/// declaration-side references (type refs, imports) and the annotation
/// element-value pairs, then the classpath walk ([`resolve_at`]). Empty when
/// nothing resolves — a declaration's own name is not a reference ([JLS §6.3]),
/// so `self_target` is not consulted here.
fn resolutions(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    // §15.8.3/§15.8.4: a `this`/`super` keyword names a type, not a member —
    // and it is written *inside* the receiver of the enclosing `this.f` or
    // `super.m()` when it is not a receiver of its own, so the recorded
    // resolution of that member access is not its answer.
    if let Some(keyword) = keyword_target(db, file, offset) {
        return keyword;
    }

    // §15.12: the type layer resolved this reference exactly — a local, a
    // field, an overload-selected method or constructor, a method reference.
    let recorded = recorded_reference(db, file, offset);
    if !recorded.is_empty() {
        return recorded;
    }

    let bodies = hir::file_body_tree(db, file);
    // A reference the recorded table does not cover: a lambda parameter (the
    // body IR carries it as a name/range pair, not a local), a local of a body
    // the type layer could not infer, or the enum constant of a `case` label
    // ([§14.11.1] labels are checked before inference). Only a name written at
    // the offset is a candidate, so the enclosing invocation of an argument is
    // never answered from here.
    if let Some(expr) = innermost_expr_at(&bodies, offset)
        && let ExprData::Var(name) = bodies.expr(expr).clone()
    {
        // §6.4/[§15.27.2]: a lambda parameter shadows every enclosing local of
        // the same name throughout its body, so it is looked up first.
        if let Some(resolution) = lambda_param_resolution(file, &bodies, offset, &name) {
            return vec![resolution];
        }
        if let Some(local) = resolve_local(&bodies, name.as_str(), offset) {
            return vec![Resolution::Variable {
                file,
                range: bodies.local_name_range(local).unwrap_or_default(),
                name: name.as_str().to_owned(),
            }];
        }
        let resolution = switch_label_resolution(db, file, offset, &name, expr);
        if !resolution.is_empty() {
            return resolution;
        }
    }

    // No recorded resolution: the reference is resolved through the classpath.
    // A member lookup keys on the signature the resolution selected, so an
    // invocation inference did not resolve is not guessed at by name or
    // argument count. A library declaration that is not materialized yet
    // cannot be answered here — the LSP layer loads it (reads the archive entry,
    // or decompiles the class) and re-runs the request (see
    // [`pending_library_files`]).
    let declaration = declaration_reference(db, file, offset);
    if !declaration.is_empty() {
        return declaration;
    }
    // §9.7.1: an annotation's element-value pairs — the pair's name (the
    // annotation interface's element) and the names inside its value — are
    // not part of the declaration-side enumeration above, and only their
    // literal forms are lowered into the expression arena.
    let annotation = annotation_reference(db, file, offset);
    if !annotation.is_empty() {
        return annotation;
    }
    let resolved = resolve_at(db, file, offset);
    if !resolved.is_empty() {
        return resolved;
    }
    Vec::new()
}

/// The declarations the reference at `offset` resolves to in a Java file
/// ([JLS §6.5]).
pub(super) fn definition(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<NavigationTarget> {
    let resolutions = resolutions(db, file, offset);
    if resolutions.is_empty() {
        // Nothing resolved: the offset is on a declaration's own name, which is
        // not a reference to itself (§6.3) but still has a definition — the
        // declaration it names. Self-navigation answers it.
        return self_target(db, file, offset).into_iter().collect();
    }
    targets(db, resolutions)
}

/// The reference sites of the declaration(s) the reference at `offset` names:
/// every identifier token in the swept files that resolves — through the same
/// forward pipeline `definition` uses — to one of those declarations.
///
/// The query's own answer is the invariant of the whole feature: a site counts
/// when the declarations `resolutions` produces for it map to a
/// `(FileId, TextRange)` pair that is one of the query's. There is no second
/// resolution path, so `references` can never disagree with `definition`.
///
/// The boundaries of the sweep, where a reader will look for them:
/// * Only the workspace's own sources ([`RootDatabase::source_files`]) and the
///   query file are swept, so a reference *inside* library sources — an
///   override's call to a supertype member, say — is out of scope.
/// * A site whose own resolution needs a library file that is not loaded yet
///   resolves to nothing ([`Resolution::Pending`]) and is therefore not
///   reported: the LSP layer defers a request only when the *query* resolves to
///   nothing (see [`pending_library_files`]), never to complete a sweep.
/// * A record component is identified by the component itself
///   ([`component_member`]), not by its record: a reference to the component's
///   field or accessor is written under the component's name, so the component
///   declaration and every such reference are sites of one declaration.
///   A *different* synthesized member with no item of its own — an implicit
///   constructor, an implicit `equals`/`hashCode`/`toString` — is identified by
///   its owner class's item ([`member_or_owner`]) instead, and a reference to
///   it is written under the member's name, not the class's: a query on the
///   class declaration therefore does not report it, even though `definition`
///   navigates it to the class.
/// * Only `IDENTIFIER` tokens are candidates ([`file_references`]), so a javadoc
///   `{@link ...}` — one `JAVADOC` token — is not a site; `definition` answers
///   nothing at such an offset either, so the two requests stay consistent.
pub(super) fn references(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
    include_declaration: bool,
) -> Vec<ReferenceTarget> {
    let found = definition(db, file, offset);
    if found.is_empty() {
        return Vec::new();
    }
    let names: FxHashSet<String> = found.iter().map(|t| t.name.clone()).collect();
    let decls: FxHashSet<(FileId, TextRange)> = found.iter().map(|t| (t.file, t.range)).collect();

    let mut files = db.source_files();
    files.push(file);
    if found.iter().all(|t| !is_workspace_visible(db, t)) {
        // A local, a parameter, a pattern binding or a type parameter: only the
        // file declaring it can name it, so a workspace sweep would resolve
        // every other file for nothing.
        files = found.iter().map(|t| t.file).collect();
        files.push(file);
    }
    files.sort_unstable();
    files.dedup();

    let mut hits = sweep(db, &files, &names, &decls);
    if include_declaration {
        hits.extend(found.iter().map(|t| ReferenceTarget {
            file: t.file,
            range: t.range,
        }));
    }
    hits.sort_by_key(|hit| (hit.file, hit.range.start()));
    hits.dedup();
    hits
}

/// Whether a target of a file can be named from *every* file of the workspace:
/// a declaration with an item of its own — an item of `target.file` whose name
/// token is `target.range` — or a record component ([JLS §8.10.1]), whose
/// accessor ([§8.10.3]) is a public member any file may call.
///
/// `false` for a name only the file that declares it can write — a local, a
/// parameter, a pattern binding, a lambda parameter or a type parameter —
/// which is what makes the narrowing in [`references`] sound. A *library*
/// member that is loaded produces a `(library_file, name_range)` target and is
/// an item name of that library file — still correct, since the library source
/// is in the database.
fn is_workspace_visible(db: &RootDatabase, target: &NavigationTarget) -> bool {
    let tree = hir::java_item_tree(db, target.file);
    if all_items(db, target.file, &tree)
        .into_iter()
        .any(|(_, item)| {
            // §6.7: a *local* declaration — and every declaration nested in it
            // — has no canonical name, so it can only be spelled inside its own
            // file; the reference sweep stays narrowed to the declaring file
            // instead of sweeping the workspace.
            !is_local(&tree, item)
                && item_name_range(db, target.file, &tree, item) == Some(target.range)
        })
    {
        return true;
    }
    // A record component carries no item of its own, and its own file is not
    // the only one that can name it: the private field is read from the
    // record's body, but the accessor is public, so a query on the component
    // sweeps the workspace exactly as a query on a field does.
    file_components(db, target.file, &tree)
        .into_iter()
        .any(|component| component.range == target.range)
}

/// Every reference site of `names` in `files`, one worker per chunk of files.
///
/// A `RootDatabase` is `Send` but not `Sync`, so each rayon worker runs on its
/// own clone (the shape [`crate::workspace::workspace_reports`] uses); the
/// clones share salsa's memo tables, so the parses and lowerings the sweep
/// resolves against are computed once.
fn sweep(
    db: &RootDatabase,
    files: &[FileId],
    names: &FxHashSet<String>,
    decls: &FxHashSet<(FileId, TextRange)>,
) -> Vec<ReferenceTarget> {
    let num_workers = rayon::current_num_threads().max(1);
    let chunk_size = files.len().div_ceil(num_workers);
    let chunks: Vec<&[FileId]> = files.chunks(chunk_size.max(1)).collect();
    let databases: Vec<RootDatabase> = (0..chunks.len()).map(|_| db.clone()).collect();
    chunks
        .into_par_iter()
        .zip(databases.into_par_iter())
        .flat_map_iter(|(chunk, db)| {
            chunk
                .iter()
                .flat_map(move |&file| file_references(&db, file, names, decls))
        })
        .collect()
}

/// The reference sites of `names` inside one file: every `IDENTIFIER` token
/// whose decoded text names one of the query's declarations and whose
/// resolution is one of them.
///
/// The token walk is the candidate source because the alternative —
/// enumerating sites from type references, the resolved-body table, imports and
/// annotation pairs — duplicates each step of the forward pipeline and drifts
/// from it. A reference can only be written as an identifier token of the
/// declaration's name (constants, imports, qualifiers and supertype clauses
/// included), so the filter is sound, and it bounds the number of
/// `resolutions` calls to the tokens that could possibly answer.
fn file_references(
    db: &RootDatabase,
    file: FileId,
    names: &FxHashSet<String>,
    decls: &FxHashSet<(FileId, TextRange)>,
) -> Vec<ReferenceTarget> {
    let tree = hir::java_item_tree(db, file);
    if tree.language == LanguageKind::Unknown {
        return Vec::new();
    }
    let parse = base_db::parse(db, file, tree.language);
    // A Kotlin file has no HIR to resolve through: nothing is a reference there
    // (`kotlin::references`).
    let syntax::SourceFile::Java(source) = &parse.syntax_node(tree.language) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for element in source.syntax_node.descendants_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        if token.kind() != J::IDENTIFIER {
            continue;
        }
        // [JLS §3.3]: an identifier written through a Unicode escape names what
        // the escape spells, so the candidate filter decodes the token text the
        // way the lexer does before comparing it with the declaration's name.
        if !names.contains(translate_unicode_escapes(token.text()).as_ref()) {
            continue;
        }
        let range = token.text_range();
        if targets(db, resolutions(db, file, range.start()))
            .iter()
            .any(|target| decls.contains(&(target.file, target.range)))
        {
            out.push(ReferenceTarget { file, range });
        }
    }
    out
}

/// Goto-definition on a declaration's own name answers with the declaration
/// itself: `m` in `Main m`, `Main` in `class Main`, `local` in
/// `int local = 0`, `x` in `record Point(int x, int y)`. These names are
/// declarations, not references ([JLS §6.3] scopes a local from its own
/// declarator on; a type's or member's name is written in its declaration), so
/// no step above resolves them.
///
/// Only consulted once every *reference* step found nothing, so a name that is
/// also read as a reference — the `Main` of `Main m` — is still answered by
/// the reference (the class `Main`), never by a self-target.
fn self_target(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<NavigationTarget> {
    let tree = hir::java_item_tree(db, file);
    if tree.language == LanguageKind::Unknown {
        return None;
    }
    let source = base_db::parse(db, file, tree.language).syntax_node(tree.language);
    let map = hir::hir_def::db::ast_id_map(db, file, tree.language);

    // A declared item: its own name token carries the offset. The *innermost*
    // declaration wins (the name ranges of nested declarations are disjoint),
    // and a *local* declaration ([JLS §14.3]) answers as any other — its item
    // is in the file's item tree, whose symbol set lists no local declaration
    // ([§6.7]).
    if let Some(item) = all_items(db, file, &tree)
        .into_iter()
        .filter(|(_, item)| {
            hir::hir_def::java::ranges::item_name_range(map, &source, &tree, *item)
                .is_some_and(|range| range.contains(offset))
        })
        .min_by_key(|(_, item)| {
            hir::hir_def::java::ranges::item_name_range(map, &source, &tree, *item)
                .map_or(u32::MAX, |range| u32::from(range.len()))
        })
        .map(|(_, item)| item)
        && let Some(target) = item_decl_target(db, file, &tree, item)
    {
        return Some(target);
    }

    // A record component the offset is written as ([JLS §8.10.1]): the `x` of
    // `record Point(int x, int y)`. Its declaration is not an item of its own,
    // so the symbol walk above — which matches a declaration's own name — never
    // sees it.
    if let Some(component) = component_at(db, file, &tree, offset) {
        return Some(NavigationTarget {
            file,
            range: component.range,
            name: component.name,
        });
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

/// The navigation target of the declaration an item's own name token writes —
/// the item's declared name, read from the item tree (a local class-like
/// declaration has no canonical name, [§6.7]).
fn item_decl_target(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    item: ItemId,
) -> Option<NavigationTarget> {
    let name = tree.data(item).name()?.as_str().to_owned();
    decl_target(db, file, item, &name)
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

/// A record component
/// ([JLS §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1))
/// declared in a source file: the record that declares it, the component's
/// position in that record's component list, and the range and text of its own
/// name.
///
/// A component is not an [`ItemId`] — the HIR keeps the component list on the
/// record's declaration — so neither the arena nor the symbol index addresses
/// it; the list is the only way to reach one.
struct ComponentAt {
    /// The item of the record that declares the component.
    record: ItemId,
    /// The component's position in `record`'s component list, in declaration
    /// order.
    index: usize,
    /// The range of the component's own name token — what a navigation target
    /// selects and a hover describes. Resolved from the file's parse, because
    /// the HIR's component list carries no offsets.
    range: TextRange,
    /// The component's name.
    name: String,
}

/// Every record component the file declares, in item and declaration order.
fn file_components(db: &RootDatabase, file: FileId, tree: &ItemTree) -> Vec<ComponentAt> {
    if tree.language == LanguageKind::Unknown {
        return Vec::new();
    }
    let source = base_db::parse(db, file, tree.language).syntax_node(tree.language);
    let map = hir::hir_def::db::ast_id_map(db, file, tree.language);
    let mut out = Vec::new();
    for (_, item) in all_items(db, file, tree) {
        let ItemData::Record(record) = tree.data(item) else {
            continue;
        };
        for (index, component) in record.components.iter().enumerate() {
            if let Some(range) =
                hir::hir_def::java::ranges::component_name_range(map, &source, component)
            {
                out.push(ComponentAt {
                    record: item,
                    index,
                    range,
                    name: component.name.as_str().to_owned(),
                });
            }
        }
    }
    out
}

/// The record component whose own name token contains `offset` — the
/// declaration a record writes for a component in its declaration header.
fn component_at(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Option<ComponentAt> {
    file_components(db, file, tree)
        .into_iter()
        .find(|component| component.range.contains(offset))
}

/// The hover of a record component's declaration ([JLS
/// §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1)):
/// the type and name of the private final field the component declares,
/// rendered like a field declaration's. A variable-arity component's field and
/// accessor carry the array type its element type packs into ([§8.4.1]). A
/// *use* of a component is answered by the expression's type instead, like a
/// field's or a local's.
fn component_hover(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    component: &ComponentAt,
) -> Option<HoverInfo> {
    let ItemData::Record(record) = tree.data(component.record) else {
        return None;
    };
    let declared = record.components.get(component.index)?;
    let element = hir_ty::record_component_types(db, file, component.record)
        .get(component.index)?
        .display_simple(db)
        .to_string();
    let ty = if declared.varargs {
        format!("{element}[]")
    } else {
        element
    };
    Some(HoverInfo {
        value: format!("{}: {ty}", component.name),
        // A record component has no doc comment of its own (the specification
        // recognises none before it); the record's `@param <component>` text
        // documents it.
        docs: crate::docs::doc_param(db, file, component.record, &component.name),
    })
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

/// The declarations an annotation's element-value pair at `offset` denotes
/// ([JLS §9.7.1]): the element method of the pair's name, or the declaration
/// a name inside its value reads — a class literal's or nested annotation's
/// type ([§15.8.2], [§9.7.1]), an enum constant or constant variable
/// ([§6.5.6]).
fn annotation_reference(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    match hir_ty::annotation_target(db, file, offset) {
        Some(hir_ty::AnnotationTarget::Element(method)) => member_resolution(
            db,
            file,
            &hir_ty::ResolvedMember::Method(method),
            Reference::Member,
        ),
        Some(hir_ty::AnnotationTarget::Field(field)) => member_resolution(
            db,
            file,
            &hir_ty::ResolvedMember::Field(field),
            Reference::Member,
        ),
        Some(hir_ty::AnnotationTarget::Type(fqn)) => class_resolution(db, file, &fqn),
        None => Vec::new(),
    }
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
    let tree = hir::java_item_tree(db, file);
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
                let resolved = type_resolution(
                    db,
                    file,
                    Some(item),
                    &name,
                    range.map(|range| range.start()),
                );
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
    let tree = hir::java_item_tree(db, file);
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
                match member_of_named_owner(
                    db,
                    file,
                    None,
                    &owner,
                    member,
                    use_kind,
                    Params::Unknown,
                ) {
                    MemberLookup::Found(resolution) => return vec![resolution],
                    MemberLookup::PendingSource(source) => {
                        return vec![Resolution::Pending(source)];
                    }
                    MemberLookup::Absent => {}
                }
            }
            return Vec::new();
        }
        return type_resolution(db, file, None, &Name::new(&written(index)), None);
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
/// innermost expression there. See [`hir_ty::BodyTypes::resolved`]: an
/// expression inference never resolved has no entry, and then no *outer*
/// expression has one for this reference either — the enclosing invocation or
/// field access names a different declaration ([`innermost_expr_at`]).
fn recorded_reference(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let tree = hir::java_item_tree(db, file);
    let items = body_items_at(db, file, &tree, offset);
    let Some(expr) = innermost_expr_at(&bodies, offset) else {
        return Vec::new();
    };
    if !names_a_reference(&bodies, expr) {
        return Vec::new();
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
            | ExprData::CtorCall { .. }
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
    /// A constructor selection ([JLS §15.9], [§8.8.7.1]): a class instance
    /// creation `new C(...)`, or an explicit constructor invocation
    /// `this(...)`/`super(...)`, resolves to the constructor it selected.
    Constructor,
}

/// The reference the navigable expression at `expr` is.
fn reference_at(bodies: &BodyTree, expr: ExprId) -> Reference {
    match bodies.expr(expr) {
        ExprData::New { .. } | ExprData::CtorCall { .. } => Reference::Constructor,
        _ => Reference::Member,
    }
}

/// Whether the item is a constructor *declaration* ([JLS §8.8]). A class may
/// declare a method carrying its own name (`void C(int)`, legal with the
/// required return type), so being named like the class does not make a
/// declaration a constructor.
fn is_constructor_decl(db: &RootDatabase, file: FileId, item: ItemId) -> bool {
    matches!(
        hir::java_item_tree(db, file).data(item),
        ItemData::Method(method) if method.is_constructor()
    )
}

/// The kind of member a recorded *method* resolution is looked up with: a
/// constructor selection resolves a constructor, every other reference the
/// method itself.
fn member_use_kind(reference: Reference) -> Use {
    match reference {
        Reference::Constructor => Use::Constructor,
        Reference::Member => Use::Method,
    }
}

/// The name the resolved member is *declared* under. A source constructor is
/// declared under the class's own simple name; a classfile one under `<init>`
/// ([JVMS §4.6]), which is no declaration's name and has to be read back as
/// the class the owner names.
fn member_decl_name(
    db: &RootDatabase,
    method: &hir_ty::MethodData,
    reference: Reference,
) -> String {
    if reference != Reference::Constructor || method.name != "<init>" {
        return method.name.clone();
    }
    // A library owner is spelled with binary names ([JVMS §4.2]): nesting is
    // `$` there and `.` in the source declaration, the same normalization
    // [`hir::library_source_decl`] applies before it looks a type up in its
    // archive. `$` stays an ordinary identifier character in a source name
    // ([JLS §3.8]), which is why the rewrite only touches a classfile name.
    match method.owner.as_fqn() {
        Some(fqn) => Name::new(&fqn.as_str().replace('$', "."))
            .simple_name()
            .to_owned(),
        // §6.7: a local declaration has no canonical name — it is declared
        // under its own simple name.
        None => method.owner.simple_name(db).as_str().to_owned(),
    }
}

/// The declarations a recorded body resolution names, through the classpath
/// (mirrors [`members_of_owner`]). A constructor selection ([§15.9],
/// [§8.8.7.1]) is looked up as the constructor it resolved to instead, and an
/// ambiguous invocation ([§15.12.2.5]) with every declaration it denotes.
fn member_resolution(
    db: &RootDatabase,
    file: FileId,
    member: &hir_ty::ResolvedMember,
    reference: Reference,
) -> Vec<Resolution> {
    match member {
        hir_ty::ResolvedMember::Method(method) => method_resolution(db, file, method, reference),
        hir_ty::ResolvedMember::Field(field) => declared_resolution(
            db,
            file,
            field.name.clone(),
            Use::Field,
            Params::Unknown,
            field.owner_file.zip(field.decl_item),
            &field.owner,
        ),
        hir_ty::ResolvedMember::Local(_) => Vec::new(),
        // The invocation selected no declaration — the applicable candidates
        // tie ([§15.12.2.5]), or none is applicable ([§15.12.2]). The reference
        // still names each of them, and navigation is not a compile check, so
        // every one of them is a definition.
        hir_ty::ResolvedMember::Unresolved(methods) => methods
            .iter()
            .flat_map(|method| method_resolution(db, file, method, reference))
            .collect(),
    }
}

/// The declaration a recorded method resolution names ([§15.12.2]).
fn method_resolution(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    reference: Reference,
) -> Vec<Resolution> {
    declared_resolution(
        db,
        file,
        member_decl_name(db, method, reference),
        member_use_kind(reference),
        // §15.12.2.2: the member the invocation resolved to — its classfile
        // descriptor and parameter types name one declaration, where the
        // count alone would answer the first overload of that arity.
        Params::Recorded {
            types: &method.params,
            descriptor: method.descriptor.as_ref(),
        },
        // Only a *constructor declaration* answers a constructor selection:
        // the recorded item of an inference fallback (a method named like the
        // class) is not one, so the lookup below finds the constructor — or
        // the class, when it declares none.
        method
            .owner_file
            .zip(method.decl_item)
            .filter(|(decl_file, item)| {
                reference != Reference::Constructor || is_constructor_decl(db, *decl_file, *item)
            }),
        &method.owner,
    )
}

/// The declarations the member `name` of the declaring class `owner` denotes:
/// the declaration's own item when the resolution carries one (a source
/// declaration), or the member of the class through the classpath otherwise.
fn declared_resolution(
    db: &RootDatabase,
    file: FileId,
    name: String,
    use_kind: Use,
    params: Params<'_>,
    source_decl: Option<(FileId, ItemId)>,
    owner: &hir_ty::ClassKey,
) -> Vec<Resolution> {
    if let Some((decl_file, item)) = source_decl {
        return vec![Resolution::Decl {
            file: decl_file,
            item,
            name,
        }];
    }
    // §6.7: a *local* declaring class has no canonical name to look up
    // through the classpath — its own declaration answers.
    if let Some(class) = owner.source() {
        return vec![member_or_owner(
            db, class.file, class.item, &name, use_kind, params,
        )];
    }
    // Otherwise a library member: `owner` is the *binary* FQN of the
    // declaring class, which is the key the classfile index and the source
    // index are looked up by.
    let Some(owner) = owner.as_fqn() else {
        return Vec::new();
    };
    match owner_lookup(db, file, owner.as_str()) {
        OwnerLookup::Source(class) => vec![member_or_owner(
            db, class.file, class.item, &name, use_kind, params,
        )],
        OwnerLookup::Library {
            decl: hir::LibrarySourceDecl::Loaded { file, item },
            ..
        } => vec![member_or_owner(db, file, item, &name, use_kind, params)],
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Pending { entry, path },
            ..
        } => match hir::library_sources(db, library) {
            Some(sources) => vec![Resolution::Pending(LibraryFileRef::Source {
                library,
                archive: sources.archive,
                entry,
                path,
            })],
            None => Vec::new(),
        },
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Decompiled { class, path },
            ..
        } => vec![Resolution::Pending(LibraryFileRef::Decompile {
            library,
            class,
            path,
        })],
        OwnerLookup::Unresolved => Vec::new(),
    }
}

/// The declaration the member `name` of the class declared by `owner_item` in
/// `decl_file` denotes: the member's own item, the record component that
/// *implicitly* declares it ([`component_member`]), or the owner declaration
/// itself when the member has neither — an implicit constructor, an implicit
/// `equals`/`hashCode`/`toString`.
fn member_or_owner(
    db: &RootDatabase,
    decl_file: FileId,
    owner_item: ItemId,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> Resolution {
    if let Some(component) = component_member(db, decl_file, owner_item, name, use_kind, params) {
        return component;
    }
    let tree = hir::java_item_tree(db, decl_file);
    Resolution::Decl {
        file: decl_file,
        item: member_item(db, decl_file, &tree, owner_item, name, use_kind, params)
            .unwrap_or(owner_item),
        name: name.to_owned(),
    }
}

/// The record component ([JLS
/// §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1))
/// that implicitly declares the member `name` of the class declared by
/// `owner_item` in `decl_file`: the private final field a component declares
/// ([§8.10.1]) or its public accessor
/// ([§8.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.3)).
///
/// The HIR carries a component without an item of its own — the record's
/// declaration holds the component list (`RecordData::components`) and nothing
/// in the arena addresses one — so the field and the accessor the class
/// synthesizes from a component (`hir_ty`'s `source_class_fields` /
/// `source_class_methods`) have no declaration to point at. The component *is*
/// their declaration, exactly: `§8.10.1` gives each component one field of its
/// own name and `§8.10.3` one accessor of its own name, and two components of
/// one record may not share a name (`§8.10.1`), so the name identifies the
/// component. Only the zero-argument method is the accessor; a same-name
/// method with parameters is the record's own declaration, which has an item
/// and therefore never reaches this lookup.
///
/// `None` when the owner declares no such component — a class that is not a
/// record, a member the record itself declares, an implicit constructor or an
/// implicit `equals`/`hashCode`/`toString`.
fn component_member(
    db: &RootDatabase,
    decl_file: FileId,
    owner_item: ItemId,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> Option<Resolution> {
    let tree = hir::java_item_tree(db, decl_file);
    let ItemData::Record(record) = tree.data(owner_item) else {
        return None;
    };
    match use_kind {
        // §8.10.1: the component's private final field, whatever its signature.
        Use::Field => {}
        // §8.10.3: the component's accessor, which the class declares only when
        // its body declares no method of the component's own signature — so the
        // member must take no arguments. `Params::Unknown` (a member lookup
        // with no recorded invocation) cannot contradict that: the caller
        // reaches here only after finding no item of the name, and no *declared*
        // zero-argument method of the name would then be left.
        Use::Method => match params {
            Params::Recorded { types, .. } if !types.is_empty() => return None,
            _ => {}
        },
        // §8.10.4: a canonical constructor is declared by the record, and the
        // component list supplies its parameters — the component declares no
        // constructor of its own.
        Use::Constructor => return None,
    }
    let index = record
        .components
        .iter()
        .position(|component| component.name.as_str() == name)?;
    let component = &record.components[index];
    let map = hir::hir_def::db::ast_id_map(db, decl_file, tree.language);
    let source = base_db::parse(db, decl_file, tree.language).syntax_node(tree.language);
    let range = hir::hir_def::java::ranges::component_name_range(map, &source, component)?;
    Some(Resolution::Variable {
        file: decl_file,
        range,
        name: name.to_owned(),
    })
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
    let tree = hir::java_item_tree(db, file);
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

/// The library files a reference in a Java file resolves into but which are not
/// loaded into the database yet, in resolution order — the Java arm of
/// [`super::pending_library_files`], whose doc states the contract.
pub(super) fn pending_library_files(
    db: &RootDatabase,
    file: FileId,
    offset: TextSize,
) -> Vec<LibraryFileRef> {
    // The recorded resolution names the declaring source; a declaration-side
    // reference names its own; and the classpath walk names every unloaded
    // owner along a member's hierarchy, so a hover — which still resolves
    // through [`resolve_at`] — materializes everything it needs in one round.
    // One load per file: the walk reaches the same owner from several types.
    let mut seen: FxHashSet<(hir::LibraryId, Arc<str>)> = FxHashSet::default();
    let mut out = Vec::new();
    for resolution in recorded_reference(db, file, offset)
        .into_iter()
        .chain(declaration_reference(db, file, offset))
        .chain(annotation_reference(db, file, offset))
        .chain(resolve_at(db, file, offset))
    {
        let pending = match resolution {
            Resolution::Pending(pending) => pending,
            // A member the classfile stub declares on a *sourceless* owner:
            // hover renders its signature without the source, but a location
            // needs the decompiled file, so the ref is collected here.
            Resolution::LibraryMember {
                library,
                decl: hir::LibrarySourceDecl::Decompiled { class, path },
                ..
            } => LibraryFileRef::Decompile {
                library,
                class,
                path,
            },
            _ => continue,
        };
        if seen.insert(pending.load_key()) {
            out.push(pending);
        }
    }
    out
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
    /// parameter, a pattern binding, a lambda parameter, a type parameter or a
    /// record component
    /// ([JLS §8.10.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.1))
    /// — at the range of its own name, not of the declaration it was written
    /// in (`Base b` and `int x = 0` target `b` and `x`).
    ///
    /// A record component is the one such declaration a *different* file can
    /// name: its private final field is read inside the record, but its
    /// accessor
    /// ([§8.10.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.3))
    /// is public, so `p.x()` in another file names it too
    /// ([`is_workspace_visible`] tells the two apart).
    Variable {
        file: FileId,
        range: TextRange,
        name: String,
    },
    /// A resolved library member: everything the signature renderer needs plus
    /// where its declaring source is, if it is loaded. `descriptor` is the
    /// classfile identity the resolution selected ([JVMS §4.6]), which the
    /// renderer matches the stub by — `None` only for a member the walk could
    /// not resolve through a signature.
    LibraryMember {
        library: hir::LibraryId,
        owner_fqn: Name,
        name: String,
        use_kind: Use,
        descriptor: Option<SmolStr>,
        decl: hir::LibrarySourceDecl,
    },
    /// A library file that has to be materialized before the reference can be
    /// answered: an archive entry to read, or a class to decompile.
    Pending(LibraryFileRef),
}

/// The class a `this` or `super` *keyword* at `offset` denotes
/// ([JLS §15.8.3]/[§15.8.4]): the enclosing class for a bare `this`, the
/// direct superclass for a bare `super` ([§8.1.4]), and the class or interface
/// a qualified `TypeName.this`/`TypeName.super` writes ([§15.11.2]).
///
/// `None` when the innermost expression at the offset is no such keyword. An
/// explicit constructor invocation is lowered to [`ExprData::CtorCall`], not
/// to a keyword expression, so `this(args)`/`super(args)` are answered from the
/// recorded table like a class instance creation.
///
/// The step runs before the recorded table is read for a reason: a keyword
/// written as a receiver — `super.m()`, `this.f` — lies inside the range of
/// the member access, whose recorded resolution names the *member*, not the
/// class the keyword denotes. `Some` with no resolution when the keyword names
/// nothing: a qualifier that resolves to no type, a class whose superclass is
/// not on the classpath.
fn keyword_target(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<Vec<Resolution>> {
    let bodies = hir::file_body_tree(db, file);
    let expr = exprs_at(&bodies, offset).into_iter().next()?;
    let (qualifier, keyword) = match bodies.expr(expr) {
        ExprData::This { qualifier } => (qualifier, Keyword::This),
        ExprData::Super { qualifier } => (qualifier, Keyword::Super),
        _ => return None,
    };
    let tree = hir::java_item_tree(db, file);
    // §6.5.5.1: a qualified keyword's `TypeName` is resolved in the scope of
    // the declaration whose body writes it, like any written type name.
    if let Some(qualifier) = qualifier {
        let item = body_items_at(db, file, &tree, offset).first().copied();
        return Some(type_ref_name(qualifier).map_or_else(Vec::new, |name| {
            type_resolution(db, file, item, &Name::new(&name), Some(offset))
        }));
    }
    let Some(enclosing) = enclosing_class(db, file, &tree, offset) else {
        return Some(Vec::new());
    };
    let enclosing = hir_ty::ClassKey::of(&tree, file, enclosing);
    let class = match keyword {
        Keyword::This => enclosing,
        // §8.1.4/§4.10.2: the *direct* superclass is the first supertype of a
        // class. (`super` is not written in an interface, whose supertypes are
        // its superinterfaces.)
        Keyword::Super => {
            let scope = hir_ty::scope_for_file(db, file);
            let supertypes = hir_ty::supertypes(db, &scope, &enclosing.as_ty(db, Vec::new()));
            let Some(super_key) = supertypes
                .first()
                .and_then(|ty| hir_ty::ClassKey::of_ty(db, ty))
            else {
                return Some(Vec::new());
            };
            super_key
        }
    };
    Some(key_resolution(db, file, &class))
}

/// The keyword a bare or qualified `this`/`super` expression is written as.
#[derive(Debug, Clone, Copy)]
enum Keyword {
    This,
    Super,
}

/// The classpath resolution of the reference written at `offset` ([JLS §6.5]):
/// the declaration the innermost expression there denotes, resolved through the
/// classpath. The resolution an expression carries is about its own name,
/// never about an argument or a type inside it, so the walk never ascends past
/// the innermost expression ([`innermost_expr_at`]).
fn resolve_at(db: &RootDatabase, file: FileId, offset: TextSize) -> Vec<Resolution> {
    let bodies = hir::file_body_tree(db, file);
    let tree = hir::java_item_tree(db, file);
    let items = body_items_at(db, file, &tree, offset);
    let item = items.first().copied();

    let Some(expr_id) = innermost_expr_at(&bodies, offset) else {
        return Vec::new();
    };
    match bodies.expr(expr_id).clone() {
        ExprData::New { ty, .. } | ExprData::ClassLit(ty) => {
            type_ref_name(&ty).map_or_else(Vec::new, |name| {
                type_resolution(
                    db,
                    file,
                    item,
                    &Name::new(&name),
                    bodies.expr_name_range(expr_id).map(|range| range.start()),
                )
            })
        }
        ExprData::InstanceOf { ty, .. } => {
            ty.as_ref()
                .and_then(|t| type_ref_name(t))
                .map_or_else(Vec::new, |name| {
                    type_resolution(
                        db,
                        file,
                        item,
                        &Name::new(&name),
                        bodies.expr_name_range(expr_id).map(|range| range.start()),
                    )
                })
        }
        // A method invocation, with an implicit `this` receiver when
        // `receiver` is empty ([JLS §15.12.1]). The member is keyed on the
        // recorded resolution ([§15.12.2]): without one the walk has no
        // signature to select by, and an unresolved invocation is not
        // guessed at by argument count.
        ExprData::MethodCall { receiver, name, .. } => {
            let body = items
                .iter()
                .find_map(|&item| hir_ty::body_types(db, file, item));
            let params = match body.as_deref().and_then(|body| body.resolved.get(&expr_id)) {
                Some(hir_ty::ResolvedMember::Method(method)) => Params::Recorded {
                    types: &method.params,
                    descriptor: method.descriptor.as_ref(),
                },
                _ => Params::Unknown,
            };
            match receiver {
                Some(receiver) => receiver_ty(db, file, &items, receiver)
                    .map_or_else(Vec::new, |ty| {
                        member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, params)
                    }),
                None => enclosing_class_receiver(db, file, &tree, offset)
                    .map_or_else(Vec::new, |ty| {
                        member_in_hierarchy(db, file, ty, name.as_str(), Use::Method, params)
                    }),
            }
        }
        // A field access, with an implicit receiver when `target` is empty.
        ExprData::FieldAccess { target, name } => match target {
            Some(target) => receiver_ty(db, file, &items, target).map_or_else(Vec::new, |ty| {
                member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, Params::Unknown)
            }),
            None => enclosing_class_receiver(db, file, &tree, offset).map_or_else(Vec::new, |ty| {
                member_in_hierarchy(db, file, ty, name.as_str(), Use::Field, Params::Unknown)
            }),
        },
        // A simple name. [JLS §6.5.2] reclassifies a contextually
        // ambiguous name: an expression name — a local, parameter or field
        // in scope — first, a type name otherwise, and a package name
        // last. A local or a field of the name is already answered from
        // the recorded table ([`recorded_reference`]), and a statically
        // imported member ([§7.5.4]) is an expression name whose declaring
        // type has to be probed here.
        //
        // The type-name step is what answers the *qualifier* of a
        // qualified name: a bare leading segment lowers to a `Var` (a
        // `LITERAL` identifier), never to a `NamePath` — `Main` in
        // `Main.field`, `System` in `System.out` — and inference records
        // the *member* the qualified access names on the enclosing
        // `FieldAccess`/`MethodCall`, not the type the qualifier denotes.
        ExprData::Var(name) => {
            let resolver = hir_ty::Resolver::for_file(&tree);
            let mut found = Vec::new();
            let mut pending = Vec::new();
            for (owner, member) in resolver.static_import_owners(name.as_str()) {
                match member_of_named_owner(
                    db,
                    file,
                    item,
                    &owner,
                    &member,
                    Use::Field,
                    Params::Unknown,
                ) {
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
            } else if !pending.is_empty() {
                // An unloaded owner may still declare the member, an
                // expression name that would win over a type of the same
                // name; materialize it and decide on the re-run.
                pending.into_iter().map(Resolution::Pending).collect()
            } else {
                // §6.5.2/§6.5.5.1: with no expression name in scope, the
                // name is reclassified as a type name when one is in scope.
                // A package name (and a type variable) resolves to no
                // declaration, so it stays unanswered.
                type_resolution(
                    db,
                    file,
                    item,
                    &name,
                    bodies.expr_name_range(expr_id).map(|range| range.start()),
                )
            }
        }
        // A qualified name in expression position: `Outer.Inner`,
        // `Type.field`. [JLS §6.5.2] reclassifies a qualified ambiguous
        // name through its prefix, so the whole text is tried as a type
        // reference first, and its last segment is then read as a member
        // of the class its prefix denotes.
        ExprData::NamePath(name) => {
            let as_type = type_resolution(
                db,
                file,
                item,
                &name,
                bodies.expr_name_range(expr_id).map(|range| range.start()),
            );
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
                            Params::Unknown,
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
        // The name written at the offset belongs to an expression that names
        // no declaration (a literal, a `this`/`super` keyword, a cast, an
        // operator): nothing here, and nothing *enclosing* it either.
        _ => Vec::new(),
    }
}

/// The type of the expression `receiver`, read from the body that owns it.
fn receiver_ty(db: &RootDatabase, file: FileId, items: &[ItemId], receiver: ExprId) -> Option<Ty> {
    items.iter().find_map(|&item| {
        hir_ty::body_types(db, file, item).and_then(|body| body.exprs.get(&receiver).cloned())
    })
}

/// The receiver type of an implicit-`this` member access: the enclosing
/// class-like declaration of the offset — a *local* one included ([§6.7]).
fn enclosing_class_receiver(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Option<Ty> {
    let enclosing = enclosing_class(db, file, tree, offset)?;
    Some(hir_ty::ClassKey::of(tree, file, enclosing).as_ty(db, Vec::new()))
}

/// The innermost class-like declaration whose range contains `offset` — the
/// class a bare `this` is an instance of, and whose direct superclass a bare
/// `super` names.
fn enclosing_class(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Option<ItemId> {
    items_at(db, file, tree, offset)
        .into_iter()
        .find(|item| tree.data(*item).is_type())
}

/// The navigation resolutions of the class `key` denotes: the declaration of a
/// source class — a *local* one ([JLS §14.3]) included — or the library source
/// the classpath class lives in.
fn key_resolution(db: &RootDatabase, file: FileId, key: &hir_ty::ClassKey) -> Vec<Resolution> {
    match key {
        hir_ty::ClassKey::Named(fqn) => class_resolution(db, file, fqn),
        hir_ty::ClassKey::Local(class) => {
            let tree = hir::java_item_tree(db, class.file);
            let Some(name) = tree.data(class.item).name() else {
                return Vec::new();
            };
            vec![Resolution::Decl {
                file: class.file,
                item: class.item,
                name: name.as_str().to_owned(),
            }]
        }
    }
}

/// Whether the declaration `item` has no canonical name ([JLS §6.7]): it is a
/// *local* declaration ([JLS §14.3]) or nested in one.
fn is_local(tree: &ItemTree, item: ItemId) -> bool {
    let mut current = Some(item);
    while let Some(id) = current {
        if tree.is_local_type(id) {
            return true;
        }
        current = tree.parent_of(id);
    }
    false
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
    params: Params<'_>,
) -> Vec<Resolution> {
    let scope = hir_ty::scope_for_file(db, file);
    let mut queue = VecDeque::from([receiver]);
    let mut seen: FxHashSet<Name> = FxHashSet::default();
    let mut pending: Vec<LibraryFileRef> = Vec::new();
    while let Some(ty) = queue.pop_front() {
        // A primitive or array receiver has no reference type to search.
        let Some((fqn, _)) = ty.as_reference(db) else {
            continue;
        };
        if !seen.insert(fqn.clone()) {
            continue;
        }
        match members_of_owner(db, file, fqn.as_str(), name, use_kind, params) {
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
    /// constructor, a record accessor's component).
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
    PendingSource(LibraryFileRef),
    Absent,
}

/// The declared member named `name` of the source class `owner`: its own
/// body's items ([`ItemTree::body`]), then the member a record component
/// implicitly declares ([`component_member`]). Read from the declaration
/// rather than from a canonical name, so a *local* owner
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// — which has none ([§6.7]) — answers too.
fn members_of_source_owner(
    db: &RootDatabase,
    owner: hir::SourceClass,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> MemberLookup {
    let owner_file = owner.file;
    let tree = hir::java_item_tree(db, owner_file);
    if let Some(item) = member_item(db, owner_file, &tree, owner.item, name, use_kind, params) {
        return MemberLookup::Found(Resolution::Decl {
            file: owner_file,
            item,
            name: name.to_owned(),
        });
    }
    // A member the class declares without an item of its own: the field or the
    // accessor a record component declares ([`component_member`]). Everything
    // else — an implicit constructor, an implicit `hashCode` — has no
    // declaration to point at, and a *loaded* owner's member set is
    // conclusive.
    match component_member(db, owner_file, owner.item, name, use_kind, params) {
        Some(component) => MemberLookup::Found(component),
        None => MemberLookup::Absent,
    }
}

/// The declared member named `name` of the owner class `owner_fqn`, which must
/// already be canonical ([JLS §6.7]).
fn members_of_owner(
    db: &RootDatabase,
    file: FileId,
    owner_fqn: &str,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> MemberLookup {
    match owner_lookup(db, file, owner_fqn) {
        OwnerLookup::Source(class) => members_of_source_owner(db, class, name, use_kind, params),
        OwnerLookup::Library {
            library,
            fqn,
            decl:
                hir::LibrarySourceDecl::Loaded {
                    file: decl_file,
                    item: owner_item,
                },
        } => {
            let tree = hir::java_item_tree(db, decl_file);
            match member_item(db, decl_file, &tree, owner_item, name, use_kind, params) {
                Some(item) => MemberLookup::Found(Resolution::LibraryMember {
                    library,
                    owner_fqn: fqn,
                    name: name.to_owned(),
                    use_kind,
                    descriptor: params.descriptor().cloned(),
                    decl: hir::LibrarySourceDecl::Loaded {
                        file: decl_file,
                        item,
                    },
                }),
                // A library record's accessor and field are classfile members
                // javac emitted from its components; their declaration is the
                // component the loaded source declares.
                None => match component_member(db, decl_file, owner_item, name, use_kind, params) {
                    Some(component) => MemberLookup::Found(component),
                    None => MemberLookup::Absent,
                },
            }
        }
        // The owning source is not loaded, so nothing about its members is
        // known: the file has to be read before the member set can be.
        OwnerLookup::Library {
            library,
            decl: hir::LibrarySourceDecl::Pending { entry, path },
            ..
        } => match hir::library_sources(db, library) {
            Some(sources) => MemberLookup::PendingSource(LibraryFileRef::Source {
                library,
                archive: sources.archive,
                entry,
                path,
            }),
            None => MemberLookup::Absent,
        },
        // The owner ships no sources at all, so its member *set* is only known
        // once the class has been decompiled — but the classfile stub already
        // knows which members exist and how they are typed, which is all a
        // rendered signature needs. A member the stub declares is answered as
        // such (hover renders it without starting a JVM) while its *location*
        // still waits for the decompiler (goto-definition defers on the ref).
        OwnerLookup::Library {
            library,
            fqn,
            decl: hir::LibrarySourceDecl::Decompiled { class, path },
        } => {
            if library_declares_member(db, library, &fqn, name, use_kind, params) {
                MemberLookup::Found(Resolution::LibraryMember {
                    library,
                    owner_fqn: fqn,
                    name: name.to_owned(),
                    use_kind,
                    descriptor: params.descriptor().cloned(),
                    decl: hir::LibrarySourceDecl::Decompiled { class, path },
                })
            } else {
                MemberLookup::Absent
            }
        }
        OwnerLookup::Unresolved => MemberLookup::Absent,
    }
}

/// Whether the classfile stub of `owner_fqn` declares the member and signature
/// `params` selects — the only way to ask a sourceless owner about its members
/// without running a decompiler over it.
fn library_declares_member(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> bool {
    let Some(record) = library_owner_stub(db, library, owner_fqn) else {
        return false;
    };
    let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
        return false;
    };
    let interner = &db.hir_state().interner;
    match use_kind {
        Use::Field => stub
            .fields
            .iter()
            .any(|field| interner.resolve(&field.name) == name),
        // §8.8/[JVMS §4.6]: a constructor is declared `<init>` in a classfile,
        // whatever the source calls it.
        Use::Method | Use::Constructor => {
            let classfile_name = match use_kind {
                Use::Constructor => "<init>",
                _ => name,
            };
            let mut named = stub
                .methods
                .iter()
                .filter(|method| interner.resolve(&method.name) == classfile_name);
            match params.descriptor() {
                // The classfile descriptor is the member's identity
                // ([JVMS §4.6]): this names exactly the declaration the
                // resolution selected.
                Some(descriptor) => {
                    named.any(|method| interner.resolve(&method.descriptor) == descriptor.as_str())
                }
                // No recorded signature: a name that denotes exactly one
                // method is answerable, an overloaded one is not guessed at.
                None => named.next().is_some() && named.next().is_none(),
            }
        }
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
    params: Params<'_>,
) -> MemberLookup {
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, owner_name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        // §6.7: a local declaration has no canonical name to resolve a member
        // through; its members are read from its own declaration.
        hir_ty::NameResolution::ResolvedLocal(class) => {
            return members_of_source_owner(db, class, name, use_kind, params);
        }
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return MemberLookup::Absent,
    };
    members_of_owner(db, file, fqn.as_str(), name, use_kind, params)
}

/// The class `owner_fqn` denotes in `file`'s scope, and where its source is.
fn owner_lookup(db: &RootDatabase, file: FileId, owner_fqn: &str) -> OwnerLookup {
    let scope = hir_ty::scope_for_file(db, file);
    let Some(resolved) = hir::fqn_resolve(db, &scope, owner_fqn) else {
        return OwnerLookup::Unresolved;
    };
    match &resolved {
        hir::Resolved::Source(class) => OwnerLookup::Source(*class),
        // A Kotlin file's facade owns nothing a Java navigation can enter — its
        // members are the file's top-level declarations, which the Java layer
        // answers for a *member* lookup, not for navigation into a body.
        hir::Resolved::Facade { .. } => OwnerLookup::Unresolved,
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

/// The signature a member lookup keys on: what the JLS resolution
/// ([JLS §15.12.2]) recorded for the reference, or nothing for a reference
/// that names no invocation.
#[derive(Debug, Clone, Copy)]
enum Params<'a> {
    /// Nothing: a field access, or a name-only reference — a static import, an
    /// `import` clause — with no invocation whose resolution could be read. A
    /// name that denotes exactly one member is still answerable; an overloaded
    /// one is not guessed at.
    Unknown,
    /// The declaration the resolution selected ([`hir_ty::MethodData`]): its
    /// classfile descriptor for a library member
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)),
    /// its parameter types for a source one. The descriptor is the member's
    /// identity on the classpath ([JVMS §4.6]), and erasure ([JLS §4.6]) names
    /// a source declaration uniquely because no two members of one class share
    /// an erasure ([§8.4.2]) — so the lookup names one declaration where a
    /// parameter *count* would answer the first overload of that arity
    /// (`new ArrayList<>(c)` is `ArrayList(Collection)`, never the
    /// `ArrayList(int)` of the same arity).
    Recorded {
        types: &'a [Ty],
        descriptor: Option<&'a SmolStr>,
    },
}

impl Params<'_> {
    /// The classfile identity of the selected member, when the resolution
    /// carried one.
    fn descriptor(&self) -> Option<&SmolStr> {
        match self {
            Params::Unknown => None,
            Params::Recorded { descriptor, .. } => *descriptor,
        }
    }
}

/// The item of the member `name` declared by `owner` that `params` selects.
/// The candidates are the owner's own members ([`ItemData::body`]) — not the
/// file's symbol set, which lists no *local* declaration ([JLS §6.7]: a local
/// class has no canonical name and is not indexed) and no member of one.
/// A name that denotes exactly one declaration needs no signature to select
/// it; an overloaded name the walk could not resolve is left unanswered rather
/// than guessed by declaration order.
fn member_item(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    owner: ItemId,
    name: &str,
    use_kind: Use,
    params: Params<'_>,
) -> Option<ItemId> {
    let candidates: Vec<ItemId> = tree
        .data(owner)
        .body()
        .iter()
        .copied()
        .filter(|item| {
            let data = tree.data(*item);
            let kind_matches = match use_kind {
                Use::Field => {
                    matches!(data, ItemData::Field(_) | ItemData::EnumConstant(_))
                }
                Use::Method => matches!(data, ItemData::Method(_)),
                // §8.8: a constructor is a method of its class by name and
                // parameter list, but no other declaration is one — `void C()`
                // in `class C` is a method that carries the class's name.
                Use::Constructor => {
                    matches!(data, ItemData::Method(method) if method.is_constructor())
                }
            };
            kind_matches
                && data
                    .name()
                    .is_some_and(|declared| declared.as_str() == name)
        })
        .collect();
    let selected = match params {
        Params::Recorded { types, .. } => candidates
            .iter()
            .find(|item| declares_params(db, file, **item, types))
            .copied(),
        Params::Unknown => None,
    };
    selected.or_else(|| (candidates.len() == 1).then(|| candidates[0]))
}

/// Whether the declaration at `item` takes exactly the parameter types
/// `expected` — compared *erased* ([JLS §4.6]), because the resolution holds
/// the declaring type's arguments substituted where the declaration writes its
/// own type parameters.
fn declares_params(db: &RootDatabase, file: FileId, item: ItemId, expected: &[Ty]) -> bool {
    let mut declared = hir_ty::method_params(db, file, item);
    if declared.len() != expected.len() {
        return false;
    }
    // §8.4.1: a variable-arity parameter resolves to its *element* type, while
    // the resolution recorded the array type its signature erases to.
    let tree = hir::java_item_tree(db, file);
    let varargs = matches!(
        tree.data(item),
        ItemData::Method(method) if method.sig.params.last().is_some_and(|param| param.varargs)
    );
    if varargs && let Some(last) = declared.last_mut() {
        *last = Ty::array(db, *last);
    }
    declared
        .iter()
        .zip(expected)
        .all(|(declared, expected)| declared.erasure(db) == expected.erasure(db))
}

/// The classpath resolution of the type name written at `item` in `file`.
fn type_resolution(
    db: &RootDatabase,
    file: FileId,
    item: Option<ItemId>,
    name: &Name,
    at: Option<TextSize>,
) -> Vec<Resolution> {
    // §14.3/[§6.3]: a *local* class-like declaration is in scope positionally —
    // from its declaration to the end of its block — so the item's own scope
    // map (which describes a declaration's scope, not a body's) cannot answer
    // a reference a body writes. A reference at a known range is answered from
    // the local declarations whose declaring body encloses it and whose own
    // declaration precedes it.
    if let (Some(item), Some(at)) = (item, at) {
        let tree = hir::java_item_tree(db, file);
        if let Some(local) = local_type_in_scope(db, file, &tree, item, at, name) {
            return vec![Resolution::Decl {
                file,
                item: local,
                name: name.simple_name().to_owned(),
            }];
        }
    }
    // §7.4.3: a name that exists on the classpath but is not visible from the
    // file's module still denotes that class — navigation is not a compile
    // check.
    let fqn = match hir_ty::resolve_type_name_at(db, file, item, name) {
        hir_ty::NameResolution::Resolved(fqn) | hir_ty::NameResolution::NotAccessible(fqn) => fqn,
        // §14.3/[§6.7]: a local declaration is denoted by its declaration, not
        // by a name the workspace could resolve anywhere else.
        hir_ty::NameResolution::ResolvedLocal(class) => {
            return vec![Resolution::Decl {
                file: class.file,
                item: class.item,
                name: name.simple_name().to_owned(),
            }];
        }
        hir_ty::NameResolution::TypeVar
        | hir_ty::NameResolution::Ambiguous(_)
        | hir_ty::NameResolution::Unresolved => return Vec::new(),
    };
    class_resolution(db, file, &fqn)
}

/// The *local* class-like declaration named `name` that a reference at
/// `offset` inside the body owned by `item` may denote ([JLS §6.3]): a
/// declaration whose own declaring body encloses the reference — the
/// declaration's owner is `item` or an ancestor of it in the item tree — and
/// whose declaration precedes it. The innermost (latest) such declaration wins
/// ([§6.4.1]).
///
/// Deliberately positional rather than exact: navigation is not a compile
/// check, so a same-named declaration of a *sibling* block that follows is not
/// distinguished from one in scope. The type layer's own resolution is exact.
fn local_type_in_scope(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    item: ItemId,
    offset: TextSize,
    name: &Name,
) -> Option<ItemId> {
    let mut best: Option<(TextSize, ItemId)> = None;
    for &local in &tree.local_types {
        if tree.data(local).name() != Some(name) {
            continue;
        }
        let Some(owner) = tree.parent_of(local) else {
            continue;
        };
        if owner != item && !encloses(tree, owner, item) {
            continue;
        }
        let Some(range) = item_range(db, file, tree, local) else {
            continue;
        };
        if range.start() >= offset {
            continue;
        }
        if best.is_none_or(|(start, _)| range.start() > start) {
            best = Some((range.start(), local));
        }
    }
    best.map(|(_, local)| local)
}

/// Whether the declaration `ancestor` encloses `item` (or is `item`).
fn encloses(tree: &ItemTree, ancestor: ItemId, item: ItemId) -> bool {
    let mut current = Some(item);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = tree.parent_of(id);
    }
    false
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
        // A facade has no declaration to navigate to.
        hir::Resolved::Facade { .. } => Vec::new(),
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
            vec![Resolution::Pending(LibraryFileRef::Source {
                library,
                archive,
                entry,
                path,
            })]
        }
        // The class ships no sources: the decompiler has to produce (and the
        // caller materialize) its declaring view before it can be answered.
        Some(hir::LibrarySourceDecl::Decompiled { class, path }) => {
            vec![Resolution::Pending(LibraryFileRef::Decompile {
                library,
                class,
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
    let tree = hir::java_item_tree(db, decl_file);
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

/// The parsed classfile stub of the class `owner_fqn` in `library`, or `None`
/// when the library has no such class.
///
/// The class index is keyed by binary names
/// ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)),
/// which is the spelling `Resolved::fqn` hands out for a library class.
fn library_owner_stub(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
) -> Option<Arc<hir::ClassOrModuleRecord>> {
    let symbol = db.hir_state().interner.get_or_intern(owner_fqn.as_str());
    let index = hir::library_name_index(db, library);
    let (entry_idx, entry) = index.lookup(symbol)?;
    hir::class_record(
        db,
        &hir::ResolvedClass {
            library,
            entry_idx,
            entry: entry.clone(),
        },
    )
}

/// The rendered signature of a resolved library member: types, flags and the
/// return type come from the classfile stub (the authority); parameter names
/// are the classfile's `MethodParameters` names when it has them
/// ([JVMS §4.7.24](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.24))
/// and the selected source declaration's names at the same index otherwise,
/// and `arg{i}` when neither is available.
///
/// Renders as `ret name(T p, T p2)` for a method and `type name` for a field —
/// the shape a Java declaration reads as, rather than a classfile descriptor.
/// The member is the one the resolution selected: `descriptor` is the
/// classfile identity it recorded ([JVMS §4.6]), which names one stub method
/// where a parameter count would render the first overload of that arity.
///
/// A classfile carries no documentation ([JVMS §4.7] has no comment
/// attribute), so the documentation is whatever the member's *loaded source*
/// declares — and a sourceless library has none: the signature renders without
/// it, and nothing starts a JVM for a hover.
///
/// [JVMS §4.7]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7
fn library_member_signature(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
    name: &str,
    use_kind: Use,
    descriptor: Option<&str>,
    source: Option<(FileId, ItemId)>,
) -> Option<HoverInfo> {
    let record = library_owner_stub(db, library, owner_fqn)?;
    let hir::ClassOrModuleStub::Class(stub) = record.as_ref() else {
        return None;
    };
    let interner = &db.hir_state().interner;

    let value = match use_kind {
        Use::Field => {
            let field = stub
                .fields
                .iter()
                .find(|field| interner.resolve(&field.name) == name)?;
            let field_ty = hir_ty::ty_from_library(db, &field.field_type);
            let ty = field_ty.display_simple(db);
            format!("{ty} {name}")
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
                    && descriptor
                        .is_none_or(|descriptor| interner.resolve(&method.descriptor) == descriptor)
            })?;
            let head = match use_kind {
                Use::Constructor => String::new(),
                _ => {
                    let return_ty = hir_ty::ty_from_library(db, &method.return_type);
                    format!("{} ", return_ty.display_simple(db))
                }
            };
            let params: Vec<String> = method
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    let param_ty = hir_ty::ty_from_library(db, &param.param_type);
                    let ty = param_ty.display_simple(db);
                    let name = param
                        .name
                        .map(|symbol| interner.resolve(&symbol).to_owned())
                        .or_else(|| source_parameter_name(db, source, index))
                        .unwrap_or_else(|| format!("arg{index}"));
                    format!("{ty} {name}")
                })
                .collect();
            format!("{head}{name}({})", params.join(", "))
        }
    };

    Some(HoverInfo {
        value,
        docs: source.and_then(|(file, item)| crate::docs::hover_docs(db, file, item)),
    })
}

/// The hover of a resolved library member.
///
/// The declaration is where its source is: a *loaded* source contributes the
/// parameter *names* the classfile may omit (and its documentation, which no
/// classfile carries); a *sourceless* owner contributes none — only its
/// parameter names would come out of the decompiled file, and they are not
/// worth a JVM start on hover, the classfile stub already rendering the
/// signature. A *pending* source is materialized and the request re-run
/// instead, so `None` is the deferral the LSP layer drives.
fn library_member_hover(
    db: &RootDatabase,
    library: hir::LibraryId,
    owner_fqn: &Name,
    name: &str,
    use_kind: Use,
    descriptor: Option<&str>,
    decl: &hir::LibrarySourceDecl,
) -> Option<HoverInfo> {
    let source = match decl {
        hir::LibrarySourceDecl::Loaded { file, item } => Some((*file, *item)),
        hir::LibrarySourceDecl::Decompiled { .. } => None,
        hir::LibrarySourceDecl::Pending { .. } => return None,
    };
    library_member_signature(db, library, owner_fqn, name, use_kind, descriptor, source)
}

/// The parameter names the declaration an invocation selected writes
/// ([JLS §8.4.1]), in order — or `None` when that declaration is no *loaded
/// source* one.
///
/// A library member's names are read back from its declaring *source* (a
/// classfile records them only in a `MethodParameters` attribute this server
/// does not read), through the same lookup goto-definition resolves the member
/// with — so a name can only come from the declaration a click on the
/// invocation would open. A library whose source is not materialized yet (a
/// `Pending` archive entry, a class a decompiler would have to produce), or a
/// synthesized implicit member, answers `None` rather than a guessed name.
///
/// The inlay-hint layer asks for these when the type layer recorded none: a
/// source declaration carries its own names already, so only a classpath member
/// reaches here.
///
/// Resolving them costs the library's source layout and, for a class whose
/// source is loaded, that file's symbols. Both the member's answer and the
/// layout are therefore persisted ([`hir::cached_member_params`],
/// [`hir::lmdb_store`]), and what is *stable* is what is persisted:
///
/// * a loaded declaring source is the authority, so its answer — names, or
///   none because it declares no such member — is written back;
/// * a library that ships no sources and no decompiler can never answer, which
///   is written back as "no names" under a stamp that changes the day sources
///   or a decompiler are configured for it;
/// * a declaring view that exists but is not materialized yet is *not* written
///   back: it can name the member once it is, so the miss stays a miss.
///
/// The pending case is not lost: [`pending_parameter_names`] reports the file
/// that has to be loaded for it, which the inlay-hint layer defers on — so a
/// library member's names render on the first request rather than only once its
/// source happens to be open.
pub(super) fn declared_parameter_names(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    constructor: bool,
) -> Option<Vec<String>> {
    match declared_parameter_names_of(db, file, method, constructor) {
        DeclaredParameterNames::Names(names) => Some(names),
        DeclaredParameterNames::Pending(_) | DeclaredParameterNames::Unavailable => None,
    }
}

/// The library file that has to be loaded before the member the invocation
/// `method` selected can be named — the pending source of its declaring class,
/// when there is one. The inlay-hint path drives the load with it, exactly as
/// goto-definition and hover defer through [`pending_library_files`]; `None`
/// when no load can name the member.
pub(super) fn pending_parameter_names(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    constructor: bool,
) -> Option<LibraryFileRef> {
    match declared_parameter_names_of(db, file, method, constructor) {
        DeclaredParameterNames::Pending(file) => Some(file),
        DeclaredParameterNames::Names(_) | DeclaredParameterNames::Unavailable => None,
    }
}

/// What the parameter-name lookup found for the declaration an invocation
/// selected.
enum DeclaredParameterNames {
    /// The declaration's own names, in order.
    Names(Vec<String>),
    /// The declaring source is in the library's archive but not materialized:
    /// the caller can load it and ask again.
    Pending(LibraryFileRef),
    /// No source this session reads will ever name the member: the library
    /// ships none and no decompiler is configured for it, its declaring view
    /// is decompiled rather than sourced, or the member is synthesized.
    Unavailable,
}

fn declared_parameter_names_of(
    db: &RootDatabase,
    file: FileId,
    method: &hir_ty::MethodData,
    constructor: bool,
) -> DeclaredParameterNames {
    let reference = if constructor {
        Reference::Constructor
    } else {
        Reference::Member
    };
    // A classpath member is always named and described; one without either is
    // no library member, and no declaring source to read names from exists.
    // Every early exit past this point that finds no source is `Unavailable`.
    let unavailable = DeclaredParameterNames::Unavailable;
    let (Some(owner), Some(descriptor)) = (method.owner.as_fqn(), method.descriptor.as_deref())
    else {
        return unavailable;
    };
    let Some(library) = library_of(db, file, owner.as_str()) else {
        return unavailable;
    };
    if let Some(cached) =
        hir::cached_member_params(db, library, owner.as_str(), &method.name, descriptor)
    {
        return cached
            .into_names()
            .map_or(unavailable, DeclaredParameterNames::Names);
    }
    let Some(decl) = hir::library_source_decl(db, library, owner.as_str()) else {
        // Nothing will ever declare the class from a source this session reads:
        // the library ships no sources, and no decompiler is configured for it.
        hir::cache_member_params(db, library, owner.as_str(), &method.name, descriptor, None);
        return unavailable;
    };
    let (decl_file, item) = match decl {
        hir::LibrarySourceDecl::Loaded { file, item } => (file, item),
        // The declaring view is not materialized yet. A sourced one can name
        // the member once it is loaded, so the caller is handed the file to
        // materialize; a decompiled one would start a JVM the hint path does
        // not drive.
        hir::LibrarySourceDecl::Pending { entry, path } => {
            return match hir::library_sources(db, library) {
                Some(sources) => DeclaredParameterNames::Pending(LibraryFileRef::Source {
                    library,
                    archive: sources.archive,
                    entry,
                    path,
                }),
                None => unavailable,
            };
        }
        hir::LibrarySourceDecl::Decompiled { .. } => return unavailable,
    };
    let tree = hir::java_item_tree(db, decl_file);
    let member = member_item(
        db,
        decl_file,
        &tree,
        item,
        &member_decl_name(db, method, reference),
        member_use_kind(reference),
        Params::Recorded {
            types: &method.params,
            descriptor: method.descriptor.as_ref(),
        },
    );
    let names = member.and_then(|member| match tree.data(member) {
        ItemData::Method(method) => Some(
            method
                .sig
                .params
                .iter()
                .map(|param| param.name.as_str().to_owned())
                .collect::<Vec<_>>(),
        ),
        // The member resolved to its owner — an implicit constructor, an
        // implicit `equals`/`hashCode`/`toString` — which declares no
        // parameters of its own.
        _ => None,
    });
    hir::cache_member_params(
        db,
        library,
        owner.as_str(),
        &method.name,
        descriptor,
        names.as_deref(),
    );
    names.map_or(unavailable, DeclaredParameterNames::Names)
}

/// The library the class-like type `owner_fqn` denotes in `file`'s scope —
/// `None` for a name that resolves to no library class, which is the only case
/// a classpath member's names could still be read from.
fn library_of(db: &RootDatabase, file: FileId, owner_fqn: &str) -> Option<hir::LibraryId> {
    let scope = hir_ty::scope_for_file(db, file);
    match hir::fqn_resolve(db, &scope, owner_fqn)? {
        hir::Resolved::Library(class) => Some(class.library),
        hir::Resolved::Source(_) | hir::Resolved::Facade { .. } => None,
    }
}

/// The declared name of parameter `index` of the library member the resolution
/// selected, when its source declaration names one there.
fn source_parameter_name(
    db: &RootDatabase,
    source: Option<(FileId, ItemId)>,
    index: usize,
) -> Option<String> {
    let (file, item) = source?;
    let tree = hir::java_item_tree(db, file);
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

/// The hover at `offset`: the header and documentation of the declaration a
/// reference names, else the type of the expression the offset is inside, else
/// the signature of the declaration whose own *name* the offset is on. An
/// offset that names nothing — a modifier, a keyword, punctuation, whitespace,
/// a literal, an argument — answers nothing.
///
/// That last step is the one a *declaration* is asked about, and it answers
/// exactly where goto-definition's [`self_target`] does: on the declaration's
/// own name ([JLS §6.3] — a declaration's name is not a reference to itself,
/// and nothing else written in a declaration is a reference to it), so the two
/// requests agree on which declaration an offset names. A *type parameter* has
/// no item of its own, so a hover on its declaration or on a reference to it
/// (both of which a definition answers) renders nothing yet.
pub(super) fn hover(db: &RootDatabase, file: FileId, offset: TextSize) -> Option<HoverInfo> {
    let tree = hir::java_item_tree(db, file);
    let bodies = hir::file_body_tree(db, file);

    // A resolved reference outranks everything below: what the user is asking
    // about is the declaration the reference names — its header *and* its
    // documentation — the way an IDE answers a hover on a use. The resolutions
    // are the ones goto-definition uses, so the two requests always agree on
    // what a reference denotes.
    //
    // A *library* member is the one exception, and the classpath walk is asked
    // for it first: the classfile is the authority for what a library declares,
    // so its stub renders the member — with the parameter names the declaring
    // source supplies when that source is loaded — where the resolution
    // recorded from the body names the loaded source declaration itself. A
    // sourceless library has nothing else to render at all. When the declaring
    // source is not loaded yet, hover answers `None` *without* consulting the
    // fallbacks, so the LSP layer materializes the file and the retried hover
    // shows the merged signature — answering the expression's type (or the
    // bytecode-only rendering) here would hide the merge on the first, and most
    // likely only, hover. A *sourceless* owner has no source to wait for: the
    // LSP layer never defers a hover for a decompile, and such a reference is
    // either rendered from its classfile stub or answered `None`.
    if let Some(resolution) = resolve_at(db, file, offset).into_iter().next() {
        match &resolution {
            Resolution::Pending(_) => return None,
            Resolution::LibraryMember {
                library,
                owner_fqn,
                name,
                use_kind,
                descriptor,
                decl,
            } => {
                return library_member_hover(
                    db,
                    *library,
                    owner_fqn,
                    name,
                    *use_kind,
                    descriptor.as_deref(),
                    decl,
                );
            }
            // A declaration or a variable the walk found is answered by the
            // resolved declaration below, which names the same thing.
            Resolution::Decl { .. } | Resolution::Variable { .. } => {}
        }
    }
    match resolutions(db, file, offset).into_iter().next() {
        Some(Resolution::Pending(_)) => return None,
        Some(Resolution::LibraryMember {
            library,
            owner_fqn,
            name,
            use_kind,
            descriptor,
            decl,
        }) => {
            return library_member_hover(
                db,
                library,
                &owner_fqn,
                &name,
                use_kind,
                descriptor.as_deref(),
                &decl,
            );
        }
        // A declaration has the header of its own kind, plus its
        // documentation. A nameless one (an initializer, which no reference
        // names) falls through like a `Variable`: a local, a lambda parameter,
        // a type parameter and a record component are declarations without an
        // item of their own, and the steps below answer them from the
        // expression or the component list.
        Some(Resolution::Decl {
            file: decl_file,
            item,
            ..
        }) => {
            if let Some(hover) = declaration_hover(db, decl_file, item) {
                return Some(hover);
            }
        }
        Some(Resolution::Variable { .. }) | None => {}
    }

    // An expression's inferred type, from the enclosing body — walk the
    // innermost enclosing expressions first. It has no declaration of its own,
    // so it has no documentation.
    for expr_id in exprs_at(&bodies, offset) {
        for item in body_items_at(db, file, &tree, offset) {
            if let Some(body) = hir_ty::body_types(db, file, item)
                && let Some(ty) = body.exprs.get(&expr_id)
            {
                return Some(HoverInfo {
                    value: ty.display_simple(db).to_string(),
                    docs: None,
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
                        value: format!("{}: {}", local.name.as_str(), ty.display_simple(db)),
                        // A local is not a declaration the file's doc-comment
                        // index carries.
                        docs: None,
                    });
                }
            }
        }
    }

    // A record component's own name ([JLS §8.10.1]): the declaration of the
    // private final field and the public accessor ([§8.10.3]) a member use
    // names — the same declaration `render_symbol_decl` below cannot see,
    // because the component is not an item.
    if let Some(component) = component_at(db, file, &tree, offset) {
        return component_hover(db, file, &tree, &component);
    }

    // A declaration's signature.
    render_symbol_decl(db, file, &tree, offset)
}

/// The declaration of the class-like type `fqn` names in `file`'s scope — the
/// declaration an inlay hint's type label navigates to.
///
/// A library class whose source is not loaded resolves to a *pending*
/// reference, which has no target: a click on such a label simply does not
/// navigate, rather than deferring the request to a materialization the hint
/// path does not drive.
pub(super) fn class_declaration(
    db: &RootDatabase,
    file: FileId,
    fqn: &str,
) -> Option<NavigationTarget> {
    targets(db, class_resolution(db, file, &Name::new(fqn)))
        .into_iter()
        .next()
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
        // A local class-like declaration ([JLS §14.3]) is not a member, so it
        // is not in any `body()`: it — and its members — are walked from the
        // declaration whose body declares it.
        for local in tree.local_types_of(item) {
            walk(db, file, tree, local, out);
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

/// The declared header of an item: `kind name` for a class-like declaration,
/// `name(params): ret` for a method, `name: ty` for a field, the bare name for
/// an enum constant. `None` for a nameless declaration (an initializer).
fn item_header(db: &RootDatabase, file: FileId, tree: &ItemTree, item: ItemId) -> Option<String> {
    let data = tree.data(item);
    let simple = data.name()?.simple_name();
    Some(match hir::SourceSymbolKind::of(data)? {
        hir::SourceSymbolKind::Method => {
            crate::symbols::method_signature(db, file, item, simple, true)
        }
        hir::SourceSymbolKind::Field => {
            format!("{simple}: {}", crate::symbols::item_ty(db, file, item))
        }
        hir::SourceSymbolKind::EnumConstant => simple.to_owned(),
        kind => format!("{} {}", kind.label(), simple),
    })
}

/// The hover of a declaration a reference resolved to ([`Resolution::Decl`]):
/// the header of the declaration's kind, plus its documentation. The
/// declaration may live in another file — a reference into an already-loaded
/// library source resolves to an item of that file, not of the one hovered.
fn declaration_hover(db: &RootDatabase, file: FileId, item: ItemId) -> Option<HoverInfo> {
    let tree = hir::java_item_tree(db, file);
    Some(HoverInfo {
        value: item_header(db, file, &tree, item)?,
        docs: crate::docs::hover_docs(db, file, item),
    })
}

/// The rendered signature of the declaration the offset *names*: a method's
/// `name(params): ret`, a field's `name: ty`, a class-like declaration's
/// `kind name` — and the declaration's documentation. `None` for an offset
/// inside a declaration that is not on its name: its modifiers, its keywords,
/// its punctuation and its body name nothing ([JLS §6.3] — a declaration's name
/// is not a reference to itself, and nothing else in the declaration is a
/// reference to it).
///
/// The candidates come from the item tree, innermost first ([`items_at`]), not
/// from the file's symbol index: a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// has no canonical name ([§6.7]) and is deliberately absent from that index
/// (`hir::file_symbols`), yet its own name is exactly what a hover on it asks
/// about.
///
/// The name is the same token [`self_target`] answers a definition with, so the
/// two requests agree on which declaration an offset names — and both answer
/// nothing on `private static final String field`.
fn render_symbol_decl(
    db: &RootDatabase,
    file: FileId,
    tree: &ItemTree,
    offset: TextSize,
) -> Option<HoverInfo> {
    let item = items_at(db, file, tree, offset).into_iter().find(|&item| {
        item_name_range(db, file, tree, item).is_some_and(|range| range.contains(offset))
    })?;
    Some(HoverInfo {
        value: item_header(db, file, tree, item)?,
        docs: crate::docs::hover_docs(db, file, item),
    })
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

/// The innermost expression at `offset` — the expression the offset is inside
/// of — or `None` when no expression is written there at all (a declaration's
/// modifiers, a brace, a semicolon).
///
/// A resolution belongs to the expression that carries the reference, and is
/// never handed up to an enclosing one. The offset on an argument, a cast's
/// type or a receiver lands inside a *nested* expression, whose own resolution
/// is about that nested name; the enclosing invocation's or creation's
/// resolution is about a name that is not written where the offset is, so it
/// must not answer for it. [`exprs_at`] is innermost first, so the resolution
/// walks stop here instead of ascending.
fn innermost_expr_at(bodies: &BodyTree, offset: TextSize) -> Option<ExprId> {
    exprs_at(bodies, offset).first().copied()
}
