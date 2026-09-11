//! Java CST → item tree.
//!
//! The walker mirrors the parser grammar: `TYPE`, `MODIFIER_LIST`,
//! `TYPE_PARAMETERS`, the various `*_CLAUSE`s and the declaration nodes. The
//! tree is a *declaration* IR: method bodies and initializer expressions are
//! lowered into the per-file body tree ([`crate::lower::java::body`]) and the
//! source ranges of every declaration are kept.

use java_syntax::{Lang, SyntaxKind as J};
use rowan::{NodeOrToken, SyntaxNode, SyntaxToken, TextRange, TextSize};
use syntax::stub::{PrimitiveType, TypeBound, TypeRef};

use hir_expand::{
    ast_id_map::{AstIdMap, FileAstId, node_ptr},
    body::ExprData,
    name::Name,
    span::{AnnotationArg, AnnotationRef, AnnotationValue, NameRef, SpannedTypeRef},
};

use super::super::item_tree::{
    AnnotationData, ClassData, EnumConstantData, EnumData, FieldData, InstanceInitData,
    ItemAnnotationRef, ItemData, ItemId, ItemTypeRef, MethodData, MethodExtra, MethodExtraJava,
    ModuleData, ModuleExports, ModuleProvides, ModuleRequires, Param, RecordComponent, RecordData,
    Signature, StaticInitData, TypeParam,
};
use super::super::modifiers::JavaModifiers;
use super::{LowerCtx, body};

pub(super) fn lower_file(ctx: &mut LowerCtx<'_>, file: &java_syntax::SourceFile) {
    for child in file.syntax_node.children() {
        if is(&child, J::PACKAGE_DECL) {
            lower_package(ctx, &child);
        } else if is(&child, J::IMPORT_DECL) {
            lower_import(ctx, &child);
        } else if is(&child, J::MODULE_DECL) {
            let id = lower_module(ctx, &child);
            ctx.tree.top.push(id);
        } else if let Some(id) = lower_member(ctx, &child) {
            ctx.tree.top.push(id);
        }
    }
}

fn lower_package(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) {
    if let Some(name) = qualified_name_text(node) {
        ctx.tree.package = Some(name);
        // Every package declaration with a name, for the duplicate-package
        // check ([JLS §7.4.1]); the *last* entry is the one the package
        // symbol's name range derives from, exactly like the old
        // `package_range`.
        ctx.tree.package_decls.push(
            ctx.map
                .ast_id(&node_ptr(node))
                .expect("every PACKAGE_DECL is indexed"),
        );
    }
}

fn lower_import(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) {
    let Some(path) = node.children().find(|child| is(child, J::IMPORT_PATH)) else {
        return;
    };

    let full = trimmed_text(&path);
    let (name_text, is_asterisk) = if let Some(stripped) = full.strip_suffix(".*") {
        (stripped, true)
    } else {
        (full.as_str(), false)
    };

    let is_static = node.children_with_tokens().any(|element| {
        element
            .as_token()
            .is_some_and(|token| token_is(token, J::STATIC_KW))
    });

    ctx.tree.imports.push(crate::java::item_tree::ImportItem {
        name: Name::new(name_text),
        is_static,
        is_asterisk,
        path: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every IMPORT_DECL is indexed"),
    });
}

/// Lowers any declaration that can appear in a class body, returning `None`
/// for node kinds that carry no items (`EMPTY_DECL`, `ERROR`).
fn lower_member(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    if is(node, J::STATIC_INITIALIZER) {
        let block = node.children().find(|child| is(child, J::BLOCK));
        let id = ctx.alloc(ItemData::StaticInit(StaticInitData {
            body: None,
            ast: ctx
                .map
                .ast_id(&node_ptr(node))
                .expect("every STATIC_INITIALIZER is indexed"),
        }));
        if let Some(block) = block {
            let body = body::lower_initializer_body(ctx, id, &block);
            let ItemData::StaticInit(data) = ctx.tree.items.get_mut(id.0) else {
                unreachable!("static initializer");
            };
            data.body = body;
        }
        Some(id)
    } else if is(node, J::INSTANCE_INITIALIZER) {
        let block = node.children().find(|child| is(child, J::BLOCK));
        let id = ctx.alloc(ItemData::InstanceInit(InstanceInitData {
            body: None,
            ast: ctx
                .map
                .ast_id(&node_ptr(node))
                .expect("every INSTANCE_INITIALIZER is indexed"),
        }));
        if let Some(block) = block {
            let body = body::lower_initializer_body(ctx, id, &block);
            let ItemData::InstanceInit(data) = ctx.tree.items.get_mut(id.0) else {
                unreachable!("instance initializer");
            };
            data.body = body;
        }
        Some(id)
    } else if is(node, J::METHOD_DECL) {
        lower_method(ctx, node)
    } else if is(node, J::CONSTRUCTOR_DECL) {
        lower_constructor(ctx, node, false)
    } else if is(node, J::COMPACT_CONSTRUCTOR_DECL) {
        lower_constructor(ctx, node, true)
    } else if is(node, J::ANNOTATION_TYPE_ELEMENT_DECL) {
        lower_annotation_element(ctx, node)
    } else if is(node, J::CLASS_DECL) {
        Some(lower_class(ctx, node))
    } else if is(node, J::INTERFACE_DECL) {
        Some(lower_interface(ctx, node))
    } else if is(node, J::ENUM_DECL) {
        Some(lower_enum(ctx, node))
    } else if is(node, J::RECORD_DECL) {
        Some(lower_record(ctx, node))
    } else if is(node, J::ANNOTATION_TYPE_DECL) {
        Some(lower_annotation_type(ctx, node))
    } else {
        None
    }
}

fn lower_class(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_type_identifier(node);
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let type_params = child_type_params(ctx.map, node);
    let super_class = clause_item_types(ctx.map, node, J::EXTENDS_CLAUSE)
        .into_iter()
        .next();
    let interfaces = clause_item_types(ctx.map, node, J::IMPLEMENTS_CLAUSE);
    let permits = clause_item_types(ctx.map, node, J::PERMITS_CLAUSE);
    let body = body_members(ctx, node, J::CLASS_BODY);
    ctx.alloc(ItemData::Class(ClassData {
        name,
        modifiers,
        annotations,
        super_class,
        interfaces,
        permits,
        type_params,
        body,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every CLASS_DECL is indexed"),
    }))
}

