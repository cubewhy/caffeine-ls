//! Kotlin CST → item tree.
//!
//! The walker mirrors the parser grammar: `MODIFIER_LIST`, the declaration
//! nodes (`CLASS_DECL`, `OBJECT_DECL`, `COMPANION_OBJECT`, `FUNCTION_DECL`,
//! `PROPERTY_DECL`, `SECONDARY_CONSTRUCTOR`, `ANONYMOUS_INITIALIZER`,
//! `ENUM_ENTRY`, `TYPE_ALIAS`), their declaration parts (`TYPE_PARAMETERS`,
//! `PRIMARY_CONSTRUCTOR`, `DELEGATION_SPECIFIERS`, `RECEIVER_TYPE`, the
//! accessors) and the type nodes.
//!
//! The tree is a *declaration* IR: bodies and initializer expressions are
//! lowered into the per-file body tree ([`super::super::lower`]'s `bodies`)
//! and the source ranges of every declaration are kept in the id map rather
//! than in the tree.
//!
//! Every production cites its KLS rule.

use rowan::{NodeOrToken, SyntaxNode, SyntaxToken, TextRange};
use syntax::kotlin::{Lang, SyntaxKind as K};
use syntax::stub::{TypeBound, TypeRef};

use hir_expand::{ast_id_map::FileAstId, name::Name};

use super::LowerCtx;
use super::body;
use crate::kotlin::item_tree::{
    AccessorData, AnonymousInitializerNode, ClassData, ClassDeclNode, ConstructorData,
    ConstructorDeclNode, EnumEntryData, EnumEntryNode, FileAnnotationNode, FunctionData,
    FunctionDeclNode, ImportHeaderNode, InitData, ItemAnnotationArg, ItemAnnotationRef,
    ItemAnnotationValue, ItemId, ItemTypeRef, KotlinClassKind, KotlinImportItem, KotlinItemData,
    KotlinTypeParam, PackageHeaderNode, Param, PropertyData, PropertyNode, TypeAliasData,
    TypeAliasNode, ast_id_of, ast_id_or_placeholder,
};
use crate::kotlin::modifiers::{KotlinModifiers, KotlinVariance};

pub(super) fn lower_file(ctx: &mut LowerCtx<'_>, file: &kotlin_syntax::SourceFile) {
    for child in file.syntax_node.children() {
        match child.kind() {
            K::FILE_ANNOTATION => ctx
                .tree
                .file_annotations
                .push(ast_id_of::<FileAnnotationNode, _>(ctx.map, &child)),
            K::PACKAGE_HEADER => lower_package(ctx, &child),
            K::IMPORT_LIST => {
                for import in child.children() {
                    lower_import(ctx, &import);
                }
            }
            K::CLASS_DECL | K::OBJECT_DECL => {
                let id = lower_class(ctx, &child);
                ctx.tree.top.push(id);
            }
            K::FUNCTION_DECL => {
                let id = lower_function(ctx, &child);
                ctx.tree.top.push(id);
            }
            K::PROPERTY_DECL => {
                let ids = lower_property(ctx, &child);
                ctx.tree.top.extend(ids);
            }
            K::TYPE_ALIAS => {
                let id = lower_type_alias(ctx, &child);
                ctx.tree.top.push(id);
            }
            _ => {}
        }
    }
}

/// `packageHeader`: ['package' {NL} identifier {NL} {'.' {NL} identifier}]
/// [spec: grammar-rule-packageHeader] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-packageHeader
fn lower_package(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) {
    // The declared name is the `QUALIFIED_NAME` child; the header itself holds
    // only the `package` keyword and that node.
    let Some(name) = node
        .children()
        .find(|child| is(child, K::QUALIFIED_NAME))
        .and_then(|qualified| qualified_name(&qualified))
    else {
        return;
    };
    ctx.tree.package = Some(name);
    ctx.tree.package_header = Some(ast_id_of::<PackageHeaderNode, _>(ctx.map, node));
}

/// `importHeader`: 'import' {NL} identifier {NL} {'.' {NL} identifier}
///                  {NL} ['.' {NL} '*'] {NL} [importAlias]
/// `importAlias`: 'as' {NL} simpleIdentifier
/// [spec: grammar-rule-importHeader] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-importHeader
fn lower_import(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) {
    let Some(path) = node.children().find(|child| is(child, K::IMPORT_PATH)) else {
        return;
    };

    let mut segments = Vec::new();
    let mut alias = None;
    let mut is_asterisk = false;
    for element in path.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) => match child.kind() {
                K::QUALIFIED_NAME => segments = dotted_segments(&child),
                K::IMPORT_ALIAS => {
                    alias = child
                        .children_with_tokens()
                        .filter_map(NodeOrToken::into_token)
                        .find(|token| is_token(token, K::IDENTIFIER))
                        .map(|token| Name::new(token.text()));
                }
                _ => {}
            },
            NodeOrToken::Token(token) => is_asterisk |= is_token(&token, K::STAR),
        }
    }

    let name = if segments.is_empty() {
        missing_name()
    } else {
        Name::new(&segments.join("."))
    };
    ctx.tree.imports.push(KotlinImportItem {
        path: name,
        alias,
        is_asterisk,
        ast: ast_id_of::<ImportHeaderNode, _>(ctx.map, node),
    });
}

