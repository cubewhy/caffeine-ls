//! Kotlin semantic highlighting.
//!
//! Kotlin has no HIR yet — [`hir_def::kotlin::lower`] is a placeholder and a
//! Kotlin CST lowers to an empty item tree — so every token is classified from
//! the CST, by node kind: the lexical layer for keywords, modifiers, literals,
//! operators and comments ([`lexical`]), and one identifier pass for
//! declarations, references and types ([`identifiers`]). When the Kotlin
//! lowering lands this module is replaced the way
//! [`crate::nav::kotlin`] documents for navigation.
//!
//! # The uppercase rule
//!
//! One classification cannot be read off the syntax at all: whether a name in
//! expression position is a *type* — a class instance creation (`Impl()`), a
//! nested class (`Settings.Dark`) — or a value. Kotlin's own coding conventions
//! capitalise class and object names and lowercase function and property names,
//! so the first character of the identifier is the available signal, and it is
//! the only convention-based approximation in this module. It disappears
//! together with the rest of the CST pass when the Kotlin lowering lands.
//!
//! Because there is no resolution, a reference to an *enum constant*
//! (`Color.RED`) is classified by that same convention too, so it reads as a
//! class — the declaration keeps its `enumMember` tag, which comes from the CST
//! shape.

use rowan::{SyntaxNode, SyntaxToken};
use syntax::SourceFile;
use syntax::kotlin::{Lang, SyntaxKind as K};

use super::{Highlight, Highlights, HlMods, HlTag, insert};

/// The semantic highlighting of a Kotlin file, sorted by range start.
pub(crate) fn highlight(source: &SourceFile) -> Vec<Highlight> {
    let Some(root) = kotlin_root(source) else {
        return Vec::new();
    };
    let mut out = Highlights::new();
    lexical(root, &mut out);
    identifiers(root, &mut out);
    out.into_values().collect()
}

/// The Kotlin root node of `source`; `None` for a non-Kotlin file.
fn kotlin_root(source: &SourceFile) -> Option<&SyntaxNode<Lang>> {
    match source {
        SourceFile::Kotlin(file) => Some(&file.syntax_node),
        SourceFile::Java(_) => None,
    }
}

/// The lexical layer: every token whose tag is a property of the token alone.
///
/// `NEWLINE` is *not* trivia in the Kotlin lexer, so it reaches this walk like
/// any other token and is dropped by the `_` arm of [`lexical_tag`].
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
/// (punctuation, whitespace, a line break).
fn lexical_tag(kind: K) -> Option<HlTag> {
    use HlTag::*;
    let tag = match kind {
        K::LINE_COMMENT | K::BLOCK_COMMENT | K::KDOC | K::SHEBANG_LINE => Comment,
        // A string is several tokens — the quotes, the content, an escape
        // sequence, the delimiters of an interpolation — and each is tagged on
        // its own so no two tokens of a template overlap (`$name`'s identifier
        // is tagged by the identifier pass).
        K::OPEN_QUOTE
        | K::CLOSE_QUOTE
        | K::OPEN_RAW_QUOTE
        | K::CLOSE_RAW_QUOTE
        | K::STRING_CONTENT
        | K::ESCAPE_SEQUENCE
        | K::TEMPLATE_SHORT_START
        | K::TEMPLATE_EXPR_START => String,
        K::INTEGER_LITERAL | K::FLOAT_LITERAL => Number,
        K::AS_KW
        | K::AS_SAFE
        | K::BREAK_KW
        | K::CLASS_KW
        | K::CONTINUE_KW
        | K::DO_KW
        | K::IF_KW
        | K::ELSE_KW
        | K::FALSE_KW
        | K::FOR_KW
        | K::FUN_KW
        | K::IN_KW
        | K::NOT_IN
        | K::INTERFACE_KW
        | K::IS_KW
        | K::NOT_IS
        | K::NULL_KW
        | K::OBJECT_KW
        | K::PACKAGE_KW
        | K::RETURN_KW
        | K::SUPER_KW
        | K::THIS_KW
        | K::THROW_KW
        | K::TRUE_KW
        | K::TRY_KW
        | K::TYPEALIAS_KW
        | K::TYPEOF_KW
        | K::VAL_KW
        | K::VAR_KW
        | K::WHEN_KW
        | K::WHILE_KW => Keyword,
        K::PLUS
        | K::MINUS
        | K::STAR
        | K::SLASH
        | K::MODULO
        | K::EQUAL
        | K::PLUS_EQUAL
        | K::MINUS_EQUAL
        | K::MUL_EQUAL
        | K::DIV_EQUAL
        | K::MODULO_EQUAL
        | K::PLUS_PLUS
        | K::MINUS_MINUS
        | K::AND
        | K::BIT_AND
        | K::OR
        | K::NOT
        | K::EQUAL_EQUAL
        | K::NOT_EQUAL
        | K::SHEQ
        | K::SHNE
        | K::LESS
        | K::GREATER
        | K::LESS_EQUAL
        | K::GREATER_EQUAL
        | K::NOT_NULL_ASSERT
        | K::SAFE_ACCESS
        | K::ELVIS
        | K::COLON_COLON
        | K::RANGE
        | K::RANGE_UNTIL
        | K::COLON
        | K::QUESTION
        | K::ARROW => Operator,
        _ => return None,
    };
    Some(tag)
}

