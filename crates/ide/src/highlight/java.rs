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
//! The parser's CST is consulted for identifiers only where the HIR has none
//! (the declaration gap-fill), so a name's tag never depends on a syntactic
//! guess.

use rowan::SyntaxNode;
use syntax::SourceFile;
use syntax::java::{Lang, SyntaxKind as J};
use vfs::FileId;

use super::{Highlight, Highlights, HlMods, HlTag, insert};
use crate::RootDatabase;

/// The semantic highlighting of a Java file, sorted by range start.
pub(super) fn highlight(
    _db: &RootDatabase,
    _file_id: FileId,
    source: &SourceFile,
) -> Vec<Highlight> {
    let Some(root) = java_root(source) else {
        return Vec::new();
    };
    let mut out = Highlights::new();
    lexical(root, &mut out);
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
