//! Java semantic highlighting.
//!
//! Two layers:
//!
//! * the **lexical** layer ([`lexical`]) tags what the HIR cannot represent —
//!   keywords, modifiers, literals, operators and comments — straight from the
//!   CST;
//! * the **semantic** layer tags the identifiers: declarations from the item
//!   tree (with the JLS-defaulted modifiers the lowering computed), references
//!   from the resolution the type layer recorded while inferring each body
//!   ([`hir_ty::BodyTypes::resolved`]), type references and annotation names
//!   from the item tree and the body IR.
//!
//! The passes run in a fixed order and a later one overrides an earlier one at
//! the same range: the lexical layer first, then names ([`names`]), declarations
//! ([`declarations`]), type references ([`type_refs`]), annotation names
//! ([`annotation_names`]), the variables of every body ([`body_locals`]), the
//! resolved body references ([`references`]) and finally the two fallbacks — the
//! unclassified references of a body ([`unresolved_references`]) and the
//! declarations the HIR does not lower ([`unspecified_declarations`]) — which
//! only fill ranges nothing has classified.
//!
//! The parser's CST is consulted for identifiers only in that last fallback, so
//! a name's tag never depends on a syntactic guess where the HIR knows better.

use hir::SourceSymbolKind;
use hir::hir_def::java::item_tree::{ItemData, ItemId, ItemTree, TypeParam};
use hir::hir_def::java::modifiers::{JavaModality, JavaModifierFlags, JavaModifiers};
use hir::hir_def::java::ranges;
use hir_expand::ast_id_map::AstIdMap;
use hir_expand::body::{BodyTree, ExprData, ExprId, LocalId, PostfixOp, UnaryOp};
use hir_expand::name::Name;
use hir_ty::ResolvedMember;
use rowan::SyntaxNode;
use rustc_hash::FxHashSet;
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J};
use vfs::FileId;

use super::{Highlight, Highlights, HlMods, HlTag, insert};
use crate::RootDatabase;

/// The semantic highlighting of a Java file, sorted by range start.
pub(super) fn highlight(db: &RootDatabase, file_id: FileId, source: &SourceFile) -> Vec<Highlight> {
    let Some(root) = java_root(source) else {
        return Vec::new();
    };
    let tree = hir::file_item_tree(db, file_id);
    let bodies = hir::file_body_tree(db, file_id);
    let map = hir::hir_def::db::ast_id_map(db, file_id, tree.language);
    // The parameters of every body, and the expressions an assignment writes:
    // both classify body records, so they are computed once for the file.
    let params: FxHashSet<LocalId> = bodies
        .bodies
        .iter()
        .flat_map(|(_, body)| body.params.iter().copied())
        .collect();
    let writes = written_exprs(&bodies);

    let mut out = Highlights::new();
    lexical(root, &mut out);
    names(&tree, map, source, &mut out);
    declarations(&tree, map, source, &mut out);
    type_refs(db, file_id, &tree, &mut out);
    annotation_names(db, file_id, &tree, &mut out);
    body_locals(&bodies, &params, &mut out);
    references(db, file_id, &tree, &bodies, &params, &writes, &mut out);
    unresolved_references(&bodies, &mut out);
    unspecified_declarations(root, &mut out);
    out.into_values().collect()
}

/// The Java root node of `source`; `None` for a non-Java file.
fn java_root(source: &SourceFile) -> Option<&SyntaxNode<Lang>> {
    match source {
        SourceFile::Java(file) => Some(&file.syntax_node),
        SourceFile::Kotlin(_) => None,
    }
}

/// The lexical layer: every token whose tag is a property of the token alone.
fn lexical(root: &SyntaxNode<Lang>, out: &mut Highlights) {
    for element in root.descendants_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        let Some(tag) = lexical_tag(token.kind()) else {
            continue;
        };
        insert(out, token.text_range(), tag, HlMods::empty());
    }
}