/// `classDeclaration` — the one production behind `class`, `interface`,
/// `enum class`, `annotation class`, `data class` and `value class`
/// [spec: grammar-rule-classDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classDeclaration
/// `objectDeclaration` — `object`
/// [spec: grammar-rule-objectDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-objectDeclaration
/// `companionObject` — `companion object`
/// [spec: grammar-rule-companionObject] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-companionObject
///
/// All three lower to [`ClassData`], differing in [`KotlinClassKind`]. The
/// primary constructor and the properties its `val`/`var` class parameters
/// declare are lowered *before* the class-body members, in source order.
fn lower_class(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let (mut modifiers, annotations) = modifiers_of(ctx, node);
    if node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::FUN_KW))
    {
        // `fun interface X` writes the `fun` as a hard keyword, not a modifier
        // keyword ([spec: grammar-rule-classDeclaration]).
        modifiers
            .flags
            .insert(crate::kotlin::modifiers::KotlinModifierFlags::FUN_INTERFACE);
    }
    let kind = class_kind(node, &modifiers);
    let name = class_name(node, kind).unwrap_or_else(missing_name);

    let id = ctx.alloc(KotlinItemData::Class(ClassData {
        name,
        kind,
        modifiers,
        annotations,
        type_params: lower_type_params(ctx, node),
        super_types: lower_super_types(ctx, node),
        primary_constructor: None,
        body: Vec::new(),
        ast: ast_id_of::<ClassDeclNode, _>(ctx.map, node),
    }));

    let mut body = Vec::new();
    let mut primary_constructor = None;
    if let Some(parameters) = node
        .children()
        .find(|child| is(child, K::PRIMARY_CONSTRUCTOR))
    {
        let (constructor, properties) = lower_primary_constructor(ctx, &parameters);
        primary_constructor = Some(constructor);
        body.extend(properties);
    }
    if let Some(class_body) = node
        .children()
        .find(|child| matches!(child.kind(), K::CLASS_BODY | K::ENUM_CLASS_BODY))
    {
        body.extend(lower_class_members(ctx, &class_body));
    }

    let KotlinItemData::Class(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a class");
    };
    data.primary_constructor = primary_constructor;
    data.body = body;
    id
}

/// The kind of a classifier, from its node kind and modifiers
/// ([KLS `declarations.html#classifier-declaration`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)):
/// an `object`/`companion object` node kind names itself, and an
/// `annotation`/`enum` modifier turns the single `CLASS_DECL` production into
/// the other two classifier kinds.
fn class_kind(node: &SyntaxNode<Lang>, modifiers: &KotlinModifiers) -> KotlinClassKind {
    use crate::kotlin::modifiers::KotlinModifierFlags;
    match node.kind() {
        K::OBJECT_DECL => KotlinClassKind::Object,
        K::COMPANION_OBJECT => KotlinClassKind::CompanionObject,
        _ if modifiers.flags.contains(KotlinModifierFlags::ANNOTATION) => {
            KotlinClassKind::Annotation
        }
        _ if modifiers.flags.contains(KotlinModifierFlags::ENUM) => KotlinClassKind::Enum,
        _ if node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .any(|token| is_token(&token, K::INTERFACE_KW)) =>
        {
            KotlinClassKind::Interface
        }
        _ => KotlinClassKind::Class,
    }
}

/// The declared name of a classifier: the identifier after the introducing
/// keyword (`class`/`interface`/`object`) — never the `companion` of a
/// companion object, which precedes `object`. An unnamed companion takes the
/// name the compiler gives it ([KLS
/// `declarations.html#companion-objects`](https://kotlinlang.org/spec/declarations.html#companion-objects)).
fn class_name(node: &SyntaxNode<Lang>, kind: KotlinClassKind) -> Option<Name> {
    let mut after_keyword = false;
    for element in node.children_with_tokens() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        match token.kind() {
            K::CLASS_KW | K::INTERFACE_KW | K::OBJECT_KW => after_keyword = true,
            K::IDENTIFIER if after_keyword => return Some(Name::new(token.text())),
            _ => {}
        }
    }
    (kind == KotlinClassKind::CompanionObject).then(|| Name::new("Companion"))
}

/// The members of a `classBody`/`enumClassBody`
/// ([spec: grammar-rule-classBody],
/// [spec: grammar-rule-enumClassBody]).
fn lower_class_members(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let mut out = Vec::new();
    for child in node.children() {
        match child.kind() {
            K::ENUM_ENTRIES => {
                for entry in child.children().filter(|c| is(c, K::ENUM_ENTRY)) {
                    out.push(lower_enum_entry(ctx, &entry));
                }
            }
            K::CLASS_DECL | K::OBJECT_DECL | K::COMPANION_OBJECT => {
                out.push(lower_class(ctx, &child));
            }
            K::FUNCTION_DECL => out.push(lower_function(ctx, &child)),
            K::PROPERTY_DECL => out.extend(lower_property(ctx, &child)),
            K::TYPE_ALIAS => out.push(lower_type_alias(ctx, &child)),
            K::SECONDARY_CONSTRUCTOR => out.push(lower_secondary_constructor(ctx, &child)),
            K::ANONYMOUS_INITIALIZER => out.push(lower_init(ctx, &child)),
            _ => {}
        }
    }
    out
}

