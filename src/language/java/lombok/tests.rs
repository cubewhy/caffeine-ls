//! Integration tests for Lombok support
//!
//! These tests verify that Lombok annotations are properly processed during
//! Java source parsing and that synthetic members are correctly generated.

use crate::index::ClassOrigin;
use crate::language::java::class_parser::parse_java_source;
use rust_asm::constants::{ACC_PUBLIC, ACC_STATIC};

/// Helper function to parse Java source and return the first class
fn parse_first_class(src: &str) -> crate::index::ClassMetadata {
    let classes = parse_java_source(src, ClassOrigin::Unknown, None);
    assert_eq!(classes.len(), 1, "Expected exactly one class");
    classes.into_iter().next().unwrap()
}

mod getter_tests {
    use super::*;

    #[test]
    fn test_exact_user_example() {
        let src = indoc::indoc! {"
            import lombok.Getter;
            import lombok.Setter;

            @Getter
            @Setter
            public class MyConfig {
                private String randomStringField = \"Hello\";
            }
        "};

        let class = parse_first_class(src);

        println!("Class: {}", class.internal_name);
        println!(
            "Fields: {:?}",
            class
                .fields
                .iter()
                .map(|f| f.name.as_ref())
                .collect::<Vec<_>>()
        );
        println!(
            "Methods: {:?}",
            class
                .methods
                .iter()
                .map(|m| m.name.as_ref())
                .collect::<Vec<_>>()
        );

        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getRandomStringField"),
            "Should generate getRandomStringField() method"
        );
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setRandomStringField"),
            "Should generate setRandomStringField(String) method"
        );
    }

    #[test]
    fn field_level_getter_generates_method() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Main {
                @Getter
                private String name;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getName"),
            "Should generate getName() method"
        );
    }

    #[test]
    fn class_level_getter_generates_methods_for_all_fields() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            @Getter
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getName"),
            "Should generate getName() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getAge"),
            "Should generate getAge() method"
        );
    }

    #[test]
    fn boolean_field_uses_is_prefix() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Main {
                @Getter
                private boolean active;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isActive"),
            "Boolean field should generate isActive() method"
        );
    }

    #[test]
    fn getter_is_public_by_default() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Person {
                @Getter
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getName")
            .expect("getName() should be generated");

        assert_eq!(
            getter.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "getName() should be public"
        );
    }

    #[test]
    fn static_field_with_field_level_getter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class MyConfig {
                @Getter
                private static final String randomStringField = \"Hello\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static getter
        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getRandomStringField");

        assert!(
            getter.is_some(),
            "Should generate getter for static field with field-level @Getter"
        );

        let method = getter.unwrap();

        // Verify it's static
        assert_eq!(
            method.access_flags & rust_asm::constants::ACC_STATIC,
            rust_asm::constants::ACC_STATIC,
            "Getter for static field should be static"
        );

        // Verify it's public
        assert_eq!(
            method.access_flags & rust_asm::constants::ACC_PUBLIC,
            rust_asm::constants::ACC_PUBLIC,
            "Getter should be public"
        );

        // Verify return type (accept both qualified and unqualified forms)
        let return_type = method.return_type.as_ref().map(|t| t.as_ref());
        assert!(
            return_type == Some("Ljava/lang/String;") || return_type == Some("LString;"),
            "Should return String, got: {:?}",
            return_type
        );
    }

    #[test]
    fn static_field_skipped_with_class_level_getter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            @Getter
            public class MyConfig {
                private String instanceField;
                private static String staticField = \"Hello\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate getter for instance field
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getInstanceField"),
            "Should generate getter for instance field"
        );

        // Should NOT generate getter for static field
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getStaticField"),
            "Should NOT generate getter for static field with class-level @Getter"
        );
    }

    #[test]
    fn static_final_field_with_getter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Constants {
                @Getter
                private static final int MAX_SIZE = 100;

                @Getter
                private static final String APP_NAME = \"MyApp\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static getters for both fields
        // Note: Lombok preserves the exact field name for all-caps constants
        let max_size_getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getMAX_SIZE");

        let app_name_getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getAPP_NAME");

        assert!(max_size_getter.is_some(), "Should generate getMAX_SIZE()");
        assert!(app_name_getter.is_some(), "Should generate getAPP_NAME()");

        // Verify both are static
        assert_eq!(
            max_size_getter.unwrap().access_flags & ACC_STATIC,
            ACC_STATIC,
            "getMAX_SIZE() should be static"
        );

        assert_eq!(
            app_name_getter.unwrap().access_flags & ACC_STATIC,
            ACC_STATIC,
            "getAPP_NAME() should be static"
        );
    }

    #[test]
    fn getter_has_correct_return_type() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Main {
                @Getter
                private String name;
            }
        "};

        let class = parse_first_class(src);
        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getName")
            .expect("getName() should exist");

        assert!(
            getter.return_type.is_some(),
            "Getter should have a return type"
        );
    }

    #[test]
    fn class_level_getter_with_multiple_field_types() {
        let src = indoc::indoc! {"
            import lombok.Getter;

            @Getter
            public class ComplexClass {
                private String name;
                private int age;
                private boolean active;
                private double salary;
                private long timestamp;
            }
        "};

        let class = parse_first_class(src);

        // Verify all getters are generated
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getName"),
            "Should generate getName()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getAge"),
            "Should generate getAge()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isActive"),
            "Should generate isActive() for boolean"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getSalary"),
            "Should generate getSalary()"
        );
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getTimestamp"),
            "Should generate getTimestamp()"
        );
    }

    #[test]
    fn class_level_getter_setter_combined() {
        let src = indoc::indoc! {"
            import lombok.Getter;
            import lombok.Setter;

            @Getter
            @Setter
            public class Person {
                private String firstName;
                private String lastName;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Verify all getters
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getFirstName"),
            "Should generate getFirstName()"
        );
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getLastName"),
            "Should generate getLastName()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getAge"),
            "Should generate getAge()"
        );

        // Verify all setters
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setFirstName"),
            "Should generate setFirstName()"
        );
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setLastName"),
            "Should generate setLastName()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setAge"),
            "Should generate setAge()"
        );
    }
}