/// The tag of a *lexical* token, or `None` for a token an identifier pass has
/// to classify (an identifier) or one that carries no color at all
/// (punctuation, whitespace).
fn lexical_tag(kind: J) -> Option<HlTag> {
    use HlTag::*;
    let tag = match kind {
        J::LINE_COMMENT | J::BLOCK_COMMENT | J::JAVADOC | J::JAVADOC_LINE => Comment,
        J::STRING_LITERAL
        | J::CHAR_LITERAL
        | J::TEXT_BLOCK
        | J::STRING_TEMPLATE_BEGIN
        | J::STRING_TEMPLATE_MID
        | J::STRING_TEMPLATE_END
        | J::TEXT_BLOCK_TEMPLATE_BEGIN
        | J::TEXT_BLOCK_TEMPLATE_MID
        | J::TEXT_BLOCK_TEMPLATE_END => String,
        J::INTEGER_LITERAL | J::FLOAT_LITERAL => Number,
        // The literals that are keywords, so they color like the keywords they
        // are ([JLS §3.9]).
        J::TRUE_LITERAL | J::FALSE_LITERAL | J::NULL_LITERAL => Keyword,
        J::PUBLIC_KW
        | J::PRIVATE_KW
        | J::PROTECTED_KW
        | J::STATIC_KW
        | J::FINAL_KW
        | J::ABSTRACT_KW
        | J::TRANSIENT_KW
        | J::VOLATILE_KW
        | J::NATIVE_KW
        | J::SYNCHRONIZED_KW
        | J::STRICTFP_KW => Modifier,
        J::PACKAGE_KW
        | J::IMPORT_KW
        | J::CLASS_KW
        | J::VOID_KW
        | J::BYTE_KW
        | J::ENUM_KW
        | J::INTERFACE_KW
        | J::FOR_KW
        | J::WHILE_KW
        | J::CONTINUE_KW
        | J::BREAK_KW
        | J::INSTANCEOF_KW
        | J::RETURN_KW
        | J::EXTENDS_KW
        | J::IMPLEMENTS_KW
        | J::NEW_KW
        | J::ASSERT_KW
        | J::SWITCH_KW
        | J::CASE_KW
        | J::DEFAULT_KW
        | J::DO_KW
        | J::IF_KW
        | J::ELSE_KW
        | J::THIS_KW
        | J::SUPER_KW
        | J::THROW_KW
        | J::THROWS_KW
        | J::TRY_KW
        | J::CATCH_KW
        | J::FINALLY_KW
        | J::DOUBLE_KW
        | J::INT_KW
        | J::SHORT_KW
        | J::LONG_KW
        | J::FLOAT_KW
        | J::CHAR_KW
        | J::BOOLEAN_KW
        | J::GOTO_KW
        | J::CONST_KW => Keyword,
        J::PLUS
        | J::MINUS
        | J::STAR
        | J::SLASH
        | J::LESS
        | J::LESS_EQUAL
        | J::GREATER
        | J::GREATER_EQUAL
        | J::EQUAL
        | J::EQUAL_EQUAL
        | J::NOT_EQUAL
        | J::OR
        | J::BIT_OR
        | J::OR_EQUAL
        | J::AND
        | J::BIT_AND
        | J::AND_EQUAL
        | J::NOT
        | J::TILDE
        | J::MODULO
        | J::CARET
        | J::DIVIDE_EQUAL
        | J::MULTIPLE_EQUAL
        | J::PLUS_EQUAL
        | J::PLUS_PLUS
        | J::MINUS_EQUAL
        | J::MINUS_MINUS
        | J::XOR_EQUAL
        | J::MODULO_EQUAL
        | J::LEFT_SHIFT
        | J::RIGHT_SHIFT
        | J::UNSIGNED_RIGHT_SHIFT
        | J::LEFT_SHIFT_EQUAL
        | J::RIGHT_SHIFT_EQUAL
        | J::UNSIGNED_RIGHT_SHIFT_EQUAL
        | J::QUESTION
        | J::COLON
        | J::COLON_COLON
        | J::ARROW
        | J::ELLIPSIS => Operator,
        _ => return None,
    };
    Some(tag)
}

/// The package and import names ([JLS §7.4], [§7.5]). A written name here may be
/// a package, a type or a static member, and telling them apart needs a
/// classpath lookup per prefix — which a static import's member has no answer to
/// — so every segment is a `namespace`, the one tag that is never wrong by
/// construction.
fn names(tree: &ItemTree, map: &AstIdMap, source: &SourceFile, out: &mut Highlights) {
    for decl in &tree.package_decls {
        if let Some(range) = ranges::package_name_range(map, source, *decl) {
            insert(out, range, HlTag::Namespace, HlMods::empty());
        }
    }
    for import in &tree.imports {
        for (_segment, range) in ranges::import_segments(map, source, import) {
            insert(out, range, HlTag::Namespace, HlMods::empty());
        }
    }
}

