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

use hir_expand::{ast_id_map::FileAstId, body::ExprData, name::Name};

use super::LowerCtx;
use super::body::{self, is_expression};
use crate::jvm::decl::{
    AnnotationNode, ItemAnnotationArg, ItemAnnotationRef, ItemAnnotationValue, ItemTypeRef, Param,
};
use crate::kotlin::item_tree::{
    AccessorData, AnonymousInitializerNode, ClassData, ClassDeclNode, ConstructorData,
    ConstructorDeclNode, ConstructorDelegation, EnumEntryData, EnumEntryNode, FunctionData,
    FunctionDeclNode, ImportHeaderNode, InitData, ItemId, KotlinAnnotationRef, KotlinClassKind,
    KotlinImportItem, KotlinItemData, KotlinParam, KotlinSuperType, KotlinTypeParam,
    PackageHeaderNode, PropertyData, PropertyNode, TypeAliasData, TypeAliasNode, ast_id_of,
    ast_id_or_placeholder,
};
use crate::kotlin::modifiers::{KotlinModifiers, KotlinVariance};

pub(super) fn lower_file(ctx: &mut LowerCtx<'_>, file: &kotlin_syntax::SourceFile) {
    for child in file.syntax_node.children() {
        match child.kind() {
            K::FILE_ANNOTATION => ctx
                .tree
                .file_annotations
                .extend(lower_annotation(ctx, &child)),
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
/// `objectLiteral` — the anonymous class of `object : … { … }`
/// [spec: grammar-rule-objectLiteral] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-objectLiteral
///
/// All four lower to [`ClassData`], differing in [`KotlinClassKind`]. The
/// primary constructor and the properties its `val`/`var` class parameters
/// declare are lowered *before* the class-body members, in source order.
fn lower_class(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let mut modifiers = modifiers_of(node);
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
        annotations: Vec::new(),
        type_params: lower_type_params(ctx, node),
        super_types: Vec::new(),
        primary_constructor: None,
        body: Vec::new(),
        ast: ast_id_of::<ClassDeclNode, _>(ctx.map, node),
    }));

    // The annotations and the supertypes carry expressions — an element value,
    // a superclass's constructor arguments, a delegate — and an expression is
    // anchored to the declaration it belongs to, so both are lowered once the
    // item exists.
    let annotations = annotations_of(ctx, node);
    let super_types = lower_super_types(ctx, id, node);

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
    data.annotations = annotations;
    data.super_types = super_types;
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
        K::OBJECT_LITERAL => KotlinClassKind::Object,
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
    if is(node, K::OBJECT_LITERAL) {
        // An object literal's class has no name in the source: the compiler
        // gives it a positional binary name (`Foo$1`) that no source writes, so
        // the item carries a stable printable one. ([KLS
        // `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals))
        return Some(Name::new("<anonymous>"));
    }
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
    let modifiers = modifiers_of(node);
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

    // The parameters as locals too: the class body's initializers and `init`
    // blocks are separate bodies that see them ([KLS
    // `declarations.html#constructor-declaration-scopes`](https://kotlinlang.org/spec/declarations.html#constructor-declaration-scopes)).
    let param_locals: Vec<hir_expand::body::LocalId> = parameters
        .iter()
        .map(|parameter| body::lower_param(ctx, parameter))
        .collect();
    let id = ctx.alloc(KotlinItemData::Constructor(ConstructorData {
        params: parameters
            .iter()
            .map(|parameter| lower_param(ctx, parameter))
            .collect(),
        param_locals,
        defaults: Vec::new(),
        modifiers,
        annotations: Vec::new(),
        delegation: None,
        body: None,
        ast: ast_id_of::<ConstructorDeclNode, _>(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let defaults = body::lower_defaults(ctx, id, node);
    let KotlinItemData::Constructor(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a primary constructor");
    };
    data.annotations = annotations;
    data.defaults = defaults;

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
    let modifiers = modifiers_of(node);
    let id = ctx.alloc(KotlinItemData::Constructor(ConstructorData {
        params: lower_params(ctx, node),
        // A secondary constructor's parameters are the parameters of its own
        // body, bound there ([`body::lower_constructor_body`]).
        param_locals: Vec::new(),
        defaults: Vec::new(),
        modifiers,
        annotations: Vec::new(),
        delegation: None,
        body: None,
        ast: ast_id_of::<ConstructorDeclNode, _>(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let defaults = body::lower_defaults(ctx, id, node);
    let delegation = lower_constructor_delegation(ctx, id, node);
    let body = body::lower_constructor_body(ctx, id, node);
    let KotlinItemData::Constructor(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a constructor");
    };
    data.annotations = annotations;
    data.defaults = defaults;
    data.delegation = delegation;
    data.body = body;
    id
}

/// The `: this(…)` / `: super(…)` call of a secondary constructor ([spec:
/// grammar-rule-constructorDelegationCall]), with its arguments lowered in the
/// constructor's own context.
fn lower_constructor_delegation(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<ConstructorDelegation> {
    let call = node
        .children()
        .find(|child| is(child, K::CONSTRUCTOR_DELEGATION_CALL))?;
    let is_super = call
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| is_token(&token, K::SUPER_KW));
    Some(ConstructorDelegation {
        is_super,
        args: call
            .children()
            .find(|child| is(child, K::VALUE_ARGUMENTS))
            .map(|arguments| body::lower_value_arguments(ctx, owner, &arguments))
            .unwrap_or_default(),
        ast: ast_id_of(ctx.map, &call),
    })
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
    let modifiers = modifiers_of(node);
    let id = ctx.alloc(KotlinItemData::Function(FunctionData {
        name: function_name(node).unwrap_or_else(missing_name),
        modifiers,
        annotations: Vec::new(),
        type_params: lower_type_params(ctx, node),
        receiver: receiver_type(ctx, node),
        params: lower_params(ctx, node),
        defaults: Vec::new(),
        ret: declared_type(ctx, node),
        body: None,
        expression_body: body::expression_body_form(node),
        ast: ast_id_of::<FunctionDeclNode, _>(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let defaults = body::lower_defaults(ctx, id, node);
    let body = body::lower_function_body(ctx, id, node);
    let KotlinItemData::Function(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a function");
    };
    data.annotations = annotations;
    data.defaults = defaults;
    data.body = body;
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
    let modifiers = modifiers_of(node);
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

    let annotations = annotations_of(ctx, node);
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
    let modifiers = modifiers_of(node);
    let id = ctx.alloc(KotlinItemData::Accessor(AccessorData {
        is_setter,
        modifiers,
        annotations: Vec::new(),
        params: node
            .children()
            .filter(|child| is(child, K::VALUE_PARAMETER))
            .map(|parameter| lower_param(ctx, &parameter))
            .collect(),
        body: None,
        expression_body: body::expression_body_form(node),
        ast: ast_id_of(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let body = body::lower_accessor_body(ctx, id, node);
    let KotlinItemData::Accessor(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated an accessor");
    };
    data.annotations = annotations;
    data.body = body;
    id
}

/// `enumEntry`: [modifiers] simpleIdentifier [valueArguments] [classBody]
/// [spec: grammar-rule-enumEntry] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-enumEntry
fn lower_enum_entry(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
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
        annotations: Vec::new(),
        argument_exprs: Vec::new(),
        body,
        ast: ast_id_of::<EnumEntryNode, _>(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let arguments = body::lower_enum_entry_arguments(ctx, id, node);
    let KotlinItemData::EnumEntry(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated an enum entry");
    };
    data.annotations = annotations;
    data.argument_exprs = arguments;
    id
}

/// `typeAlias`: [modifiers] 'typealias' simpleIdentifier [typeParameters]
///              {NL} '=' {NL} type
/// [spec: grammar-rule-typeAlias] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-typeAlias
fn lower_type_alias(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemId {
    let modifiers = modifiers_of(node);
    let id = ctx.alloc(KotlinItemData::TypeAlias(TypeAliasData {
        name: first_identifier(node).unwrap_or_else(missing_name),
        modifiers,
        annotations: Vec::new(),
        type_params: lower_type_params(ctx, node),
        target: declared_type(ctx, node).unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error)),
        ast: ast_id_of::<TypeAliasNode, _>(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let KotlinItemData::TypeAlias(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a type alias");
    };
    data.annotations = annotations;
    id
}

/// Lowers a *local* declaration the body walker found in a block or in an
/// expression: a local class, object, function, type alias or an object
/// literal. The item is recorded as a local declaration of the file (the tree's
/// `local_types`) and given the declaration whose body declares it as its
/// parent, so the workspace symbol index — which walks `top` and `body()` —
/// never surfaces it, while the item tree's own ancestry ([KLS
/// `declarations.html#local-class-declaration`](https://kotlinlang.org/spec/declarations.html#local-class-declaration))
/// answers for the type layer.
///
/// `owner` is that declaration: the function, accessor, initializer or
/// property whose body lowering found the node (`owner` of the walker).
pub(super) fn lower_local_declaration(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<ItemId> {
    let item = match node.kind() {
        K::CLASS_DECL | K::OBJECT_DECL | K::COMPANION_OBJECT => lower_class(ctx, node),
        K::FUNCTION_DECL => lower_function(ctx, node),
        K::TYPE_ALIAS => lower_type_alias(ctx, node),
        K::OBJECT_LITERAL => lower_object_literal(ctx, node)?,
        _ => return None,
    };
    record_local(ctx, owner, item);
    Some(item)
}

/// An object literal `object : Base() { … }` ([KLS
/// `expressions.html#object-literals`](https://kotlinlang.org/spec/expressions.html#object-literals)):
/// the anonymous class its body declares, lowered as an `object` classifier
/// whose name the source does not write (`class_name` names it
/// `<anonymous>`). The item is the class the literal *is*, not the declaration
/// that follows it, and the body and the delegation specifiers are the ones
/// `lower_class` reads from the same node.
fn lower_object_literal(ctx: &mut LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemId> {
    Some(lower_class(ctx, node))
}

/// Marks `item` as a local declaration of the file: it joins `local_types`
/// (in lowering order, which is source order) and takes `owner`, the
/// declaration whose body is being lowered, as its parent.
fn record_local(ctx: &mut LowerCtx<'_>, owner: ItemId, item: ItemId) {
    if !ctx.tree.local_types.contains(&item) {
        ctx.tree.local_types.push(item);
    }
    ctx.tree.parent[item.0.0 as usize] = Some(owner);
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
    let modifiers = modifiers_of(node);
    let id = ctx.alloc(KotlinItemData::Property(PropertyData {
        name: first_identifier(node).unwrap_or_else(missing_name),
        modifiers,
        annotations: Vec::new(),
        type_params: Vec::new(),
        receiver: None,
        ty: declared_type(ctx, node),
        is_var,
        initializer_expr: None,
        delegate_expr: None,
        accessors: Vec::new(),
        ast: ast_id_of(ctx.map, node),
    }));
    let annotations = annotations_of(ctx, node);
    let KotlinItemData::Property(data) = ctx.tree.items.get_mut(id.0) else {
        unreachable!("just allocated a class property");
    };
    data.annotations = annotations;
    Some(id)
}

/// `classParameters` / `functionValueParameters`: the declared parameters of a
/// constructor or function, in source order ([spec:
/// grammar-rule-classParameters], [spec:
/// grammar-rule-functionValueParameters]).
///
/// `node` is the declaration or the parameter list itself; the parameters are
/// the `CLASS_PARAMETER`/`VALUE_PARAMETER` children of the list.
fn lower_params(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<KotlinParam> {
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
/// A parameter that writes no type is a *setter*'s, whose type is the
/// property's ([KLS
/// `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters)):
/// it records the error type, which the type layer replaces — a parameter with
/// no type anywhere is an erroneous declaration.
fn lower_param(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> KotlinParam {
    // A parameter writes no use-site target ([`KotlinAnnotationRef`]), so its
    // annotations are the shared ones ([`Param::annotations`]).
    let mut annotations = Vec::new();
    let mut modifiers = (false, false, false);
    if let Some(list) = node.children().find(|child| is(child, K::MODIFIER_LIST)) {
        for element in list.children_with_tokens() {
            if let NodeOrToken::Node(annotation) = element
                && is(&annotation, K::ANNOTATION)
            {
                annotations.extend(
                    lower_annotation(ctx, &annotation)
                        .into_iter()
                        .map(|application| application.annotation),
                );
            }
        }
        // A class parameter writes its modifiers in a list (`class C(private
        // val x: Int)`); a function parameter writes them directly in the
        // parameter node ([spec: grammar-rule-parameterModifiers]), so both
        // regions are read.
        modifiers = parameter_modifiers(&list);
    }
    for child in node.children().filter(|child| is(child, K::ANNOTATION)) {
        annotations.extend(
            lower_annotation(ctx, &child)
                .into_iter()
                .map(|application| application.annotation),
        );
    }
    let (vararg, noinline, crossinline) = {
        let (v, n, c) = parameter_modifiers(node);
        (modifiers.0 || v, modifiers.1 || n, modifiers.2 || c)
    };

    KotlinParam {
        param: Param {
            name: parameter_name(node).unwrap_or_else(missing_name),
            ty: declared_type(ctx, node).unwrap_or_else(|| ItemTypeRef::synthetic(TypeRef::Error)),
            varargs: vararg,
            annotations,
        },
        noinline,
        crossinline,
    }
}

/// The `vararg`/`noinline`/`crossinline` parameter modifiers of a
/// `parameterModifiers` region or of a parameter node ([spec:
/// grammar-rule-parameterModifiers]): each is written as a bare identifier
/// token, not as a modifier keyword.
fn parameter_modifiers(node: &SyntaxNode<Lang>) -> (bool, bool, bool) {
    let mut vararg = false;
    let mut noinline = false;
    let mut crossinline = false;
    for element in node.children_with_tokens() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        if !is_token(&token, K::IDENTIFIER) {
            continue;
        }
        match token.text() {
            "vararg" => vararg = true,
            "noinline" => noinline = true,
            "crossinline" => crossinline = true,
            _ => {}
        }
    }
    (vararg, noinline, crossinline)
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
/// supertypes of a classifier, in source order: each the type it names, plus
/// the constructor arguments of a supertype *call* (`class C : Base(1)`) or the
/// delegate expression of `interface I by delegate`
/// ([`KotlinSuperType`]).
///
/// Both are body content — the parser keeps them as children of the specifier
/// ([`Skeleton::plan`](hir_expand::ast_id_map)) — so `owner`, the classifier's
/// item, is what they lower against.
fn lower_super_types(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Vec<KotlinSuperType> {
    let Some(specifiers) = node
        .children()
        .find(|child| is(child, K::DELEGATION_SPECIFIERS))
    else {
        return Vec::new();
    };
    specifiers
        .children()
        .filter(|child| is(child, K::DELEGATION_SPECIFIER))
        .filter_map(|specifier| lower_super_type(ctx, owner, &specifier))
        .collect()
}

/// One delegation specifier: `Base`, `Base(1)` or `I by impl`
/// ([spec: grammar-rule-delegationSpecifier]).
fn lower_super_type(
    ctx: &mut LowerCtx<'_>,
    owner: ItemId,
    node: &SyntaxNode<Lang>,
) -> Option<KotlinSuperType> {
    // A called supertype (`CONSTRUCTOR_INVOCATION`) and a delegated one
    // (`EXPLICIT_DELEGATION`) wrap the type; the arguments and the delegate
    // expression sit beside the wrapper, in the specifier itself.
    let wrapper = node.children().find(|child| {
        matches!(
            child.kind(),
            K::CONSTRUCTOR_INVOCATION | K::EXPLICIT_DELEGATION
        )
    });
    let ty = match &wrapper {
        Some(wrapper) => child_type(wrapper)?,
        None => child_type(node)?,
    };
    let Some(wrapper) = wrapper else {
        return Some(KotlinSuperType {
            ty: item_type_ref(ctx, &ty),
            args: Vec::new(),
            delegate: None,
        });
    };
    let args = node
        .children()
        .find(|child| is(child, K::VALUE_ARGUMENTS))
        .map(|arguments| body::lower_value_arguments(ctx, owner, &arguments))
        .unwrap_or_default();
    // The delegate expression is the specifier's expression child that is not
    // the delegated type (`I by impl`), and `by` is a plain identifier token.
    let delegate = is(&wrapper, K::EXPLICIT_DELEGATION)
        .then(|| {
            node.children()
                .filter(|child| is_expression(child.kind()))
                .last()
        })
        .flatten()
        .map(|value| body::lower_expr(ctx, owner, &value));
    Some(KotlinSuperType {
        ty: item_type_ref(ctx, &ty),
        args,
        delegate,
    })
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
    // A type-use annotation writes no use-site target ([`KotlinAnnotationRef`]),
    // so the shared reference is what a type carries.
    node.children()
        .filter(|child| is(child, K::ANNOTATION))
        .flat_map(|child| lower_annotation(ctx, &child))
        .map(|application| application.annotation)
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
                annotations.extend(
                    lower_annotation(ctx, &child)
                        .into_iter()
                        .map(|application| application.annotation),
                );
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
/// `fileAnnotation`: '@' 'file' ':' (multiAnnotation | unescapedAnnotation)
/// [spec: grammar-rule-fileAnnotation] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-fileAnnotation
///
/// A multi-annotation (`@[A B]`) is sugar for several annotations that share
/// one syntax node — and therefore one range, one
/// [`ItemAnnotationRef::node`] — so it yields one item per annotation body, all
/// with the one use-site target the node writes.
///
/// Element values ([KLS
/// `annotations.html`](https://kotlinlang.org/spec/annotations.html)) are
/// lowered to the constant forms a classfile can carry
/// ([`lower_annotation_value`]).
fn lower_annotation(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<KotlinAnnotationRef> {
    let id = ast_id_or_placeholder(ctx.map, node);
    let target = annotation_use_site_target(node);
    let mut out: Vec<KotlinAnnotationRef> = Vec::new();
    let mut pending: Option<Name> = None;
    for child in node.children() {
        match child.kind() {
            K::USER_TYPE => {
                if let Some(name) = pending.take() {
                    out.push(application(target.clone(), name, Vec::new(), id));
                }
                pending = lower_user_type(ctx, &child).ty.as_reference_name().cloned();
            }
            K::VALUE_ARGUMENTS => {
                if let Some(name) = pending.take() {
                    out.push(application(
                        target.clone(),
                        name,
                        lower_annotation_args(ctx, &child),
                        id,
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(name) = pending {
        out.push(application(target, name, Vec::new(), id));
    }
    out
}

/// One annotation application with its use-site target.
fn application(
    target: Option<Name>,
    name: Name,
    args: Vec<ItemAnnotationArg>,
    node: FileAstId<AnnotationNode>,
) -> KotlinAnnotationRef {
    KotlinAnnotationRef {
        target,
        annotation: ItemAnnotationRef { name, args, node },
    }
}

/// The use-site target of an annotation ([KLS
/// `annotations.html#annotation-use-site-targets`](https://kotlinlang.org/spec/annotations.html#annotation-use-site-targets)):
/// the `get` of `@get:JvmName`, the `file` of `@file:JvmName`. `None` for an
/// annotation that writes none, and for a *multi*-annotation of several types
/// (`@[A B]`), which carries no target.
fn annotation_use_site_target(node: &SyntaxNode<Lang>) -> Option<Name> {
    let target = node
        .children()
        .find(|child| is(child, K::ANNOTATION_USE_SITE_TARGET))?;
    target
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| is_token(token, K::IDENTIFIER))
        .map(|token| Name::new(token.text()))
}

/// `valueArgument`: [annotation] {NL} [simpleIdentifier {NL} '=' {NL}] ['*']
///                  {NL} expression
/// [spec: grammar-rule-valueArgument] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-valueArgument
///
/// The name is the argument's own (`@Ann(name = 1)`), the implicit `value`
/// otherwise — the element name kotlinc resolves a single unnamed argument to.
fn lower_annotation_args(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<ItemAnnotationArg> {
    node.children()
        .filter(|child| is(child, K::VALUE_ARGUMENT))
        .map(|argument| {
            // The implicit `value` is the name of an argument written without
            // one: the element it binds is the annotation's own parameter,
            // whose name only its declaration knows
            // ([KLS `annotations.html`](https://kotlinlang.org/spec/annotations.html)).
            let name = argument
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|token| is_token(token, K::IDENTIFIER))
                .map(|token| Name::new(token.text()))
                .unwrap_or_else(|| Name::new("value"));
            let value = argument.children().find(|child| !is(child, K::ANNOTATION));
            ItemAnnotationArg {
                name,
                value: value
                    .map(|value| lower_annotation_value(ctx, &value))
                    .unwrap_or(ItemAnnotationValue::Unresolved {
                        text: argument.text().to_string(),
                    }),
            }
        })
        .collect()
}

/// The value of an annotation argument ([KLS
/// `annotations.html`](https://kotlinlang.org/spec/annotations.html)).
///
/// The forms are the ones a classfile annotation can carry — a literal, an enum
/// constant, a class literal, a nested annotation and an array of those — and
/// the shapes are the parser's: a nested annotation is written *without* an
/// `@` (kotlinc 2.4.20 rejects `@Outer(@Inner("x"))` with "annotations cannot
/// be used as annotation arguments", and accepts `@Outer(Inner("x"))`), so it
/// parses as a call of a bare name, and an enum constant is a postfix access on
/// a name.
///
/// A value that is none of these — an arbitrary expression, which the JVM
/// cannot carry — keeps its source text ([`ItemAnnotationValue::Unresolved`]):
/// the annotation is lowered with the declaration's *signature*, before the
/// item it belongs to exists, so there is no owner to anchor an expression to.
fn lower_annotation_value(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemAnnotationValue {
    match node.kind() {
        // A string literal is a node of its own (its *content* is the token
        // that carries the text).
        K::STRING_LITERAL => literal_value(ctx, node),
        // A bare name: an enum constant whose declaring type is the element's
        // ([`ItemAnnotationValue::EnumConstant`] with no qualifier), or a
        // literal — an integer, float, character or boolean one is the single
        // token of the `PRIMARY_EXPRESSION` the expression grammar wraps it in.
        K::PRIMARY_EXPRESSION => match node
            .children_with_tokens()
            .find_map(NodeOrToken::into_token)
        {
            Some(token) if is_literal_token(&token) => literal_value(ctx, node),
            Some(token) if is_token(&token, K::IDENTIFIER) => ItemAnnotationValue::EnumConstant {
                qualifier: None,
                member: Name::new(token.text()),
            },
            _ => unresolved(node),
        },
        // `Type.CONSTANT` is a postfix access on a name and `Inner("x")` a postfix
        // *call* of one — the nested-annotation form Kotlin writes without an
        // `@` (see the doc above).
        K::POSTFIX_UNARY_EXPRESSION => {
            if let Some((qualifier, member)) = enum_constant(node) {
                ItemAnnotationValue::EnumConstant { qualifier, member }
            } else if let Some(annotation) = nested_annotation(ctx, node) {
                ItemAnnotationValue::Annotation(Box::new(annotation))
            } else if let Some(ty) = qualified_class_literal(ctx, node) {
                ItemAnnotationValue::ClassLit(Box::new(ty))
            } else {
                unresolved(node)
            }
        }
        // A class literal `Foo::class` ([KLS
        // `reflection.html#class-references`](https://kotlinlang.org/spec/reflection.html#class-references)).
        K::CALLABLE_REFERENCE => match node.children().find(|child| is_type_node(child.kind())) {
            Some(ty) => ItemAnnotationValue::ClassLit(Box::new(item_type_ref(ctx, &ty))),
            None => unresolved(node),
        },
        // An array initializer `[v1, v2]` ([spec:
        // grammar-rule-collectionLiteral]).
        K::COLLECTION_LITERAL => ItemAnnotationValue::Array(
            node.children()
                .filter(|child| is_expression(child.kind()))
                .map(|element| lower_annotation_value(ctx, &element))
                .collect(),
        ),
        _ => unresolved(node),
    }
}

/// The nested annotation `Inner("x")` an argument holds: the called name is the
/// annotation's and the call's arguments are its element values, lowered
/// recursively. `None` for a postfix expression that calls nothing.
fn nested_annotation(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemAnnotationRef> {
    let call = node
        .children()
        .find(|child| is(child, K::CALL_EXPRESSION))?;
    let name = base_identifier(node)?;
    let args = call
        .children()
        .find(|child| is(child, K::VALUE_ARGUMENTS))
        .map(|arguments| lower_annotation_args(ctx, &arguments))
        .unwrap_or_default();
    Some(ItemAnnotationRef {
        name,
        args,
        node: ast_id_or_placeholder(ctx.map, node),
    })
}

/// The `(qualifier, member)` of an enum constant `Type.CONSTANT` — a postfix
/// access on a bare name, whose qualifier is the type it is declared in. `None`
/// for any other postfix expression (`Foo.bar()`, `Foo.BAR.baz`), which is not
/// a constant.
fn enum_constant(node: &SyntaxNode<Lang>) -> Option<(Option<Name>, Name)> {
    if node.children().any(|child| is(&child, K::CALL_EXPRESSION)) {
        return None;
    }
    let member = node
        .children()
        .filter(|child| is(child, K::NAVIGATION_SUFFIX))
        .find_map(|suffix| {
            suffix
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|token| is_token(token, K::IDENTIFIER))
        })
        .map(|token| Name::new(token.text()))?;
    // Exactly one access: `Foo.BAR` and not `Foo.BAR.baz`.
    if node
        .children()
        .filter(|child| is(child, K::NAVIGATION_SUFFIX))
        .count()
        != 1
    {
        return None;
    }
    Some((base_identifier(node), member))
}

/// The type of a *qualified* class literal — `java.io.IOException::class`
/// ([KLS
/// `reflection.html#class-references`](https://kotlinlang.org/spec/reflection.html#class-references)).
///
/// A class literal whose receiver is a single identifier parses as a
/// [`K::CALLABLE_REFERENCE`]; a *qualified* one parses as a navigation chain
/// whose last suffix is the `::class` access — `java` `.io` `.IOException`
/// `::class` — since the callable-reference lookahead reads one identifier
/// ([`lower_annotation_value`] lowers both to the same
/// [`ItemAnnotationValue::ClassLit`]). The chain must be a dotted name and
/// nothing else: any other token (`Foo<Int>::class`, `(x).y::class`) is not a
/// class literal this reads, and lowers as unresolved.
fn qualified_class_literal(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Option<ItemTypeRef> {
    let mut name = base_identifier(node)?.to_string();
    let suffixes: Vec<SyntaxNode<Lang>> = node
        .children()
        .filter(|child| is(child, K::NAVIGATION_SUFFIX))
        .collect();
    let (last, leading) = suffixes.split_last()?;
    for suffix in leading {
        let member = suffix
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .find(|token| is_token(token, K::IDENTIFIER))?;
        name.push('.');
        name.push_str(member.text());
    }
    let mut tokens = last
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token);
    if !tokens
        .next()
        .is_some_and(|token| is_token(&token, K::COLON_COLON))
        || !tokens
            .next()
            .is_some_and(|token| is_token(&token, K::CLASS_KW))
    {
        return None;
    }
    let name = Name::new(&name);
    Some(ItemTypeRef {
        ty: TypeRef::Reference {
            name: name.clone(),
            generic_args: Vec::new(),
        },
        refs: vec![name],
        type_use_annotations: Vec::new(),
        node: ast_id_or_placeholder(ctx.map, node),
    })
}

/// The identifier of a `PRIMARY_EXPRESSION` base — a name written as a token.
fn base_identifier(node: &SyntaxNode<Lang>) -> Option<Name> {
    node.children()
        .find(|child| is(child, K::PRIMARY_EXPRESSION))
        .and_then(|base| {
            base.children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find(|token| is_token(token, K::IDENTIFIER))
        })
        .map(|token| Name::new(token.text()))
}

/// Whether a token is one of the literal tokens an annotation element value may
/// hold ([KLS
/// `expressions.html#constant-literals`](https://kotlinlang.org/spec/expressions.html#constant-literals)).
fn is_literal_token(token: &SyntaxToken<Lang>) -> bool {
    matches!(
        token.kind(),
        K::INTEGER_LITERAL
            | K::FLOAT_LITERAL
            | K::CHAR_LITERAL
            | K::STRING_CONTENT
            | K::TRUE_KW
            | K::FALSE_KW
    )
}

/// The literal value of a node that holds a literal token — the token, decoded
/// exactly as the body lowering decodes it.
fn literal_value(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> ItemAnnotationValue {
    let Some(token) = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(is_literal_token)
    else {
        return unresolved(node);
    };
    match body::literal(ctx, &token) {
        ExprData::Literal(literal) => ItemAnnotationValue::Literal(literal),
        _ => unresolved(node),
    }
}

/// A value the constant forms above do not name, kept as its source text.
fn unresolved(node: &SyntaxNode<Lang>) -> ItemAnnotationValue {
    ItemAnnotationValue::Unresolved {
        text: node.text().to_string(),
    }
}

/// The source modifiers of a declaration, from its `MODIFIER_LIST`
/// ([spec: grammar-rule-modifiers]). Annotations are declaration attributes,
/// not modifiers, and are lowered separately ([`annotations_of`]) because an
/// annotation carries element values.
fn modifiers_of(node: &SyntaxNode<Lang>) -> KotlinModifiers {
    let mut modifiers = KotlinModifiers::none();
    let Some(list) = node.children().find(|child| is(child, K::MODIFIER_LIST)) else {
        return modifiers;
    };
    for element in list.children_with_tokens() {
        if let NodeOrToken::Token(token) = element
            && is_token(&token, K::IDENTIFIER)
        {
            modifiers.push_keyword(token.text());
        }
    }
    modifiers
}

/// The annotations of a declaration, in source order: the `ANNOTATION` children
/// of its `MODIFIER_LIST` and the ones written directly beside it.
fn annotations_of(ctx: &LowerCtx<'_>, node: &SyntaxNode<Lang>) -> Vec<KotlinAnnotationRef> {
    let list = node
        .children()
        .find(|child| is(child, K::MODIFIER_LIST))
        .into_iter()
        .flat_map(|list| {
            list.children()
                .filter(|child| is(child, K::ANNOTATION))
                .collect::<Vec<_>>()
        })
        .chain(node.children().filter(|child| is(child, K::ANNOTATION)));
    list.flat_map(|annotation| lower_annotation(ctx, &annotation))
        .collect()
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
