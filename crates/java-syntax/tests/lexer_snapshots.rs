mod common;

use common::{lexer_error_snapshot, lexer_snapshot};
use indoc::indoc;

lexer_snapshot!(lex_empty, "");
lexer_snapshot!(lex_whitespace_only, " \t\n\r  ");

lexer_snapshot!(lex_line_comment_only, "// this is a line comment\n");
lexer_snapshot!(lex_block_comment_only, "/* this is a \n block comment */");
lexer_snapshot!(lex_comments_in_code, "int /* comment */ x // line");

lexer_snapshot!(
    lex_javadoc_comment,
    indoc! {"
        /**
         * This is a Javadoc comment
         * @param x the value
         */
        public int x;
    "}
);

lexer_snapshot!(lex_keywords_and_identifiers_1, "public static void main");
lexer_snapshot!(
    lex_keywords_and_identifiers_2,
    "class interface enum record"
);
lexer_snapshot!(
    lex_keywords_and_identifiers_3,
    "$myVar _underscore value123"
);

lexer_snapshot!(lex_boolean_and_null_literals, "true false null");

lexer_snapshot!(lex_separators, "( ) { } [ ] ; , . ... :: @");

lexer_snapshot!(
    lex_operators,
    indoc! {"
        + += ++ - -= -- ->
        * *= / /= % %= == = != !
        < <= << <<= > >= >> >>= >>> >>>=
        & &= | |= ^ ^= && ||
    "}
);

lexer_snapshot!(lex_integer_literals_decimal, "0 123 1_000_000 456L");
lexer_snapshot!(lex_integer_literals_hex, "0x0 0x1A2B 0XCAFE_BABE 0xFFl");
lexer_snapshot!(lex_integer_literals_binary, "0b0 0B1010_0101 0b11L");

lexer_snapshot!(
    lex_floating_point_literals_decimal,
    "1.23 .5 10. 3.14f 6.022e23 1e-9d"
);
lexer_snapshot!(lex_floating_point_literals_hex, "0x1.0p3 0x.8P-2f");

lexer_snapshot!(
    lex_string_literals,
    r#" "hello world" "escape \" test" "" "#
);

lexer_snapshot!(
    lex_text_blocks,
    indoc! {r#"
        """
        Hello
          World
        """
    "#}
);

lexer_snapshot!(lex_char_literals, "'a' '\\n' '\\''");

lexer_snapshot!(
    lex_complex_jls_scenario,
    "List<String> list = new ArrayList<>();"
);

lexer_error_snapshot!(
    lex_error_unterminated_string_eof,
    "\"this string has no end"
);
lexer_error_snapshot!(lex_error_unterminated_string_newline, "\"line1\nline2\"");

lexer_error_snapshot!(
    lex_error_unterminated_comment,
    "/* this block comment never ends "
);

lexer_error_snapshot!(lex_error_invalid_number_trailing_underscore, "123_");
lexer_error_snapshot!(lex_error_invalid_number_binary_digit, "0b1012");
lexer_error_snapshot!(lex_error_invalid_number_fraction_underscore, "0._1f");
lexer_error_snapshot!(lex_error_invalid_number_octal_like, "019");

lexer_error_snapshot!(lex_error_illegal_text_block_open, "\"\"\"illegal");

lexer_error_snapshot!(lex_error_invalid_char_multiple, "'abc'");
lexer_error_snapshot!(lex_error_invalid_char_empty, "''");

lexer_snapshot!(lex_unicode_escape_keyword, "\\u0070ublic class Test {}");
lexer_snapshot!(lex_unicode_escape_identifier, "int my\\u005Fvar = 1;");
lexer_snapshot!(lex_unicode_escape_multiple_u, "char \\uuuu0061 = 'a';");
lexer_snapshot!(lex_unicode_escape_operator, "int a = 1 \\u002B\\u002B;");

lexer_error_snapshot!(
    lex_error_unicode_escape_in_string_quote,
    "String s = \"\\u0022\";"
);
lexer_snapshot!(
    lex_unicode_escape_in_string_backslash_quote,
    "String s = \"\\u005C\\u0022\";"
);

lexer_error_snapshot!(lex_error_invalid_unicode_escape_short, "int \\u006 = 1;");
lexer_error_snapshot!(lex_error_invalid_unicode_escape_non_hex, "int \\u006G = 1;");

lexer_snapshot!(
    lex_comment_termination_unicode_line_break,
    "// hidden comment \\u000A int x = 1;"
);
lexer_snapshot!(
    lex_comment_termination_cr,
    "// normal comment \r int z = 3;"
);
lexer_snapshot!(
    lex_comment_termination_crlf,
    "// normal comment \r\n int w = 4;"
);

lexer_error_snapshot!(lex_error_invalid_underscore_integer, "123_");
lexer_error_snapshot!(lex_error_invalid_underscore_float_1, "123_.45");
lexer_error_snapshot!(lex_error_invalid_underscore_float_2, "123._45");
lexer_error_snapshot!(lex_error_invalid_underscore_exponent_1, "1e_10");
lexer_error_snapshot!(lex_error_invalid_underscore_exponent_2, "1e+_10");

lexer_snapshot!(lex_string_template_str_prefix, "STR.\"Hello \\{name}!\"");
lexer_snapshot!(lex_string_template_multiple_holes, "\"a \\{b} c \\{d} e\"");
lexer_snapshot!(
    lex_string_template_nested_expression,
    indoc! {r#"
        "Result: \{ new int[]{1, 2} }"
    "#}
);
lexer_snapshot!(
    lex_string_template_nested_string,
    "\"Outer \\{ \"Inner \\{x}\" }\""
);

lexer_snapshot!(
    lex_text_block_template,
    indoc! {r#"
        """
          Line 1 \{a}
          Line 2"""
    "#}
);

lexer_snapshot!(lex_bom, "\u{FEFF}int x = 1;");
lexer_snapshot!(lex_eof_sub_character, "int x = 1;\x1A");
lexer_snapshot!(lex_unicode_char_bmp, "'你'");
lexer_error_snapshot!(lex_error_unicode_char_non_bmp, "'🐘'");

lexer_snapshot!(lex_escape_sequence_s_string, r#" "trailing space\s" "#);
lexer_snapshot!(
    lex_escape_sequence_s_text_block,
    indoc! {r#"
        """
            line 1\s
            line 2
        """
    "#}
);

lexer_snapshot!(
    lex_incomplete_ellipsis,
    indoc! {r#"
        ..
    "#}
);

lexer_snapshot!(
    lex_underscope,
    indoc! {r#"
        String _;
    "#}
);

lexer_snapshot!(
    lex_tilde,
    indoc! {r#"
        ~1
    "#}
);

lexer_snapshot!(
    lex_utf_surrogates,
    indoc! {r#"
        \uD83C\uDF51
    "#}
);
