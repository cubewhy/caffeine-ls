use crate::{
    ContextualKeyword, Parser,
    SyntaxKind::*,
    grammar::{annotations::annotation, eat_nl},
    tokenset,
};

/// `modifiers`: (annotation | modifier) {(annotation | modifier)}
/// [spec: grammar-rule-modifiers] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-modifiers
pub(crate) fn modifiers(p: &mut Parser) {
    let m = p.start();
    let mut is_empty = true;

    while at_modifier(p) {
        if p.at(AT) {
            annotation(p);
        } else {
            p.bump();
            eat_nl(p);
        }
        is_empty = false;
    }

    if is_empty {
        m.abandon(p);
    } else {
        m.complete(p, MODIFIER_LIST);
    }
}

/// `modifier`: classModifier | memberModifier | visibilityModifier |
///             functionModifier | propertyModifier | inheritanceModifier |
///             parameterModifier | platformModifier
/// [spec: grammar-rule-modifier] https://kotlinlang.org/spec/syntax-and-grammar.html#grammar-rule-modifier
///
/// All of these lex as IDENTIFIER (soft keywords) except `in` (variance),
/// which is the hard `IN_KW`.
fn at_modifier(p: &Parser) -> bool {
    p.at(AT)
        || p.at(IN_KW)
        || p.at_contextual_kw_set(tokenset![
            ContextualKeyword::Abstract,
            ContextualKeyword::Actual,
            ContextualKeyword::Annotation,
            ContextualKeyword::Companion,
            ContextualKeyword::Const,
            ContextualKeyword::CrossInline,
            ContextualKeyword::Data,
            ContextualKeyword::Enum,
            ContextualKeyword::Expect,
            ContextualKeyword::External,
            ContextualKeyword::Final,
            ContextualKeyword::Infix,
            ContextualKeyword::Inline,
            ContextualKeyword::Inner,
            ContextualKeyword::Internal,
            ContextualKeyword::LateInit,
            ContextualKeyword::NoInline,
            ContextualKeyword::Open,
            ContextualKeyword::Operator,
            ContextualKeyword::Out,
            ContextualKeyword::Override,
            ContextualKeyword::Private,
            ContextualKeyword::Protected,
            ContextualKeyword::Public,
            ContextualKeyword::Reified,
            ContextualKeyword::Sealed,
            ContextualKeyword::Suspend,
            ContextualKeyword::Tailrec,
            ContextualKeyword::Vararg,
            ContextualKeyword::Value,
        ])
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::{Event, Parser, lex};

    fn parse_modifiers(src: &str) -> String {
        let (tokens, _lex_errors) = lex(src);
        let mut p = Parser::new(tokens);
        modifiers(&mut p);

        let mut out = String::new();
        for ev in &p.events {
            match ev {
                Event::Tombstone => out.push_str("Tombstone\n"),
                Event::AddToken => out.push_str("AddToken\n"),
                Event::AddVirtualToken { kind, lexeme } => {
                    out.push_str(&format!("AddVirtualToken({kind:?}, {lexeme:?})\n"))
                }
                Event::AdvanceSource => out.push_str("AdvanceSource\n"),
                Event::FinishNode => out.push_str("FinishNode\n"),
                Event::Error(err) => out.push_str(&format!("Error({err:?})\n")),
                Event::StartNode { kind, .. } => out.push_str(&format!("StartNode({kind:?})\n")),
            }
        }
        for err in &p.errors {
            out.push_str(&format!("ERROR {err:?}\n"));
        }
        out
    }

    #[test]
    fn modifiers_and_annotations() {
        let out = parse_modifiers(indoc! {r#"
            public final @Deprecated("x") @get:[A B] private suspend
        "#});
        insta::assert_snapshot!(out);
    }

    #[test]
    fn no_modifiers() {
        let out = parse_modifiers("fun main()");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn variance_and_vararg_modifiers() {
        let out = parse_modifiers("in out vararg noinline crossinline reified");
        insta::assert_snapshot!(out);
    }
}