/// The declared names of every item: the declaration itself, its type parameters
/// and — for a record — its components, which are properties of the record
/// ([JLS §8.10.3]).
///
/// The item arena is walked directly rather than through the symbol index, which
/// skips the nameless initializers: their *bodies* still carry expressions, and
/// the body passes below cover the whole file regardless.
fn declarations(tree: &ItemTree, map: &AstIdMap, source: &SourceFile, out: &mut Highlights) {
    for (raw, data) in tree.items.iter() {
        let Some(kind) = SourceSymbolKind::of(data) else {
            continue;
        };
        let item = ItemId(raw);
        if let Some(range) = ranges::item_name_range(map, source, tree, item) {
            insert(out, range, tag_of(kind), declaration_mods(data));
        }
        for index in 0..type_param_count(data) {
            if let Some(range) = ranges::type_param_name_range(map, source, tree, item, index) {
                insert(out, range, HlTag::TypeParameter, HlMods::DECLARATION);
            }
        }
        if let ItemData::Record(record) = data {
            for component in &record.components {
                if let Some(range) = ranges::component_name_range(map, source, component) {
                    insert(out, range, HlTag::Property, HlMods::DECLARATION);
                }
            }
        }
    }
}

/// The token type of a declared symbol. An annotation type *is* an interface at
/// the JVM level (`ACC_ANNOTATION` implies `ACC_INTERFACE`,
/// [JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1)),
/// and a record is the language's product type, which the legend spells
/// `struct`.
fn tag_of(kind: SourceSymbolKind) -> HlTag {
    match kind {
        SourceSymbolKind::Class => HlTag::Class,
        SourceSymbolKind::Interface => HlTag::Interface,
        SourceSymbolKind::Enum => HlTag::Enum,
        SourceSymbolKind::Record => HlTag::Struct,
        SourceSymbolKind::Annotation => HlTag::Interface,
        SourceSymbolKind::Module => HlTag::Namespace,
        SourceSymbolKind::Method => HlTag::Method,
        SourceSymbolKind::Field => HlTag::Property,
        SourceSymbolKind::EnumConstant => HlTag::EnumMember,
        SourceSymbolKind::Package => HlTag::Namespace,
    }
}

/// The modifiers of a declaration: `declaration` always, plus the modifiers the
/// lowering recorded — which carry the JLS defaults, so an interface member is
/// `abstract` and an interface field `static` without the source spelling either
/// ([JLS §9.3], [§9.4]).
fn declaration_mods(data: &ItemData) -> HlMods {
    let mut mods = HlMods::DECLARATION;
    let Some(java) = java_modifiers(data) else {
        return mods;
    };
    if java.flags.contains(JavaModifierFlags::STATIC) {
        mods |= HlMods::STATIC;
    }
    if java.modality.contains(JavaModality::ABSTRACT) {
        mods |= HlMods::ABSTRACT;
    }
    // `final` is *readonly* on a variable only: on a method it forbids
    // overriding ([JLS §8.4.3.3]) and on a class subclassing ([§8.1.1.2]).
    if matches!(data, ItemData::Field(_)) && java.modality.contains(JavaModality::FINAL) {
        mods |= HlMods::READONLY;
    }
    mods
}

/// The source modifiers of an item that carries any; `None` for the nameless
/// initializers and for enum constants, whose implicit `public static final`
/// ([JLS §8.9.1]) the item tree does not record.
fn java_modifiers(data: &ItemData) -> Option<&JavaModifiers> {
    match data {
        ItemData::Class(data) | ItemData::Interface(data) => Some(&data.modifiers),
        ItemData::Enum(data) => Some(&data.modifiers),
        ItemData::Record(data) => Some(&data.modifiers),
        ItemData::Annotation(data) => Some(&data.modifiers),
        ItemData::Module(data) => Some(&data.modifiers),
        ItemData::Method(data) => Some(&data.modifiers),
        ItemData::Field(data) => Some(&data.modifiers),
        ItemData::EnumConstant(_) | ItemData::StaticInit(_) | ItemData::InstanceInit(_) => None,
    }
}