mod setter_tests {
    use super::*;

    #[test]
    fn class_level_setter_generates_methods_for_all_fields() {
        let src = indoc::indoc! {"
            import lombok.Setter;

            @Setter
            public class Person {
                private String name;
                private int age;
                private boolean active;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setName"),
            "Should generate setName() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setAge"),
            "Should generate setAge() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setActive"),
            "Should generate setActive() method"
        );
    }

    #[test]
    fn field_level_setter_generates_method() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class Main {
                @Setter
                private String name;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setName"),
            "Should generate setName() method"
        );
    }

    #[test]
    fn setter_has_one_parameter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class Main {
                @Setter
                private String name;
            }
        "};

        let class = parse_first_class(src);
        let setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setName")
            .expect("setName() should exist");

        assert_eq!(
            setter.params.items.len(),
            1,
            "Setter should have exactly one parameter"
        );
    }

    #[test]
    fn setter_not_generated_for_final_field() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class Main {
                @Setter
                private final String name = \"John\";
            }
        "};

        let class = parse_first_class(src);

        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "setName"),
            "Setter should not be generated for final field"
        );
    }

    #[test]
    fn class_level_setter_skips_final_fields() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            @Setter
            public class Person {
                private String name;
                private final int age = 25;
            }
        "};

        let class = parse_first_class(src);

        // Should generate setter for name
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setName"),
            "Should generate setName() for non-final field"
        );

        // Should NOT generate setter for age (final)
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "setAge"),
            "Should NOT generate setAge() for final field"
        );
    }

    #[test]
    fn static_field_with_field_level_setter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class Config {
                @Setter
                private static String configValue = \"default\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static setter
        let setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setConfigValue");

        assert!(
            setter.is_some(),
            "Should generate setConfigValue() for static field with field-level @Setter"
        );

        let method = setter.unwrap();

        // Verify it's static
        assert_eq!(
            method.access_flags & ACC_STATIC,
            ACC_STATIC,
            "setConfigValue() should be static"
        );

        // Verify it's public
        assert_eq!(
            method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "setConfigValue() should be public"
        );

        // Verify it has one parameter
        assert_eq!(method.params.len(), 1, "Setter should have one parameter");
    }

    #[test]
    fn static_field_skipped_with_class_level_setter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            @Setter
            public class Config {
                private String instanceField;
                private static String staticField = \"default\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate setter for instance field
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setInstanceField"),
            "Should generate setter for instance field"
        );

        // Should NOT generate setter for static field
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setStaticField"),
            "Should NOT generate setter for static field with class-level @Setter"
        );
    }

    #[test]
    fn static_final_field_no_setter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class Constants {
                @Setter
                private static final String CONSTANT = \"value\";
            }
        "};

        let class = parse_first_class(src);

        // Should NOT generate setter for static final field
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setCONSTANT"),
            "Should NOT generate setter for static final field"
        );
    }
}

mod annotation_resolution_tests {
    use super::*;

    #[test]
    fn simple_annotation_name_is_resolved() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Main {
                @Getter
                private String a;
            }
        "};

        let class = parse_first_class(src);

        // Verify field has annotation
        assert_eq!(class.fields.len(), 1, "Should have one field");
        assert_eq!(
            class.fields[0].annotations.len(),
            1,
            "Field should have one annotation"
        );

        // Verify getter was generated
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getA"),
            "Getter should be generated from simple annotation name"
        );
    }

    #[test]
    fn qualified_annotation_name_works() {
        let src = indoc::indoc! {"
            package org.example;

            public class Main {
                @lombok.Getter
                private String a;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getA"),
            "Getter should be generated from qualified annotation name"
        );
    }
}

mod user_reported_issues {
    use super::*;

    #[test]
    fn original_user_example() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class Main {
                @Getter
                private String a;
            }
        "};

        let class = parse_first_class(src);

        // Verify getA() is generated
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getA"),
            "getA() should be generated"
        );

        // Verify it's accessible (public)
        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getA")
            .unwrap();
        assert_eq!(
            getter.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "getA() should be public"
        );
    }

    #[test]
    fn static_field_getter_issue() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;

            public class MyConfig {
                @Getter
                private static final String randomStringField = \"Hello\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static getter
        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getRandomStringField");

        assert!(
            getter.is_some(),
            "Should generate getRandomStringField() for static field with @Getter"
        );

        let method = getter.unwrap();

        // Verify it's static
        assert_eq!(
            method.access_flags & ACC_STATIC,
            ACC_STATIC,
            "getRandomStringField() should be static"
        );

        // Verify it's public
        assert_eq!(
            method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "getRandomStringField() should be public"
        );
    }

    #[test]
    fn static_field_setter_issue() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Setter;

            public class MyConfig {
                @Setter
                private static String configValue = \"default\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static setter
        let setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setConfigValue");

        assert!(
            setter.is_some(),
            "Should generate setConfigValue() for static field with @Setter"
        );

        let method = setter.unwrap();

        // Verify it's static
        assert_eq!(
            method.access_flags & ACC_STATIC,
            ACC_STATIC,
            "setConfigValue() should be static"
        );

        // Verify it's public
        assert_eq!(
            method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "setConfigValue() should be public"
        );
    }

    #[test]
    fn static_field_getter_and_setter() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.Getter;
            import lombok.Setter;

            public class MyConfig {
                @Getter
                @Setter
                private static String sharedConfig = \"default\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate static getter
        let getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getSharedConfig");

        assert!(getter.is_some(), "Should generate static getter");
        assert_eq!(
            getter.unwrap().access_flags & ACC_STATIC,
            ACC_STATIC,
            "Getter should be static"
        );

        // Should generate static setter
        let setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setSharedConfig");

        assert!(setter.is_some(), "Should generate static setter");
        assert_eq!(
            setter.unwrap().access_flags & ACC_STATIC,
            ACC_STATIC,
            "Setter should be static"
        );
    }
}

