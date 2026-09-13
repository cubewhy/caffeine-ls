//! The Kotlin declaration lowering: one snapshot per declaration form.
//!
//! Every fixture here is valid Kotlin and compiles clean with kotlinc 2.4.20
//! (JRE 25) — the empirical oracle for the shapes these snapshots pin. The
//! two forms the parser used to reject (a modified `constructor`, a modified
//! `companion object`) are covered by the last case, which is the one this
//! milestone's parser fix unblocks.

#[macro_use]
mod common;

use base_db::LanguageKind;

// -- files: package, imports, file annotations ------------------------------

lower_snapshot_lang! {
    kotlin_empty_file,
    LanguageKind::Kotlin,
    "",
}

lower_snapshot_lang! {
    kotlin_package_and_imports,
    LanguageKind::Kotlin,
    r#"
package com.example.app

import com.example.app.Foo
import com.example.app.*
import com.example.app.Foo as Bar
import kotlin.collections.List

class Foo
"#,
}

lower_snapshot_lang! {
    kotlin_file_annotations,
    LanguageKind::Kotlin,
    r#"
@file:JvmName("FooKt")
@file:Suppress("UNUSED")

package com.example
"#,
}

// -- classifiers ------------------------------------------------------------

lower_snapshot_lang! {
    kotlin_class_with_primary_constructor,
    LanguageKind::Kotlin,
    r#"
class Point(val x: Int, var y: Int = 0, label: String)
"#,
}

lower_snapshot_lang! {
    kotlin_class_with_type_params_and_supertypes,
    LanguageKind::Kotlin,
    r#"
class Box<out T : Any>(private val items: List<T>) : Iterable<T> {
    override fun iterator(): Iterator<T> = items.iterator()
}

class Sorted<T>(val items: List<T>) : Iterable<T> where T : Comparable<T> {
    override fun iterator(): Iterator<T> = items.iterator()
}
"#,
}

lower_snapshot_lang! {
    kotlin_data_class,
    LanguageKind::Kotlin,
    r#"
data class User(val name: String, val age: Int)
"#,
}

lower_snapshot_lang! {
    kotlin_interface_and_fun_interface,
    LanguageKind::Kotlin,
    r#"
interface Repository<T> {
    fun find(id: Int): T?
    fun save(value: T)
}

fun interface Factory<out T> {
    fun create(): T
}
"#,
}

lower_snapshot_lang! {
    kotlin_enum_class,
    LanguageKind::Kotlin,
    r#"
enum class Direction(val deg: Int) {
    NORTH(0),
    EAST(90) {
        override fun opposite(): Direction = WEST
    },
    WEST(270);

    open fun opposite(): Direction = this
}
"#,
}

lower_snapshot_lang! {
    kotlin_annotation_class,
    LanguageKind::Kotlin,
    r#"
annotation class Ann(val name: String, val level: Int = 0)
"#,
}

lower_snapshot_lang! {
    kotlin_objects_and_companion,
    LanguageKind::Kotlin,
    r#"
object Util {
    fun f(): Int = 1
}

class Factory {
    private companion object {
        const val DEFAULT = 1
    }
}

class Named {
    companion object Factory
}
"#,
}

lower_snapshot_lang! {
    kotlin_nested_classes,
    LanguageKind::Kotlin,
    r#"
class Outer {
    class Nested

    inner class Inner

    val x: Int = 1
}
"#,
}

// -- functions --------------------------------------------------------------

lower_snapshot_lang! {
    kotlin_functions,
    LanguageKind::Kotlin,
    r#"
fun top(): Int = 1

internal suspend fun <T> List<T>.firstOrNull(predicate: (T) -> Boolean): T? = null

fun varargFunction(vararg names: String, count: Int = 0): String = names[count]

infix fun Int.shift(amount: Int): Int = this + amount
"#,
}

lower_snapshot_lang! {
    kotlin_function_types,
    LanguageKind::Kotlin,
    r#"
fun apply(f: (Int, String) -> Boolean): Boolean = f(1, "")

fun withReceiver(f: String.(Int) -> Boolean): Boolean = "".f(1)

fun <T> generic(value: T?): T? = value
"#,
}

// -- properties -------------------------------------------------------------

lower_snapshot_lang! {
    kotlin_properties,
    LanguageKind::Kotlin,
    r#"
val topLevel: String = "x"

var counter: Int = 0
    private set

val total: Int
    get() = counter + 1

val delegated: String by lazy { "x" }

lateinit var initialised: String

val Response.body: String
    get() = "body"

class Response

val <T> List<T>.first: T?
    get() = null
"#,
}

lower_snapshot_lang! {
    kotlin_class_with_accessors,
    LanguageKind::Kotlin,
    r#"
class Counter {
    var count: Int = 0
        private set

    val total: Int
        get() = count + 1

    var custom: Int = 0
        get() = field
        set(value) {
            field = value
        }
}
"#,
}

// -- constructors and initializers ------------------------------------------

lower_snapshot_lang! {
    kotlin_constructors,
    LanguageKind::Kotlin,
    r#"
class Server private constructor(val port: Int) {
    @Deprecated("use the other one")
    constructor(host: String) : this(port = 0) {
        println(host)
    }

    init {
        println(port)
    }
}
"#,
}

