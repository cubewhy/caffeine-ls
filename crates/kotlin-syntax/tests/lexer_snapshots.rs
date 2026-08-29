use crate::common::lexer_snapshot;
use indoc::indoc;

mod common;

lexer_snapshot!(
    basic_function_and_vars,
    indoc! {r#"
        fun main() {
            val name = "Kotlin"
            var count = 42
        }
    "#}
);

lexer_snapshot!(
    string_interpolations,
    indoc! {r#"
        fun greet(name: String) {
            println("Hello, $name!")
            println("Length: ${name.length}")
        }
    "#}
);

lexer_snapshot!(
    raw_strings,
    indoc! {r#"
        val json = """
            {
                "name": "kotlin",
                "version": 2
            }
        """
    "#}
);

lexer_snapshot!(
    nested_interpolations_expressions,
    indoc! {r#"
        val result = "sum = ${1 + ${'$'}{2 + 3}}"
    "#}
);

lexer_snapshot!(
    comments_and_kdoc,
    indoc! {r#"
        /**
         * Adds two numbers
         */
        fun add(a: Int, b: Int): Int {
            // return result
            return a + b
        }
    "#}
);

lexer_snapshot!(
    nested_block_comments,
    indoc! {r#"
        /*
            outer
            /* inner */
            still outer
        */
        val x = 1
    "#}
);

lexer_snapshot!(
    number_literals,
    indoc! {r#"
        123
        0xFFEE
        0b1010
        1.5
        1e10
        1.5f
        123UL
    "#}
);

lexer_snapshot!(
    invalid_number_literals,
    indoc! {r#"
        1.0u
        1.0l
        1_
        0x_
        0b_
        1e
        1e+ 
        1_.2
        1._2
        0x_FF
        0b_1010
    "#}
);

lexer_snapshot!(
    operators,
    indoc! {r#"
        val a = b ?: c
        val d = a?.length
        val e = !!d
        val f = 1..10
        val g = 1..<10
        val h = x == y
        val i = x === y
        val j = x != y
        val k = x !== y
    "#}
);

lexer_snapshot!(
    char_literals,
    indoc! {r#"
        val a = 'a'
        val b = '\n'
        val c = '\''
    "#}
);

lexer_snapshot!(
    backtick_identifiers,
    indoc! {r#"
        val `when` = 42

        fun `strange function name`() {
            println(`when`)
        }
    "#}
);

lexer_snapshot!(
    control_flow_keywords,
    indoc! {r#"
        fun test(x: Int) {
            if (x > 0) {
                while (x > 1) {
                    break
                }
            } else {
                return
            }
        }
    "#}
);

lexer_snapshot!(
    package_and_import_like_syntax,
    indoc! {r#"
        package com.example.app

        class User
        interface Repository
        object Singleton
    "#}
);

lexer_snapshot!(
    unterminated_string_error,
    indoc! {r#"
        val x = "hello
    "#}
);

lexer_snapshot!(
    unterminated_block_comment_error,
    indoc! {r#"
        /*
            never closed
    "#}
);

lexer_snapshot!(
    unsupported_escape_sequence_error,
    indoc! {r#"
        val bad = "\q"
    "#}
);

lexer_snapshot!(
    empty_char_literal_error,
    indoc! {r#"
        val c = ''
    "#}
);

lexer_snapshot!(
    wrong_long_suffix_case_error,
    indoc! {r#"
        val a = 1l
        val b = 1Ul
    "#}
);

lexer_snapshot!(
    leading_zero_error,
    indoc! {r#"
        val x = 0123
    "#}
);

lexer_snapshot!(
    leading_zero_with_separator,
    indoc! {r#"
        val a = 0_1
        val b = 0_2
        val c = 0_
    "#}
);

lexer_snapshot!(
    dot_leading_float,
    indoc! {r#"
        val a = .5
        val b = .5e2
        val c = .5E+2
        val d = .5f
        val e = .0
    "#}
);

lexer_snapshot!(
    raw_string_quote_runs,
    indoc! {r#"
        val a = """x"""" 
        val b = """x"""""
        val c = """x""""""
        val d = """"""""
    "#}
);

lexer_snapshot!(
    form_feed_whitespace,
    "val a = 1\u{000C}+\u{000C}2\nval b = 3\u{000C}\u{000C}4\n",
);

lexer_snapshot!(
    semicolon,
    indoc! {r#"
        ;;;
    "#}
);

lexer_snapshot!(
    shebang_line,
    indoc! {r#"
        #!/usr/bin/env kotlin
        fun main() {
            println("Hello, script!")
        }
    "#}
);

lexer_snapshot!(
    unicode_escapes,
    indoc! {r#"
        val copyright = "\u00A9 2024"
        val heart = '\u2764'
        val invalidStr = "\uXXYY"
        val invalidChar = '\uXX'
    "#}
);

lexer_snapshot!(
    multi_dollar_interpolation,
    indoc! {r#"
        val short = $$"User: $$name"
        val long = $$$"User: $$${user.name}"
        val mixed = $$"Literal $ and $$variable"
        val rawTemplate = $$"""
            Cost: $$$$price
            Items: $${items.size}
        """
    "#}
);

lexer_snapshot!(
    char_literal_too_many_chars,
    indoc! {r#"
        val valid = 'a'
        val invalidDouble = 'ab'
        val invalidTriple = 'abc'
        val empty = ''
        val nextValid = 'c'
    "#}
);

lexer_snapshot!(
    zero,
    indoc! {r#"
        0
    "#}
);

lexer_snapshot!(
    backtick_identifier_in_string_interpolation,
    indoc! {r#"
        "$`an identifier`"
    "#}
);

lexer_snapshot!(
    not_is_and_not_in,
    indoc! {r#"
        !is !in
    "#}
);

lexer_snapshot!(
    safe_as,
    indoc! {r#"
        as?
    "#}
);

lexer_snapshot!(
    unterminated_raw_string,
    indoc! {r#"
        """
        unterminated
    "#}
);

lexer_snapshot!(
    char_literals_valid,
    indoc! {r#"
        'a'
        '0'
        '\t'
        '\b'
        '\n'
        '\r'
        '\''
        '\"'
        '\\'
        '\$'
        '\u0041'
    "#}
);

lexer_snapshot!(
    char_literals_invalid,
    indoc! {r#"
        ''
        'abc'
        '\x'
        '\u12G4'
        'a
    "#}
);

lexer_snapshot!(
    unicode_identifiers,
    indoc! {r#"
        fun 问候(名字: String): String {
            val 簡明 = "你好"
            val x١ = 42
            return 名字
        }
    "#}
);

lexer_snapshot!(
    unicode_in_strings_and_comments,
    indoc! {r#"
        /**
         * 添加两个数字
         */
        fun add(a: Int, b: Int): Int {
            // 注释包含中文
            val msg = "欢乐 😀 世界"
            val ch = '好'
            return a + b
        }
    "#}
);

lexer_snapshot!(
    unicode_template_interpolation,
    indoc! {r#"
        fun main() {
            val name = "世界"
            println("你好, $name!")
            println("$名称 是 $name")
        }
    "#}
);

lexer_snapshot!(
    unicode_illegal_character,
    indoc! {r#"
        val face = 😀1
    "#}
);