mod to_string_tests {
    use super::*;

    #[test]
    fn class_level_to_string_generates_method() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        let to_string = class.methods.iter().find(|m| m.name.as_ref() == "toString");

        assert!(to_string.is_some(), "Should generate toString() method");

        let method = to_string.unwrap();
        assert_eq!(
            method.return_type.as_ref().map(|t| t.as_ref()),
            Some("java/lang/String"),
            "toString() should return String"
        );
        assert!(
            method.params.is_empty(),
            "toString() should have no parameters"
        );
    }

    #[test]
    fn to_string_with_exclude() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString(exclude = \"password\")
            public class User {
                private String username;
                private String password;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "toString"),
            "Should generate toString() method"
        );
    }

    #[test]
    fn to_string_with_of() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString(of = {\"name\", \"email\"})
            public class User {
                private String name;
                private String email;
                private String password;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "toString"),
            "Should generate toString() method"
        );
    }

    #[test]
    fn to_string_does_not_override_existing() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString
            public class Person {
                private String name;

                @Override
                public String toString() {
                    return \"Custom: \" + name;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have exactly one toString method (the explicit one)
        let to_string_count = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "toString")
            .count();

        assert_eq!(
            to_string_count, 1,
            "Should not generate toString() when it already exists"
        );
    }

    #[test]
    fn to_string_skips_static_fields() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString
            public class Config {
                private String name;
                private static String DEFAULT_NAME = \"default\";
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "toString"),
            "Should generate toString() method"
        );
    }

    #[test]
    fn to_string_with_call_super() {
        let src = indoc::indoc! {"
            package org.example;
            
            import lombok.ToString;
            
            @ToString(callSuper = true)
            public class Employee extends Person {
                private String employeeId;
            }
        "};

        let class = parse_first_class(src);

        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "toString"),
            "Should generate toString() method with callSuper"
        );
    }

    #[test]
    fn to_string_is_public() {
        let src = indoc::indoc! {"
            package org.example;

            import lombok.ToString;

            @ToString
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let to_string = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "toString")
            .expect("toString() should be generated");

        assert_eq!(
            to_string.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "toString() should be public"
        );
    }

    #[test]
    fn to_string_comprehensive_example() {
        let src = indoc::indoc! {"
            package com.example;

            import lombok.ToString;

            @ToString(exclude = {\"password\", \"internalId\"})
            public class User {
                private String username;
                private String email;
                private String password;
                private long internalId;
                private boolean active;
                private static String DEFAULT_ROLE = \"user\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate toString()
        let to_string = class.methods.iter().find(|m| m.name.as_ref() == "toString");

        assert!(to_string.is_some(), "Should generate toString() method");

        let method = to_string.unwrap();

        // Verify signature
        assert_eq!(
            method.return_type.as_ref().map(|t| t.as_ref()),
            Some("java/lang/String"),
            "toString() should return String"
        );
        assert!(
            method.params.is_empty(),
            "toString() should have no parameters"
        );
        assert_eq!(
            method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "toString() should be public"
        );
    }
}

mod equals_hash_code_tests {
    use super::*;

    #[test]
    fn class_level_equals_and_hash_code_generates_methods() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should generate equals()
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method"
        );

        // Should generate hashCode()
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method"
        );

        // Should generate canEqual()
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "canEqual"),
            "Should generate canEqual() method"
        );
    }

    #[test]
    fn equals_method_has_correct_signature() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let equals = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "equals")
            .expect("equals() should be generated");

        // Check return type
        assert_eq!(
            equals.return_type.as_ref().map(|t| t.as_ref()),
            Some("Z"),
            "equals() should return boolean"
        );

        // Check parameters
        assert_eq!(equals.params.len(), 1, "equals() should have one parameter");
        assert_eq!(
            equals.params.items[0].descriptor.as_ref(),
            "Ljava/lang/Object;",
            "equals() parameter should be Object"
        );

        // Check access flags
        assert_eq!(
            equals.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "equals() should be public"
        );
    }

    #[test]
    fn hash_code_method_has_correct_signature() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let hash_code = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "hashCode")
            .expect("hashCode() should be generated");

        // Check return type
        assert_eq!(
            hash_code.return_type.as_ref().map(|t| t.as_ref()),
            Some("I"),
            "hashCode() should return int"
        );

        // Check parameters
        assert!(
            hash_code.params.is_empty(),
            "hashCode() should have no parameters"
        );

        // Check access flags
        assert_eq!(
            hash_code.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "hashCode() should be public"
        );
    }

    #[test]
    fn can_equal_method_has_correct_signature() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let can_equal = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "canEqual")
            .expect("canEqual() should be generated");

        // Check return type
        assert_eq!(
            can_equal.return_type.as_ref().map(|t| t.as_ref()),
            Some("Z"),
            "canEqual() should return boolean"
        );

        // Check parameters
        assert_eq!(
            can_equal.params.len(),
            1,
            "canEqual() should have one parameter"
        );

        // Check access flags (should be protected)
        assert_eq!(
            can_equal.access_flags & rust_asm::constants::ACC_PROTECTED,
            rust_asm::constants::ACC_PROTECTED,
            "canEqual() should be protected"
        );
    }

    #[test]
    fn equals_and_hash_code_with_exclude() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode(exclude = {\"password\", \"internalId\"})
            public class User {
                private String username;
                private String password;
                private long internalId;
            }
        "};

        let class = parse_first_class(src);

        // Should still generate all three methods
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "canEqual"),
            "Should generate canEqual() method"
        );
    }

    #[test]
    fn equals_and_hash_code_with_of() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode(of = {\"id\", \"email\"})
            public class User {
                private long id;
                private String email;
                private String name;
                private String password;
            }
        "};

        let class = parse_first_class(src);

        // Should generate all three methods
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "canEqual"),
            "Should generate canEqual() method"
        );
    }

    #[test]
    fn equals_and_hash_code_skips_static_fields() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Config {
                private String instanceField;
                private static String staticField = \"default\";
            }
        "};

        let class = parse_first_class(src);

        // Should generate methods (static fields are automatically excluded)
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method"
        );
    }

    #[test]
    fn equals_and_hash_code_skips_transient_fields() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class CachedObject {
                private String data;
                private transient String cachedValue;
            }
        "};

        let class = parse_first_class(src);

        // Should generate methods (transient fields are automatically excluded)
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method"
        );
    }

    #[test]
    fn equals_and_hash_code_with_call_super() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode(callSuper = true)
            public class Employee extends Person {
                private String employeeId;
                private String department;
            }
        "};

        let class = parse_first_class(src);

        // Should generate all methods even with callSuper=true
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals() method with callSuper"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode() method with callSuper"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "canEqual"),
            "Should generate canEqual() method with callSuper"
        );
    }

    #[test]
    fn equals_and_hash_code_does_not_override_existing_equals() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;

                @Override
                public boolean equals(Object other) {
                    return false;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have exactly one equals() method (the explicit one)
        let equals_count = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "equals")
            .count();
        assert_eq!(
            equals_count, 1,
            "Should not generate equals() when it already exists"
        );

        // Should not generate hashCode() either (they must be in sync)
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should not generate hashCode() when equals() already exists"
        );
    }

    #[test]
    fn equals_and_hash_code_does_not_override_existing_hash_code() {
        let src = indoc::indoc! {"
            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode
            public class Person {
                private String name;

                @Override
                public int hashCode() {
                    return 42;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have exactly one hashCode() method (the explicit one)
        let hash_code_count = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "hashCode")
            .count();
        assert_eq!(
            hash_code_count, 1,
            "Should not generate hashCode() when it already exists"
        );

        // Should not generate equals() either (they must be in sync)
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should not generate equals() when hashCode() already exists"
        );
    }

    #[test]
    fn equals_and_hash_code_comprehensive_example() {
        let src = indoc::indoc! {"
            package com.example;

            import lombok.EqualsAndHashCode;

            @EqualsAndHashCode(exclude = {\"password\", \"lastLogin\"})
            public class User {
                private long id;
                private String username;
                private String email;
                private String password;
                private java.util.Date lastLogin;
                private boolean active;
                private static String DEFAULT_ROLE = \"user\";
                private transient String sessionToken;
            }
        "};

        let class = parse_first_class(src);

        // Should generate equals()
        let equals = class.methods.iter().find(|m| m.name.as_ref() == "equals");
        assert!(equals.is_some(), "Should generate equals() method");

        let equals_method = equals.unwrap();
        assert_eq!(
            equals_method.return_type.as_ref().map(|t| t.as_ref()),
            Some("Z"),
            "equals() should return boolean"
        );
        assert_eq!(
            equals_method.params.len(),
            1,
            "equals() should have one parameter"
        );

        // Should generate hashCode()
        let hash_code = class.methods.iter().find(|m| m.name.as_ref() == "hashCode");
        assert!(hash_code.is_some(), "Should generate hashCode() method");

        let hash_code_method = hash_code.unwrap();
        assert_eq!(
            hash_code_method.return_type.as_ref().map(|t| t.as_ref()),
            Some("I"),
            "hashCode() should return int"
        );
        assert!(
            hash_code_method.params.is_empty(),
            "hashCode() should have no parameters"
        );

        // Should generate canEqual()
        let can_equal = class.methods.iter().find(|m| m.name.as_ref() == "canEqual");
        assert!(can_equal.is_some(), "Should generate canEqual() method");
    }
}

mod constructor_tests {
    use super::*;

    #[test]
    fn test_no_args_constructor_basic() {
        let src = indoc::indoc! {"
            import lombok.NoArgsConstructor;

            @NoArgsConstructor
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have <init> constructor with no parameters
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());

        assert!(constructor.is_some(), "Should generate no-args constructor");

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 0);
        assert!(
            (ctor.access_flags & rust_asm::constants::ACC_PUBLIC) != 0,
            "Constructor should be public"
        );
    }

    #[test]
    fn test_no_args_constructor_with_access_level() {
        let src = indoc::indoc! {"
            import lombok.NoArgsConstructor;
            import lombok.AccessLevel;

            @NoArgsConstructor(access = AccessLevel.PROTECTED)
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());

        assert!(constructor.is_some(), "Should generate constructor");

        let ctor = constructor.unwrap();
        assert!(
            (ctor.access_flags & rust_asm::constants::ACC_PROTECTED) != 0,
            "Constructor should be protected"
        );
    }

    #[test]
    fn test_no_args_constructor_with_static_name() {
        let src = indoc::indoc! {"
            import lombok.NoArgsConstructor;

            @NoArgsConstructor(staticName = \"of\")
            public class Person {
                private String name;
            }
        "};

        let class = parse_first_class(src);

        // Should have the constructor
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());
        assert!(constructor.is_some(), "Should generate constructor");

        // Should have static factory method
        let factory = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "of" && m.params.is_empty());
        assert!(factory.is_some(), "Should generate static factory method");

        let factory_method = factory.unwrap();
        assert!(
            (factory_method.access_flags & rust_asm::constants::ACC_STATIC) != 0,
            "Factory method should be static"
        );
        assert!(
            (factory_method.access_flags & rust_asm::constants::ACC_PUBLIC) != 0,
            "Factory method should be public"
        );
    }

    #[test]
    fn test_all_args_constructor_basic() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;

            @AllArgsConstructor
            public class Person {
                private String name;
                private int age;
                private boolean active;
            }
        "};

        let class = parse_first_class(src);

        // Should have <init> constructor with 3 parameters
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 3);

        assert!(
            constructor.is_some(),
            "Should generate all-args constructor"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 3);

        // Check parameter types - descriptors come from field descriptors which may be simplified
        // The parser generates descriptors based on source type resolution
        assert!(
            ctor.params.items[0].descriptor.as_ref().contains("String"),
            "First param should be String type, got: {}",
            ctor.params.items[0].descriptor
        );
        assert_eq!(ctor.params.items[1].descriptor.as_ref(), "I");
        assert_eq!(ctor.params.items[2].descriptor.as_ref(), "Z");

        // Check parameter names
        assert_eq!(ctor.params.items[0].name.as_ref(), "name");
        assert_eq!(ctor.params.items[1].name.as_ref(), "age");
        assert_eq!(ctor.params.items[2].name.as_ref(), "active");
    }

    #[test]
    fn test_all_args_constructor_skips_static_fields() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;

            @AllArgsConstructor
            public class Person {
                private static final String DEFAULT_NAME = \"Unknown\";
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor with only 2 parameters (skipping static field)
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);

        assert!(
            constructor.is_some(),
            "Should generate constructor with 2 params"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 2);
        assert_eq!(ctor.params.items[0].name.as_ref(), "name");
        assert_eq!(ctor.params.items[1].name.as_ref(), "age");
    }

    #[test]
    fn test_all_args_constructor_with_static_name() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;

            @AllArgsConstructor(staticName = \"create\")
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have the constructor
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);
        assert!(constructor.is_some(), "Should generate constructor");

        // Should have static factory method
        let factory = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "create" && m.params.len() == 2);
        assert!(factory.is_some(), "Should generate static factory method");

        let factory_method = factory.unwrap();
        assert!(
            (factory_method.access_flags & rust_asm::constants::ACC_STATIC) != 0,
            "Factory method should be static"
        );
    }

    #[test]
    fn test_required_args_constructor_final_fields() {
        let src = indoc::indoc! {"
            import lombok.RequiredArgsConstructor;

            @RequiredArgsConstructor
            public class Person {
                private final String name;
                private final int age;
                private String nickname;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor with 2 parameters (only final fields)
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);

        assert!(
            constructor.is_some(),
            "Should generate required-args constructor"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 2);
        assert_eq!(ctor.params.items[0].name.as_ref(), "name");
        assert_eq!(ctor.params.items[1].name.as_ref(), "age");
    }

    #[test]
    fn test_required_args_constructor_nonnull_fields() {
        let src = indoc::indoc! {"
            import lombok.RequiredArgsConstructor;
            import lombok.NonNull;

            @RequiredArgsConstructor
            public class Person {
                @NonNull
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor with 1 parameter (@NonNull field)
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 1);

        assert!(
            constructor.is_some(),
            "Should generate required-args constructor"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 1);
        assert_eq!(ctor.params.items[0].name.as_ref(), "name");
    }

    #[test]
    fn test_required_args_constructor_mixed() {
        let src = indoc::indoc! {"
            import lombok.RequiredArgsConstructor;
            import lombok.NonNull;

            @RequiredArgsConstructor
            public class Person {
                private final String id;
                @NonNull
                private String name;
                private int age;
                private final boolean active;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor with 3 parameters (2 final + 1 @NonNull)
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 3);

        assert!(
            constructor.is_some(),
            "Should generate required-args constructor"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 3);
        // Should be in field declaration order
        assert_eq!(ctor.params.items[0].name.as_ref(), "id");
        assert_eq!(ctor.params.items[1].name.as_ref(), "name");
        assert_eq!(ctor.params.items[2].name.as_ref(), "active");
    }

    #[test]
    fn test_required_args_constructor_no_required_fields() {
        let src = indoc::indoc! {"
            import lombok.RequiredArgsConstructor;

            @RequiredArgsConstructor
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have no-args constructor when no required fields
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());

        assert!(
            constructor.is_some(),
            "Should generate no-args constructor when no required fields"
        );
    }

    #[test]
    fn test_multiple_constructor_annotations() {
        let src = indoc::indoc! {"
            import lombok.NoArgsConstructor;
            import lombok.AllArgsConstructor;

            @NoArgsConstructor
            @AllArgsConstructor
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have both constructors
        let no_args = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());
        assert!(no_args.is_some(), "Should generate no-args constructor");

        let all_args = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);
        assert!(all_args.is_some(), "Should generate all-args constructor");
    }

    #[test]
    fn test_constructor_does_not_override_explicit() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;

            @AllArgsConstructor
            public class Person {
                private String name;
                private int age;

                public Person(String name, int age) {
                    this.name = name.toUpperCase();
                    this.age = age;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should only have one constructor (the explicit one)
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();

        assert_eq!(
            constructors.len(),
            1,
            "Should not generate duplicate constructor"
        );
    }

    #[test]
    fn test_constructor_with_generics() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;
            import java.util.List;

            @AllArgsConstructor
            public class Container<T> {
                private T value;
                private List<T> items;
            }
        "};

        let class = parse_first_class(src);

        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);

        assert!(
            constructor.is_some(),
            "Should generate constructor with generic types"
        );

        let ctor = constructor.unwrap();
        assert_eq!(ctor.params.len(), 2);
        assert_eq!(ctor.params.items[0].name.as_ref(), "value");
        assert_eq!(ctor.params.items[1].name.as_ref(), "items");
    }

    #[test]
    fn test_enum_constructor_is_private() {
        let src = indoc::indoc! {"
            import lombok.AllArgsConstructor;

            @AllArgsConstructor
            public enum Status {
                ACTIVE, INACTIVE;

                private String description;
            }
        "};

        let class = parse_first_class(src);

        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 1);

        assert!(
            constructor.is_some(),
            "Should generate constructor for enum"
        );

        let ctor = constructor.unwrap();
        assert!(
            (ctor.access_flags & rust_asm::constants::ACC_PRIVATE) != 0,
            "Enum constructor should be private"
        );
    }

    #[test]
    fn test_required_args_with_static_name() {
        let src = indoc::indoc! {"
            import lombok.RequiredArgsConstructor;

            @RequiredArgsConstructor(staticName = \"of\")
            public class Person {
                private final String name;
                private final int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have the constructor
        let constructor = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 2);
        assert!(constructor.is_some(), "Should generate constructor");

        // Should have static factory method
        let factory = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "of" && m.params.len() == 2);
        assert!(
            factory.is_some(),
            "Should generate static factory method 'of'"
        );

        let factory_method = factory.unwrap();
        assert!(
            (factory_method.access_flags & rust_asm::constants::ACC_STATIC) != 0,
            "Factory method should be static"
        );
        assert_eq!(factory_method.params.len(), 2);
    }

    #[test]
    fn test_all_three_constructor_annotations() {
        let src = indoc::indoc! {"
            import lombok.NoArgsConstructor;
            import lombok.RequiredArgsConstructor;
            import lombok.AllArgsConstructor;

            @NoArgsConstructor(force = true)
            @RequiredArgsConstructor
            @AllArgsConstructor
            public class Person {
                private final String id;
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have all three constructors
        let no_args = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.is_empty());
        assert!(
            no_args.is_some(),
            "Should generate no-args constructor with force=true"
        );

        let required_args = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 1);
        assert!(
            required_args.is_some(),
            "Should generate required-args constructor"
        );

        let all_args = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "<init>" && m.params.len() == 3);
        assert!(all_args.is_some(), "Should generate all-args constructor");
    }
}
#[cfg(test)]
mod manual_test {
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source;

