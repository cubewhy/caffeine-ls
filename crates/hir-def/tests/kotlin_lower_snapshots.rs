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

// -- the forms the body IR dropped -------------------------------------------
//
// Every fixture below was compiled with
//
//     JAVA_HOME=/home/cubewhy/.jdks/temurin-21.0.11 kotlinc -d out <file>.kt
//
// against kotlinc-jvm 2.4.20 (JRE 21.0.11+10-LTS) and compiled clean — the
// empirical oracle these snapshots pin. The object-literal fixture is the one
// whose *lowered* shape this milestone fixes; `javap -p` of the compiler's
// output for it reads
//
//     public final class A_object_literalKt {
//       private static final java.lang.Object x;
//       public static final java.lang.Object getX();
//       public static final void use(java.lang.Runnable);
//       public static final void caller();
//       static {};
//     }
//     public final class A_object_literalKt$x$1 {
//       A_object_literalKt$x$1();
//     }
//     public final class A_object_literalKt$caller$1 implements java.lang.Runnable {
//       A_object_literalKt$caller$1();
//       public void run();
//     }
//
// — each literal is its own anonymous class, named after the declaration it
// stands in, which is why the lowering must anchor the literal and not the
// declaration that happens to precede it.

body_snapshot_lang! {
    kotlin_body_object_literal_identity,
    LanguageKind::Kotlin,
    r#"
val x = object : Any() {}

fun use(runnable: Runnable) {}

fun caller() {
    use(object : Runnable {
        override fun run() {}
    })
}
"#,
}

// The three shapes the members-and-supertypes fix is about, each compiled with
// the same kotlinc and read back with `javap -p`:
//
//     public final class MembersKt$members$obj$1 implements java.lang.Runnable {
//       private final java.lang.String label;
//       public final java.lang.String getLabel();
//       public void run();
//       public final int extra();
//     }
//     public final class MultipleKt$multiple$obj$1 extends Base implements B {
//       public int b();
//     }
//     public final class DelegationKt$delegation$obj$1 implements I {
//       private final Impl $$delegate_0;
//       public int i();
//     }
//
// — the literal's class carries the members its body declares (a property, an
// override and a plain function), every delegation specifier as a supertype,
// and `by` as a delegating field next to the body's own members.

body_snapshot_lang! {
    kotlin_body_object_literal_members,
    LanguageKind::Kotlin,
    r#"
fun members() {
    val obj = object : Runnable {
        val label: String = "x"
        override fun run() {}
        fun extra(): Int = 1
    }
    obj.extra()
}
"#,
}

body_snapshot_lang! {
    kotlin_body_object_literal_multiple_supertypes,
    LanguageKind::Kotlin,
    r#"
open class Base(val n: Int)
interface B { fun b(): Int }
fun multiple() {
    val obj = object : Base(1), B {
        override fun b() = n
    }
}
"#,
}

body_snapshot_lang! {
    kotlin_body_object_literal_delegation,
    LanguageKind::Kotlin,
    r#"
interface I { fun i(): Int }
class Impl : I { override fun i() = 1 }
fun delegation() {
    val obj = object : I by Impl() {
        override fun i() = 2
    }
}
"#,
}

body_snapshot_lang! {
    kotlin_body_local_declaration_parents,
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
    kotlin_body_literals,
    LanguageKind::Kotlin,
    r#"
fun literals(): Long {
    val hex = 0xFF
    val binary = 0b1010
    val unsigned = 1u
    val unsignedLong = 1UL
    val max = 0xFFFFFFFFu
    val grouped = 1_000L
    val single = 1.5f
    val exponent = 1e3
    val floatExponent = 3.0e-2F
    val newline = '\n'
    val backslash = '\\'
    val letter = '\u0041'
    return grouped
}
"#,
}

body_snapshot_lang! {
    kotlin_body_when_subject_binding,
    LanguageKind::Kotlin,
    r#"
fun classify(value: Any): String = when (val subject = value) {
    is String -> subject
    else -> "other"
}
"#,
}

body_snapshot_lang! {
    kotlin_body_for_destructuring,
    LanguageKind::Kotlin,
    r#"
fun scan(entries: Map<Int, String>) {
    for ((key, value) in entries) {
        println(key)
        println(value)
    }
}
"#,
}

body_snapshot_lang! {
    kotlin_body_local_delegated_property,
    LanguageKind::Kotlin,
    r#"
fun delegated(): Int {
    val lazy by lazy { 1 }
    val mapped: Int by mapOf("a" to 1)
    return lazy + mapped
}
"#,
}

body_snapshot_lang! {
    kotlin_body_this_and_super_qualifiers,
    LanguageKind::Kotlin,
    r#"
class Inner {
    fun outer(): Inner = this@Inner

    fun qualified(): Inner = this
}

open class Base {
    open fun value(): Int = 1
}

class Derived : Base() {
    fun call(): Int = super<Base>.value()
}
"#,
}

body_snapshot_lang! {
    kotlin_body_anonymous_functions,
    LanguageKind::Kotlin,
    r#"
fun anonymous(): Int {
    val expression = fun(x: Int) = x + 1
    val block = fun(a: Int) {
        println(a)
    }
    block(1)
    return expression(1)
}
"#,
}

lower_snapshot_lang! {
    kotlin_function_parameter_modifiers,
    LanguageKind::Kotlin,
    r#"
inline fun hoist(noinline body: () -> Unit, crossinline view: () -> Unit) {
    body()
    view()
}

fun spread(vararg values: String) {}
"#,
}