/// `primaryConstructor`: [[modifiers] 'constructor' {NL}] classParameters
/// [spec: grammar-rule-primaryConstructor] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-primaryConstructor
///
/// Returns the constructor item plus the property of every `val`/`var` class
/// parameter. A class parameter *without* `val`/`var` is a constructor
/// parameter only — it declares no property (observed with javap: `class
/// Point(val x: Int)` emits `getX()`, a plain `class Point(x: Int)` emits no
/// accessor).
fn lower_primary_constructor(
    ctx: &mut LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> (ItemId, Vec<ItemId>) {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    let parameters: Vec<SyntaxNode<Lang>> = node
        .children()
        .find(|child| is(child, K::CLASS_PARAMETERS))
        .map(|parameters| {
            parameters
                .children()
                .filter(|child| is(child, K::CLASS_PARAMETER))
                .collect()
        })
        .unwrap_or_default();

    let id = ctx.alloc(KotlinItemData::Constructor(ConstructorData {
        params: parameters
            .iter()
            .map(|parameter| lower_param(ctx, parameter))
            .collect(),
        modifiers,
        annotations,
        delegation: None,
        body: None,
        ast: ast_id_of::<ConstructorDeclNode, _>(ctx.map, node),
    }));

    let mut properties = Vec::new();
    for parameter in &parameters {
        let Some(id) = lower_class_parameter_property(ctx, parameter) else {
            continue;
        };
        properties.push(id);
    }
    (id, properties)
}

/// `secondaryConstructor`: [modifiers] 'constructor' functionValueParameters
///                         [':' constructorDelegationCall] [block]
/// [spec: grammar-rule-secondaryConstructor] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-secondaryConstructor
fn lower_secondary_constructor(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    let id = ctx.alloc(KotlinItemData::Constructor(ConstructorData {
        params: lower_params(ctx, node),
        modifiers,
        annotations,
        delegation: node
            .children()
            .find(|child| is(child, K::CONSTRUCTOR_DELEGATION_CALL))
            .map(|child| ast_id_of(ctx.map, &child)),
        body: None,
        ast: ast_id_of::<ConstructorDeclNode, _>(ctx.map, node),
    }));
    if let Some(body) = body::lower_constructor_body(ctx, id, node) {
        let KotlinItemData::Constructor(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("just allocated a constructor");
        };
        data.body = Some(body);
    }
    id
}

/// `anonymousInitializer`: 'init' {NL} block
/// [spec: grammar-rule-anonymousInitializer] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-anonymousInitializer
fn lower_init(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let id = ctx.alloc(KotlinItemData::AnonymousInitializer(InitData {
        body: None,
        ast: ast_id_of::<AnonymousInitializerNode, _>(ctx.map, node),
    }));
    if let Some(body) = body::lower_init_body(ctx, id, node) {
        let KotlinItemData::AnonymousInitializer(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("just allocated an initializer");
        };
        data.body = Some(body);
    }
    id
}

/// `functionDeclaration`: [modifiers] 'fun' [typeParameters] [receiverType '.']
///                        simpleIdentifier functionValueParameters [':' type]
///                        [typeConstraints] [functionBody]
/// [spec: grammar-rule-functionDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionDeclaration
fn lower_function(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    let id = ctx.alloc(KotlinItemData::Function(FunctionData {
        name: function_name(node).unwrap_or_else(missing_name),
        modifiers,
        annotations,
        type_params: lower_type_params(ctx, node),
        receiver: receiver_type(ctx, node),
        defaults: trailing_defaults(node),
        params: lower_params(ctx, node),
        ret: declared_type(ctx, node),
        body: None,
        ast: ast_id_of::<FunctionDeclNode, _>(ctx.map, node),
    }));
    if let Some(body) = body::lower_function_body(ctx, id, node) {
        let KotlinItemData::Function(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("just allocated a function");
        };
        data.body = Some(body);
    }
    id
}

/// `propertyDeclaration`: [modifiers] ('val' | 'var') [typeParameters]
///                        [receiverType '.'] (variableDeclaration |
///                        multiVariableDeclaration) [typeConstraints]
///                        [('=' expression) | propertyDelegate] [';']
///                        [accessors]
/// [spec: grammar-rule-propertyDeclaration] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-propertyDeclaration
///
/// A destructuring declaration (`val (a, b) = pair`) declares one property per
/// bound name, so this returns an item per name, in source order. The declared
/// accessors (which a destructuring declaration cannot have) are attached to
/// the first.
fn lower_property(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemId> {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    let is_var = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::VAR_KW));
    let type_params = lower_type_params(ctx, node);
    let receiver = receiver_type(ctx, node);
    let ast: FileAstId<PropertyNode> = ast_id_of(ctx.map, node);

    let declarations: Vec<SyntaxNode<Lang>> = match node
        .children()
        .find(|child| is(child, K::MULTI_VARIABLE_DECLARATION))
    {
        Some(multi) => multi
            .children()
            .filter(|child| is(child, K::VARIABLE_DECLARATION))
            .collect(),
        None => node
            .children()
            .filter(|child| is(child, K::VARIABLE_DECLARATION))
            .collect(),
    };

    let mut ids = Vec::new();
    let mut declared = Vec::new();
    for declaration in &declarations {
        let name = variable_declaration_name(declaration).unwrap_or_else(missing_name);
        let ty = variable_declaration_type(ctx, declaration);
        declared.push((name, ty));
    }
    if declared.is_empty() {
        declared.push((missing_name(), None));
    }

    let mut declared_types = Vec::with_capacity(declared.len());
    for (name, ty) in declared {
        declared_types.push(ty.clone());
        ids.push(ctx.alloc(KotlinItemData::Property(PropertyData {
            name,
            modifiers,
            annotations: annotations.clone(),
            type_params: type_params.clone(),
            receiver: receiver.clone(),
            ty,
            is_var,
            initializer_expr: None,
            delegate_expr: None,
            accessors: Vec::new(),
            ast,
        })));
    }

    // The initializer, the delegate expression and the accessors belong to
    // every property the declaration binds (a destructuring declaration binds
    // several; only a plain one can declare accessors).
    let initializer = body::lower_property_initializer(ctx, ids[0], node);
    let delegate = body::lower_property_delegate(ctx, ids[0], node);
    for &id in &ids {
        let KotlinItemData::Property(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("just allocated a property");
        };
        data.initializer_expr = initializer;
        data.delegate_expr = delegate;
    }

    let accessors: Vec<ItemId> = node
        .children()
        .filter(|child| matches!(child.kind(), K::GETTER | K::SETTER))
        .map(|accessor| {
            let is_setter = is(&accessor, K::SETTER);
            lower_accessor(ctx, &accessor, is_setter)
        })
        .collect();
    if let (Some(&first), false) = (ids.first(), accessors.is_empty())
        && let KotlinItemData::Property(data) = ctx.tree.items.get_mut(first.0)
    {
        data.accessors = accessors;
    }
    ids
}