fn lower_interface(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_type_identifier(node);
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let type_params = child_type_params(ctx.map, node);
    let interfaces = clause_item_types(ctx.map, node, J::INTERFACE_EXTENDS_CLAUSE);
    let permits = clause_item_types(ctx.map, node, J::PERMITS_CLAUSE);
    let body = body_members(ctx, node, J::INTERFACE_BODY);
    ctx.alloc(ItemData::Interface(ClassData {
        name,
        modifiers,
        annotations,
        super_class: None,
        interfaces,
        permits,
        type_params,
        body,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every INTERFACE_DECL is indexed"),
    }))
}

fn lower_enum(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_type_identifier(node);
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let interfaces = clause_item_types(ctx.map, node, J::IMPLEMENTS_CLAUSE);
    let body = node
        .children()
        .find(|child| is(child, J::ENUM_BODY))
        .map(|body| enum_body_members(ctx, &body))
        .unwrap_or_default();
    ctx.alloc(ItemData::Enum(EnumData {
        name,
        modifiers,
        annotations,
        interfaces,
        body,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every ENUM_DECL is indexed"),
    }))
}

fn lower_record(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_type_identifier(node);
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let type_params = child_type_params(ctx.map, node);
    let components = node
        .children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| {
            params
                .children()
                .filter(|child| is(child, J::FORMAL_PARAMETER) || is(child, J::SPREAD_PARAMETER))
                .map(|child| component_from(&child, ctx.map))
                .collect()
        })
        .unwrap_or_default();
    let interfaces = clause_item_types(ctx.map, node, J::IMPLEMENTS_CLAUSE);
    let permits = clause_item_types(ctx.map, node, J::PERMITS_CLAUSE);
    // The component list `(int x, int y)` and the declaration header ranges
    // (the record's "definition") are derived from the declaration node by
    // [`crate::java::ranges::record_components_range`] /
    // [`crate::java::ranges::record_header_range`].
    let body = body_members(ctx, node, J::RECORD_BODY);
    ctx.alloc(ItemData::Record(RecordData {
        name,
        modifiers,
        annotations,
        components,
        interfaces,
        permits,
        type_params,
        body,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every RECORD_DECL is indexed"),
    }))
}

fn lower_annotation_type(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_type_identifier(node);
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let body = body_members(ctx, node, J::ANNOTATION_TYPE_BODY);
    ctx.alloc(ItemData::Annotation(AnnotationData {
        name,
        modifiers,
        annotations,
        body,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every ANNOTATION_TYPE_DECL is indexed"),
    }))
}

fn lower_method(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let ret = if token_is_direct(node, J::VOID_KW) {
        Some(ItemTypeRef::synthetic(TypeRef::Primitive(
            PrimitiveType::Void,
        )))
    } else {
        node.children()
            .find(|child| is(child, J::TYPE))
            .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, ctx.map))
    };
    let sig = Signature {
        type_params: child_type_params(ctx.map, node),
        params: formal_params(ctx.map, node),
        ret,
        throws: clause_item_types(ctx.map, node, J::THROWS_CLAUSE),
    };
    let block = node.children().find(|child| is(child, J::BLOCK));
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        annotations,
        sig,
        extra: MethodExtra::Java(MethodExtraJava {
            is_constructor: false,
            is_compact_constructor: false,
            body: None,
            default_expr: None,
        }),
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every METHOD_DECL is indexed"),
    }));
    if let Some(block) = block {
        let params = node
            .children()
            .find(|child| is(child, J::FORMAL_PARAMETERS));
        let body = body::lower_method_body(ctx, id, &block, params.as_ref());
        let ItemData::Method(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("method");
        };
        let MethodExtra::Java(java) = &mut data.extra;
        java.body = Some(body);
    }
    Some(id)
}

fn lower_constructor(
    ctx: &mut LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
    compact: bool,
) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let sig = Signature {
        type_params: child_type_params(ctx.map, node),
        params: if compact {
            Vec::new()
        } else {
            formal_params(ctx.map, node)
        },
        ret: None,
        throws: if compact {
            Vec::new()
        } else {
            clause_item_types(ctx.map, node, J::THROWS_CLAUSE)
        },
    };
    let block = node.children().find(|child| is(child, J::BLOCK));
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        annotations,
        sig,
        extra: MethodExtra::Java(MethodExtraJava {
            is_constructor: true,
            is_compact_constructor: compact,
            body: None,
            default_expr: None,
        }),
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every CONSTRUCTOR_DECL/COMPACT_CONSTRUCTOR_DECL is indexed"),
    }));
    if let Some(block) = block {
        let params = if compact {
            None
        } else {
            node.children()
                .find(|child| is(child, J::FORMAL_PARAMETERS))
        };
        let body = body::lower_method_body(ctx, id, &block, params.as_ref());
        let ItemData::Method(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("constructor");
        };
        let MethodExtra::Java(java) = &mut data.extra;
        java.body = Some(body);
    }
    Some(id)
}

fn lower_annotation_element(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let ret = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, ctx.map));
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        annotations,
        sig: Signature {
            type_params: Vec::new(),
            params: Vec::new(),
            ret,
            throws: Vec::new(),
        },
        extra: MethodExtra::Java(MethodExtraJava {
            is_constructor: false,
            is_compact_constructor: false,
            body: None,
            default_expr: None,
        }),
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every ANNOTATION_TYPE_ELEMENT_DECL is indexed"),
    }));
    if let Some(value_node) = body::find_expression_child(node)
        && let Some(expr_id) = body::lower_expr(ctx, id, &value_node)
    {
        let ItemData::Method(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("annotation element");
        };
        let MethodExtra::Java(java) = &mut data.extra;
        java.default_expr = Some(expr_id);
    }
    Some(id)
}

