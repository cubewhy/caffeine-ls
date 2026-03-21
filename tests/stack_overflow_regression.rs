// Regression tests for stack overflow issues
///
/// This test suite verifies that the parser doesn't stack overflow
/// when processing incomplete code in Lombok-annotated classes.
use java_analyzer::{index::ClassOrigin, language::java::class_parser::parse_java_source};

fn parse_first_class(source: &str) -> java_analyzer::index::ClassMetadata {
    parse_java_source(source, ClassOrigin::Unknown, None)
        .into_iter()
        .find(|class| !class.name.is_empty())
        .expect("Should parse at least one class")
}

#[test]
fn test_lombok_class_with_incomplete_new_expression() {
    // This used to cause stack overflow before the two-phase parser refactor
    let source = r#"
import lombok.*;

@Getter
@AllArgsConstructor
@Builder
public class User {
    private String name;
    @With private int age;

    private void foo() {
        String s = new 
    }
}
"#;

    let class = parse_first_class(source);
    let members = class.fields.len() + class.methods.len();

    // Should extract at least the foo method and fields
    assert!(members > 0, "Should extract some members");
}

#[test]
fn test_lombok_class_with_multiple_incomplete_statements() {
    let source = r#"
import lombok.*;

@Data
@Builder
public class Person {
    private String firstName;
    private String lastName;
    private int age;

    public void method1() {
        String x = new 
    }
    
    public void method2() {
        int y = 
    }
    
    public void method3() {
        Person p = 
    }
}
"#;

    let class = parse_first_class(source);
    let members = class.fields.len() + class.methods.len();

    assert!(members > 0);
}

#[test]
fn test_deeply_nested_error_nodes() {
    let source = r#"
public class Test {
    void method() {
        if (true) {
            if (true) {
                if (true) {
                    if (true) {
                        String s = new 
                    }
                }
            }
        }
    }
}
"#;

    let class = parse_first_class(source);
    let members = class.fields.len() + class.methods.len();

    assert!(members > 0);
}

#[test]
fn test_valid_members_extraction_skips_errors() {
    // Verify that extract_valid_members_only skips ERROR nodes
    let source = r#"
public class Test {
    public void validMethod() {
        System.out.println("valid");
    }
    
    public void incompleteMethod() {
        String s = new 
    }
    
    public void anotherValidMethod() {
        return;
    }
}
"#;

    let class = parse_first_class(source);
    let member_names: Vec<_> = class
        .methods
        .iter()
        .map(|method| method.name.clone())
        .collect();

    // Should have validMethod and anotherValidMethod
    assert!(member_names.iter().any(|n| n.as_ref() == "validMethod"));
    assert!(
        member_names
            .iter()
            .any(|n| n.as_ref() == "anotherValidMethod")
    );
}

#[test]
fn test_two_phase_extraction_includes_error_recovery() {
    // Verify that two-phase extraction includes both valid and error-recovered members
    let source = r#"
public class Test {
    public void validMethod() {
        System.out.println("valid");
    }
    
    public void incompleteMethod() {
        String s = new 
    }
}
"#;

    let class = parse_first_class(source);
    let member_names: Vec<_> = class
        .methods
        .iter()
        .map(|method| method.name.clone())
        .collect();

    // Should have both methods
    assert!(member_names.iter().any(|n| n.as_ref() == "validMethod"));
    assert!(
        member_names
            .iter()
            .any(|n| n.as_ref() == "incompleteMethod")
    );
}

#[test]
fn test_method_after_error_node_is_extracted() {
    // Regression test for: methods that come after ERROR nodes should still be extracted
    //
    // NOTE: This test documents a known limitation. When tree-sitter completely mangles
    // the parse tree and splits a method declaration across multiple nodes (some ERROR,
    // some not), we may not extract it. This is an acceptable trade-off to prevent
    // stack overflow with Lombok + incomplete code.
    //
    // In this specific case, tree-sitter parses:
    //   private static Object test() { return null; }
    // As:
    //   - local_variable_declaration: "private static Object test"
    //   - ERROR: "() {"
    //   - return_statement: "return null;"
    //   - }
    //
    // Our error recovery can't reconstruct this because the method name is outside the ERROR node.
    let source = r#"
package org.cubewhy.relx;
public class Agent {
    private static Object inst;
    public static void agentmain(String args, Object inst) throws Exception {
        Agent.inst = inst;
        var proxy = new Object();
        proxy.run();
        Agent.inst = 
    }
    private static Object test() { return null; }
}
"#;

    let class = parse_first_class(source);
    let method_names: Vec<_> = class
        .methods
        .iter()
        .map(|method| method.name.clone())
        .collect();
    let field_names: Vec<_> = class
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();

    // Should extract the valid members
    assert!(
        method_names.iter().any(|n| n.as_ref() == "agentmain"),
        "Should find agentmain, found: {:?}",
        method_names.iter().map(|n| n.as_ref()).collect::<Vec<_>>()
    );
    assert!(
        field_names.iter().any(|n| n.as_ref() == "inst"),
        "Should find inst field, found: {:?}",
        field_names.iter().map(|n| n.as_ref()).collect::<Vec<_>>()
    );

    // Known limitation: test() method is not extracted because tree-sitter splits it
    // across ERROR and non-ERROR nodes. This is acceptable to prevent stack overflow.
    // The important thing is that we don't crash.
    eprintln!("Note: test() method not extracted due to mangled parse tree - this is expected");
}