/// `getter`: [modifiers] 'get' ['(' {NL} ')' [':' type]] functionBody
/// [spec: grammar-rule-getter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-getter
/// `setter`: [modifiers] 'set' ['(' functionValueParameterWithOptionalType
///            [{NL} ','] ')' [':' type]] functionBody
/// [spec: grammar-rule-setter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-setter
///
/// `property_ty` is the property's declared type: a setter parameter that
/// writes no type takes it ([KLS
/// `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters)).
fn lower_accessor(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>, is_setter: bool) -> ItemId {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    let id = ctx.alloc(KotlinItemData::Accessor(AccessorData {
        is_setter,
        modifiers,
        annotations,
        params: node
            .children()
            .filter(|child| is(child, K::VALUE_PARAMETER))
            .map(|parameter| lower_param(ctx, &parameter))
            .collect(),
        body: None,
        ast: ast_id_of(ctx.map, node),
    }));
    if let Some(body) = body::lower_accessor_body(ctx, id, node) {
        let KotlinItemData::Accessor(data) = ctx.tree.items.get_mut(id.0) else {
            unreachable!("just allocated an accessor");
        };
        data.body = Some(body);
    }
    id
}

/// `enumEntry`: [modifiers] simpleIdentifier [valueArguments] [classBody]
/// [spec: grammar-rule-enumEntry] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-enumEntry
fn lower_enum_entry(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let (_, annotations) = modifiers_of(ctx, node);
    let name = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
        .unwrap_or_else(missing_name);
    let body = node
        .children()
        .find(|child| is(child, K::CLASS_BODY))
        .map(|body| lower_class_members(ctx, &body))
        .unwrap_or_default();
    let id = ctx.alloc(KotlinItemData::EnumEntry(EnumEntryData {
        name,
        annotations,
        argument_exprs: Vec::new(),
        body,
        ast: ast_id_of::<EnumEntryNode, _>(ctx.map, node),
    }));
    let arguments = body::lower_enum_entry_arguments(ctx, id, node);
    let KotlinItemData::EnumEntry(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated an enum entry");
    };
    data.argument_exprs = arguments;
    id
}

/// `typeAlias`: [modifiers] 'typealias' simpleIdentifier [typeParameters]
///              {NL} '=' {NL} type
/// [spec: grammar-rule-typeAlias] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeAlias
fn lower_type_alias(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let (modifiers, annotations) = modifiers_of(ctx, node);
    ctx.alloc(KotlinItemData::TypeAlias(TypeAliasData {
        name: first_identifier(node).unwrap_or_else(missing_name),
        modifiers,
        annotations,
        type_params: lower_type_params(ctx, node),
        target: declared_type(ctx, node).unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error)),
        ast: ast_id_of::<TypeAliasNode, _>(ctx.map, node),
    }))
}

/// Lowers a *local* declaration the body walker found in a block or in an
/// expression: a local class, object, function, type alias or an object
/// literal. The item is recorded as a local declaration of the file (the tree's
/// `local_types`) and given its declaring item as its parent — only the body
/// knows which declaration declares it — so the workspace symbol index, which
/// walks `top` and `body()` only, never surfaces it ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration)).
pub(super) fn lower_local_declaration(
    ctx: &mut LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> Option<ItemId> {
    let item = match node.kind() {
        K::CLASS_DECL | K::OBJECT_DECL | K::COMPANION_OBJECT => lower_class(ctx, node),
        K::FUNCTION_DECL => lower_function(ctx, node),
        K::TYPE_ALIAS => lower_type_alias(ctx, node),
        K::OBJECT_LITERAL => lower_object_literal(ctx, node)?,
        _ => return None,
    };
    record_local(ctx, item);
    Some(item)
}

/// An object literal `object : Base() { … }` ([KLS
/// `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals)):
/// the anonymous class its body declares, lowered as an `object` classifier
/// with no name of its own.
fn lower_object_literal(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    lower_class(ctx, node);
    // `lower_class` reads the classifier's own keyword; an object literal has
    // none, so the item it allocated carries the missing name — the class of
    // the anonymous type, named the way the compiler names it.
    Some(
        *ctx.tree
            .top
            .last()
            .unwrap_or(&ItemId(hir_expand::arena::ArenaId(u32::MAX))),
    )
}

/// Marks `item` as a local declaration of the file: it joins `local_types`
/// (in lowering order, which is source order) and takes the declaration whose
/// body is being lowered as its parent.
fn record_local(ctx: &mut LowerCtx<'_>, item: ItemId) {
    if !ctx.tree.local_types.contains(&item) {
        ctx.tree.local_types.push(item);
    }
}

/// The property a `val`/`var` class parameter declares ([KLS
/// `declarations.html#primary-constructor`](https://kotlinlang.org/spec/declarations.html#primary-constructor)),
/// or `None` for a parameter without `val`/`var` (which declares none).
fn lower_class_parameter_property(
    ctx: &mut LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> Option<ItemId> {
    let is_var = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::VAR_KW));
    let is_property = is_var
        || node
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .any(|token| is_token(&token, K::VAL_KW));
    if !is_property {
        return None;
    }
    let (modifiers, annotations) = modifiers_of(ctx, node);
    Some(ctx.alloc(KotlinItemData::Property(PropertyData {
        name: first_identifier(node).unwrap_or_else(missing_name),
        modifiers,
        annotations,
        type_params: Vec::new(),
        receiver: None,
        ty: declared_type(ctx, node),
        is_var,
        initializer_expr: None,
        delegate_expr: None,
        accessors: Vec::new(),
        ast: ast_id_of(ctx.map, node),
    })))
}