    #[test]
    fn test_lombok_constructors_manual() {
        let src = r#"
import lombok.NoArgsConstructor;
import lombok.RequiredArgsConstructor;
import lombok.AllArgsConstructor;
import lombok.NonNull;

@NoArgsConstructor
@AllArgsConstructor
class Person {
    private String name;
    private int age;
}

@RequiredArgsConstructor
class User {
    private final String id;
    @NonNull
    private String username;
    private String email;
}
        "#;

        let classes = parse_java_source(src, ClassOrigin::Unknown, None);

        println!("\n=== Parsed {} classes ===", classes.len());

        for class in &classes {
            println!("\nClass: {}", class.name);

            let constructors: Vec<_> = class
                .methods
                .iter()
                .filter(|m| m.name.as_ref() == "<init>")
                .collect();

            println!("  Constructors: {}", constructors.len());
            for ctor in constructors {
                print!("    <init>(");
                for (i, param) in ctor.params.items.iter().enumerate() {
                    if i > 0 {
                        print!(", ");
                    }
                    print!("{}: {}", param.name, param.descriptor);
                }
                println!(")");
            }
        }

        // Verify Person has 2 constructors
        let person = classes
            .iter()
            .find(|c| c.name.as_ref() == "Person")
            .unwrap();
        let person_ctors: Vec<_> = person
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(person_ctors.len(), 2, "Person should have 2 constructors");

        // Verify User has 1 constructor with 2 params
        let user = classes.iter().find(|c| c.name.as_ref() == "User").unwrap();
        let user_ctors: Vec<_> = user
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(user_ctors.len(), 1, "User should have 1 constructor");
        assert_eq!(
            user_ctors[0].params.len(),
            2,
            "User constructor should have 2 params"
        );
    }
}

