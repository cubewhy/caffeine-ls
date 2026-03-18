# Lombok Support Implementation

This document describes the Lombok support implementation for the Java LSP.

## Overview

Lombok support is implemented as a synthetic member generation system that processes Lombok annotations during Java source parsing and generates the corresponding methods and fields that Lombok would generate at compile time.

## Architecture

### Module Structure

```
src/language/java/lombok/
├── config.rs                    # lombok.config file parsing (175 lines)
├── types.rs                     # Type definitions and constants (118 lines)
├── utils.rs                     # Helper functions (400 lines)
├── rules.rs                     # Rules module entry point (5 lines)
├── rules/
│   ├── getter_setter_rule.rs   # @Getter and @Setter implementation (420 lines)
│   └── to_string_rule.rs       # @ToString implementation (278 lines)
└── tests.rs                     # Comprehensive test suite (571 lines)

Total: ~1,967 lines of code
```

### Integration Points

1. **Synthetic Member System** (`src/language/java/synthetic/`)
   - Lombok rules implement the `SyntheticMemberRule` trait
   - Rules are registered in `SYNTHETIC_RULES` array
   - Called during class parsing to generate synthetic members

2. **Class Parser** (`src/language/java/class_parser.rs`)
   - Calls `synthesize_for_type()` during class parsing
   - Merges synthetic members with explicit members
   - Synthetic members appear in `ClassMetadata`

3. **Type Context** (`src/language/java/type_ctx.rs`)
   - Used to resolve type names to internal JVM format
   - Handles import resolution for annotation names

## Implemented Features

### @Getter Annotation

**Field-level usage:**
```java
@Getter
private String name;
```
Generates: `public String getName()`

**Class-level usage:**
```java
@Getter
public class Person {
    private String name;
    private int age;
}
```
Generates: `getName()` and `getAge()` for all fields

**Features:**
- Boolean fields use `is` prefix: `boolean active` → `isActive()`
- Respects `AccessLevel` parameter: `@Getter(AccessLevel.PROTECTED)`
- Skips static fields automatically
- Won't override existing methods
- Supports `lazy=true` for lazy initialization

### @Setter Annotation

**Field-level usage:**
```java
@Setter
private String name;
```
Generates: `public void setName(String name)`

**Class-level usage:**
```java
@Setter
public class Person {
    private String name;
    private final int id;
}
```
Generates: `setName()` only (skips final field `id`)

**Features:**
- Automatically skips final fields
- Respects `AccessLevel` parameter
- Won't override existing methods
- Skips static fields automatically

### Configuration Support

Lombok behavior can be customized via `lombok.config` files:

**lombok.accessors.fluent**
```
lombok.accessors.fluent = true
```
Generates: `name()` instead of `getName()`

**lombok.accessors.prefix**
```
lombok.accessors.prefix = m_;f_
```
Strips prefixes: `m_name` → `getName()`

**lombok.accessors.chain**
```
lombok.accessors.chain = true
```
Setters return `this` for method chaining

**Configuration hierarchy:**
- Searches up directory tree for `lombok.config` files
- Merges configurations (child overrides parent)
- Supports `config.stopBubbling = true` to stop search

## Implementation Details

### Annotation Resolution

Lombok annotations may appear in source code as:
- Simple name: `@Getter` (requires import)
- Qualified name: `@lombok.Getter`

The implementation handles both by checking:
1. Full internal name: `lombok/Getter`
2. Simple name: `Getter`

This is necessary because Lombok classes aren't in the classpath during source parsing, so type resolution may fail.

### Class-level Annotation Extraction

Class-level annotations are extracted using `first_child_of_kind(node, "modifiers")` rather than `child_by_field_name("modifiers")` to ensure compatibility with the tree-sitter Java grammar.

### Synthetic Member Generation

1. Parse class declaration and extract fields
2. Check for class-level Lombok annotations
3. For each field:
   - Check for field-level Lombok annotations
   - Field-level overrides class-level
   - Generate methods if not already present
4. Create `SyntheticDefinition` for go-to-definition support

### Go-to-Definition Support

Synthetic members track their origin via `SyntheticOrigin` enum:
- `LombokGetter { field_name }` - resolves to field declaration
- `LombokSetter { field_name }` - resolves to field declaration
- `LombokToString` - resolves to class declaration

When user navigates to a synthetic method, the LSP resolves it back to the source field or class.

## Implemented Features

### Phase 1: @Getter and @Setter (Complete)

See sections below for detailed documentation.

### Phase 2: @ToString and @EqualsAndHashCode (Complete)

See sections below for detailed documentation.

### Phase 3: Constructor Annotations (Complete)

**@NoArgsConstructor** - Generates a no-args constructor
**@RequiredArgsConstructor** - Generates constructor for final and @NonNull fields
**@AllArgsConstructor** - Generates constructor with all fields