fn lower_field_decl(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let ty = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, ctx.map));
    let Some(ty) = ty else { return Vec::new() };
    let mut ids = Vec::new();
    for declarator in node
        .children()
        .filter(|child| is(child, J::VARIABLE_DECLARATOR_LIST))
        .flat_map(|list| list.children())
        .filter(|child| is(child, J::VARIABLE_DECLARATOR))
    {
        let Some(name) = declarator
            .children_with_tokens()
            .filter_map(|element| element.as_token().cloned())
            .find(|token| token_is(token, J::IDENTIFIER))
            .map(|token| Name::new(token.text()))
        else {
            continue;
        };
        let mut ty = ty.clone();
        if let Some(dims) = declarator.children().find(|child| is(child, J::DIMENSIONS)) {
            ty = wrap_dims(ty, &dims);
        }
        let has_initializer = declarator.children_with_tokens().any(|element| {
            element
                .as_token()
                .is_some_and(|token| token_is(token, J::EQUAL))
        });
        let expr_slot = body::find_expression_child(&declarator);
        let field_id = ctx.alloc(ItemData::Field(FieldData {
            name,
            modifiers,
            annotations: annotations.clone(),
            ty,
            has_initializer,
            initializer_expr: None,
            ast: ctx
                .map
                .ast_id(&node_ptr(&declarator))
                .expect("every VARIABLE_DECLARATOR is indexed"),
        }));
        if let Some(expr_node) = expr_slot
            && let Some(expr_id) = body::lower_expr(ctx, field_id, &expr_node)
        {
            let ItemData::Field(data) = ctx.tree.items.get_mut(field_id.0) else {
                unreachable!("field");
            };
            data.initializer_expr = Some(expr_id);
        }
        ids.push(field_id);
    }
    ids
}

fn enum_body_members(ctx: &mut LowerCtx<'_>, body: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let mut ids = Vec::new();
    for child in body.children() {
        if is(&child, J::ENUM_CONSTANT) {
            let name = first_token(&child, J::IDENTIFIER)
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(missing_name);
            let constant_id = ctx.alloc(ItemData::EnumConstant(EnumConstantData {
                name,
                argument_exprs: Vec::new(),
                ast: ctx
                    .map
                    .ast_id(&node_ptr(&child))
                    .expect("every ENUM_CONSTANT is indexed"),
            }));
            if let Some(list) = child.children().find(|nested| is(nested, J::ARGUMENT_LIST)) {
                let exprs: Vec<_> = list
                    .children()
                    .filter(|arg| body::is_expr_kind(arg.kind()))
                    .filter_map(|arg| body::lower_expr(ctx, constant_id, &arg))
                    .collect();
                let ItemData::EnumConstant(data) = ctx.tree.items.get_mut(constant_id.0) else {
                    unreachable!("enum constant");
                };
                data.argument_exprs = exprs;
            }
            ids.push(constant_id);
        } else if is(&child, J::FIELD_DECL) {
            // §8.9.2: an enum body may declare fields (and initializers),
            // exactly like a class body — they must not be dropped.
            ids.extend(lower_field_decl(ctx, &child));
        } else if let Some(id) = lower_member(ctx, &child) {
            ids.push(id);
        }
    }
    ids
}

/// Lower all members of the named body node (a direct child of `node`).
fn body_members(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>, body_kind: J) -> Vec<ItemId> {
    let mut ids = Vec::new();
    if let Some(body) = node.children().find(|child| is(child, body_kind)) {
        for child in body.children() {
            if is(&child, J::FIELD_DECL) {
                ids.extend(lower_field_decl(ctx, &child));
            } else if let Some(id) = lower_member(ctx, &child) {
                ids.push(id);
            }
        }
    }
    ids
}

// --- module declarations ---

fn lower_module(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let name = qualified_name_child(node)
        .map(|child| Name::new(&trimmed_text(&child)))
        .unwrap_or_else(missing_name);
    // §9.7: the module declaration's annotations live in its leading modifier
    // list (`@Ann module com.example {}`).
    let (modifiers, annotations) = child_modifiers_and_annotations(ctx.map, node);
    let is_open = node.children_with_tokens().next().is_some_and(|element| {
        element
            .as_token()
            .is_some_and(|token| token.text() == "open")
    });
    let mut requires = Vec::new();
    let mut exports = Vec::new();
    let mut opens = Vec::new();
    let mut uses = Vec::new();
    let mut provides = Vec::new();

    if let Some(body) = node.children().find(|child| is(child, J::MODULE_BODY)) {
        for directive in body.children() {
            if is(&directive, J::REQUIRES_DIRECTIVE) {
                requires.push(requires_from(&directive, ctx.map));
            } else if is(&directive, J::EXPORTS_DIRECTIVE) {
                exports.push(package_exports_from(&directive, ctx.map));
            } else if is(&directive, J::OPENS_DIRECTIVE) {
                opens.push(package_exports_from(&directive, ctx.map));
            } else if is(&directive, J::USES_DIRECTIVE) {
                if let Some(child) = qualified_name_child(&directive) {
                    uses.push(qualified_name_item_ref(&child, ctx.map));
                }
            } else if is(&directive, J::PROVIDES_DIRECTIVE) {
                provides.push(provides_from(&directive, ctx.map));
            }
        }
    }

    ctx.alloc(ItemData::Module(ModuleData {
        name,
        modifiers,
        annotations,
        is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
        ast: ctx
            .map
            .ast_id(&node_ptr(node))
            .expect("every MODULE_DECL is indexed"),
    }))
}

fn requires_from(directive: &SyntaxNode<Lang>, map: &AstIdMap) -> ModuleRequires {
    let name = qualified_name_text(directive).unwrap_or_else(missing_name);
    let (transitive, statik) = directive
        .children()
        .find(|child| is(child, J::MODIFIER_LIST))
        .map_or((false, false), |mods| {
            let transitive = mods.children_with_tokens().any(|element| {
                element
                    .as_token()
                    .is_some_and(|t| token_text(t, "transitive"))
            });
            let statik = mods.children_with_tokens().any(|element| {
                element
                    .as_token()
                    .is_some_and(|t| token_is(t, J::STATIC_KW))
            });
            (transitive, statik)
        });
    ModuleRequires {
        name,
        transitive,
        statik,
        ast: map
            .ast_id(&node_ptr(directive))
            .expect("every REQUIRES_DIRECTIVE is indexed"),
    }
}