/// The identifier layer: every `IDENTIFIER` token, classified by where the CST
/// put it.
fn identifiers(root: &SyntaxNode<Lang>, out: &mut Highlights) {
    for element in root.descendants_with_tokens() {
        let Some(token) = element.as_token() else {
            continue;
        };
        if token.kind() != K::IDENTIFIER {
            continue;
        }
        let Some(parent) = token.parent() else {
            continue;
        };
        let Some((tag, mods)) = classify(token, &parent) else {
            continue;
        };
        insert(out, token.text_range(), tag, mods);
    }
}

/// The tag and modifiers of one identifier.
fn classify(token: &SyntaxToken<Lang>, parent: &SyntaxNode<Lang>) -> Option<(HlTag, HlMods)> {
    declaration(token, parent)
        .or_else(|| soft_keyword(parent))
        .or_else(|| reference(token, parent))
}

/// The declarations an identifier can *be*: the name of a declaration node, or —
/// in a declaration that writes a modifier lexeme before its name (`out T`,
/// `vararg x`, `reified T`) — that lexeme.
fn declaration(token: &SyntaxToken<Lang>, parent: &SyntaxNode<Lang>) -> Option<(HlTag, HlMods)> {
    let declares = matches!(
        parent.kind(),
        K::CLASS_DECL
            | K::OBJECT_DECL
            | K::FUNCTION_DECL
            | K::TYPE_ALIAS
            | K::TYPE_PARAMETER
            | K::VALUE_PARAMETER
            | K::LAMBDA_PARAMETER
            | K::CLASS_PARAMETER
            | K::VARIABLE_DECLARATION
            | K::ENUM_ENTRY
    );
    if !declares {
        return None;
    }
    if name_token(parent)?.text_range() != token.text_range() {
        return Some((HlTag::Modifier, HlMods::empty()));
    }

    let mut mods = HlMods::DECLARATION;
    let tag = match parent.kind() {
        K::CLASS_DECL => {
            if has_modifier(parent, "enum") {
                HlTag::Enum
            } else if has_direct_token(parent, K::INTERFACE_KW)
                || has_modifier(parent, "annotation")
            {
                HlTag::Interface
            } else {
                HlTag::Class
            }
        }
        K::OBJECT_DECL => HlTag::Class,
        K::FUNCTION_DECL => {
            if is_member_function(parent) {
                HlTag::Method
            } else {
                HlTag::Function
            }
        }
        K::TYPE_ALIAS => HlTag::Type,
        K::TYPE_PARAMETER => HlTag::TypeParameter,
        K::VALUE_PARAMETER | K::LAMBDA_PARAMETER => HlTag::Parameter,
        // `class Point(val x: Int)`: a `val`/`var` primary-constructor parameter
        // declares a property of the class, a plain one a parameter only.
        K::CLASS_PARAMETER => {
            if has_direct_token(parent, K::VAL_KW) {
                mods |= HlMods::READONLY;
                HlTag::Property
            } else if has_direct_token(parent, K::VAR_KW) {
                HlTag::Property
            } else {
                HlTag::Parameter
            }
        }
        K::VARIABLE_DECLARATION => {
            // The property this declarator belongs to, if any: a class member, a
            // top-level property or a local one, each wrapped in a
            // `PROPERTY_DECL` whose `val`/`var` says whether it is readonly. A
            // `for` loop's variable and a destructuring binding have none.
            let property = parent.parent().filter(|up| up.kind() == K::PROPERTY_DECL);
            if property
                .as_ref()
                .is_some_and(|decl| has_direct_token(decl, K::VAL_KW))
            {
                mods |= HlMods::READONLY;
            }
            let member = property.and_then(|decl| decl.parent()).is_some_and(|up| {
                matches!(
                    up.kind(),
                    K::CLASS_BODY | K::ENUM_CLASS_BODY | K::OBJECT_LITERAL | K::ROOT
                )
            });
            if member {
                HlTag::Property
            } else {
                HlTag::Variable
            }
        }
        K::ENUM_ENTRY => HlTag::EnumMember,
        _ => return None,
    };
    Some((tag, mods))
}

