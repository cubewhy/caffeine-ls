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