fn package_exports_from(directive: &SyntaxNode<Lang>, map: &AstIdMap) -> ModuleExports {
    let names: Vec<Name> = directive
        .children()
        .filter(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| Name::new(&trimmed_text(&child)))
        .collect();
    let package = names.first().cloned().unwrap_or_else(missing_name);
    let to = names.into_iter().skip(1).collect();
    ModuleExports {
        package,
        to,
        ast: map
            .ast_id(&node_ptr(directive))
            .expect("every EXPORTS/OPENS_DIRECTIVE is indexed"),
    }
}

fn provides_from(directive: &SyntaxNode<Lang>, map: &AstIdMap) -> ModuleProvides {
    let names: Vec<ItemTypeRef> = directive
        .children()
        .filter(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| qualified_name_item_ref(&child, map))
        .collect();
    let service = names
        .first()
        .cloned()
        .unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error));
    let implementations = names.into_iter().skip(1).collect();
    ModuleProvides {
        service,
        implementations,
    }
}

/// A single fully qualified *type* name child of a syntax node.
pub(crate) fn qualified_name_child(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children().find(|child| is(child, J::QUALIFIED_NAME))
}

/// The lowered name of a single fully qualified *type* name child.
pub(crate) fn qualified_name_text(node: &SyntaxNode<Lang>) -> Option<Name> {
    qualified_name_child(node).map(|child| Name::new(&trimmed_text(&child)))
}

/// A range-free `ItemTypeRef::Reference` over a qualified-name syntax node
/// (a module directive's service or implementation type), carrying the node's
/// id.
fn qualified_name_item_ref(node: &SyntaxNode<Lang>, map: &AstIdMap) -> ItemTypeRef {
    let name = Name::new(&trimmed_text(node));
    ItemTypeRef {
        ty: TypeRef::Reference {
            name: name.clone(),
            generic_args: Vec::new(),
        },
        refs: vec![name],
        type_use_annotations: Vec::new(),
        node: map
            .ast_id(&node_ptr(node))
            .unwrap_or_else(FileAstId::placeholder),
    }
}

// --- helpers ---

pub(crate) fn is(node: &SyntaxNode<Lang>, kind: J) -> bool {
    node.kind() == kind
}

pub(crate) fn token_is(token: &SyntaxToken<Lang>, kind: J) -> bool {
    token.kind() == kind
}

pub(crate) fn token_text(token: &SyntaxToken<Lang>, text: &str) -> bool {
    token.text() == text
}

pub(crate) fn trimmed_text(node: &SyntaxNode<Lang>) -> String {
    node.text().to_string().trim().to_owned()
}

/// The declared name of a class-like type declaration: the first direct-child
/// `IDENTIFIER` token that is not a contextual keyword (e.g. `record`,
/// `open`).
///
/// Unlike a method or constructor name, a type name may not be one of the
/// restricted identifiers `record`, `sealed` or `permits` ([JLS
/// §3.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.9)):
/// `record` is a restricted *type* name (it precedes the type in a record
/// declaration, which this helper is not called on) and the three cannot name
/// a type at all. The exclusion applies only here — the same tokens are
/// ordinary method, field and constructor identifiers elsewhere (§3.9).
fn decl_type_identifier(node: &SyntaxNode<Lang>) -> Name {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token()
            && token.kind() == J::IDENTIFIER
            && !matches!(token.text(), "record" | "sealed" | "non-sealed" | "permits")
        {
            return Name::new(token.text());
        }
    }
    missing_name()
}

fn decl_identifier(node: &SyntaxNode<Lang>) -> Option<Name> {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token()
            && token.kind() == J::IDENTIFIER
        {
            return Some(Name::new(token.text()));
        }
    }
    None
}

pub(crate) fn first_token(node: &SyntaxNode<Lang>, kind: J) -> Option<SyntaxToken<Lang>> {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token()
            && token.kind() == kind
        {
            return Some(token.clone());
        }
    }
    None
}

fn token_is_direct(node: &SyntaxNode<Lang>, kind: J) -> bool {
    first_token(node, kind).is_some()
}

/// The first `MODIFIER_LIST` child, split into its syntax modifiers
/// ([`JavaModifiers`]) and its declared annotation references ([JLS §9.7]),
/// which are decoupled from the modifier flags.
fn child_modifiers_and_annotations(
    map: &AstIdMap,
    node: &SyntaxNode<Lang>,
) -> (JavaModifiers, Vec<ItemAnnotationRef>) {
    node.children()
        .find(|child| is(child, J::MODIFIER_LIST))
        .map(|mods| {
            let mut modifiers = JavaModifiers::none();
            // §8.1.1.2: `non-sealed` lexes as `non - sealed` (three tokens);
            // join them into the single modifier keyword the modifiers model
            // knows.
            let tokens: Vec<_> = mods
                .children_with_tokens()
                .filter_map(|e| e.as_token().cloned())
                .collect();
            let mut i = 0;
            while i < tokens.len() {
                let text = tokens[i].text();
                if text == "non"
                    && tokens.get(i + 1).is_some_and(|t| t.kind() == J::MINUS)
                    && tokens.get(i + 2).is_some_and(|t| t.text() == "sealed")
                {
                    modifiers.push("non-sealed");
                    i += 3;
                } else {
                    modifiers.push(text);
                    i += 1;
                }
            }
            let annotations = item_annotations_from(&mods, map);
            (modifiers, annotations)
        })
        .unwrap_or_default()
}

/// The reference name (`NameRef`) of an `ANNOTATION`/`MARKER_ANNOTATION`
/// syntax node: the (possibly qualified) name after the `@`
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7))
/// with its source range.
pub(crate) fn annotation_name_ref(annotation: &SyntaxNode<Lang>) -> Option<NameRef> {
    annotation
        .descendants()
        .find(|d| d.kind() == J::QUALIFIED_NAME)
        .map(|name| NameRef::new(Name::new(&name.text().to_string()), name.text_range()))
}

/// The annotation of an `ANNOTATION`/`MARKER_ANNOTATION` syntax node with its
/// element-value arguments ([JLS §9.7.1]).
fn annotation_ref(annotation: &SyntaxNode<Lang>) -> Option<AnnotationRef> {
    let name = annotation_name_ref(annotation)?;
    let args = annotation
        .children()
        .find(|child| is(child, J::ANNOTATION_ARGUMENT_LIST))
        .map(|list| annotation_args_from(&list))
        .unwrap_or_default();
    Some(AnnotationRef { name, args })
}