/// `classParameters` / `functionValueParameters`: the declared parameters of a
/// constructor or function, in source order ([spec:
/// grammar-rule-classParameters], [spec:
/// grammar-rule-functionValueParameters]).
///
/// `node` is the declaration or the parameter list itself; the parameters are
/// the `CLASS_PARAMETER`/`VALUE_PARAMETER` children of the list.
fn lower_params(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<Param> {
    let list = node
        .children()
        .find(|child| matches!(child.kind(), K::VALUE_PARAMETERS | K::CLASS_PARAMETERS))
        .unwrap_or_else(|| node.clone());
    list.children()
        .filter(|child| matches!(child.kind(), K::VALUE_PARAMETER | K::CLASS_PARAMETER))
        .map(|child| lower_param(ctx, &child))
        .collect()
}

/// `classParameter`: [modifiers] ['val' | 'var'] {NL} simpleIdentifier ':' type
///                   ['=' expression]
/// [spec: grammar-rule-classParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-classParameter
/// `functionValueParameter`: [parameterModifiers] parameter ['=' expression]
/// [spec: grammar-rule-functionValueParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionValueParameter
/// `parameterWithOptionalType`: simpleIdentifier [':' type]
/// [spec: grammar-rule-parameterWithOptionalType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-parameterWithOptionalType
///
/// `fallback_ty` is the type of a parameter that writes none — only a setter's
/// parameter may ([`lower_accessor`]).
fn lower_param(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Param {
    let mut annotations = Vec::new();
    let mut varargs = false;
    if let Some(modifiers) = node.children().find(|child| is(child, K::MODIFIER_LIST)) {
        for element in modifiers.children_with_tokens() {
            match element {
                NodeOrToken::Node(annotation) if is(&annotation, K::ANNOTATION) => {
                    annotations.extend(lower_annotation(ctx, &annotation));
                }
                NodeOrToken::Token(token) if is_token(&token, K::IDENTIFIER) => {
                    varargs |= token.text() == "vararg";
                }
                _ => {}
            }
        }
    }
    for child in node.children().filter(|child| is(child, K::ANNOTATION)) {
        annotations.extend(lower_annotation(ctx, &child));
    }
    // `vararg` is a parameter *modifier*, written as a bare identifier token
    // ([spec: grammar-rule-parameterModifiers]).
    varargs |= node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::IDENTIFIER) && token.text() == "vararg");

    Param {
        name: parameter_name(node).unwrap_or_else(missing_name),
        // A parameter without a declared type is a *setter*'s, whose type is
        // the property's ([KLS
        // `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters)):
        // the parameter records no type, and the type layer takes the
        // property's — a parameter with no type anywhere is an erroneous
        // declaration, and its type is the error type.
        ty: declared_type(ctx, node).unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error)),
        varargs,
        annotations,
    }
}

/// `typeParameters`: '<' {NL} typeParameter {{NL} ',' {NL} typeParameter}
///                   [{NL} ','] {NL} '>'
/// [spec: grammar-rule-typeParameters] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeParameters
///
/// The `where` constraints of the same declaration
/// ([spec: grammar-rule-typeConstraints]) are bounds of the type parameter
/// they name, and are folded into it.
fn lower_type_params(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<KotlinTypeParam> {
    let Some(parameters) = node.children().find(|child| is(child, K::TYPE_PARAMETERS)) else {
        return Vec::new();
    };
    let mut out: Vec<KotlinTypeParam> = parameters
        .children()
        .filter(|child| is(child, K::TYPE_PARAMETER))
        .map(|child| lower_type_param(ctx, &child))
        .collect();

    if let Some(constraints) = node.children().find(|child| is(child, K::TYPE_CONSTRAINTS)) {
        for constraint in constraints.children().filter(|c| is(c, K::TYPE_CONSTRAINT)) {
            let Some(name) = first_identifier(&constraint) else {
                continue;
            };
            let Some(ty) = declared_type(ctx, &constraint) else {
                continue;
            };
            if let Some(param) = out.iter_mut().find(|param| param.name == name) {
                param.bounds.push(ty);
            }
        }
    }
    out
}

/// `typeParameter`: [typeParameterModifiers] simpleIdentifier [':' type]
/// [spec: grammar-rule-typeParameter] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeParameter
fn lower_type_param(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> KotlinTypeParam {
    let mut variance = None;
    let mut reified = false;
    let mut annotations = Vec::new();
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) if is(&child, K::ANNOTATION) => {
                annotations.extend(lower_annotation(ctx, &child));
            }
            NodeOrToken::Token(token) => match token.kind() {
                K::IN_KW => variance = Some(KotlinVariance::In),
                K::IDENTIFIER if token.text() == "out" => variance = Some(KotlinVariance::Out),
                K::IDENTIFIER if token.text() == "reified" => reified = true,
                _ => {}
            },
            _ => {}
        }
    }
    KotlinTypeParam {
        name: type_parameter_name(node).unwrap_or_else(missing_name),
        variance,
        reified,
        bounds: declared_type(ctx, node).into_iter().collect(),
        annotations,
    }
}