/// The soft keywords: the keywords the lexer leaves as identifiers, because the
/// grammar treats them as such — the modifier lexemes of a modifier list, an
/// annotation's use-site target, and the ones the grammar spells with an
/// identifier (`import`, `by`, `init`, `constructor`, `where`).
fn soft_keyword(parent: &SyntaxNode<Lang>) -> Option<(HlTag, HlMods)> {
    let tag = match parent.kind() {
        K::MODIFIER_LIST | K::COMPANION_OBJECT => HlTag::Modifier,
        K::ANNOTATION_USE_SITE_TARGET
        | K::IMPORT_HEADER
        | K::PROPERTY_DELEGATE
        | K::ANONYMOUS_INITIALIZER
        | K::PRIMARY_CONSTRUCTOR
        | K::SECONDARY_CONSTRUCTOR
        | K::TYPE_CONSTRAINTS => HlTag::Keyword,
        // An import alias names a type the file refers to it by; a type
        // constraint names a type parameter declared elsewhere in the
        // declaration, so it colors as one.
        K::IMPORT_ALIAS => HlTag::Type,
        K::TYPE_CONSTRAINT => HlTag::TypeParameter,
        _ => return None,
    };
    Some((tag, HlMods::empty()))
}

/// The reference an identifier is: the nearest-ancestor-wins classification of
/// the expression, type or name the CST wrote it in.
fn reference(token: &SyntaxToken<Lang>, parent: &SyntaxNode<Lang>) -> Option<(HlTag, HlMods)> {
    let tag = if is_annotation_name(parent) {
        HlTag::Decorator
    } else if matches!(parent.kind(), K::USER_TYPE | K::RECEIVER_TYPE) {
        if in_type_parameter_scope(token) {
            HlTag::TypeParameter
        } else {
            HlTag::Type
        }
    } else if is_directive_name(parent) {
        HlTag::Namespace
    } else if parent.kind() == K::STRING_TEMPLATE {
        HlTag::Variable
    } else if parent.kind() == K::NAVIGATION_SUFFIX {
        if upper(token) {
            HlTag::Class
        } else if called(parent) {
            HlTag::Method
        } else {
            HlTag::Property
        }
    } else if parent.kind() == K::PRIMARY_EXPRESSION {
        if upper(token) {
            HlTag::Class
        } else if called(parent) {
            HlTag::Function
        } else {
            HlTag::Variable
        }
    } else if matches!(
        parent.kind(),
        K::INFIX_FUNCTION_CALL | K::CALLABLE_REFERENCE
    ) {
        if upper(token) {
            HlTag::Class
        } else {
            HlTag::Function
        }
    } else {
        return None;
    };
    let mods = if written(token) {
        HlMods::MODIFICATION
    } else {
        HlMods::empty()
    };
    Some((tag, mods))
}

/// Whether this `USER_TYPE` is an annotation's *name*: the annotation node is
/// its direct parent. A type name written inside the annotation's argument list
/// has that list between it and the annotation, so it stays an ordinary type
/// reference.
fn is_annotation_name(user_type: &SyntaxNode<Lang>) -> bool {
    user_type.kind() == K::USER_TYPE
        && user_type
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), K::ANNOTATION))
}

/// A name written in a `package` or `import` directive. Every segment is a
/// namespace — the one tag that is right for a package, a type and a static
/// member alike without a classpath lookup.
fn is_directive_name(name: &SyntaxNode<Lang>) -> bool {
    name.kind() == K::QUALIFIED_NAME
        && name
            .ancestors()
            .any(|ancestor| matches!(ancestor.kind(), K::PACKAGE_HEADER | K::IMPORT_PATH))
}