See detailed documentation below.

### @ToString (Complete)

**Class-level usage:**
```java
@ToString
public class Person {
    private String name;
    private int age;
}
// Generates: public String toString()
```

**Supported Parameters:**
- `includeFieldNames` - Include field names in output (default: true)
- `exclude` - Exclude specific fields from toString
- `of` - Only include specific fields
- `callSuper` - Include super.toString() in output
- `doNotUseGetters` - Access fields directly instead of using getters
- `onlyExplicitlyIncluded` - Only include fields marked with @ToString.Include

**Features:**
- Generates `public String toString()` method
- Skips static fields automatically
- Respects `exclude` and `of` parameters
- Does not override existing toString() methods
- Supports @ToString.Include and @ToString.Exclude field annotations

**Example with parameters:**
```java
@ToString(exclude = {"password", "internalId"})
public class User {
    private String username;
    private String email;
    private String password;  // excluded
    private long internalId;  // excluded
}
```

### Constructor Annotations (Complete)

#### @NoArgsConstructor

Generates a constructor with no parameters.

**Basic usage:**
```java
@NoArgsConstructor
public class Person {
    private String name;
    private int age;
}
// Generates: public Person()
```

**Supported Parameters:**
- `access` - AccessLevel (default: PUBLIC) - Set constructor visibility
- `force` - boolean (default: false) - Initialize final fields to 0/false/null
- `staticName` - String (optional) - Generate static factory method instead

**Features:**
- Generates public no-args constructor by default
- Skips generation if final fields exist (unless `force = true`)
- With `force = true`, initializes final fields to default values
- Static factory method makes constructor private and adds public static method
- Enum constructors are always private

**Examples:**
```java
// With access level
@NoArgsConstructor(access = AccessLevel.PRIVATE)
public class Singleton {
    private static Singleton instance;
}

// With force for final fields
@NoArgsConstructor(force = true)
public class Config {
    private final String value = "default";
}

// With static factory
@NoArgsConstructor(staticName = "create")
public class Factory {
    private String data;
}
// Generates: private Factory() and public static Factory create()
```

#### @RequiredArgsConstructor

Generates a constructor with parameters for fields that require special handling:
- All non-initialized `final` fields
- All fields marked with `@NonNull` that aren't initialized

**Basic usage:**
```java
@RequiredArgsConstructor
public class Person {
    private final String id;
    @NonNull
    private String name;
    private int age;  // Not included
}
// Generates: public Person(String id, String name)
```

**Supported Parameters:**
- `access` - AccessLevel (default: PUBLIC)
- `staticName` - String (optional) - Generate static factory method

**Features:**
- Includes final fields in constructor parameters
- Includes @NonNull fields in constructor parameters
- Generates null checks for @NonNull parameters
- Parameters appear in field declaration order
- Skips static fields
- If no required fields, generates no-args constructor

**Examples:**
```java
// Mixed final and @NonNull
@RequiredArgsConstructor
public class User {
    private final String id;
    @NonNull
    private String username;
    @NonNull
    private String email;
    private String phone;  // Optional
}
// Generates: public User(String id, String username, String email)

// With static factory
@RequiredArgsConstructor(staticName = "of")
public class Point {
    private final int x;
    private final int y;
}
// Generates: private Point(int x, int y) and public static Point of(int x, int y)
```

#### @AllArgsConstructor

Generates a constructor with one parameter for each field in the class.

**Basic usage:**
```java
@AllArgsConstructor
public class Person {
    private String name;
    private int age;
    private boolean active;
}
// Generates: public Person(String name, int age, boolean active)
```

**Supported Parameters:**
- `access` - AccessLevel (default: PUBLIC)
- `staticName` - String (optional) - Generate static factory method

**Features:**
- Includes all non-static fields
- Generates null checks for @NonNull fields
- Parameters appear in field declaration order
- Works with generic types
- Enum constructors are always private

**Examples:**
```java
// With access level
@AllArgsConstructor(access = AccessLevel.PROTECTED)
public class Base {
    private String value;
}

// With static factory
@AllArgsConstructor(staticName = "of")
public class Pair<T, U> {
    private T first;
    private U second;
}
// Generates: private Pair(T first, U second) and public static <T,U> Pair<T,U> of(T first, U second)

// Multiple constructors
@NoArgsConstructor
@AllArgsConstructor
public class Flexible {
    private String name;
    private int value;
}
// Generates both: public Flexible() and public Flexible(String name, int value)
```

#### Combining Constructor Annotations

You can use multiple constructor annotations on the same class:

```java
@NoArgsConstructor(force = true)
@RequiredArgsConstructor
@AllArgsConstructor
public class Person {
    private final String id;
    private String name;
    private int age;
}
// Generates three constructors:
// - public Person()                           // NoArgs with force
// - public Person(String id)                  // RequiredArgs (only final field)
// - public Person(String id, String name, int age)  // AllArgs
```

**Important Notes:**
- Constructors are not generated if an explicit constructor with the same signature exists
- Static fields are always excluded from constructor parameters
- Enum constructors are always private regardless of access level setting
- Field initialization detection is conservative (assumes final fields need initialization)

## Testing Strategy

### Test Organization

Tests are organized in `tests.rs` with the following structure:

```rust
mod getter_tests {
    // Tests for @Getter annotation
}

mod setter_tests {
    // Tests for @Setter annotation
}

mod to_string_tests {
    // Tests for @ToString annotation
}

mod constructor_tests {
    // Tests for constructor annotations
}

mod equals_hash_code_tests {
    // Tests for @EqualsAndHashCode annotation
}

mod annotation_resolution_tests {
    // Tests for annotation name resolution
}

mod user_reported_issues {
    // Regression tests for user-reported bugs
}
```

### Test Coverage

**Unit Tests** (in implementation files):
- Configuration parsing
- Utility functions
- Name transformations
- Access level parsing
- Field inclusion/exclusion logic

**Integration Tests** (in tests.rs):
- Field-level annotations
- Class-level annotations
- Boolean field handling
- Final field handling
- Access modifiers
- Annotation resolution
- Parameter handling (exclude, of, callSuper, access, staticName, force)
- Constructor generation (NoArgs, RequiredArgs, AllArgs)
- Static factory methods
- Multiple constructor annotations
- Enum constructors
- Generic types
- User-reported issues

**Total: 61 tests, all passing**

### Running Tests

```bash
# Run all Lombok tests
cargo test --lib lombok

# Run specific test module
cargo test --lib lombok::tests::getter_tests

# Run with output
cargo test --lib lombok -- --nocapture
```

## Future Enhancements

### Planned Features (Priority Order)

1. **@Data** - Composite annotation combining @Getter, @Setter, @ToString, @EqualsAndHashCode, @RequiredArgsConstructor
2. **@Value** - Immutable class support (final class, final fields, @AllArgsConstructor, @Getter, @ToString, @EqualsAndHashCode)
3. **@Builder** - Builder pattern generation
4. **@With** - Immutable setters (return new instance with changed field)
5. **@Log** - Logger field generation (@Slf4j, @Log4j2, etc.)
6. **@Delegate** - Method delegation
7. **@SneakyThrows** - Exception wrapping
8. **lombok.var/val** - Local variable type inference

### Extension Points

New Lombok rules should:
1. Implement `SyntheticMemberRule` trait
2. Be added to `SYNTHETIC_RULES` array in `common.rs`
3. Add corresponding `SyntheticOrigin` variant
4. Include comprehensive tests in `tests.rs`

## Performance Considerations

- Synthetic member generation happens during indexing (one-time cost)
- No runtime overhead during completion or navigation
- Configuration files are parsed once per workspace
- Annotation matching uses simple string comparison

## Known Limitations

1. **Field Initialization Detection**: Cannot reliably detect if a field has an initializer in source code. Conservative approach treats all final fields as requiring initialization for @RequiredArgsConstructor.
2. **Type Resolution**: Field types may not resolve to fully qualified names if imports are missing
3. **Complex Annotations**: Some advanced Lombok features (e.g., `@Builder.Default`) not yet supported
4. **Delombok**: No support for delombok operation (converting Lombok code to plain Java)

## Troubleshooting

### Constructors Not Generated

**Check:**
1. Annotation is imported: `import lombok.NoArgsConstructor;`
2. For @NoArgsConstructor with final fields: use `force = true`
3. No existing constructor with same signature
4. Enum constructors are always private

### Getters/Setters Not Generated

**Check:**
1. Annotation is imported: `import lombok.Getter;`
2. Field is not static (Lombok skips static fields)
3. For setters: field is not final
4. No existing method with same name

### Wrong Method Name

**Check:**
1. `lombok.config` for `accessors.fluent` or `accessors.prefix` settings
2. Boolean fields use `is` prefix by default

### Class-level Annotation Not Working

**Check:**
1. Annotation is on the class declaration, not inside the class
2. Annotation has correct import

## Contributing

When adding new Lombok features:

1. Add annotation constant to `types.rs`
2. Create new rule in `rules/` directory
3. Implement `SyntheticMemberRule` trait
4. Register rule in `common.rs`
5. Add `SyntheticOrigin` variant
6. Write comprehensive tests in `tests.rs`
7. Update this documentation

## References

- [Project Lombok](https://projectlombok.org/)
- [Lombok Features](https://projectlombok.org/features/all)
- [Lombok Configuration](https://projectlombok.org/features/configuration)