/// `delegationSpecifiers` ([spec: grammar-rule-delegationSpecifiers]) — the
/// supertypes of a classifier, in source order. The constructor arguments of a
/// supertype *call* (`class C : Base(1)`) and the delegate expression of
/// `interface I by delegate` are body content and are not part of the type.
fn lower_super_types(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemTypeRef> {
    let Some(specifiers) = node
        .children()
        .find(|child| is(child, K::DELEGATION_SPECIFIERS))
    else {
        return Vec::new();
    };
    specifiers
        .children()
        .filter(|child| is(child, K::DELEGATION_SPECIFIER))
        .filter_map(|specifier| {
            let ty = child_type(&specifier).or_else(|| {
                specifier
                    .children()
                    .find(|child| {
                        matches!(
                            child.kind(),
                            K::CONSTRUCTOR_INVOCATION | K::EXPLICIT_DELEGATION
                        )
                    })
                    .and_then(|wrapper| child_type(&wrapper))
            })?;
            Some(item_type_ref(ctx, &ty))
        })
        .collect()
}

/// The declared type of a declaration: its direct type-node child (`: type`
/// after the name, `= type` for a type alias), resolved into an
/// [`ItemTypeRef`].
fn declared_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemTypeRef> {
    child_type(node).map(|ty| item_type_ref(ctx, &ty))
}

/// The extension receiver type of a declaration, if it declares one
/// ([spec: grammar-rule-receiverType]).
///
/// A declaration's receiver is a `RECEIVER_TYPE` node holding the receiver's
/// dotted name and type arguments *directly* — the parser writes no inner type
/// node there — so the node itself is lowered as the type
/// ([`lower_type_node`]).
fn receiver_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemTypeRef> {
    node.children()
        .find(|child| is(child, K::RECEIVER_TYPE))
        .map(|receiver| item_type_ref(ctx, &receiver))
}

/// The lowered form of a Kotlin `type` ([spec: grammar-rule-type]): the
/// range-free [`TypeRef`], every reference name it mentions with its source
/// range (depth-first, source order — the order [`ItemTypeRef::refs`] keeps,
/// and the order the navigation layer's occurrence walk re-derives), and the
/// type-use annotations it carries.
pub(crate) struct LoweredType {
    pub ty: TypeRef<Name>,
    pub refs: Vec<(Name, TextRange)>,
    pub annotations: Vec<ItemAnnotationRef>,
}

impl LoweredType {
    fn error() -> LoweredType {
        LoweredType {
            ty: TypeRef::Error,
            refs: Vec::new(),
            annotations: Vec::new(),
        }
    }
}

/// Lowers a type node into an [`ItemTypeRef`], anchoring the type's syntax
/// node.
pub(crate) fn item_type_ref(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemTypeRef {
    let lowered = lower_type_node(ctx, node);
    ItemTypeRef {
        ty: lowered.ty,
        refs: lowered.refs.into_iter().map(|(name, _)| name).collect(),
        type_use_annotations: lowered.annotations,
        node: ast_id_or_placeholder(ctx.map, node),
    }
}

/// `type`: [typeModifiers] (functionType | parenthesizedType | nullableType
///         | typeReference | definitelyNonNullableType)
/// [spec: grammar-rule-type] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-type
///
/// The node kind *is* the type's nullability: the parser completes a nullable
/// type as `NULLABLE_TYPE` and a definitely-non-nullable one as
/// `DEFINITELY_NON_NULLABLE_TYPE`, so `T?` and `T & Any` are [`TypeRef::Nullable`]
/// and [`TypeRef::DefinitelyNonNull`].
///
/// A `suspend` function type is lowered as its `FunctionN` classifier: K2 keeps
/// suspend-ness out of the classifier (it is an attribute of the type), and
/// this model carries no such attribute — a recorded deviation.
///
/// A `dynamic` type (`typeReference`) is not a JVM type and lowers to
/// [`TypeRef::Error`] — a recorded deviation.
pub(crate) fn lower_type_node(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> LoweredType {
    match node.kind() {
        K::USER_TYPE => lower_user_type(ctx, node),
        K::FUNCTION_TYPE => lower_function_type(ctx, node),
        // A declaration's receiver (`val Response.body: String`): the node
        // holds the receiver's name and type arguments directly, and a
        // `String?` receiver is spelled with a `?` inside it.
        K::RECEIVER_TYPE => {
            let mut lowered = lower_user_type(ctx, node);
            if node
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| is_token(&token, K::QUESTION))
            {
                lowered.ty = TypeRef::Nullable(Box::new(lowered.ty));
            }
            lowered
        }
        K::NULLABLE_TYPE => {
            let mut inner = lower_type_body(ctx, node);
            inner.ty = TypeRef::Nullable(Box::new(inner.ty));
            inner
        }
        K::DEFINITELY_NON_NULLABLE_TYPE => {
            // `T & Any`: the `& Any` conjunct is what makes `T` definitely
            // non-null, and the grammar admits no second conjunct of another
            // kind, so the marker carries the whole meaning.
            let mut inner = lower_type_body(ctx, node);
            inner.ty = TypeRef::DefinitelyNonNull(Box::new(inner.ty));
            inner
        }
        _ => lower_type_body(ctx, node),
    }
}

/// The type proper of a `type` node, past its type modifiers
/// ([spec: grammar-rule-typeModifiers]).
fn lower_type_body(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> LoweredType {
    let Some(inner) = child_type(node) else {
        // No type node: a `dynamic` type, or an error.
        return LoweredType::error();
    };
    // A wrapper node's child is the type proper — a parenthesized type's
    // parentheses are not part of the type — so the recursion handles every
    // shape through one entry point.
    let mut lowered = lower_type_node(ctx, &inner);
    let mut annotations = type_modifier_annotations(ctx, node);
    annotations.append(&mut lowered.annotations);
    lowered.annotations = annotations;
    lowered
}

/// The annotations of a `typeModifiers` prefix ([spec:
/// grammar-rule-typeModifiers]) — the ones on the type node itself, as
/// opposed to those of a nested type.
fn type_modifier_annotations(
    ctx: &LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> Vec<ItemAnnotationRef> {
    node.children()
        .filter(|child| is(child, K::ANNOTATION))
        .flat_map(|child| lower_annotation(ctx, &child))
        .collect()
}

/// `userType`: simpleUserType {{NL} '.' {NL} simpleUserType}
/// [spec: grammar-rule-userType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-userType
///
/// A Kotlin source type is always a *classifier* reference: `Int` is the class
/// `kotlin.Int`, not a JVM primitive, so the reference name is the dotted name
/// as written and the type layer resolves it.
fn lower_user_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> LoweredType {
    let segments = dotted_segments(node);
    let name = if segments.is_empty() {
        missing_name()
    } else {
        Name::new(&segments.join("."))
    };

    let mut refs = vec![(name.clone(), dotted_name_range(node))];
    let mut annotations = Vec::new();
    let mut generic_args = Vec::new();
    for arguments in node.children().filter(|child| is(child, K::TYPE_ARGUMENTS)) {
        for projection in arguments
            .children()
            .filter(|child| is(child, K::TYPE_PROJECTION))
        {
            let (ty, mut projection_refs, mut projection_annotations) =
                lower_projection(ctx, &projection);
            generic_args.push(ty);
            refs.append(&mut projection_refs);
            annotations.append(&mut projection_annotations);
        }
    }
    LoweredType {
        ty: TypeRef::Reference { name, generic_args },
        refs,
        annotations,
    }
}

/// `typeProjection`: ([typeProjectionModifiers] type) | '*'
/// [spec: grammar-rule-typeProjection] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeProjection
///
/// Declaration-site variance is the *declaring* class's concern; a use-site
/// projection is a wildcard: `out T` is an upper-bounded wildcard, `in T` a
/// lower-bounded one, `*` an unbounded one
/// ([KLS `type-system.html#type-containment`](https://kotlinlang.org/spec/type-system.html#type-containment)).
fn lower_projection(
    ctx: &LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> (
    TypeRef<Name>,
    Vec<(Name, TextRange)>,
    Vec<ItemAnnotationRef>,
) {
    let mut variance = None;
    let mut annotations = type_modifier_annotations(ctx, node);
    for element in node.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) if is(&child, K::ANNOTATION) => {
                annotations.extend(lower_annotation(ctx, &child));
            }
            NodeOrToken::Token(token) => match token.kind() {
                K::STAR => return (TypeRef::Wildcard { bound: None }, Vec::new(), annotations),
                K::IN_KW => variance = Some(KotlinVariance::In),
                K::IDENTIFIER if token.text() == "out" => variance = Some(KotlinVariance::Out),
                _ => {}
            },
            _ => {}
        }
    }
    let Some(variance) = variance else {
        // An invariant projection is the type itself.
        let Some(inner) = child_type(node) else {
            return (TypeRef::Error, Vec::new(), annotations);
        };
        let lowered = lower_type_node(ctx, &inner);
        annotations.extend(lowered.annotations);
        return (lowered.ty, lowered.refs, annotations);
    };
    let Some(inner) = child_type(node) else {
        return (TypeRef::Error, Vec::new(), annotations);
    };
    let lowered = lower_type_node(ctx, &inner);
    annotations.extend(lowered.annotations);
    let bound = match variance {
        KotlinVariance::Out => TypeBound::Upper(lowered.ty),
        KotlinVariance::In => TypeBound::Lower(lowered.ty),
    };
    (
        TypeRef::Wildcard {
            bound: Some(Box::new(bound)),
        },
        lowered.refs,
        annotations,
    )
}

/// `functionType`: [receiverType {NL} '.' {NL}] functionTypeParameters
///                 {NL} '->' {NL} type
/// [spec: grammar-rule-functionType] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-functionType
///
/// A Kotlin function type is the classifier `kotlin.FunctionN` over its
/// parameters followed by its return type
/// ([KLS `type-system.html#function-types`](https://kotlinlang.org/spec/type-system.html#function-types)).
/// A *receiver* function type `C.(A) -> R` is an extension function type,
/// whose receiver is its first parameter (K2 marks it with
/// `@ExtensionFunctionType`; this model puts the receiver first instead) — a
/// recorded deviation.
fn lower_function_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> LoweredType {
    let mut refs = Vec::new();
    let mut annotations = Vec::new();
    let mut args = Vec::new();

    if let Some(receiver) = node
        .children()
        .find(|child| is(child, K::RECEIVER_TYPE))
        .and_then(|receiver| child_type(&receiver))
    {
        let lowered = lower_type_node(ctx, &receiver);
        args.push(lowered.ty);
        refs.extend(lowered.refs);
        annotations.extend(lowered.annotations);
    }

    if let Some(parameters) = node.children().find(|child| is(child, K::VALUE_PARAMETERS)) {
        for parameter in parameters.children() {
            let ty = if is(&parameter, K::VALUE_PARAMETER) {
                child_type(&parameter)
            } else {
                Some(parameter.clone())
            };
            let Some(ty) = ty else {
                continue;
            };
            let lowered = lower_type_node(ctx, &ty);
            args.push(lowered.ty);
            refs.extend(lowered.refs);
            annotations.extend(lowered.annotations);
        }
    }

    if let Some(ret) = node
        .children()
        .filter(|child| is_type_node(child.kind()))
        .last()
    {
        let lowered = lower_type_node(ctx, &ret);
        args.push(lowered.ty);
        refs.extend(lowered.refs);
        annotations.extend(lowered.annotations);
    }

    // `kotlin.FunctionN<P1, …, PN, R>`: N is the number of *parameters*, so
    // the return type is one argument more. Confirmed with kotlinc 2.4.20:
    // `(Int) -> String` is `Function1<Int, String>`, `(Int, String) -> Boolean`
    // is `Function2<Int, String, Boolean>`, `String.(Int) -> Boolean` is
    // `Function2<String, Int, Boolean>` (the receiver first).
    let params = args.len().saturating_sub(1);
    LoweredType {
        ty: TypeRef::Reference {
            name: Name::new(&format!("Function{params}")),
            generic_args: args,
        },
        refs,
        annotations,
    }
}

/// `annotation`: (singleAnnotation | multiAnnotation) {NL}
/// [spec: grammar-rule-annotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-annotation
///
/// A multi-annotation (`@[A B]`) is sugar for several annotations that share
/// one syntax node — and therefore one range, one
/// [`ItemAnnotationRef::node`] — so it yields one item per annotation body.
///
/// The element values are kept as their source text
/// ([`ItemAnnotationValue::Unresolved`]) until the body lowering can anchor
/// them to expressions; the *names* are authoritative either way.
fn lower_annotation(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemAnnotationRef> {
    let id = ast_id_or_placeholder(ctx.map, node);
    let mut out: Vec<ItemAnnotationRef> = Vec::new();
    let mut pending: Option<Name> = None;
    for child in node.children() {
        match child.kind() {
            K::USER_TYPE => {
                if let Some(name) = pending.take() {
                    out.push(ItemAnnotationRef {
                        name,
                        args: Vec::new(),
                        node: id,
                    });
                }
                pending = lower_user_type(ctx, &child).ty.as_reference_name().cloned();
            }
            K::VALUE_ARGUMENTS => {
                if let Some(name) = pending.take() {
                    out.push(ItemAnnotationRef {
                        name,
                        args: lower_annotation_args(&child),
                        node: id,
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(name) = pending {
        out.push(ItemAnnotationRef {
            name,
            args: Vec::new(),
            node: id,
        });
    }
    out
}

/// `valueArgument`: [annotation] {NL} [simpleIdentifier {NL} '=' {NL}] ['*']
///                  {NL} expression
/// [spec: grammar-rule-valueArgument] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-valueArgument
fn lower_annotation_args(node: &SyntaxNode<Lang>) -> Vec<ItemAnnotationArg> {
    node.children()
        .filter(|child| is(child, K::VALUE_ARGUMENT))
        .map(|argument| {
            let name = argument
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|token| is_token(token, K::IDENTIFIER))
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(|| Name::new("value"));
            let text = argument
                .children()
                .find(|child| !is(child, K::ANNOTATION))
                .map(|expression| expression.text().to_string().trim().to_owned())
                .unwrap_or_default();
            ItemAnnotationArg {
                name,
                value: ItemAnnotationValue::Unresolved { text },
            }
        })
        .collect()
}

/// The source modifiers and the annotations of a declaration, from its
/// `MODIFIER_LIST` ([spec: grammar-rule-modifiers]). Annotations are
/// declaration attributes, not modifiers, and are returned separately.
fn modifiers_of(
    ctx: &LowerCtx<'_>,
    node: &SyntaxNode<Lang>,
) -> (KotlinModifiers, Vec<ItemAnnotationRef>) {
    let mut modifiers = KotlinModifiers::none();
    let mut annotations = Vec::new();
    let Some(list) = node.children().find(|child| is(child, K::MODIFIER_LIST)) else {
        return (modifiers, annotations);
    };
    for element in list.children_with_tokens() {
        match element {
            NodeOrToken::Node(child) if is(&child, K::ANNOTATION) => {
                annotations.extend(lower_annotation(ctx, &child));
            }
            NodeOrToken::Token(token) if is_token(&token, K::IDENTIFIER) => {
                modifiers.push_keyword(token.text());
            }
            _ => {}
        }
    }
    (modifiers, annotations)
}

/// How many trailing parameters of a declaration's parameter list declare a
/// default value (`fun f(a: Int, b: Int = 0, c: Int = 1)`) — the arity a call
/// may omit ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
fn trailing_defaults(node: &SyntaxNode<Lang>) -> usize {
    let Some(parameters) = node.children().find(|child| is(child, K::VALUE_PARAMETERS)) else {
        return 0;
    };
    let parameters: Vec<SyntaxNode<Lang>> = parameters
        .children()
        .filter(|child| is(child, K::VALUE_PARAMETER))
        .collect();
    parameters
        .iter()
        .rev()
        .take_while(|parameter| {
            parameter
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| is_token(&token, K::EQUAL))
        })
        .count()
}

/// The name of a `functionDeclaration`: the identifier between the optional
/// receiver and the parameter list.
fn function_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    let mut after_fun = false;
    for element in node.children_with_tokens() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        match token.kind() {
            K::FUN_KW => after_fun = true,
            K::IDENTIFIER if after_fun => return Some(Name::new(token.text())),
            _ => {}
        }
    }
    None
}

/// The name of a `parameter`/`parameterWithOptionalType`: the first identifier
/// token that is not a parameter modifier (`vararg`, `noinline`,
/// `crossinline`).
fn parameter_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| is_token(token, K::IDENTIFIER))
        .find(|token| !matches!(token.text(), "vararg" | "noinline" | "crossinline"))
        .map(|token| Name::new(token.text()))
}

/// The name of a `typeParameter`: the identifier token that is not one of its
/// modifiers (`reified`, `in`, `out`).
fn type_parameter_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| is_token(token, K::IDENTIFIER))
        .find(|token| !matches!(token.text(), "reified" | "in" | "out"))
        .map(|token| Name::new(token.text()))
}

/// The name of a `variableDeclaration`: its identifier, or `_` for a
/// destructuring placeholder ([spec: grammar-rule-variableDeclaration]).
fn variable_declaration_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| matches!(token.kind(), K::IDENTIFIER | K::UNDERSCORE))
        .map(|token| Name::new(token.text()))
}

