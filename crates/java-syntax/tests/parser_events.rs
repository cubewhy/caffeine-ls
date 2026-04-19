use java_syntax::{Event, LexicalError, ParseError, Parser, Token, grammar, lex};

fn collect_lex(src: &str) -> (Vec<Token<'_>>, Vec<LexicalError>) {
    match lex(src) {
        Ok(tokens) => (tokens, vec![]),
        Err((tokens, errors)) => (tokens, errors),
    }
}

fn dump_tokens(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    for tok in tokens {
        out.push_str(&format!("{:?} {:?}\n", tok.kind, tok.lexeme));
    }
    out
}

fn dump_events(events: &[Event]) -> String {
    let mut out = String::new();
    for ev in events {
        match ev {
            Event::Tombstone => out.push_str("Tombstone\n"),
            Event::AddToken => out.push_str("AddToken\n"),
            Event::FinishNode => out.push_str("FinishNode\n"),
            Event::Error(err) => out.push_str(&format!("Error({err:?})\n")),
            Event::StartNode {
                kind,
                forward_parent,
            } => {
                out.push_str(&format!(
                    "StartNode({kind:?}, forward_parent={forward_parent:?})\n"
                ));
            }
        }
    }
    out
}

fn dump_lex_errors(errors: &[LexicalError]) -> String {
    if errors.is_empty() {
        return "<none>\n".to_string();
    }

    let mut out = String::new();
    for err in errors {
        out.push_str(&format!("{err:?}\n"));
    }
    out
}

fn dump_parse_errors(errors: &[ParseError]) -> String {
    if errors.is_empty() {
        return "<none>\n".to_string();
    }

    let mut out = String::new();
    for err in errors {
        out.push_str(&format!("{err:?}\n"));
    }
    out
}

fn check_parser(src: &str) -> String {
    let (tokens, lex_errors) = collect_lex(src);

    let mut p = Parser::new(tokens.clone());
    grammar::root(&mut p);

    format!(
        "\
SOURCE:
{src}

TOKENS:
{}

LEX_ERRORS:
{}

EVENTS:
{}

PARSE_ERRORS:
{}",
        dump_tokens(&tokens),
        dump_lex_errors(&lex_errors),
        dump_events(&p.events),
        dump_parse_errors(&p.errors),
    )
}

macro_rules! parser_snapshot {
    ($name:ident, $src:expr) => {
        #[test]
        fn $name() {
            let out = check_parser($src);
            insta::assert_snapshot!(stringify!($name), out);
        }
    };
}

