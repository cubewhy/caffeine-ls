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
    item_tree::{
        AnnotationData, ClassData, EnumConstantData, EnumData, FieldData, InstanceInitData,
        ItemData, ItemId, MethodData, ModuleData, ModuleExports, ModuleProvides, ModuleRequires,
        Param, RecordComponent, RecordData, Signature, StaticInitData, TypeParam,
    },
    modifiers::Modifiers,
    name::Name,
    span::{NameRef, SpannedTypeRef},
};

use crate::lower::LowerCtx;

pub(super) mod body;

pub(super) fn lower_file(ctx: &mut LowerCtx, file: &java_syntax::SourceFile) {
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

fn lower_package(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) {
    if let Some(name) = qualified_name_text(node) {
        ctx.tree.package = Some(name);
    }
}

fn lower_import(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) {
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

    ctx.tree.imports.push(hir_expand::item_tree::ImportItem {
        name: Name::new(name_text),
        is_static,
        is_asterisk,
        range: node.text_range(),
    });
}

/// Lowers any declaration that can appear in a class body, returning `None`
/// for node kinds that carry no items (`EMPTY_DECL`, `ERROR`).
fn lower_member(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    if is(node, J::STATIC_INITIALIZER) {
        let block = node.children().find(|child| is(child, J::BLOCK));
        let id = ctx.alloc(ItemData::StaticInit(StaticInitData {
            body: None,
            range: node.text_range(),
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
            range: node.text_range(),
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

fn lower_class(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_identifier(node).unwrap_or_else(missing_name);
    let modifiers = child_modifiers(node);
    let type_params = child_type_params(node);
    let super_class = clause_types(node, J::EXTENDS_CLAUSE).into_iter().next();
    let interfaces = clause_types(node, J::IMPLEMENTS_CLAUSE);
    let body = body_members(ctx, node, J::CLASS_BODY);
    ctx.alloc(ItemData::Class(ClassData {
        name,
        modifiers,
        super_class,
        interfaces,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_interface(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_identifier(node).unwrap_or_else(missing_name);
    let modifiers = child_modifiers(node);
    let type_params = child_type_params(node);
    let interfaces = clause_types(node, J::INTERFACE_EXTENDS_CLAUSE);
    let body = body_members(ctx, node, J::INTERFACE_BODY);
    ctx.alloc(ItemData::Interface(ClassData {
        name,
        modifiers,
        super_class: None,
        interfaces,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_enum(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_identifier(node).unwrap_or_else(missing_name);
    let modifiers = child_modifiers(node);
    let interfaces = clause_types(node, J::IMPLEMENTS_CLAUSE);
    let body = node
        .children()
        .find(|child| is(child, J::ENUM_BODY))
        .map(|body| enum_body_members(ctx, &body))
        .unwrap_or_default();
    ctx.alloc(ItemData::Enum(EnumData {
        name,
        modifiers,
        interfaces,
        body,
        range: node.text_range(),
    }))
}

fn lower_record(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_identifier(node).unwrap_or_else(missing_name);
    let modifiers = child_modifiers(node);
    let type_params = child_type_params(node);
    let components = node
        .children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| {
            params
                .children()
                .filter(|child| is(child, J::FORMAL_PARAMETER) || is(child, J::SPREAD_PARAMETER))
                .map(|child| component_from(&child))
                .collect()
        })
        .unwrap_or_default();
    let interfaces = clause_types(node, J::IMPLEMENTS_CLAUSE);
    let body = body_members(ctx, node, J::RECORD_BODY);
    ctx.alloc(ItemData::Record(RecordData {
        name,
        modifiers,
        components,
        interfaces,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_annotation_type(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = decl_identifier(node).unwrap_or_else(missing_name);
    let modifiers = child_modifiers(node);
    let body = body_members(ctx, node, J::ANNOTATION_TYPE_BODY);
    ctx.alloc(ItemData::Annotation(AnnotationData {
        name,
        modifiers,
        body,
        range: node.text_range(),
    }))
}

fn lower_method(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let modifiers = child_modifiers(node);
    let ret = if token_is_direct(node, J::VOID_KW) {
        Some(SpannedTypeRef::synthetic(TypeRef::Primitive(
            PrimitiveType::Void,
        )))
    } else {
        node.children()
            .find(|child| is(child, J::TYPE))
            .map(|child| type_from(&child))
    };
    let sig = Signature {
        type_params: child_type_params(node),
        params: formal_params(node),
        ret,
        throws: clause_types(node, J::THROWS_CLAUSE),
    };
    let block = node.children().find(|child| is(child, J::BLOCK));
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        sig,
        is_constructor: false,
        body: None,
        default_value: None,
        default_expr: None,
        range: node.text_range(),
    }));
    if let Some(block) = block {
        let params = node
            .children()
            .find(|child| is(child, J::FORMAL_PARAMETERS));
        let body = body::lower_method_body(ctx, id, &block, params.as_ref());
        let ItemData::Method(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("method");
        };
        data.body = Some(body);
    }
    Some(id)
}

fn lower_constructor(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>, compact: bool) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let modifiers = child_modifiers(node);
    let sig = Signature {
        type_params: child_type_params(node),
        params: if compact {
            Vec::new()
        } else {
            formal_params(node)
        },
        ret: None,
        throws: if compact {
            Vec::new()
        } else {
            clause_types(node, J::THROWS_CLAUSE)
        },
    };
    let block = node.children().find(|child| is(child, J::BLOCK));
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        sig,
        is_constructor: true,
        body: None,
        default_value: None,
        default_expr: None,
        range: node.text_range(),
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
        data.body = Some(body);
    }
    Some(id)
}

fn lower_annotation_element(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let name = decl_identifier(node)?;
    let modifiers = child_modifiers(node);
    let ret = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| type_from(&child));
    let default_value = token_range_after(node, J::DEFAULT_KW, J::SEMICOLON);
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        modifiers,
        sig: Signature {
            type_params: Vec::new(),
            params: Vec::new(),
            ret,
            throws: Vec::new(),
        },
        is_constructor: false,
        body: None,
        default_value,
        default_expr: None,
        range: node.text_range(),
    }));
    if let Some(value_node) = body::find_expression_child(node)
        && let Some(expr_id) = body::lower_expr(ctx, id, &value_node)
    {
        let ItemData::Method(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("annotation element");
        };
        data.default_expr = Some(expr_id);
    }
    Some(id)
}

fn lower_field_decl(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let modifiers = child_modifiers(node);
    let ty = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| type_from(&child));
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
        let initializer = declarator
            .children_with_tokens()
            .filter_map(|element| element.as_token().cloned())
            .find(|token| token_is(token, J::EQUAL))
            .map(|equal| TextRange::new(equal.text_range().end(), declarator.text_range().end()));
        let expr_slot = body::find_expression_child(&declarator);
        let field_id = ctx.alloc(ItemData::Field(FieldData {
            name,
            modifiers: modifiers.clone(),
            ty,
            initializer,
            initializer_expr: None,
            range: declarator.text_range(),
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

fn enum_body_members(ctx: &mut LowerCtx, body: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let mut ids = Vec::new();
    for child in body.children() {
        if is(&child, J::ENUM_CONSTANT) {
            let name = first_token(&child, J::IDENTIFIER)
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(missing_name);
            let arguments = child
                .children()
                .find(|nested| is(nested, J::ARGUMENT_LIST))
                .map(|nested| nested.text_range());
            let class_body = child
                .children()
                .find(|nested| is(nested, J::CLASS_BODY))
                .map(|nested| nested.text_range());
            let constant_id = ctx.alloc(ItemData::EnumConstant(EnumConstantData {
                name,
                arguments,
                argument_exprs: Vec::new(),
                class_body,
                range: child.text_range(),
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
        } else if let Some(id) = lower_member(ctx, &child) {
            ids.push(id);
        }
    }
    ids
}

/// Lower all members of the named body node (a direct child of `node`).
fn body_members(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>, body_kind: J) -> Vec<ItemId> {
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

fn lower_module(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let name = qualified_name_text(node).unwrap_or_else(missing_name);
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
                requires.push(requires_from(&directive));
            } else if is(&directive, J::EXPORTS_DIRECTIVE) {
                exports.push(package_exports_from(&directive));
            } else if is(&directive, J::OPENS_DIRECTIVE) {
                opens.push(package_exports_from(&directive));
            } else if is(&directive, J::USES_DIRECTIVE) {
                if let Some(child) = qualified_name_child(&directive) {
                    uses.push(qualified_name_ref(&child));
                }
            } else if is(&directive, J::PROVIDES_DIRECTIVE) {
                provides.push(provides_from(&directive));
            }
        }
    }

    ctx.alloc(ItemData::Module(ModuleData {
        name,
        modifiers: Modifiers::default(),
        is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
        range: node.text_range(),
    }))
}

fn requires_from(directive: &SyntaxNode<Lang>) -> ModuleRequires {
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
    }
}

fn package_exports_from(directive: &SyntaxNode<Lang>) -> ModuleExports {
    let names: Vec<Name> = directive
        .children()
        .filter(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| Name::new(&trimmed_text(&child)))
        .collect();
    let package = names.first().cloned().unwrap_or_else(missing_name);
    let to = names.into_iter().skip(1).collect();
    ModuleExports { package, to }
}

fn provides_from(directive: &SyntaxNode<Lang>) -> ModuleProvides {
    let names: Vec<SpannedTypeRef> = directive
        .children()
        .filter(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| qualified_name_ref(&child))
        .collect();
    let service = names
        .first()
        .cloned()
        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error));
    let implementations = names.into_iter().skip(1).collect();
    ModuleProvides {
        service,
        implementations,
    }
}

/// A single fully qualified *type* name child of a syntax node.
fn qualified_name_child(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children().find(|child| is(child, J::QUALIFIED_NAME))
}

/// A `TypeRef::Reference` over a qualified-name syntax node, carrying the
/// node's source range.
fn qualified_name_ref(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let name = &trimmed_text(node);
    SpannedTypeRef {
        ty: TypeRef::Reference {
            name: Name::new(name),
            generic_args: Vec::new(),
        },
        refs: vec![NameRef::new(Name::new(name), node.text_range())],
    }
}

// --- helpers ---

pub(super) fn is(node: &SyntaxNode<Lang>, kind: J) -> bool {
    node.kind() == kind
}

pub(super) fn token_is(token: &SyntaxToken<Lang>, kind: J) -> bool {
    token.kind() == kind
}

pub(super) fn token_text(token: &SyntaxToken<Lang>, text: &str) -> bool {
    token.text() == text
}

pub(super) fn trimmed_text(node: &SyntaxNode<Lang>) -> String {
    node.text().to_string().trim().to_owned()
}

/// The first direct-child `IDENTIFIER` token that is not a contextual keyword
/// (e.g. `record`, `open`). For a declaration this is the declared name.
fn decl_identifier(node: &SyntaxNode<Lang>) -> Option<Name> {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token()
            && token.kind() == J::IDENTIFIER
            && !matches!(
                token.text(),
                "record" | "sealed" | "non-sealed" | "open" | "module" | "permits"
            )
        {
            return Some(Name::new(token.text()));
        }
    }
    None
}

pub(super) fn first_token(node: &SyntaxNode<Lang>, kind: J) -> Option<SyntaxToken<Lang>> {
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

/// The first `MODIFIER_LIST` child, if any.
fn child_modifiers(node: &SyntaxNode<Lang>) -> Modifiers {
    node.children()
        .find(|child| is(child, J::MODIFIER_LIST))
        .map(|mods| {
            let mut modifiers = Modifiers::default();
            for element in mods.children_with_tokens() {
                match element {
                    // §9.7: an annotation in the modifier list — record its
                    // (possibly qualified) name.
                    NodeOrToken::Node(annotation)
                        if matches!(annotation.kind(), J::ANNOTATION | J::MARKER_ANNOTATION) =>
                    {
                        if let Some(name) = annotation
                            .descendants()
                            .find(|d| d.kind() == J::QUALIFIED_NAME)
                        {
                            modifiers.push_annotation(Name::new(&name.text().to_string()));
                        }
                    }
                    NodeOrToken::Node(_) => {}
                    NodeOrToken::Token(token) => {
                        modifiers.push(token.text());
                    }
                }
            }
            modifiers
        })
        .unwrap_or_default()
}

fn child_type_params(node: &SyntaxNode<Lang>) -> Vec<TypeParam> {
    node.children()
        .find(|child| is(child, J::TYPE_PARAMETERS))
        .map(|child| type_params_from(&child))
        .unwrap_or_default()
}

fn type_params_from(node: &SyntaxNode<Lang>) -> Vec<TypeParam> {
    node.children()
        .filter(|child| is(child, J::TYPE_PARAMETER))
        .map(|child| type_param_from(&child))
        .collect()
}

fn type_param_from(node: &SyntaxNode<Lang>) -> TypeParam {
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
                .map(|child| type_from(&child))
                .collect()
        })
        .unwrap_or_default();
    TypeParam {
        name,
        bounds,
        annotations: Vec::new(),
    }
}

fn formal_params(node: &SyntaxNode<Lang>) -> Vec<Param> {
    node.children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| {
            params
                .children()
                .filter(|child| is(child, J::FORMAL_PARAMETER) || is(child, J::SPREAD_PARAMETER))
                .map(|child| param_from(&child))
                .collect()
        })
        .unwrap_or_default()
}

fn param_from(node: &SyntaxNode<Lang>) -> Param {
    let varargs = is(node, J::SPREAD_PARAMETER);
    let mut ty = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| type_from(&child))
        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error));
    if let Some(dims) = node.children().find(|child| is(child, J::DIMENSIONS)) {
        ty = wrap_dims(ty, &dims);
    }
    let name = first_token(node, J::IDENTIFIER)
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    Param { name, ty, varargs }
}

fn component_from(node: &SyntaxNode<Lang>) -> RecordComponent {
    let name = first_token(node, J::IDENTIFIER)
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    let ty = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| type_from(&child))
        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error));
    let varargs = node.children_with_tokens().any(|element| match element {
        NodeOrToken::Token(token) => token.kind() == J::ELLIPSIS,
        NodeOrToken::Node(_) => false,
    });
    RecordComponent {
        name,
        ty,
        varargs,
        annotations: Vec::new(),
    }
}