/// The number of type parameters a declaration declares ([JLS §4.4]).
fn type_param_count(data: &ItemData) -> usize {
    match data {
        ItemData::Class(data) | ItemData::Interface(data) => data.type_params.len(),
        ItemData::Record(data) => data.type_params.len(),
        ItemData::Method(data) => data.sig.type_params.len(),
        _ => 0,
    }
}

/// The written type references of every item ([JLS §6.5.5.1], [§4.4]) — the
/// declaration's own signature types and the types its body writes.
///
/// A reference that names a type parameter is a `typeParameter`, not a `type`.
/// The scope is threaded down the item tree, a declaration's own parameters
/// being in scope for its *members*: a class cannot write its own parameter in
/// its `extends` clause ([JLS §8.1.4]) and a nested type is a member like any
/// other, so this is a lexical match on where the name was written rather than a
/// resolution.
fn type_refs(db: &RootDatabase, file_id: FileId, tree: &ItemTree, out: &mut Highlights) {
    let mut in_scope = FxHashSet::default();
    for item in &tree.top {
        walk_type_refs(db, file_id, tree, *item, &mut in_scope, out);
    }
}

fn walk_type_refs(
    db: &RootDatabase,
    file_id: FileId,
    tree: &ItemTree,
    item: ItemId,
    in_scope: &FxHashSet<Name>,
    out: &mut Highlights,
) {
    for (name, range) in hir_ty::item_type_references(db, file_id, item) {
        let Some(range) = range else {
            continue;
        };
        let tag = if in_scope.contains(&name) {
            HlTag::TypeParameter
        } else {
            HlTag::Type
        };
        insert(out, range, tag, HlMods::empty());
    }
    let mut inner = in_scope.clone();
    inner.extend(declared_type_params(tree, item));
    for member in tree.data(item).body() {
        walk_type_refs(db, file_id, tree, *member, &inner, out);
    }
}

/// The names of the type parameters a declaration declares.
fn declared_type_params(tree: &ItemTree, item: ItemId) -> impl Iterator<Item = Name> + '_ {
    let params: &[TypeParam] = match tree.data(item) {
        ItemData::Class(data) | ItemData::Interface(data) => &data.type_params,
        ItemData::Record(data) => &data.type_params,
        ItemData::Method(data) => &data.sig.type_params,
        _ => &[],
    };
    params.iter().map(|param| param.name.clone())
}

/// The annotation names of every declaration ([JLS §9.7]), as `decorator`.
/// Runs after [`type_refs`], which reports an annotation's name as the type name
/// it is written as ([§6.5.5.1]).
fn annotation_names(db: &RootDatabase, file_id: FileId, tree: &ItemTree, out: &mut Highlights) {
    for (raw, _) in tree.items.iter() {
        for (_name, range) in hir_ty::item_annotation_references(db, file_id, ItemId(raw)) {
            if let Some(range) = range {
                insert(out, range, HlTag::Decorator, HlMods::empty());
            }
        }
    }
}

/// The variables a body declares — parameters, locals, catch and for-each
/// variables, pattern bindings and lambda parameters alike ([JLS §6.4]) — with
/// their declared types and declaration annotations.
fn body_locals(bodies: &BodyTree, params: &FxHashSet<LocalId>, out: &mut Highlights) {
    for (raw, local) in bodies.locals.iter() {
        let id = LocalId(raw);
        let Some(range) = bodies.local_name_range(id) else {
            continue;
        };
        let tag = if params.contains(&id) {
            HlTag::Parameter
        } else {
            HlTag::Variable
        };
        let mut mods = HlMods::DECLARATION;
        if local.is_final {
            mods |= HlMods::READONLY;
        }
        insert(out, range, tag, mods);

        if let Some(ty) = &local.ty {
            for name_ref in &ty.refs {
                let Some(range) = name_ref.range else {
                    continue;
                };
                // The declared type of a local is already classified by
                // [`type_refs`], which walks the same body with the scope in
                // force where the type was written; this fills in what that
                // pass did not reach.
                if !super::contains(out, range) {
                    insert(out, range, HlTag::Type, HlMods::empty());
                }
            }
        }
        for annotation in &local.annotations {
            if let Some(range) = annotation.name.range {
                insert(out, range, HlTag::Decorator, HlMods::empty());
            }
        }
    }
}

