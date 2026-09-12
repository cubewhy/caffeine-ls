//! Kotlin semantic highlighting.
//!
//! Kotlin has no HIR yet — [`hir_def::kotlin::lower`] is a placeholder and a
//! Kotlin CST lowers to an empty item tree — so every token is classified from
//! the CST, by node kind: the lexical layer for keywords, modifiers, literals,
//! operators and comments ([`lexical`]), and one identifier pass for
//! declarations, references and types ([`identifiers`]). When the Kotlin
//! lowering lands this module is replaced the way
//! [`crate::nav::kotlin`] documents for navigation.

use rowan::SyntaxNode;
use syntax::SourceFile;
use syntax::kotlin::{Lang, SyntaxKind as K};

use super::{Highlight, Highlights, HlMods, HlTag, insert};

/// The semantic highlighting of a Kotlin file, sorted by range start.
pub(super) fn highlight(source: &SourceFile) -> Vec<Highlight> {
    let Some(root) = kotlin_root(source) else {
        return Vec::new();
    };
    let mut out = Highlights::new();
    lexical(root, &mut out);
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