parser_snapshot!(parse_package_decl, r#"package a.b;"#);

parser_snapshot!(parse_import_decl, r#"import java.util.List;"#);

parser_snapshot!(
    parse_import_static_star,
    r#"import static java.util.Collections.*;"#
);

parser_snapshot!(parse_empty_class, r#"class A {}"#);

parser_snapshot!(parse_empty_interface, r#"interface A {}"#);

parser_snapshot!(parse_empty_enum, r#"enum E {}"#);

parser_snapshot!(parse_empty_record, r#"record R(int x) {}"#);

parser_snapshot!(parse_class_with_field, r#"class A { int x; }"#);

parser_snapshot!(
    parse_class_with_initialized_field,
    r#"class A { int x = 1; }"#
);

parser_snapshot!(parse_class_with_multi_field, r#"class A { int a, b; }"#);

parser_snapshot!(
    parse_class_with_multi_initialized_field,
    r#"class A { int a = 1, b = 2; }"#
);

parser_snapshot!(parse_class_with_method, r#"class A { void f() {} }"#);

parser_snapshot!(parse_class_with_typed_method, r#"class A { int f() {} }"#);

parser_snapshot!(parse_class_with_constructor, r#"class A { A() {} }"#);

parser_snapshot!(
    parse_class_with_static_initializer,
    r#"class A { static {} }"#
);

parser_snapshot!(parse_class_with_instance_initializer, r#"class A { {} }"#);

parser_snapshot!(parse_class_with_empty_decls, r#"class A { ;;;;;; }"#);

parser_snapshot!(
    parse_interface_with_abstract_method,
    r#"interface A { void f(); }"#
);

parser_snapshot!(
    parse_interface_with_default_method,
    r#"interface A { default void f() {} }"#
);

parser_snapshot!(
    parse_interface_with_field_like_member,
    r#"interface A { int X = 1; }"#
);

parser_snapshot!(parse_annotation_type_decl, r#"@interface A {}"#);

parser_snapshot!(
    parse_annotation_type_member_like,
    r#"@interface A { int value(); }"#
);

parser_snapshot!(
    parse_record_with_compact_constructor,
    r#"record R(int x) { R {} }"#
);

parser_snapshot!(
    parse_record_with_normal_constructor_like,
    r#"record R(int x) { R() {} }"#
);

parser_snapshot!(
    parse_record_with_method_and_field,
    r#"record R(int x) { int y; void f() {} }"#
);

parser_snapshot!(parse_array_type_on_type, r#"class A { int[] a; }"#);

parser_snapshot!(parse_array_type_on_declarator, r#"class A { int a[]; }"#);

parser_snapshot!(parse_mixed_array_dimensions, r#"class A { int[] a, b[]; }"#);

parser_snapshot!(parse_type_parameters_class, r#"class A<T> {}"#);

parser_snapshot!(
    parse_type_parameters_method,
    r#"class A { <A> void func() {} }"#
);

parser_snapshot!(parse_type_parameters_multiple, r#"class A<T, U> {}"#);

parser_snapshot!(
    parse_type_parameters_with_bound,
    r#"class A<T extends Number> {}"#
);

parser_snapshot!(
    parse_type_parameters_with_intersection_bound,
    r#"interface A<T extends B & C> {}"#
);

parser_snapshot!(parse_enum_constants_simple, r#"enum E { A, B, C }"#);

parser_snapshot!(
    parse_enum_constants_with_trailing_comma,
    r#"enum E { A, B, C, }"#
);

parser_snapshot!(
    parse_enum_constants_and_members,
    r#"enum E { A, B; int x; void f() {} }"#
);

parser_snapshot!(
    parse_annotation_array_initializer,
    r#"@A({1, 2, 3}) class B {}"#
);

parser_snapshot!(
    parse_annotation_nested_array_initializer,
    r#"@A({{1}, {2}}) class B {}"#
);

parser_snapshot!(parse_recovery_missing_member_name, r#"class A { int ; }"#);

parser_snapshot!(
    parse_recovery_broken_params,
    r#"class A { void f( { int x; } }"#
);

parser_snapshot!(
    parse_recovery_constructor_in_interface,
    r#"interface A { A() {} }"#
);

parser_snapshot!(
    parse_recovery_default_in_class,
    r#"class A { default void f() {} }"#
);

parser_snapshot!(
    parse_recovery_compact_constructor_like_in_class,
    r#"class A { A {} }"#
);

parser_snapshot!(
    parse_class_with_type_parameters,
    r#"public class A<A, B> {}"#
);

parser_snapshot!(
    parse_interface_with_type_parameters,
    r#"public interface A<T, U> {}"#
);

parser_snapshot!(
    parse_record_with_type_parameters,
    r#"public record R<T, U>(T x, U y) {}"#
);

parser_snapshot!(
    parse_type_parameters_method_void,
    r#"class A { <A> void func() {} }"#
);

parser_snapshot!(
    parse_type_parameters_method_typed_return,
    r#"class A { <T> T func() {} }"#
);

parser_snapshot!(
    parse_type_parameters_method_interface,
    r#"interface A { <T> T func(); }"#
);

parser_snapshot!(
    parse_type_parameters_generic_constructor,
    r#"class A { <T> A() {} }"#
);