// -- type aliases -----------------------------------------------------------

lower_snapshot_lang! {
    kotlin_type_alias,
    LanguageKind::Kotlin,
    r#"
typealias StringMap<V> = Map<String, V>

typealias Handler = (Int) -> Unit
"#,
}

// -- types ------------------------------------------------------------------

lower_snapshot_lang! {
    kotlin_types,
    LanguageKind::Kotlin,
    r#"
class Types<in T, out U> {
    val nullable: String? = null
    val star: List<*> = listOf(1)
    val projected: List<out Number> = listOf(1)
    val function: (Int) -> String = { it.toString() }
    val suspendFunction: suspend () -> Unit = {}
    val qualified: kotlin.collections.List<Int> = listOf(1)
    val array: Array<String> = emptyArray()
    val literal: Int = 1
}
"#,
}

// -- annotations ------------------------------------------------------------

lower_snapshot_lang! {
    kotlin_annotations,
    LanguageKind::Kotlin,
    r#"
@Deprecated("use Other")
@Suppress("UNUSED", "UNCHECKED_CAST")
class Annotated @Deprecated("inject") constructor(val name: String) {
    @Deprecated("old")
    fun f() {}

    @get:JvmName("renamed")
    val x: Int = 1
}

annotation class Marker
"#,
}

// -- the placeholder this milestone replaces, and the parser fix it relies on

lower_snapshot_lang! {
    kotlin_class_and_function,
    LanguageKind::Kotlin,
    r#"
class Greeter {
    fun greet(name: String): String {
        return "hi"
    }
}
"#,
}

lower_snapshot_lang! {
    kotlin_modified_constructor_and_companion,
    LanguageKind::Kotlin,
    r#"
class C {
    private companion object {
        fun f() = 1
    }

    init {
        println(1)
    }

    @Deprecated("d") constructor(x: Int)
}
"#,
}

// -- bodies -----------------------------------------------------------------

body_snapshot_lang! {
    kotlin_body_block,
    LanguageKind::Kotlin,
    r#"
fun greet(name: String): String {
    val prefix = "hello "
    if (name.isEmpty()) {
        return prefix
    }
    return prefix + name
}
"#,
}

body_snapshot_lang! {
    kotlin_body_expression,
    LanguageKind::Kotlin,
    r#"
class Point(val x: Int, val y: Int) {
    fun length(): Double = x * x + y * y
}

val doubled: Int = 2 * 3
"#,
}

body_snapshot_lang! {
    kotlin_body_when,
    LanguageKind::Kotlin,
    r#"
fun classify(value: Any): String {
    when (value) {
        1 -> return "one"
        in 2..9 -> return "small"
        is String -> return value
        else -> return "other"
    }
}

fun exhaustive(value: Int): String = when (value) {
    0 -> "zero"
    else -> "many"
}
"#,
}

body_snapshot_lang! {
    kotlin_body_nullability_and_calls,
    LanguageKind::Kotlin,
    r#"
fun render(item: Item?): String {
    val name = item?.name ?: "unknown"
    val length = item!!.length
    val cast = item as? String
    val forced = item as String
    val text = "item $name is ${length} long"
    return text
}
"#,
}

body_snapshot_lang! {
    kotlin_body_lambdas_and_references,
    LanguageKind::Kotlin,
    r#"
fun apply(items: List<Int>): List<String> {
    val mapped = items.map { it.toString() }
    val filtered = items.filter { value -> value > 0 }
    val reference = ::apply
    val member = items::size
    return mapped + filtered
}
"#,
}

body_snapshot_lang! {
    kotlin_body_loops_and_try,
    LanguageKind::Kotlin,
    r#"
fun scan(items: List<Int>): Int {
    var total = 0
    for (item in items) {
        total += item
    }
    while (total > 100) {
        total -= 1
    }
    do {
        total += 1
    } while (total < 0)
    try {
        total = risky(total)
    } catch (e: IllegalStateException) {
        total = 0
    } finally {
        println(total)
    }
    return total
}
"#,
}

body_snapshot_lang! {
    kotlin_body_destructuring,
    LanguageKind::Kotlin,
    r#"
fun sum(pair: Pair<Int, Int>): Int {
    val (first, second) = pair
    return first + second
}
"#,
}

body_snapshot_lang! {
    kotlin_body_locals_and_object_literals,
    LanguageKind::Kotlin,
    r#"
fun local(): Int {
    class Counter(val start: Int) {
        fun next(): Int = start + 1
    }

    fun twice(value: Int): Int = value * 2

    val anonymous = object : Runnable {
        override fun run() {}
    }

    return twice(Counter(1).next())
}
"#,
}

body_snapshot_lang! {
    kotlin_body_accessors_and_initializers,
    LanguageKind::Kotlin,
    r#"
class Holder {
    val computed: Int
        get() = 1 + 2

    var stored: Int = 0
        set(value) {
            println(value)
        }

    val delegated: String by lazy { "x" }

    init {
        println(computed)
    }

    constructor(seed: Int) : this() {
        println(seed)
    }
}

enum class Level {
    LOW {
        override fun label(): String = "low"
    };

    open fun label(): String = "level"
}
"#,
}