/// Whether the name is declared by a `TYPE_PARAMETERS` list of an enclosing
/// declaration — `fun <T> …`, `class Box<T>`, `typealias A<T> = …` — a lexical
/// match on the name, since a CST cannot resolve one. The list is a *sibling* of
/// the type it applies to (`fun <T> List<T>.f()`), so the search is over the
/// declarations the name sits in, not over its own ancestors alone.
fn in_type_parameter_scope(token: &SyntaxToken<Lang>) -> bool {
    token.parent_ancestors().any(|ancestor| {
        matches!(
            ancestor.kind(),
            K::CLASS_DECL | K::FUNCTION_DECL | K::TYPE_ALIAS
        ) && ancestor
            .children()
            .filter(|child| child.kind() == K::TYPE_PARAMETERS)
            // The name is the declared parameter's own identifier — the bound's
            // type name is nested inside it ([spec: grammar-rule-typeParameter]).
            .flat_map(|parameters| parameters.children())
            .filter(|parameter| parameter.kind() == K::TYPE_PARAMETER)
            .filter_map(|parameter| name_token(&parameter))
            .any(|name| name.text() == token.text())
    })
}

/// Whether the callee node is followed by the call it names. The grammar writes
/// a call suffix as `{typeArguments} {valueArguments}` in that order, so the
/// next node is the call's `CALL_EXPRESSION`, or a `TYPE_ARGUMENTS` directly
/// before one.
fn called(node: &SyntaxNode<Lang>) -> bool {
    let mut next = node.next_sibling();
    if next
        .as_ref()
        .is_some_and(|next| next.kind() == K::TYPE_ARGUMENTS)
    {
        next = next.and_then(|arguments| arguments.next_sibling());
    }
    next.is_some_and(|next| next.kind() == K::CALL_EXPRESSION)
}

/// Whether the identifier is written on the left of an assignment operator: the
/// nearest `ASSIGNMENT_STATEMENT` ancestor writes it, because the operator the
/// statement owns sits at or after the identifier's end.
fn written(token: &SyntaxToken<Lang>) -> bool {
    token
        .parent_ancestors()
        .find(|ancestor| ancestor.kind() == K::ASSIGNMENT_STATEMENT)
        .is_some_and(|statement| {
            statement
                .children_with_tokens()
                .filter_map(|element| element.into_token())
                .any(|operator| {
                    is_assignment_operator(operator.kind())
                        && operator.text_range().start() >= token.text_range().end()
                })
        })
}

/// The assignment operators of [spec: grammar-rule-assignmentOperator].
fn is_assignment_operator(kind: K) -> bool {
    matches!(
        kind,
        K::EQUAL | K::PLUS_EQUAL | K::MINUS_EQUAL | K::MUL_EQUAL | K::DIV_EQUAL | K::MODULO_EQUAL
    )
}

/// The uppercase rule: see the module documentation.
fn upper(token: &SyntaxToken<Lang>) -> bool {
    token
        .text()
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
}

/// Whether a function is declared in a class-like body, which makes it a
/// *method*, rather than at the top level, in a block or as an extension —
/// which make it a function.
fn is_member_function(function: &SyntaxNode<Lang>) -> bool {
    function.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            K::CLASS_BODY | K::ENUM_CLASS_BODY | K::OBJECT_LITERAL
        )
    })
}

/// The last `IDENTIFIER` token that is a direct child of `node` — the name of a
/// declaration, whose earlier direct-child identifiers are modifier lexemes.
fn name_token(node: &SyntaxNode<Lang>) -> Option<SyntaxToken<Lang>> {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == K::IDENTIFIER)
        .last()
}

/// Whether `node` has a direct-child token of `kind`. Only direct children: a
/// nested declaration must not answer for the node that contains it.
fn has_direct_token(node: &SyntaxNode<Lang>, kind: K) -> bool {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == kind)
}

/// Whether the node's modifier list writes `keyword` — one of the soft keywords
/// that modify a declaration (`enum`, `annotation`, `data`, ...).
fn has_modifier(node: &SyntaxNode<Lang>, keyword: &str) -> bool {
    node.children()
        .filter(|child| child.kind() == K::MODIFIER_LIST)
        .flat_map(|list| list.children_with_tokens())
        .filter_map(|element| element.into_token())
        .any(|token| token.kind() == K::IDENTIFIER && token.text() == keyword)
}