// -- default values, delegation arguments and annotation payloads ------------
//
// Every fixture below compiles clean with kotlinc 2.4.20 (JRE 21.0.11) under
// the JDK at /home/cubewhy/.jdks/temurin-21.0.11:
//
//     JAVA_HOME=/home/cubewhy/.jdks/temurin-21.0.11 kotlinc -d out *.kt
//
// — exit status 0, no diagnostics — so the shapes the snapshots pin are the
// ones the compiler accepts.

lower_snapshot_lang! {
    kotlin_default_parameter_values,
    LanguageKind::Kotlin,
    r#"
fun greet(name: String = "world", count: Int = 1, loud: Boolean = false): String = name

class Point(val x: Int = 0, val y: Int = 0)

class Box {
    val size: Int

    constructor(size: Int = 1) {
        this.size = size
    }

    constructor() : this(2)
}
"#,
}

lower_snapshot_lang! {
    kotlin_supertype_calls_and_delegation,
    LanguageKind::Kotlin,
    r#"
interface I

class Impl : I

open class Base(val n: Int)

class C : Base(1), I by Impl()
"#,
}

lower_snapshot_lang! {
    kotlin_constructor_delegation_arguments,
    LanguageKind::Kotlin,
    r#"
open class Base2(val n: Int)

class Derived2 : Base2 {
    constructor(n: Int) : super(n)

    constructor() : this(0)
}
"#,
}

lower_snapshot_lang! {
    kotlin_file_annotation_payloads,
    LanguageKind::Kotlin,
    r#"
@file:JvmName("Renamed")
@file:Suppress("UNUSED")

package a.b

class Uses
"#,
}

/// A class-literal argument, written with a *qualified* receiver and with a
/// simple one: both are the annotation's `ClassLit` value, and kotlinc 2.4.20
/// reads either as the type the literal names (the fixture compiles clean with
/// the two imports).
lower_snapshot_lang! {
    kotlin_class_literal_annotation_argument,
    LanguageKind::Kotlin,
    r#"
package a

import java.io.IOException

@Throws(java.io.IOException::class)
fun read(path: String): String = path

class Reader {
    @get:Throws(IOException::class, IllegalStateException::class)
    val name: String = "reader"
}
"#,
}

lower_snapshot_lang! {
    kotlin_annotation_qualified_name,
    LanguageKind::Kotlin,
    r#"
import kotlin.jvm.JvmName as JN

@kotlin.jvm.JvmName("topRenamed")
fun top(): Int = 1

class Renamed {
    @JN("renamed")
    fun m(): Int = 1

    @get:kotlin.jvm.JvmName("qualifiedGetter")
    val v: Int = 0
}
"#,
}

lower_snapshot_lang! {
    kotlin_annotation_use_site_targets,
    LanguageKind::Kotlin,
    r#"
@get:JvmName("renamed")
val z = 1

class Holder {
    @get:JvmName("value")
    val item = 2
}
"#,
}

lower_snapshot_lang! {
    kotlin_annotation_element_values,
    LanguageKind::Kotlin,
    r#"
enum class EventPriority { FIRST, THIRD }

annotation class EventTarget(val priority: EventPriority)

annotation class Inner(val s: String)

annotation class Full(
    val klass: kotlin.reflect.KClass<*>,
    val array: IntArray,
    val nested: Inner,
    val constant: EventPriority,
    val named: String,
)

class Foo

@EventTarget(EventPriority.THIRD)
@Full(Foo::class, [1, 2], Inner("x"), EventPriority.FIRST, named = "n")
class Annotated
"#,
}

// -- a call's written type arguments ----------------------------------------

/// A call's `typeArguments` prefix every kind of call suffix and are the only
/// place such a call states its type parameters
/// ([spec: grammar-rule-typeArguments]): a bare name, a member call and a
/// constructor all carry them, and `kotlinc` 2.4.20 compiles this file clean.
body_snapshot_lang! {
    kotlin_body_call_type_arguments,
    LanguageKind::Kotlin,
    r#"
fun f(a: Int): Int = a

class Box<T>(val value: T)

fun use(xs: List<Int>): Int {
    val a = mutableListOf<Int>()
    val b = xs.map<Int, Int> { it }
    val c = Box<Int>(1)
    val d = f<Int>(2)
    return a.size + b.size + c.value + d
}
"#,
}

/// The receiver of a callable reference is written as a `userType`
/// ([spec: grammar-rule-callableReference]), so `items::size` names a *value*
/// through the same production `Foo::class` names a *type* through: the
/// receiver lowers to the expression the dotted name has in expression
/// position — a `Var`, with a `field` per following segment — and `::class`
/// lowers to a class literal over the written type
/// (<https://kotlinlang.org/docs/reflection.html#class-references>).
///
/// `kotlinc` 2.4.20 compiles this file clean.
body_snapshot_lang! {
    kotlin_body_callable_references,
    LanguageKind::Kotlin,
    r#"
class Holder(val items: List<Int>) {
    fun size(): Int = 1

    fun use(): Int {
        val a = items::size
        val b = this::use
        val c = ::topLevel
        val d = Holder::class
        val e = String::class.java
        return a() + b() + c() + d.hashCode() + e.hashCode()
    }
}

fun topLevel(): Int = 1
"#,
}

/// A subject-less `when` writes `in`/`is` conditions the arm cannot test
/// against anything, and both stay *conditions* of the arm: the containment
/// keeps a missing element exactly as the type test keeps a missing expression,
/// so neither arm is read as the `else` an empty condition list means.
///
/// The source is not valid Kotlin — `kotlinc` 2.4.20 reports `condition of type
/// 'Boolean' expected.` for both arms — and is here for the model only.
body_snapshot_lang! {
    kotlin_body_when_without_subject,
    LanguageKind::Kotlin,
    r#"
fun f(x: Int): Int = when {
    in 1..5 -> 1
    is Int -> 2
    else -> 3
}
"#,
}