// ============================================================================
// @Data Tests
// ============================================================================

mod data_tests {
    use super::*;

    #[test]
    fn test_data_generates_all_methods() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have getters for all fields
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getName"),
            "Should generate getName()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getAge"),
            "Should generate getAge()"
        );

        // Should have setters for all non-final fields
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setName"),
            "Should generate setName()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "setAge"),
            "Should generate setAge()"
        );

        // Should have toString, equals, hashCode
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "toString"),
            "Should generate toString()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "equals"),
            "Should generate equals()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "hashCode"),
            "Should generate hashCode()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "canEqual"),
            "Should generate canEqual()"
        );

        // Should have no-args constructor (no final fields)
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1, "Should have 1 constructor");
        assert_eq!(
            constructors[0].params.len(),
            0,
            "Constructor should have 0 params"
        );
    }

    #[test]
    fn test_data_respects_final_fields() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private final String id;
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have getters for all fields
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getId"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getAge"));

        // Should have setters only for non-final fields
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "setId"),
            "Should NOT generate setId() for final field"
        );
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setAge"));

        // Should have constructor for final field only
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(
            constructors[0].params.len(),
            1,
            "Constructor should have 1 param for final field"
        );
        assert_eq!(constructors[0].params.items[0].name.as_ref(), "id");
    }

    #[test]
    fn test_data_with_static_fields() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Config {
                private static final String VERSION = \"1.0\";
                private String name;
            }
        "};

        let class = parse_first_class(src);

        // Should NOT generate getter/setter for static field
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getVERSION"),
            "Should NOT generate getter for static field"
        );
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setVERSION"),
            "Should NOT generate setter for static field"
        );

        // Should generate getter/setter for instance field
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
    }

    #[test]
    fn test_data_with_field_level_getter_override() {
        let src = indoc::indoc! {"
            import lombok.Data;
            import lombok.Getter;
            import lombok.AccessLevel;
            
            @Data
            public class Person {
                @Getter(AccessLevel.NONE)
                private String password;
                private String name;
            }
        "};

        let class = parse_first_class(src);

        // Should NOT generate getter for password (AccessLevel.NONE)
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getPassword"),
            "Should NOT generate getter for field with AccessLevel.NONE"
        );

        // Should still generate setter for password
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setPassword"),
            "Should generate setter for password"
        );

        // Should generate getter/setter for name
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
    }

    #[test]
    fn test_data_with_field_level_setter_override() {
        let src = indoc::indoc! {"
            import lombok.Data;
            import lombok.Setter;
            import lombok.AccessLevel;
            
            @Data
            public class Person {
                @Setter(AccessLevel.PROTECTED)
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should generate protected setter for name
        let setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setName")
            .expect("Should generate setName()");

        assert_eq!(
            setter.access_flags & rust_asm::constants::ACC_PROTECTED,
            rust_asm::constants::ACC_PROTECTED,
            "setName() should be protected"
        );

        // Should generate public setter for age
        let age_setter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "setAge")
            .expect("Should generate setAge()");

        assert_eq!(
            age_setter.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "setAge() should be public"
        );
    }

    #[test]
    fn test_data_with_explicit_getter() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private String name;
                
                public String getName() {
                    return \"Custom: \" + name;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only one getName() method (the explicit one)
        let getters: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "getName")
            .collect();
        assert_eq!(
            getters.len(),
            1,
            "Should have only 1 getName() (explicit, not synthetic)"
        );

        // Should still generate setter
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
    }

    #[test]
    fn test_data_with_explicit_constructor() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private final String id;
                private String name;
                
                public Person(String id, String name, int extra) {
                    this.id = id;
                    this.name = name;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only the explicit constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(
            constructors.len(),
            1,
            "Should have only 1 constructor (explicit)"
        );
        assert_eq!(
            constructors[0].params.len(),
            3,
            "Constructor should have 3 params"
        );

        // Should still generate getters/setters
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getId"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
    }

    #[test]
    fn test_data_with_explicit_to_string() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private String name;
                
                @Override
                public String toString() {
                    return \"Person[\" + name + \"]\";
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only one toString() method (the explicit one)
        let to_strings: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "toString")
            .collect();
        assert_eq!(
            to_strings.len(),
            1,
            "Should have only 1 toString() (explicit)"
        );

        // Should still generate other methods
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));
    }

    #[test]
    fn test_data_required_args_constructor() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Person {
                private final String id;
                private final int version;
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor for final fields only
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(
            constructors[0].params.len(),
            2,
            "Constructor should have 2 params for final fields"
        );
        assert_eq!(constructors[0].params.items[0].name.as_ref(), "id");
        assert_eq!(constructors[0].params.items[1].name.as_ref(), "version");
    }

    #[test]
    fn test_data_with_boolean_fields() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Flags {
                private boolean active;
                private boolean enabled;
            }
        "};

        let class = parse_first_class(src);

        // Should generate isXxx() getters for boolean fields
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isActive"),
            "Should generate isActive() for boolean field"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isEnabled"),
            "Should generate isEnabled() for boolean field"
        );

        // Should generate setters
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setActive"));
        assert!(
            class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setEnabled")
        );
    }

    #[test]
    fn test_data_empty_class() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data
            public class Empty {
            }
        "};

        let class = parse_first_class(src);

        // Should have toString, equals, hashCode
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "toString"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));

        // Should have no-args constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].params.len(), 0);
    }

    #[test]
    fn test_data_with_static_constructor() {
        let src = indoc::indoc! {"
            import lombok.Data;
            
            @Data(staticConstructor = \"of\")
            public class Point {
                private final double x;
                private final double y;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].params.len(), 2);

        // Should have static factory method
        let of_method = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "of")
            .expect("Should generate static 'of' method");

        assert_eq!(
            of_method.access_flags & ACC_STATIC,
            ACC_STATIC,
            "'of' method should be static"
        );
        assert_eq!(
            of_method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "'of' method should be public"
        );
        assert_eq!(of_method.params.len(), 2, "'of' should have 2 params");
    }

    #[test]
    fn test_data_with_explicit_component_annotations() {
        let src = indoc::indoc! {"
            import lombok.Data;
            import lombok.ToString;
            
            @Data
            @ToString(includeFieldNames = false)
            public class Person {
                private String name;
                private int age;
            }
        "};

        let class = parse_first_class(src);

        // @Data should defer to explicit @ToString
        // We can't test the behavior difference, but we can verify toString exists
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "toString"));

        // Should still generate other methods
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "setName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));
    }
}