/// The element-value pairs of an `ANNOTATION_ARGUMENT_LIST` node
/// ([JLS §9.7.1]): either the single-argument form `(v)` — whose element name
/// is implicitly `value` — or the named-pairs form `(k = v, ...)`, in source
/// order.
pub(crate) fn annotation_args_from(list: &SyntaxNode<Lang>) -> Vec<AnnotationArg> {
    let mut out = Vec::new();
    for child in list.children() {
        if is(&child, J::ELEMENT_VALUE_PAIR) {
            // `key = value`: the element name is the identifier before `=`.
            let name = first_token(&child, J::IDENTIFIER)
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(missing_name);
            if let Some((value, range)) = child
                .children()
                .find(is_element_value)
                .and_then(|node| annotation_value_from(&node))
            {
                out.push(AnnotationArg { name, value, range });
            }
        } else if is_element_value(&child)
            && let Some((value, range)) = annotation_value_from(&child)
        {
            // `(v)` — the implicit `value` element ([§9.7.1]).
            out.push(AnnotationArg {
                name: Name::new("value"),
                value,
                range,
            });
        }
    }
    out
}

/// Whether `node` is an annotation element value ([JLS §9.7.1]): a nested
/// annotation, an array initializer, or a constant expression.
pub(crate) fn is_element_value(node: &SyntaxNode<Lang>) -> bool {
    matches!(
        node.kind(),
        J::ANNOTATION | J::MARKER_ANNOTATION | J::ARRAY_INITIALIZER | J::LITERAL
    ) || expr_node_kind(node.kind())
}

/// The node kinds an annotation element value (a constant expression,
/// [§15.28]) may take beyond the direct literal forms.
fn expr_node_kind(kind: J) -> bool {
    matches!(
        kind,
        J::FIELD_ACCESS
            | J::CLASS_LITERAL
            | J::PREFIX_EXPR
            | J::UNARY_EXPR
            | J::BINARY_EXPR
            | J::COND_EXPR
            | J::PAREN_EXPR
            | J::PARENTHESIZED_EXPR
            | J::CAST_EXPR
    )
}

/// Parses one annotation element value ([JLS §9.7.1]) into its structured
/// [`AnnotationValue`]; `None` when the node carries no value (a missing or
/// unparsed child).
pub(crate) fn annotation_value_from(
    node: &SyntaxNode<Lang>,
) -> Option<(AnnotationValue, TextRange)> {
    let range = node.text_range();
    let value = match node.kind() {
        // A nested annotation `@Foo(...)`.
        J::ANNOTATION | J::MARKER_ANNOTATION => {
            AnnotationValue::Annotation(Box::new(annotation_ref(node)?))
        }
        // An array initializer `{ v1, v2 }` ([§10.6]).
        J::ARRAY_INITIALIZER => AnnotationValue::Array(
            node.children()
                .filter(is_element_value)
                .filter_map(|child| annotation_value_from(&child).map(|(v, _)| v))
                .collect(),
        ),
        // A class literal `Foo.class` ([§15.8.2]).
        J::CLASS_LITERAL => AnnotationValue::ClassLit(class_literal_type(node)),
        // A literal — or, for an identifier token, a bare enum constant whose
        // declaring type comes from the element ([§9.7.1]).
        J::LITERAL => match body::literal(node) {
            ExprData::Literal(lit) => AnnotationValue::Literal(lit),
            ExprData::Var(name) => AnnotationValue::EnumConstant {
                qualifier: None,
                member: name,
            },
            _ => return None,
        },
        // `Type.CONSTANT` — an enum constant ([§8.9.1]).
        J::FIELD_ACCESS => {
            let member = first_token(node, J::IDENTIFIER)
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(missing_name);
            let qualifier = qualified_receiver_text(node);
            AnnotationValue::EnumConstant { qualifier, member }
        }
        // Any other constant expression (unary/binary/conditional/cast): kept
        // as its raw source text.
        _ => {
            let text = node.text().to_string();
            if text.is_empty() {
                return None;
            }
            AnnotationValue::Unresolved { text }
        }
    };
    Some((value, range))
}

/// The qualified receiver text of a `FIELD_ACCESS` (`Foo` in `Foo.BAR`).
fn qualified_receiver_text(node: &SyntaxNode<Lang>) -> Option<Name> {
    let receiver = node.children().find(is_element_value)?;
    let text = receiver.text().to_string();
    (!text.is_empty()).then(|| Name::new(&text))
}

/// The type of a class literal `Foo.class` / `Foo.Bar.class` ([§15.8.2]) as a
/// spanned reference — the identifier chain before `.class`; a primitive or
/// array form (`int[].class`) becomes the matching primitive/array type.
fn class_literal_type(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let mut name = String::new();
    let mut start: Option<TextSize> = None;
    let mut end: TextSize = node.text_range().start();
    let mut saw_dot = false;
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) => {
                // The identifier is wrapped in a nested `LITERAL` node
                // (`String` in `String.class`).
                for token in child.descendants_with_tokens() {
                    if let Some(token) = token.as_token()
                        && token_is(token, J::IDENTIFIER)
                    {
                        name.push_str(token.text());
                        start = Some(start.unwrap_or(token.text_range().start()));
                        end = token.text_range().end();
                        saw_dot = false;
                    }
                }
            }
            NodeOrToken::Token(token) => {
                if token_is(&token, J::DOT) {
                    saw_dot = true;
                } else if saw_dot && token_is(&token, J::IDENTIFIER) {
                    name.push('.');
                    name.push_str(token.text());
                    end = token.text_range().end();
                    saw_dot = false;
                }
            }
        }
    }
    if name.is_empty() {
        // A primitive class literal (`void.class`, `int[].class`).
        if let Some(prim) = node
            .children_with_tokens()
            .find_map(|element| element.as_token().and_then(primitive_from_token))
        {
            let mut ty = TypeRef::Primitive(prim);
            for _ in 0..dimension_count(node) {
                ty = TypeRef::Array(Box::new(ty));
            }
            return SpannedTypeRef::synthetic(ty);
        }
        return SpannedTypeRef::synthetic(TypeRef::Error);
    }
    let range = start
        .map(|s| TextRange::new(s, end))
        .unwrap_or_else(|| node.text_range());
    SpannedTypeRef {
        ty: TypeRef::Reference {
            name: Name::new(&name),
            generic_args: Vec::new(),
        },
        refs: vec![NameRef::new(Name::new(&name), range)],
        type_use_annotations: Vec::new(),
    }
}