/// The declared type of a `variableDeclaration`, if it writes one.
fn variable_declaration_type(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemTypeRef> {
    declared_type(ctx, node)
}

/// The name of a `userType`: its identifier segments joined with `.`, the type
/// arguments excluded.
fn qualified_name(node: &SyntaxNode<Lang>) -> Option<Name> {
    let segments = dotted_segments(node);
    (!segments.is_empty()).then(|| Name::new(&segments.join(".")))
}

/// The identifier segments of a dotted name node (`QUALIFIED_NAME`,
/// `USER_TYPE`), in source order.
fn dotted_segments(node: &SyntaxNode<Lang>) -> Vec<String> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| is_token(token, K::IDENTIFIER))
        .map(|token| token.text().to_owned())
        .collect()
}

/// The source range of a dotted name, type arguments excluded: from the first
/// identifier to the last.
fn dotted_name_range(node: &SyntaxNode<Lang>) -> TextRange {
    let mut identifiers = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| is_token(token, K::IDENTIFIER));
    let Some(first) = identifiers.next() else {
        return node.text_range();
    };
    let start = first.text_range().start();
    let end = identifiers.last().map_or_else(
        || first.text_range().end(),
        |token| token.text_range().end(),
    );
    TextRange::new(start, end)
}

/// The first identifier token of a node's own children.
fn first_identifier(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
}

/// The first child of `node` that is a type node — the type a `TYPE`-family
/// node wraps ([spec: grammar-rule-type]).
fn child_type(node: &SyntaxNode<Lang>) -> Option<SyntaxNode<Lang>> {
    node.children().find(|child| is_type_node(child.kind()))
}

/// Whether a node kind is a type node, in the shapes `type_` produces.
fn is_type_node(kind: K) -> bool {
    matches!(
        kind,
        K::TYPE
            | K::NULLABLE_TYPE
            | K::DEFINITELY_NON_NULLABLE_TYPE
            | K::PARENTHESIZED_TYPE
            | K::FUNCTION_TYPE
            | K::USER_TYPE
    )
}

fn is(node: &SyntaxNode<Lang>, kind: K) -> bool {
    node.kind() == kind
}

fn is_token(token: &SyntaxToken<Lang>, kind: K) -> bool {
    token.kind() == kind
}

/// The name of a declaration the source does not name (an erroneous
/// declaration still gets an item, so its members stay reachable).
fn missing_name() -> Name {
    Name::new("<missing>")
}