/// The body references inference resolved: the declaration each one denotes
/// ([`hir_ty::BodyTypes::resolved`]), with `modification` on the references an
/// assignment writes.
fn references(
    db: &RootDatabase,
    file_id: FileId,
    tree: &ItemTree,
    bodies: &BodyTree,
    params: &FxHashSet<LocalId>,
    writes: &FxHashSet<ExprId>,
    out: &mut Highlights,
) {
    for (raw, _) in tree.items.iter() {
        let Some(types) = hir_ty::body_types(db, file_id, ItemId(raw)) else {
            continue;
        };
        for (expr, member) in &types.resolved {
            // Only the shapes whose name is a single identifier: a class
            // instance creation and an explicit constructor invocation name a
            // declaration too, but their range covers the whole call, and the
            // type (or the `this`/`super` keyword) they write is tagged by
            // [`type_refs`] (or the lexical layer) already.
            if !is_named_reference(bodies.expr(*expr)) {
                continue;
            }
            let Some(range) = bodies.expr_name_range(*expr) else {
                continue;
            };
            let (tag, mut mods) = match member {
                ResolvedMember::Local(local) => (
                    if params.contains(local) {
                        HlTag::Parameter
                    } else {
                        HlTag::Variable
                    },
                    if bodies.local(*local).is_final {
                        HlMods::READONLY
                    } else {
                        HlMods::empty()
                    },
                ),
                ResolvedMember::Method(method) => (
                    HlTag::Method,
                    if method.is_static {
                        HlMods::STATIC
                    } else {
                        HlMods::empty()
                    },
                ),
                ResolvedMember::Field(field) => {
                    let mut mods = HlMods::empty();
                    if field.is_static {
                        mods |= HlMods::STATIC;
                    }
                    // A `final` field is a constant variable ([JLS §4.12.4]).
                    if field.is_final {
                        mods |= HlMods::READONLY;
                    }
                    (HlTag::Property, mods)
                }
                // A tie between overloads, or no applicable overload at all:
                // the invocation still names the methods it can denote
                // ([JLS §15.12.2]).
                ResolvedMember::Unresolved(_) => (HlTag::Method, HlMods::empty()),
            };
            if writes.contains(expr) {
                mods |= HlMods::MODIFICATION;
            }
            insert(out, range, tag, mods);
        }
    }
}

/// Whether an expression's name range is the single identifier that names it: a
/// member access, an invocation, a method reference or a bare name. Everything
/// else — a class instance creation `new T(args)`, an explicit constructor
/// invocation `this(args)`, a `Type.field` path — has a range that either covers
/// the whole call or spans a dotted path, so it is not one token; what it writes
/// is tagged by the pass that owns it ([`type_refs`], the lexical layer).
fn is_named_reference(expr: &ExprData) -> bool {
    matches!(
        expr,
        ExprData::MethodCall { .. }
            | ExprData::MethodRef { .. }
            | ExprData::FieldAccess { .. }
            | ExprData::Var(_)
    )
}

/// The expressions an assignment *writes*: the target of `=` and of every
/// compound assignment ([JLS §15.26]) and the operand of a prefix or postfix
/// `++`/`--` ([§15.14.2], [§15.15.1]).
fn written_exprs(bodies: &BodyTree) -> FxHashSet<ExprId> {
    let mut writes = FxHashSet::default();
    for (_, expr) in bodies.exprs.iter() {
        let target = match expr {
            ExprData::Assign { lhs, .. } => *lhs,
            ExprData::Unary {
                op: UnaryOp::Inc | UnaryOp::Dec,
                expr,
            } => *expr,
            ExprData::Postfix {
                op: PostfixOp::Inc | PostfixOp::Dec,
                expr,
            } => *expr,
            _ => continue,
        };
        // `(x) = 1` writes `x` as much as `x = 1` does ([JLS §15.8.5]).
        let mut target = target;
        while let ExprData::Paren(inner) = bodies.expr(target) {
            target = *inner;
        }
        writes.insert(target);
    }
    writes
}