/// The type-use annotations of a `TYPE` node
/// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)):
/// the leading annotations (`@Nullable Object`), the per-qualifier-segment
/// annotations (`Connection.@Nullable Response`) and the per-dimension
/// annotations (`int @Nullable []`). Nested types in type arguments are not
/// descended into — they are lowered as their own [`SpannedTypeRef`]s, so
/// their annotations are reported from there (no duplicates).
fn type_annotation_refs(node: &SyntaxNode<Lang>) -> Vec<AnnotationRef> {
    let mut out = Vec::new();
    for child in node.children() {
        if !matches!(
            child.kind(),
            J::MODIFIER_LIST | J::DIMENSIONS | J::DIMENSION
        ) {
            continue;
        }
        for annotation in child.descendants() {
            if matches!(annotation.kind(), J::ANNOTATION | J::MARKER_ANNOTATION)
                && let Some(name) = annotation_ref(&annotation)
            {
                out.push(name);
            }
        }
    }
    out
}

fn child_type_params(map: &AstIdMap, node: &SyntaxNode<Lang>) -> Vec<TypeParam> {
    node.children()
        .find(|child| is(child, J::TYPE_PARAMETERS))
        .map(|child| type_params_from(map, &child))
        .unwrap_or_default()
}

fn type_params_from(map: &AstIdMap, node: &SyntaxNode<Lang>) -> Vec<TypeParam> {
    node.children()
        .filter(|child| is(child, J::TYPE_PARAMETER))
        .map(|child| type_param_from(map, &child))
        .collect()
}

fn type_param_from(map: &AstIdMap, node: &SyntaxNode<Lang>) -> TypeParam {
    let name = first_token(node, J::IDENTIFIER)
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    let bounds = node
        .children()
        .find(|child| is(child, J::TYPE_BOUND))
        .map(|bound| {
            bound
                .children()
                .filter(|child| is(child, J::TYPE))
                .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, map))
                .collect()
        })
        .unwrap_or_default();
    TypeParam {
        name,
        bounds,
        annotations: Vec::new(),
    }
}

fn formal_params(map: &AstIdMap, node: &SyntaxNode<Lang>) -> Vec<Param> {
    node.children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| {
            params
                .children()
                .filter(|child| is(child, J::FORMAL_PARAMETER) || is(child, J::SPREAD_PARAMETER))
                .map(|child| param_from(map, &child))
                .collect()
        })
        .unwrap_or_default()
}

fn param_from(map: &AstIdMap, node: &SyntaxNode<Lang>) -> Param {
    let varargs = is(node, J::SPREAD_PARAMETER);
    let mut ty = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, map))
        .unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error));
    if let Some(dims) = node.children().find(|child| is(child, J::DIMENSIONS)) {
        ty = wrap_dims(ty, &dims);
    }
    // §8.4.1/§9.7.4: the annotations the variable-arity parameter writes
    // between its type and the `...` (`String @A ... p`) annotate the array
    // type the parameter declares, so they join its type reference. They are
    // *not* added to `ty.refs`: that list is paired positionally with the
    // occurrences [`crate::java::ranges::type_ref_occurrences`] re-derives
    // from the `TYPE` node, which does not contain them.
    for trailing in trailing_modifier_lists(node) {
        ty.type_use_annotations
            .extend(item_annotations_from(&trailing, map));
    }
    let name = first_token(node, J::IDENTIFIER)
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    // §9.7.4: the annotation modifiers of a formal parameter declaration
    // (`void m(@A int p)`) are its own modifier lists, like a field's; the
    // annotations of its *type* (`String @A ... p`) joined the type
    // reference above.
    let annotations = declaration_modifier_lists(node)
        .iter()
        .flat_map(|mods| item_annotations_from(mods, map))
        .collect();
    Param {
        name,
        ty,
        varargs,
        annotations,
    }
}

fn component_from(node: &SyntaxNode<Lang>, map: &AstIdMap) -> RecordComponent {
    let name = first_token(node, J::IDENTIFIER)
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    let ty_node = node.children().find(|child| is(child, J::TYPE));
    let ty = ty_node
        .as_ref()
        .map(|child| ItemTypeRef::from_spanned(type_from(child), child, map))
        .unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error));
    let varargs = node.children_with_tokens().any(|element| match element {
        NodeOrToken::Token(token) => token.kind() == J::ELLIPSIS,
        NodeOrToken::Node(_) => false,
    });
    // §9.7.4: the annotations on the component declaration (`record R(
    // @Ann String s)`) live in its leading `MODIFIER_LIST`, like a field's.
    let annotations = node
        .children()
        .find(|child| is(child, J::MODIFIER_LIST))
        .map(|mods| item_annotations_from(&mods, map))
        .unwrap_or_default();
    RecordComponent {
        name,
        ast: map
            .ast_id(&node_ptr(node))
            .expect("every record component FORMAL_PARAMETER is indexed"),
        ty,
        varargs,
        annotations,
    }
}

/// The annotation references of a `MODIFIER_LIST` node, in order, converted
/// to their range-free item form.
fn item_annotations_from(mods: &SyntaxNode<Lang>, map: &AstIdMap) -> Vec<ItemAnnotationRef> {
    mods.children()
        .filter(|child| matches!(child.kind(), J::ANNOTATION | J::MARKER_ANNOTATION))
        .filter_map(|annotation| {
            annotation_ref(&annotation)
                .map(|ranged| ItemAnnotationRef::from_spanned(ranged, &annotation, map))
        })
        .collect()
}

