mod common;

use common::script_snapshot;
use indoc::indoc;

// `.kts` scripts are parsed with the KLS `script` grammar
// ([spec: grammar-rule-script]): a statement list instead of top-level
// declarations only.

script_snapshot!(
    script_hello_world,
    indoc! {r#"
    #!/usr/bin/env kotlin
    println("Hello, World!")
"#}
);

script_snapshot!(
    script_imports_and_variables,
    indoc! {r#"
    import kotlin.math.sqrt

    val x = 1
    val y = 2
    var total = x + y
    total += 1
    println(total)
"#}
);

script_snapshot!(
    script_with_function,
    indoc! {r#"
    fun factorial(n: Int): Int = if (n <= 1) 1 else n * factorial(n - 1)

    val result = factorial(5)
    println("5! = $result")
"#}
);

script_snapshot!(
    script_control_flow,
    indoc! {r#"
    for (i in 1..10) {
        if (i % 2 == 0) {
            println("$i is even")
        }
    }

    var n = 0
    while (n < 3) {
        n++
    }

    run {
        println("done")
    }
"#}
);

script_snapshot!(
    script_semicolons,
    indoc! {r#"
    val a = 1; val b = 2; println(a + b)
"#}
);

script_snapshot!(
    script_with_class,
    indoc! {r#"
    class Greeter(val name: String) {
        fun greet() = "Hello, $name!"
    }

    val g = Greeter("Kotlin")
    println(g.greet())
"#}
);

script_snapshot!(script_empty, "");

script_snapshot!(
    script_recovery,
    indoc! {r#"
    val broken = ;
    println("still alive")
"#}
);

#[test]
fn source_file_parse_script() {
    let parse = kotlin_syntax::SourceFile::parse_script("println(1)\nval x = 2\n");
    assert!(
        parse.errors().is_empty(),
        "expected no errors: {:?}",
        parse.errors()
    );
    assert_eq!(
        parse.into_syntax_node().text().to_string(),
        "println(1)\nval x = 2\n"
    );
}

#[test]
fn source_file_parse_script_errors() {
    let parse = kotlin_syntax::SourceFile::parse_script("val = ;\n");
    assert!(!parse.errors().is_empty(), "expected parse errors");
}