/// The types listed in the named clause (`THROWS_CLAUSE`, `IMPLEMENTS_CLAUSE`,
/// ...).
fn clause_types(node: &SyntaxNode<Lang>, clause_kind: J) -> Vec<SpannedTypeRef> {
    node.children()
        .find(|child| is(child, clause_kind))
        .map(|clause| {
            clause
                .children()
                .filter(|child| is(child, J::TYPE))
                .map(|child| type_from(&child))
                .collect()
        })
        .unwrap_or_default()
}

fn qualified_name_text(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children()
        .find(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| Name::new(&trimmed_text(&child)))
}

/// Range from the end of the first `start_kind` token to the start of the
/// first `end_kind` token (or the end of the node).
fn token_range_after(node: &SyntaxNode<Lang>, start_kind: J, end_kind: J) -> Option<TextRange> {
    let start = first_token(node, start_kind)?.text_range().end();
    let end = node
        .children_with_tokens()
        .filter_map(|element| element.as_token().cloned())
        .find(|token| token_is(token, end_kind))
        .map_or_else(
            || node.text_range().end(),
            |token| token.text_range().start(),
        );
    Some(TextRange::new(start, end))
}

pub(super) fn type_from(node: &SyntaxNode<Lang>) -> SpannedTypeRef {
    if !is(node, J::TYPE) {
        return SpannedTypeRef::synthetic(TypeRef::Error);
    }

    // primitive
    if let Some(prim) = node
        .children_with_tokens()
        .find_map(|element| element.as_token().and_then(primitive_from_token))
    {
        let ty = TypeRef::Primitive(prim);
        return node
            .children()
            .filter(|child| is(child, J::DIMENSIONS))
            .fold(SpannedTypeRef::synthetic(ty), |spanned, dims| {
                wrap_dims(spanned, &dims)
            });
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

    // Reference names of the type: its own name first, then those of its
    // (recursively) generic arguments, depth-first.
    let mut refs = Vec::with_capacity(1 + generic_args.len());
    if let Some(start) = name_start {
        refs.push(NameRef::new(
            Name::new(&name.clone()),
            TextRange::new(start, name_end),
        ));
    }
    for arg in &generic_args {
        refs.extend(arg.refs.iter().cloned());
    }

    let ty = TypeRef::Reference {
        name: Name::new(&name),
        generic_args: generic_args.into_iter().map(|spanned| spanned.ty).collect(),
    };
    node.children()
        .filter(|child| is(child, J::DIMENSIONS))
        .fold(SpannedTypeRef::new(ty, refs), |spanned, dims| {
            wrap_dims(spanned, &dims)
        })
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
    }
}

/// Wraps `spanned` in one `Array` per `DIMENSION` child of `dims`.
fn wrap_dims(spanned: SpannedTypeRef, dims: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let ty = dims
        .children()
        .filter(|child| is(child, J::DIMENSION))
        .fold(spanned.ty, |ty, _| TypeRef::Array(Box::new(ty)));
    SpannedTypeRef {
        ty,
        refs: spanned.refs,
    }
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
