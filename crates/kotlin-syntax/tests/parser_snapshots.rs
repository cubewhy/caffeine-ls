mod common;

use common::parser_snapshot;
use indoc::indoc;

parser_snapshot!(parse_empty_file, "");

parser_snapshot!(
    parse_semicolons_only,
    indoc! {r#"
        ;;;
    "#}
);

parser_snapshot!(
    parse_shebang,
    indoc! {r#"
        #!/usr/bin/env kotlin
        fun main() {}
    "#}
);

parser_snapshot!(
    parse_package_header,
    indoc! {r#"
        package com.example.app
    "#}
);

parser_snapshot!(
    parse_package_header_with_semicolon,
    indoc! {r#"
        package com.example.app;
    "#}
);

parser_snapshot!(
    parse_import_single,
    indoc! {r#"
        import com.example.Foo
    "#}
);

parser_snapshot!(
    parse_import_star,
    indoc! {r#"
        import com.example.*
    "#}
);

parser_snapshot!(
    parse_import_alias,
    indoc! {r#"
        import com.example.Foo as Bar
    "#}
);

parser_snapshot!(
    parse_file_annotation,
    indoc! {r#"
        @file:JvmName("FooKt")
    "#}
);

parser_snapshot!(
    parse_multiple_file_annotations,
    indoc! {r#"
        @file:Suppress("UNUSED")
        @file:[JvmMultifileClass Foo Bar]
        package com.example
    "#}
);

parser_snapshot!(
    parse_simple_class,
    indoc! {r#"
        class Foo
    "#}
);

parser_snapshot!(
    parse_class_with_constructor,
    indoc! {r#"
        class Point(val x: Int, var y: Int = 0)
    "#}
);

parser_snapshot!(
    parse_class_with_type_params_and_delegation,
    indoc! {r#"
        class Box<T : Comparable<T>>(private val items: List<T>) : Iterable<T> {
            override fun iterator(): Iterator<T> = items.iterator()
        }
    "#}
);

parser_snapshot!(
    parse_data_class,
    indoc! {r#"
        data class User(val name: String, val age: Int)
    "#}
);

parser_snapshot!(
    parse_enum_class,
    indoc! {r#"
        enum class Direction(val deg: Int) {
            NORTH(0), EAST(90), SOUTH(180), WEST(270);

            fun opposite(): Direction = Direction.values()[(this.ordinal + 2) % 4]
        }
    "#}
);

parser_snapshot!(
    parse_interface,
    indoc! {r#"
        interface Repository<T> {
            fun find(id: Int): T?
            fun save(value: T)
        }
    "#}
);

parser_snapshot!(
    parse_fun_interface,
    indoc! {r#"
        fun interface Factory<out T> {
            fun create(): T
        }
    "#}
);

parser_snapshot!(
    parse_object_declaration,
    indoc! {r#"
        object Singleton : Runnable {
            override fun run() {}
        }
    "#}
);

parser_snapshot!(
    parse_companion_object_with_init,
    indoc! {r#"
        class Factory {
            companion object {
                const val DEFAULT = 1
            }

            init {
                println("init")
            }

            constructor(size: Int) : this(size, 0)

            var count: Int = 0
                private set

            val total: Int
                get() = count + 1

            fun onEach(block: (Int) -> Unit) {
                for (i in 0..count) block(i)
            }
        }
    "#}
);

parser_snapshot!(
    parse_function_declarations,
    indoc! {r#"
        fun add(a: Int, b: Int): Int = a + b

        fun greet(name: String = "world"): String {
            return "Hello, $name"
        }

        suspend fun fetch(): List<String> = emptyList()

        fun <T> List<T>.second(): T? = getOrNull(1)

        fun <T> containsIf(where: (T) -> Boolean): Boolean where T : Comparable<T>
    "#}
);

parser_snapshot!(
    parse_delegated_properties,
    indoc! {r#"
        class Lazy {
            val value: Int by lazy { 42 }
            var cached: String? by mutableLazy()
        }
    "#}
);

parser_snapshot!(
    parse_type_alias,
    indoc! {r#"
        typealias StringMap<V> = MutableMap<String, V>

        public typealias Predicate<T> = (T) -> Boolean
    "#}
);

parser_snapshot!(
    parse_annotated_declarations,
    indoc! {r#"
        @Deprecated("use new")
        @get:Synchronized
        class Legacy {
            @setparam:Inject var name: String = ""
        }
    "#}
);

parser_snapshot!(
    parse_assignments,
    indoc! {r#"
        fun main() {
            var x = 1
            x = 2
            x += 1
            x -= 1
            x *= 2
            x /= 2
            x %= 3
            obj.field = value
            arr[0] = 1
        }
    "#}
);

parser_snapshot!(
    parse_annotated_statement,
    indoc! {r#"
        fun main() {
            val x = make()

            // `statement: {label | annotation} (…)` — an annotation may prefix
            // a plain statement, not only declarations.
            @Suppress("UsePropertyAccessSyntax")
            x.setFoo(1)
        }
    "#}
);

parser_snapshot!(
    parse_loops,
    indoc! {r#"
        fun main() {
            for (i in 1..10) {
                println(i)
            }
            for ((k, v) in map) {
                println("$k=$v")
            }
            var x = 0
            while (x < 10) {
                x++
            }
            do {
                x--
            } while (x > 0)
        }
    "#}
);

parser_snapshot!(
    parse_labeled_loop,
    indoc! {r#"
        fun main() {
            outer@ for (i in 0..2) {
                for (j in 0..2) {
                    if (i == j) break@outer
                }
            }
        }
    "#}
);

parser_snapshot!(
    parse_if_as_statement,
    indoc! {r#"
        fun main() {
            if (a > 0) return
            if (a < 0) ; else a
        }
    "#}
);

parser_snapshot!(
    parse_recovery_unexpected_tokens,
    indoc! {r#"
        fun main() {
            val x = ;
            }
        }
    "#}
);

parser_snapshot!(
    parse_recovery_missing_expr,
    indoc! {r#"
        fun main() {
            while () {}
            for (i in ) {}
        }
    "#}
);

parser_snapshot!(
    parse_error_ending,
    indoc! {r#"
        class Unclosed {
            val x = 1
    "#}
);

parser_snapshot!(
    parse_unicode_identifiers,
    indoc! {r#"
        fun 问候(名字: String): String {
            val 簡明 = 名字
            return 簡明
        }
    "#}
);

// ————————————————————————————————————————————————————————————————
// Extension functions / properties with a `receiverType '.'` prefix
// ([spec: grammar-rule-receiverType]).
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_extension_function_simple,
    indoc! {r#"
        fun String.toURI(): java.net.URI = URI.create(this)
    "#}
);

parser_snapshot!(
    parse_extension_function_with_type_args,
    indoc! {r#"
        fun <T> Array<T>.forEachIsEnd(action: (T, Boolean) -> Unit) {
            this.forEachIndexed { index, t -> action(t, index == this.size - 1) }
        }
    "#}
);

parser_snapshot!(
    parse_extension_function_qualified_receiver,
    indoc! {r#"
        fun Map.Entry<String, Int>.keyOf(): String = key
    "#}
);

parser_snapshot!(
    parse_extension_property,
    indoc! {r#"
        val Response.string: String
            get() = body
    "#}
);

// ————————————————————————————————————————————————————————————————
// Class headers: `class C internal constructor(...)` and its plain
// `class C constructor(...)` form ([spec: grammar-rule-primaryConstructor]).
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_class_with_modified_primary_constructor,
    indoc! {r#"
        class ServerException internal constructor(cause: Throwable) :
            RuntimeException(cause.message, cause)
    "#}
);

parser_snapshot!(
    parse_class_with_explicit_primary_constructor,
    indoc! {r#"
        class Point constructor(x: Int, y: Int)
    "#}
);

// ————————————————————————————————————————————————————————————————
// `catch (_: Throwable)` — an underscore catch parameter.
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_catch_with_underscore_parameter,
    indoc! {r#"
        fun read(): String {
            return try {
                Files.readString(path)
            } catch (_: IOException) {
                ""
            }
        }
    "#}
);

// ————————————————————————————————————————————————————————————————
// Accessors on their own line after the initializer:
// `var x = 0` / `private set` / `get() = …` ([spec: grammar-rule-getter],
// [spec: grammar-rule-setter]).
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_property_accessors_with_modifiers,
    indoc! {r#"
        class Counter {
            var count: Int = 0
                private set
            val total: Int
                get() = count + 1
        }
    "#}
);

// ————————————————————————————————————————————————————————————————
// The elvis operator may begin a new line; and a braceless `if` branch is
// terminated by a line break (no infix call across lines,
// [spec: grammar-rule-infixFunctionCall]).
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_elvis_on_next_line,
    indoc! {r#"
        fun f() {
            val uuid = convert(input)
                ?: return
            use(uuid)
        }
    "#}
);

parser_snapshot!(
    parse_braceless_if_then_next_statement,
    indoc! {r#"
        fun f() {
            if (b == -1) throw E()
            b = stream.read()
            other as Assets
        }
    "#}
);

// ————————————————————————————————————————————————————————————————
// `$this` / `$super` short string templates
// ([spec: grammar-rule-stringLiteral]).
// ————————————————————————————————————————————————————————————————

parser_snapshot!(
    parse_string_template_this_and_super,
    indoc! {r#"
        fun f() {
            println("is $this")
            println("is $super")
        }
    "#}
);