/// The `MODIFIER_LIST` children of a variable declaration or parameter node up
/// to its `TYPE` child — the declaration's own `{VariableModifier}` prefix
/// ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)):
/// its declaration annotations and its `final`. A node without a type
/// (`var`, or a declaration whose type failed to parse) has no `TYPE` to
/// split at, so every modifier list counts as the declaration's.
pub(crate) fn declaration_modifier_lists(node: &SyntaxNode<Lang>) -> Vec<SyntaxNode<Lang>> {
    let type_start = declared_type_start(node);
    node.children()
        .filter(|child| is(child, J::MODIFIER_LIST))
        .filter(|mods| type_start.is_none_or(|start| mods.text_range().start() < start))
        .collect()
}

/// The `MODIFIER_LIST` children of a parameter node that follow its `TYPE`
/// child: the *variable-arity modifier* of a varargs parameter
/// ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1),
/// `String @A ... p`). The grammar writes `UnannType` *before* it ([§9.7.4]:
/// the annotation applies to the array type the variable-arity parameter
/// declares, [§8.4.1]), so its annotations annotate the *type*, not the
/// declaration.
pub(crate) fn trailing_modifier_lists(node: &SyntaxNode<Lang>) -> Vec<SyntaxNode<Lang>> {
    let type_start = declared_type_start(node);
    node.children()
        .filter(|child| is(child, J::MODIFIER_LIST))
        .filter(|mods| type_start.is_some_and(|start| mods.text_range().start() > start))
        .collect()
}

/// Where the `TYPE` child of a variable declaration or parameter node starts,
/// the position that separates the declaration's modifiers from its type's.
fn declared_type_start(node: &SyntaxNode<Lang>) -> Option<TextSize> {
    node.children()
        .find(|child| is(child, J::TYPE))
        .map(|ty| ty.text_range().start())
}