// ============================================================================
// @Value Tests
// ============================================================================

mod value_tests {
    use super::*;

    #[test]
    fn test_value_generates_getters_only() {
        let src = indoc::indoc! {"
            import lombok.Value;

            @Value
            public class Point {
                double x;
                double y;
            }
        "};

        let class = parse_first_class(src);

        // Should have getters
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getX"),
            "Should generate getX()"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "getY"),
            "Should generate getY()"
        );

        // Should NOT have setters (immutable)
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "setX"),
            "Should NOT generate setX() for @Value"
        );
        assert!(
            !class.methods.iter().any(|m| m.name.as_ref() == "setY"),
            "Should NOT generate setY() for @Value"
        );

        // Should have toString, equals, hashCode
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "toString"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));
    }

    #[test]
    fn test_value_all_args_constructor() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Person {
                String name;
                int age;
                String email;
            }
        "};

        let class = parse_first_class(src);

        // Should have all-args constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(
            constructors[0].params.len(),
            3,
            "Constructor should have all 3 fields"
        );
        assert_eq!(constructors[0].params.items[0].name.as_ref(), "name");
        assert_eq!(constructors[0].params.items[1].name.as_ref(), "age");
        assert_eq!(constructors[0].params.items[2].name.as_ref(), "email");
    }

    #[test]
    fn test_value_with_static_fields() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Config {
                static final String VERSION = \"1.0\";
                String name;
            }
        "};

        let class = parse_first_class(src);

        // Should NOT generate getter for static field
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "getVERSION"),
            "Should NOT generate getter for static field"
        );

        // Should generate getter for instance field
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));

        // Constructor should only include instance fields
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(
            constructors[0].params.len(),
            1,
            "Constructor should have 1 param (instance field only)"
        );
    }

    #[test]
    fn test_value_with_explicit_getter() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Person {
                String name;
                
                public String getName() {
                    return \"Mr. \" + name;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only one getName() method (the explicit one)
        let getters: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "getName")
            .collect();
        assert_eq!(getters.len(), 1, "Should have only 1 getName() (explicit)");

        // Should NOT have setter
        assert!(!class.methods.iter().any(|m| m.name.as_ref() == "setName"));
    }

    #[test]
    fn test_value_with_field_level_getter_override() {
        let src = indoc::indoc! {"
            import lombok.Value;
            import lombok.Getter;
            import lombok.AccessLevel;
            
            @Value
            public class Person {
                @Getter(AccessLevel.PACKAGE)
                String name;
                int age;
            }
        "};

        let class = parse_first_class(src);

        // Should generate package-private getter for name
        let name_getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getName")
            .expect("Should generate getName()");

        // Package-private means no access flags
        assert_eq!(
            name_getter.access_flags
                & (ACC_PUBLIC
                    | rust_asm::constants::ACC_PROTECTED
                    | rust_asm::constants::ACC_PRIVATE),
            0,
            "getName() should be package-private"
        );

        // Should generate public getter for age
        let age_getter = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "getAge")
            .expect("Should generate getAge()");

        assert_eq!(
            age_getter.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "getAge() should be public"
        );
    }

    #[test]
    fn test_value_empty_class() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Empty {
            }
        "};

        let class = parse_first_class(src);

        // Should have toString, equals, hashCode
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "toString"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));

        // Should have no-args constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].params.len(), 0);
    }

    #[test]
    fn test_value_with_static_constructor() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value(staticConstructor = \"of\")
            public class Point {
                double x;
                double y;
            }
        "};

        let class = parse_first_class(src);

        // Should have constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].params.len(), 2);

        // Should have static factory method
        let of_method = class
            .methods
            .iter()
            .find(|m| m.name.as_ref() == "of")
            .expect("Should generate static 'of' method");

        assert_eq!(
            of_method.access_flags & ACC_STATIC,
            ACC_STATIC,
            "'of' method should be static"
        );
        assert_eq!(
            of_method.access_flags & ACC_PUBLIC,
            ACC_PUBLIC,
            "'of' method should be public"
        );
        assert_eq!(of_method.params.len(), 2, "'of' should have 2 params");
    }

    #[test]
    fn test_value_with_boolean_fields() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Flags {
                boolean active;
                boolean enabled;
            }
        "};

        let class = parse_first_class(src);

        // Should generate isXxx() getters for boolean fields
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isActive"),
            "Should generate isActive() for boolean field"
        );
        assert!(
            class.methods.iter().any(|m| m.name.as_ref() == "isEnabled"),
            "Should generate isEnabled() for boolean field"
        );

        // Should NOT generate setters
        assert!(!class.methods.iter().any(|m| m.name.as_ref() == "setActive"));
        assert!(
            !class
                .methods
                .iter()
                .any(|m| m.name.as_ref() == "setEnabled")
        );
    }

    #[test]
    fn test_value_with_explicit_constructor() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Person {
                String name;
                int age;
                
                public Person(String name) {
                    this.name = name;
                    this.age = 0;
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only the explicit constructor
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1, "Should have only 1 constructor");
        assert_eq!(
            constructors[0].params.len(),
            1,
            "Constructor should have 1 param"
        );

        // Should still generate getters
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getAge"));
    }

    #[test]
    fn test_value_with_explicit_to_string() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Person {
                String name;
                
                @Override
                public String toString() {
                    return \"Person[\" + name + \"]\";
                }
            }
        "};

        let class = parse_first_class(src);

        // Should have only one toString() method (the explicit one)
        let to_strings: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "toString")
            .collect();
        assert_eq!(to_strings.len(), 1, "Should have only 1 toString()");

        // Should still generate other methods
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));
    }

    #[test]
    fn test_value_constructor_order() {
        let src = indoc::indoc! {"
            import lombok.Value;
            
            @Value
            public class Person {
                String firstName;
                String lastName;
                int age;
                String email;
            }
        "};

        let class = parse_first_class(src);

        // Constructor parameters should be in field declaration order
        let constructors: Vec<_> = class
            .methods
            .iter()
            .filter(|m| m.name.as_ref() == "<init>")
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].params.len(), 4);
        assert_eq!(constructors[0].params.items[0].name.as_ref(), "firstName");
        assert_eq!(constructors[0].params.items[1].name.as_ref(), "lastName");
        assert_eq!(constructors[0].params.items[2].name.as_ref(), "age");
        assert_eq!(constructors[0].params.items[3].name.as_ref(), "email");
    }

    #[test]
    fn test_value_with_explicit_component_annotations() {
        let src = indoc::indoc! {"
            import lombok.Value;
            import lombok.ToString;
            
            @Value
            @ToString(includeFieldNames = false)
            public class Person {
                String name;
                int age;
            }
        "};

        let class = parse_first_class(src);

        // @Value should defer to explicit @ToString
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "toString"));

        // Should still generate other methods
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "getName"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "equals"));
        assert!(class.methods.iter().any(|m| m.name.as_ref() == "hashCode"));

        // Should NOT have setters
        assert!(!class.methods.iter().any(|m| m.name.as_ref() == "setName"));
        assert!(!class.methods.iter().any(|m| m.name.as_ref() == "setAge"));
    }
}