/// The body references inference recorded nothing for: a member of a library
/// the workspace does not have loaded, or a name resolution left unanswered.
/// Their kind is read from the expression's shape alone, so only the shapes
/// whose *name* range is one identifier are classified — a `this(...)`/
/// `super(...)` invocation (a whole call) and a dotted `NamePath` are not.
///
/// A range a resolution already classified keeps that classification: this pass
/// only fills what [`references`] left.
fn unresolved_references(bodies: &BodyTree, out: &mut Highlights) {
    for (raw, expr) in bodies.exprs.iter() {
        let tag = match expr {
            ExprData::MethodCall { .. } | ExprData::MethodRef { .. } => HlTag::Method,
            ExprData::FieldAccess { .. } => HlTag::Property,
            ExprData::Var(_) => HlTag::Variable,
            _ => continue,
        };
        debug_assert!(is_named_reference(expr));
        let Some(range) = bodies.expr_name_range(ExprId(raw)) else {
            continue;
        };
        if !super::contains(out, range) {
            insert(out, range, tag, HlMods::empty());
        }
    }
}

/// The declarations the HIR does not lower — a local class, the members of an
/// anonymous class body, a lambda's parameters — filled in from the syntax
/// tree, which is the only record of them. Every range such an identifier can
/// take is a declaration's *name*: the declared type of a formal parameter, a
/// record component or a variable is a nested `TYPE` node, and an annotation is
/// a nested `ANNOTATION` node, so a direct-child `IDENTIFIER` of one of these
/// nodes is the name and nothing else.
///
/// Runs last, and only where no token is recorded: a name the HIR *did* lower
/// was classified with its resolution, which is strictly better information.
fn unspecified_declarations(root: &SyntaxNode<Lang>, out: &mut Highlights) {
    for element in root.descendants_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        if token.kind() != J::IDENTIFIER {
            continue;
        }
        let Some(parent) = token.parent() else {
            continue;
        };
        let Some(tag) = declaration_tag(&parent) else {
            continue;
        };
        // `record`, `sealed`, `permits` — and their spelling variants — are
        // *restricted identifiers*: the grammar writes them before the type
        // name a record declaration declares, and none of them can name a type
        // ([JLS §3.9]) — the same exclusion the lowering's own name walk makes.
        if is_type_tag(tag)
            && matches!(token.text(), "record" | "sealed" | "non-sealed" | "permits")
        {
            continue;
        }
        let range = token.text_range();
        if !super::contains(out, range) {
            insert(out, range, tag, HlMods::DECLARATION);
        }
    }
}

/// Whether a tag names a *type* declaration, which is what [JLS §3.9]'s
/// restricted identifiers cannot be.
fn is_type_tag(tag: HlTag) -> bool {
    matches!(
        tag,
        HlTag::Class | HlTag::Interface | HlTag::Enum | HlTag::Struct
    )
}

/// The kind of declaration whose *name* is the identifier of this node — the
/// identifier is the node's only direct-child `IDENTIFIER`, so no further token
/// filter is needed — or `None` when the node is not a declaration.
fn declaration_tag(node: &SyntaxNode<Lang>) -> Option<HlTag> {
    let tag = match node.kind() {
        J::CLASS_DECL => HlTag::Class,
        J::INTERFACE_DECL => HlTag::Interface,
        J::ENUM_DECL => HlTag::Enum,
        J::RECORD_DECL => HlTag::Struct,
        J::ANNOTATION_TYPE_DECL => HlTag::Interface,
        J::METHOD_DECL
        | J::CONSTRUCTOR_DECL
        | J::COMPACT_CONSTRUCTOR_DECL
        | J::ANNOTATION_TYPE_ELEMENT_DECL => HlTag::Method,
        J::ENUM_CONSTANT => HlTag::EnumMember,
        J::FORMAL_PARAMETER
        | J::SPREAD_PARAMETER
        | J::CATCH_FORMAL_PARAMETER
        | J::INFERRED_PARAMETERS => HlTag::Parameter,
        J::TYPE_PARAMETER => HlTag::TypeParameter,
        // A declarator is a field's, an enum constant's or a local's: only the
        // first is a property ([JLS §8.3]); the rest are locals.
        J::VARIABLE_DECLARATOR => {
            let member = node
                .ancestors()
                .any(|ancestor| matches!(ancestor.kind(), J::FIELD_DECL | J::ENUM_BODY));
            if member {
                HlTag::Property
            } else {
                HlTag::Variable
            }
        }
        _ => return None,
    };
    Some(tag)
}