/// The annotations of [`trailing_modifier_lists`], with their source ranges.
pub(crate) fn type_annotations_after_type(node: &SyntaxNode<Lang>) -> Vec<AnnotationRef> {
    trailing_modifier_lists(node)
        .iter()
        .flat_map(|mods| {
            mods.children()
                .filter(|child| matches!(child.kind(), J::ANNOTATION | J::MARKER_ANNOTATION))
                .filter_map(|annotation| annotation_ref(&annotation))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The annotations of a variable declaration's own modifier lists
/// ([`declaration_modifier_lists`]), with their source ranges — its
/// *declaration* annotations ([JLS §9.7.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.4)),
/// in source order. A method or constructor's formal parameter carries its
/// annotations in the item tree instead ([`Param::annotations`]); the
/// body-side variable declarations (locals, resources, enhanced-for
/// variables, exception parameters, pattern variables and lambda parameters)
/// carry them in the body IR.
pub(crate) fn modifier_annotations(node: &SyntaxNode<Lang>) -> Vec<AnnotationRef> {
    declaration_modifier_lists(node)
        .iter()
        .flat_map(|mods| {
            mods.children()
                .filter(|child| matches!(child.kind(), J::ANNOTATION | J::MARKER_ANNOTATION))
                .filter_map(|annotation| annotation_ref(&annotation))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The types listed in the named clause (`THROWS_CLAUSE`, `IMPLEMENTS_CLAUSE`,
/// ...), as range-free item type references.
fn clause_item_types(map: &AstIdMap, node: &SyntaxNode<Lang>, clause_kind: J) -> Vec<ItemTypeRef> {
    node.children()
        .find(|child| is(child, clause_kind))
        .map(|clause| {
            clause
                .children()
                .filter(|child| is(child, J::TYPE))
                .map(|child| ItemTypeRef::from_spanned(type_from(&child), &child, map))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn type_from(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    if !is(node, J::TYPE) {
        return SpannedTypeRef::synthetic(TypeRef::Error);
    }

    // primitive: the keyword is a direct token in most positions
    // (`int x`), but arrives wrapped in a `PRIMITIVE_TYPE_EXPR` node inside
    // a class literal's `TYPE` (`int[].class`).
    if let Some(prim) = node
        .children_with_tokens()
        .find_map(|element| element.as_token().and_then(primitive_from_token))
        .or_else(|| {
            node.children()
                .find_map(|child| primitive_from_node(&child))
        })
    {
        let mut ty = TypeRef::Primitive(prim);
        for _ in 0..dimension_count(node) {
            ty = TypeRef::Array(Box::new(ty));
        }
        // §9.7.4: the annotations on the array dimensions (`int @Nullable []`)
        // are reference names like any type name ([JLS §6.5.5.1]); the
        // structured form carries their element arguments.
        let type_use_annotations = type_annotation_refs(node);
        let refs = type_use_annotations
            .iter()
            .map(|annotation| annotation.name.clone())
            .collect();
        return SpannedTypeRef {
            ty,
            refs,
            type_use_annotations,
        };
    }

    // reference type: QUALIFIED_NAME [TYPE_ARGUMENTS] (DOT IDENTIFIER TYPE_ARGUMENTS)* [DIMENSIONS]
    let mut name = String::new();
    let mut generic_args = Vec::new();
    let mut saw_dot = false;
    // The source range of the reference name being built: from the start of
    // the first segment (the `QUALIFIED_NAME`) to the end of the last
    // identifier, excluding type arguments and dimensions.
    let mut name_start: Option<TextSize> = None;
    let mut name_end: TextSize = node.text_range().start();
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) => {
                if is(&child, J::ERROR) {
                    return SpannedTypeRef::synthetic(TypeRef::Error);
                }
                if is(&child, J::QUALIFIED_NAME) {
                    name.push_str(&trimmed_text(&child));
                    name_start = Some(name_start.unwrap_or(child.text_range().start()));
                    name_end = child.text_range().end();
                    saw_dot = false;
                } else if is(&child, J::TYPE_ARGUMENTS) {
                    generic_args.extend(type_arguments_from(&child));
                    saw_dot = false;
                } else if is(&child, J::LITERAL)
                    && child.children_with_tokens().any(|element| {
                        element
                            .as_token()
                            .is_some_and(|t| t.kind() == J::IDENTIFIER)
                    })
                    && let Some(ident) = child
                        .children_with_tokens()
                        .filter_map(|element| {
                            element
                                .as_token()
                                .filter(|t| t.kind() == J::IDENTIFIER)
                                .cloned()
                        })
                        .next()
                {
                    // A class literal reached through the *expression*
                    // grammar (`pick(String[].class)`): the type name parses
                    // as a primary expression, so its identifier arrives
                    // wrapped in a `LITERAL` node instead of a
                    // `QUALIFIED_NAME`.
                    name.push_str(ident.text());
                    name_start = Some(name_start.unwrap_or(child.text_range().start()));
                    name_end = child.text_range().end();
                    saw_dot = false;
                }
            }
            NodeOrToken::Token(token) => {
                if token_is(&token, J::DOT) {
                    saw_dot = true;
                } else if saw_dot && token_is(&token, J::IDENTIFIER) {
                    name.push('.');
                    name.push_str(token.text());
                    name_end = token.text_range().end();
                    saw_dot = false;
                }
            }
        }
    }

    if name.is_empty() {
        return SpannedTypeRef::synthetic(TypeRef::Error);
    }

    // Reference names of the type: its own name first, then its type-use
    // annotations ([§9.7.4]), then those of its (recursively) generic
    // arguments, depth-first. The structured type-use annotations likewise
    // flatten the generic arguments' own annotations, so a
    // `List<@NonNull String>` keeps the element annotation for the
    // annotation-target check ([§9.7.4]).
    let type_use_annotations = type_annotation_refs(node);
    let mut all_type_use_annotations = type_use_annotations.clone();
    for arg in &generic_args {
        all_type_use_annotations.extend(arg.type_use_annotations.iter().cloned());
    }
    let mut refs = Vec::with_capacity(1 + type_use_annotations.len() + generic_args.len());
    if let Some(start) = name_start {
        refs.push(NameRef::new(
            Name::new(&name.clone()),
            TextRange::new(start, name_end),
        ));
    }
    refs.extend(
        type_use_annotations
            .iter()
            .map(|annotation| annotation.name.clone()),
    );
    for arg in &generic_args {
        refs.extend(arg.refs.iter().cloned());
    }

    let ty = TypeRef::Reference {
        name: Name::new(&name),
        generic_args: generic_args.into_iter().map(|spanned| spanned.ty).collect(),
    };
    let mut ty = ty;
    for _ in 0..dimension_count(node) {
        ty = TypeRef::Array(Box::new(ty));
    }
    let mut spanned = SpannedTypeRef::new(ty, refs);
    spanned.type_use_annotations = all_type_use_annotations;
    spanned
}

/// The number of array dimensions attached to `node`: explicit `DIMENSIONS`
/// child nodes plus bare bracket-token pairs — the shape a class literal's
/// `TYPE` carries (`int[].class`: `PRIMITIVE_TYPE_EXPR` followed by raw
/// `[` `]` tokens, no `DIMENSIONS` wrapper).
fn dimension_count(node: &SyntaxNode<Lang>) -> usize {
    let mut dims = node
        .children()
        .filter(|child| is(child, J::DIMENSIONS))
        .map(|dims| {
            dims.children()
                .filter(|child| is(child, J::DIMENSION))
                .count()
        })
        .sum();
    let mut open = 0usize;
    for element in node.children_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        match token.kind() {
            J::L_BRACKET => open += 1,
            J::R_BRACKET if open > 0 => {
                open -= 1;
                dims += 1;
            }
            _ => {}
        }
    }
    dims
}

fn type_arguments_from(node: &SyntaxNode<Lang>) -> Vec<SpannedTypeRef> {
    node.children()
        .filter(|child| is(child, J::TYPE_ARGUMENT))
        .map(|child| type_argument_from(&child))
        .collect()
}

fn type_argument_from(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    if let Some(wildcard) = node.children().find(|child| is(child, J::WILDCARD_TYPE)) {
        wildcard_from(&wildcard)
    } else if let Some(ty) = node.children().find(|child| is(child, J::TYPE)) {
        type_from(&ty)
    } else {
        SpannedTypeRef::synthetic(TypeRef::Error)
    }
}

fn wildcard_from(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let (bound, refs) = match node.children().find(|child| is(child, J::WILDCARD_BOUNDS)) {
        Some(bounds) => {
            let is_super = bounds
                .children_with_tokens()
                .any(|element| element.as_token().is_some_and(|t| token_text(t, "super")));
            let inner = bounds
                .children()
                .find(|child| is(child, J::TYPE))
                .map(|child| type_from(&child))
                .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error));
            let refs = inner.refs.clone();
            let bound = if is_super {
                TypeBound::Lower(inner.ty)
            } else {
                TypeBound::Upper(inner.ty)
            };
            (Some(Box::new(bound)), refs)
        }
        None => (None, Vec::new()),
    };
    SpannedTypeRef {
        ty: TypeRef::Wildcard { bound },
        refs,
        type_use_annotations: Vec::new(),
    }
}

/// Wraps `ty` in one `Array` per `DIMENSION` child of `dims`, keeping the
/// reference names and type-use annotations.
fn wrap_dims(mut ty: ItemTypeRef, dims: &SyntaxNode<Lang>) -> ItemTypeRef {
    ty.ty = dims
        .children()
        .filter(|child| is(child, J::DIMENSION))
        .fold(ty.ty, |ty, _| TypeRef::Array(Box::new(ty)));
    ty
}

/// The keyword of a `PRIMITIVE_TYPE_EXPR` child node.
fn primitive_from_node(node: &SyntaxNode<Lang>) -> Option<PrimitiveType> {
    if !is(node, J::PRIMITIVE_TYPE_EXPR) {
        return None;
    }
    node.children_with_tokens()
        .find_map(|element| element.as_token().and_then(primitive_from_token))
}

fn primitive_from_token(token: &SyntaxToken<Lang>) -> Option<PrimitiveType> {
    let prim = match token.text() {
        "int" => PrimitiveType::Int,
        "long" => PrimitiveType::Long,
        "float" => PrimitiveType::Float,
        "double" => PrimitiveType::Double,
        "boolean" => PrimitiveType::Boolean,
        "byte" => PrimitiveType::Byte,
        "char" => PrimitiveType::Char,
        "short" => PrimitiveType::Short,
        "void" => PrimitiveType::Void,
        _ => return None,
    };
    Some(prim)
}

fn missing_name() -> Name {
    Name::new("<missing>")
}
