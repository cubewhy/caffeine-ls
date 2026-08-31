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
    body::ExprData,
    name::Name,
    span::{AnnotationArg, AnnotationRef, AnnotationValue, NameRef, SpannedTypeRef},
};

use super::super::item_tree::{
    AnnotationData, ClassData, EnumConstantData, EnumData, FieldData, InstanceInitData, ItemData,
    ItemId, MethodData, MethodExtra, MethodExtraJava, ModuleData, ModuleExports, ModuleProvides,
    ModuleRequires, Param, RecordComponent, RecordData, Signature, StaticInitData, TypeParam,
};
use super::super::modifiers::JavaModifiers;
use super::{LowerCtx, body};

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
        ctx.tree.package_range = qualified_name_child(node).map(|child| child.text_range());
    }
    // Every package declaration, for the duplicate-package check
    // ([JLS §7.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.4.1)).
    if let Some(range) = qualified_name_child(node).map(|child| child.text_range()) {
        ctx.tree.package_decl_ranges.push(range);
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

    ctx.tree.imports.push(crate::java::item_tree::ImportItem {
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
    let (name, name_range) =
        decl_identifier(node).unwrap_or_else(|| (missing_name(), node.text_range()));
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
    let type_params = child_type_params(node);
    let super_class = clause_types(node, J::EXTENDS_CLAUSE).into_iter().next();
    let interfaces = clause_types(node, J::IMPLEMENTS_CLAUSE);
    let permits = clause_types(node, J::PERMITS_CLAUSE);
    let body = body_members(ctx, node, J::CLASS_BODY);
    ctx.alloc(ItemData::Class(ClassData {
        name,
        name_range,
        modifiers,
        annotations,
        super_class,
        interfaces,
        permits,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_interface(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let (name, name_range) =
        decl_identifier(node).unwrap_or_else(|| (missing_name(), node.text_range()));
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
    let type_params = child_type_params(node);
    let interfaces = clause_types(node, J::INTERFACE_EXTENDS_CLAUSE);
    let permits = clause_types(node, J::PERMITS_CLAUSE);
    let body = body_members(ctx, node, J::INTERFACE_BODY);
    ctx.alloc(ItemData::Interface(ClassData {
        name,
        name_range,
        modifiers,
        annotations,
        super_class: None,
        interfaces,
        permits,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_enum(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let (name, name_range) =
        decl_identifier(node).unwrap_or_else(|| (missing_name(), node.text_range()));
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
    let interfaces = clause_types(node, J::IMPLEMENTS_CLAUSE);
    let body = node
        .children()
        .find(|child| is(child, J::ENUM_BODY))
        .map(|body| enum_body_members(ctx, &body))
        .unwrap_or_default();
    ctx.alloc(ItemData::Enum(EnumData {
        name,
        name_range,
        modifiers,
        annotations,
        interfaces,
        body,
        range: node.text_range(),
    }))
}

fn lower_record(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let (name, name_range) =
        decl_identifier(node).unwrap_or_else(|| (missing_name(), node.text_range()));
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
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
    let permits = clause_types(node, J::PERMITS_CLAUSE);
    // The component list `(int x, int y)` is the record's parameter
    // declaration — the "definition" the outline selects.
    let components_range = node
        .children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| params.text_range())
        .unwrap_or(name_range);
    // The header ends at the closing `)` of the component list, or the end of
    // the `implements` clause when present — before the body `{ ... }`.
    let header_end = node
        .children()
        .find(|child| is(child, J::FORMAL_PARAMETERS))
        .map(|params| params.text_range().end())
        .or_else(|| {
            node.children()
                .find(|child| is(child, J::IMPLEMENTS_CLAUSE))
                .map(|clause| clause.text_range().end())
        })
        .unwrap_or(name_range.end());
    let header_range = TextRange::new(node.text_range().start(), header_end);
    let body = body_members(ctx, node, J::RECORD_BODY);
    ctx.alloc(ItemData::Record(RecordData {
        name,
        name_range,
        components_range,
        header_range,
        modifiers,
        annotations,
        components,
        interfaces,
        permits,
        type_params,
        body,
        range: node.text_range(),
    }))
}

fn lower_annotation_type(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> ItemId {
    let (name, name_range) =
        decl_identifier(node).unwrap_or_else(|| (missing_name(), node.text_range()));
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
    let body = body_members(ctx, node, J::ANNOTATION_TYPE_BODY);
    ctx.alloc(ItemData::Annotation(AnnotationData {
        name,
        name_range,
        modifiers,
        annotations,
        body,
        range: node.text_range(),
    }))
}

fn lower_method(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let (name, name_range) = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
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
        name_range,
        modifiers,
        annotations,
        sig,
        extra: MethodExtra::Java(MethodExtraJava {
            is_constructor: false,
            is_compact_constructor: false,
            body: None,
            default_value: None,
            default_expr: None,
        }),
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
        let MethodExtra::Java(java) = &mut data.extra;
        java.body = Some(body);
    }
    Some(id)
}

fn lower_constructor(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>, compact: bool) -> Option<ItemId> {
    let (name, name_range) = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
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
        name_range,
        modifiers,
        annotations,
        sig,
        extra: MethodExtra::Java(MethodExtraJava {
            is_constructor: true,
            is_compact_constructor: compact,
            body: None,
            default_value: None,
            default_expr: None,
        }),
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
        let MethodExtra::Java(java) = &mut data.extra;
        java.body = Some(body);
    }
    Some(id)
}

fn lower_annotation_element(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    let (name, name_range) = decl_identifier(node)?;
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
    let ret = node
        .children()
        .find(|child| is(child, J::TYPE))
        .map(|child| type_from(&child));
    let default_value = token_range_after(node, J::DEFAULT_KW, J::SEMICOLON);
    let id = ctx.alloc(ItemData::Method(MethodData {
        name,
        name_range,
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
            default_value,
            default_expr: None,
        }),
        range: node.text_range(),
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

fn lower_field_decl(ctx: &mut LowerCtx, node: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
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
        let Some((name, name_range)) = declarator
            .children_with_tokens()
            .filter_map(|element| element.as_token().cloned())
            .find(|token| token_is(token, J::IDENTIFIER))
            .map(|token| (Name::new(token.text()), token.text_range()))
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
            name_range,
            modifiers,
            annotations: annotations.clone(),
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
            let (name, name_range) = first_token(&child, J::IDENTIFIER)
                .map(|token| (Name::new(token.text()), token.text_range()))
                .unwrap_or_else(|| (missing_name(), child.text_range()));
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
                name_range,
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
    let (name, name_range) = qualified_name_child(node)
        .map(|child| (Name::new(&trimmed_text(&child)), child.text_range()))
        .unwrap_or_else(|| (missing_name(), node.text_range()));
    // §9.7: the module declaration's annotations live in its leading modifier
    // list (`@Ann module com.example {}`).
    let (modifiers, annotations) = child_modifiers_and_annotations(node);
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
        name_range,
        modifiers,
        annotations,
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
    let range = directive
        .children()
        .find(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| child.text_range())
        .unwrap_or_else(|| directive.text_range());
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
        range,
    }
}

fn package_exports_from(directive: &SyntaxNode<Lang>) -> ModuleExports {
    let names: Vec<(Name, TextRange)> = directive
        .children()
        .filter(|child| is(child, J::QUALIFIED_NAME))
        .map(|child| (Name::new(&trimmed_text(&child)), child.text_range()))
        .collect();
    let package = names
        .first()
        .map(|(name, _)| name.clone())
        .unwrap_or_else(missing_name);
    let package_range = names
        .first()
        .map(|(_, range)| *range)
        .unwrap_or_else(|| directive.text_range());
    let to = names.iter().skip(1).map(|(name, _)| name.clone()).collect();
    let to_ranges = names.iter().skip(1).map(|(_, range)| *range).collect();
    ModuleExports {
        package,
        to,
        package_range,
        to_ranges,
    }
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
        type_use_annotations: Vec::new(),
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
/// (e.g. `record`, `open`). For a declaration this is the declared name. The
/// token's source range is returned alongside so the IDE can point the LSP
/// `selectionRange` at the name (rather than the whole declaration).
fn decl_identifier(node: &SyntaxNode<Lang>) -> Option<(Name, TextRange)> {
    for element in node.children_with_tokens() {
        if let Some(token) = element.as_token()
            && token.kind() == J::IDENTIFIER
            && !matches!(
                token.text(),
                // §3.9: the restricted identifiers `record`, `sealed`,
                // `non-sealed` and `permits` may precede a type name but are
                // not the declared name (and cannot name a type). `open` is
                // only restricted in *module* positions (the module
                // declaration is lowered separately), so a member method
                // named `open` is a perfectly ordinary declaration — it must
                // not be dropped here (an anonymous-class `close()` method is
                // fine; a `static InputStream open(...)` that vanished would
                // leave every call site reporting "cannot find symbol").
                "record" | "sealed" | "non-sealed" | "permits"
            )
        {
            return Some((Name::new(token.text()), token.text_range()));
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

/// The first `MODIFIER_LIST` child, split into its syntax modifiers
/// ([`JavaModifiers`]) and its declared annotation references ([JLS §9.7]),
/// which are decoupled from the modifier flags.
fn child_modifiers_and_annotations(node: &SyntaxNode<Lang>) -> (JavaModifiers, Vec<AnnotationRef>) {
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
            let annotations = annotations_from(&mods);
            (modifiers, annotations)
        })
        .unwrap_or_default()
}

/// The reference name (`NameRef`) of an `ANNOTATION`/`MARKER_ANNOTATION`
/// syntax node: the (possibly qualified) name after the `@`
/// ([JLS §9.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7))
/// with its source range.
fn annotation_name_ref(annotation: &SyntaxNode<Lang>) -> Option<NameRef> {
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
fn annotation_args_from(list: &SyntaxNode<Lang>) -> Vec<AnnotationArg> {
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
fn is_element_value(node: &SyntaxNode<Lang>) -> bool {
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
fn annotation_value_from(node: &SyntaxNode<Lang>) -> Option<(AnnotationValue, TextRange)> {
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
    let name_token = first_token(node, J::IDENTIFIER);
    let name = name_token
        .as_ref()
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    let name_range = name_token.map(|token| token.text_range());
    let ty_node = node.children().find(|child| is(child, J::TYPE));
    let ty = ty_node
        .as_ref()
        .map(|child| type_from(child))
        .unwrap_or(SpannedTypeRef::synthetic(TypeRef::Error));
    let ty_range = ty_node.map(|child| child.text_range());
    let varargs = node.children_with_tokens().any(|element| match element {
        NodeOrToken::Token(token) => token.kind() == J::ELLIPSIS,
        NodeOrToken::Node(_) => false,
    });
    // §9.7.4: the annotations on the component declaration (`record R(
    // @Ann String s)`) live in its leading `MODIFIER_LIST`, like a field's.
    let annotations = node
        .children()
        .find(|child| is(child, J::MODIFIER_LIST))
        .map(|mods| annotations_from(&mods))
        .unwrap_or_default();
    RecordComponent {
        name,
        name_range: name_range.unwrap_or(node.text_range()),
        range: node.text_range(),
        ty,
        ty_range: ty_range.unwrap_or(node.text_range()),
        varargs,
        annotations,
    }
}

/// The annotation references of a `MODIFIER_LIST` node, in order.
fn annotations_from(mods: &SyntaxNode<Lang>) -> Vec<AnnotationRef> {
    mods.children()
        .filter(|child| matches!(child.kind(), J::ANNOTATION | J::MARKER_ANNOTATION))
        .filter_map(|annotation| annotation_ref(&annotation))
        .collect()
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
            J::R_BRACKET => {
                if open > 0 {
                    open -= 1;
                    dims += 1;
                }
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

/// Wraps `spanned` in one `Array` per `DIMENSION` child of `dims`.
fn wrap_dims(spanned: SpannedTypeRef, dims: &SyntaxNode<Lang>) -> SpannedTypeRef {
    let ty = dims
        .children()
        .filter(|child| is(child, J::DIMENSION))
        .fold(spanned.ty, |ty, _| TypeRef::Array(Box::new(ty)));
    SpannedTypeRef {
        ty,
        refs: spanned.refs,
        type_use_annotations: spanned.type_use_annotations,
    }
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
