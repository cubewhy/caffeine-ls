//! Declaration-level checks over classes and interfaces
//! ([JLS §8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html),
//! [§9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html)) — the
//! checks that need a class's *whole* inheritance graph rather than one body:
//!
//! - the return-type-substitutability of overrides
//!   ([§8.4.8.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.8.3)),
//! - conflicting default methods inherited from unrelated superinterfaces
//!   ([§9.4.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.3)).
//!
//! Unlike the body-level [`TypeError`]s, these diagnostics are keyed to the
//! declaring method rather than an expression: they are collected per file by
//! [`class_diagnostics`] and carry the offending method's name.

use hir_def::java::item_tree::{ItemData, ItemId};
use hir_expand::body::BodyTree;
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use syntax::{DiagnosticCode, JavaDiagnosticCode};
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::java::method::{self, Access, InvocationContext, InvocationMode, MethodData};
use crate::java::resolve::scope_for_file;
use crate::java::subtyping;
use crate::java::ty::Ty;
use base_db::LanguageKind;

/// A declaration-level diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclDiagnostic {
    /// §8.4.8.3: an override's return type is not return-type-substitutable —
    /// it is not a subtype of the overridden method's return type. The return
    /// types are stored unresolved (the canonical FQN) and the owner's FQN
    /// kept, rendered simple only in [`DeclDiagnostic::message`], so future
    /// quickfixes keep the full types.
    IncompatibleOverride {
        method: Name,
        found: Ty,
        expected_owner: Name,
        expected_ret: Ty,
    },
    /// §9.4.1.3: two unrelated superinterfaces declare matching default
    /// methods and the class inherits both without overriding.
    ConflictingDefaults { method: Name },
    /// §9.6.4.4: a method annotated `@Override` overrides or implements no
    /// supertype method — either nothing matches, or the annotated method is
    /// `static` (static methods hide, they never override).
    MethodDoesNotOverride { method: Name },
    /// §6.5.5.1: a reference type name in a *declaration* — a field type, a
    /// method's parameter/return/`throws` type, a type-parameter bound, a
    /// superclass or implemented interface, a record component type or a
    /// module directive — resolves to nothing on the classpath.
    CannotResolveType {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §6.5.5.1/[§7.5.2]: a simple type name is available through two or more
    /// on-demand imports that denote different types.
    AmbiguousName {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.1: a single-type import names a class or interface that cannot be
    /// found (or is not accessible).
    UnresolvedImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.2: an on-demand import (`import pkg.*;`) names a package that is
    /// not observable on the classpath — javac reports `package pkg does not
    /// exist`. (The package may still be *empty* of the wanted simple name;
    /// that is a name-resolution error at the use site.)
    UnresolvedImportPackage {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.4: a static on-demand import (`import static pkg.Type.*;`) names
    /// a class or interface that cannot be found.
    UnresolvedStaticImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.5.1: two single-type imports name different classes with the same
    /// simple name, or an import collides with a same-name top-level
    /// declaration of the compilation unit.
    ConflictingImport {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.4.3/[§7.7.2]: a class exists on the classpath, but its package is
    /// not visible from the resolving source set's module.
    ModuleNotAccessible {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.2.1/compilation-unit packaging (javadoc-classpath convention; no
    /// javac `compiler.*` twin): the file's package directory under its
    /// source root does not equal its declared package.
    ///
    /// [JLS §7.2.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.2.1
    UnexpectedPackagePath {
        /// The declared package as written.
        expected: Name,
        /// The file's directory chain under its source root, `/`-joined
        /// (the package it resolves to on a conventional classpath).
        dir: String,
        /// The source range of the package declaration's name.
        name_range: Option<rowan::TextRange>,
    },
    /// §7.4.1: a compilation unit declares more than one `package` declaration
    /// — the second and later are errors. Each reported declaration carries
    /// its written package and the range of its name.
    DuplicatePackage {
        package: Name,
        name_range: Option<rowan::TextRange>,
    },
    /// §7.6: two or more class-like declarations in the same package share a
    /// fully qualified name ([JLS §6.7]). The non-first declaration of a
    /// duplicate FQN is reported, cross-file as well as same-file; the message
    /// mirrors javac's `duplicate class: {fqn}`.
    DuplicateClass {
        fqn: String,
        name_range: Option<rowan::TextRange>,
    },
    /// §7.6: a `public` top-level class-like declaration must be declared in a
    /// file named after its simple name — which also means at most one `public`
    /// top-level type per compilation unit. The message mirrors javac's
    /// `class {Simple} is public, should be declared in a file named {Simple}.java`.
    ClassPublicShouldBeInFile {
        name: Name,
        name_range: Option<rowan::TextRange>,
    },
    /// §9.6.4.1: an annotation's `@Target` does not include the element type
    /// of the declaration (or type) it is applied to — javac's
    /// `annotation @X is not applicable in this type context`. `name` is the
    /// annotation's (possibly qualified) name; `element_type` is the
    /// `ElementType` constant of the annotated declaration, or `TYPE_USE`
    /// for a type context that neither the annotation's target nor the
    /// declaration's element type covers.
    AnnotationNotApplicable {
        name: Name,
        element_type: &'static str,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1: an annotation element-value pair names an element the annotation
    /// type does not declare — javac's `no annotation member named {name}`.
    /// `range` is the source range of the offending value expression.
    UnknownAnnotationMember {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1: the same annotation element is given a value twice — javac's
    /// `duplicate annotation member value` on the later pair.
    DuplicateAnnotationMemberValue {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1/[§5.2]: an annotation element value is not assignable to its
    /// element's declared type ([§9.6.1]) — a literal, enum constant, class
    /// literal, nested annotation or array element of the wrong type. Types are
    /// stored unresolved (the canonical FQN), rendered simple only in
    /// [`DeclDiagnostic::message`].
    AnnotationElementTypeMismatch {
        found: Ty,
        expected: Ty,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1/[§8.9]: an enum-constant element value names a constant the
    /// element's (enum) type does not declare — javac's `cannot find symbol`. A
    /// bare `CONST` value infers its declaring type from the element's type
    /// ([§9.7.1]); a qualified `E.CONST` names `E` explicitly.
    UnknownAnnotationElementConstant {
        member: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.8 ([§8.10.4] for records): the `SimpleTypeName` in a constructor
    /// declaration must be the simple name of the class that contains it, or a
    /// compile-time error occurs. javac reports such a declaration as
    /// `invalid method declaration; return type required`
    /// ([`JavaDiagnosticCode::ConstructorNameMismatch`]); the message is
    /// IntelliJ IDEA's `Constructor name 'W' is different from the class name
    /// 'R'`. `name` is the mismatched constructor name, `class` the enclosing
    /// class's simple name, and `range` the constructor's name identifier
    /// (matching javac's caret position).
    ConstructorNameMismatch {
        name: Name,
        class: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.1.1/[§8.4.3]: a declaration carries two or more modifiers that the
    /// JLS forbids from co-occurring — two access modifiers, `abstract` with
    /// `final`/`static`/`private`/`default`/`native`/`synchronized`/`strictfp`,
    /// `final` with `sealed` or `volatile`, `sealed` with `non-sealed`. javac
    /// reports the pair (`abstract, final`) as `illegal combination of
    /// modifiers`; the message here is IntelliJ-style. `first`/`second` are
    /// the offending pair in canonical modifier order and `range` spans the
    /// whole declaration, so the error is visible at a glance.
    IllegalModifierCombination {
        first: &'static str,
        second: &'static str,
        range: Option<rowan::TextRange>,
    },
    /// §8.1.1.2: a class or interface whose direct superclass (its `extends`
    /// clause) is a `final` class — a final class cannot have subclasses.
    /// javac: `cannot inherit from final {F}`; the message is IntelliJ's
    /// `Cannot inherit from 'Base'`. `super_owner` is the canonical FQN of the
    /// final superclass (rendered simple) and `range` the source range of the
    /// superclass reference name.
    CannotInheritFromFinalClass {
        super_owner: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.4.3.3: a declaration of a method with the same signature as a
    /// `final` method inherited from a superclass or superinterface — a final
    /// method can neither be overridden (instance) nor hidden (static). javac
    /// reports `{m} in {D} cannot override {m} in {S}; overridden method is
    /// final`; the message is IntelliJ's `Cannot override final method`.
    /// `super_owner` is the declaring class of the final method.
    CannotOverrideFinalMethod { method: Name, super_owner: Name },
    /// §8.4.8.3: an override or implementation whose access is weaker than
    /// the access of the method it overrides — `public` > `protected` >
    /// package-private > `private`. javac reports `{m} in {D} cannot override
    /// {m} in {S}; attempting to assign weaker access privileges`; the message
    /// is IntelliJ's `Overrides 'm' in 'S' with weaker access privilege`.
    /// `required` is the weaker access keyword actually granted.
    WeakerAccessPrivileges { method: Name, super_owner: Name },
    /// §8.1.1.1: a non-abstract class (or record, or enum) inherits an
    /// abstract method and does not implement it with a concrete method of
    /// the same signature. javac reports `{C} is not abstract and does not
    /// override abstract method {m} in {A}`. `class` is the non-abstract
    /// class, `method` the unimplemented abstract method and `owner` the
    /// class that declares it.
    UnimplementedAbstractMethod {
        class: Name,
        method: Name,
        owner: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.1.4/[§9.1.3]: a class or interface appears in its own inheritance
    /// chain — `class A extends B` with `class B extends A`. javac: `cyclic
    /// inheritance involving {C}`. `class` is the reported class and `range`
    /// its declaration's name range.
    CyclicInheritance {
        class: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.8.7: a class that declares no constructor has an implicit default
    /// constructor whose body begins with `super()`; a direct superclass with
    /// no *accessible* no-argument constructor makes that implicit call fail.
    /// javac: `implicit super constructor {S}() is undefined`; the message is
    /// IntelliJ's `There is no default constructor available in 'Base'`.
    /// `class` is the subclass and `super_owner` the direct superclass lacking
    /// a no-arg constructor, rendered simple. `range` is the class name range.
    NoDefaultConstructor {
        class: Name,
        super_owner: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.8.7.1: a `this(...)` delegation cycle among the class's own
    /// constructors — no path reaches the supertype constructor. javac:
    /// `recursive constructor invocation` at the offending `this(...)` call.
    /// `range` is the delegating call's source range.
    RecursiveConstructorInvocation { range: Option<rowan::TextRange> },
    /// §6.4: two members of one class-like declaration (fields, or a field
    /// clashing with another field) share a name — the second is reported at
    /// its name range. javac: `{x} is already defined in {y}`.
    DuplicateDeclaration {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.3.1.2/[§16]: a blank `final` field that cannot be initialized on
    /// every constructor path (instance) or in the static initializers
    /// (static) is never assigned — `variable {f} might not have been
    /// initialized`. `field` is the field's simple name, `range` its name
    /// range.
    FinalFieldNotInitialized {
        field: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.4.2: two methods of one class-like declaration have the same
    /// erasure ([§4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6))
    /// but different parameterized signatures — `void m(List<String>)` and
    /// `void m(List<Integer>)` — and neither overrides the other. javac:
    /// `name clash: {m1} and {m2} have the same erasure, yet neither overrides
    /// the other`; the message is IntelliJ's. The methods' parameter types are
    /// stored so the message can render both signatures; both are declared in
    /// the same class, so no owner is needed.
    NameClashSameErasure {
        method: Name,
        params: Vec<Ty>,
        other_params: Vec<Ty>,
    },
    /// §8.1.2: a *generic* class may not be a direct or indirect subclass of
    /// `java.lang.Throwable` — an exception type must be a concrete class.
    /// javac: `generic class {C} may not subclass java.lang.Throwable`; the
    /// message is javac's. `class` is the generic class and `range` its name
    /// range.
    GenericCannotExtendThrowable {
        class: Name,
        range: Option<rowan::TextRange>,
    },
}

impl DeclDiagnostic {
    /// The typed code of this diagnostic ([`DiagnosticCode`]).
    pub fn code(&self) -> DiagnosticCode {
        match self {
            DeclDiagnostic::IncompatibleOverride { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::IncompatibleOverride)
            }
            DeclDiagnostic::ConflictingDefaults { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ConflictingDefaults)
            }
            DeclDiagnostic::MethodDoesNotOverride { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::MethodDoesNotOverride)
            }
            DeclDiagnostic::CannotResolveType { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::CannotResolveType)
            }
            DeclDiagnostic::AmbiguousName { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::AmbiguousName)
            }
            DeclDiagnostic::UnresolvedImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImport)
            }
            DeclDiagnostic::UnresolvedImportPackage { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedImportPackage)
            }
            DeclDiagnostic::UnresolvedStaticImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnresolvedStaticImport)
            }
            DeclDiagnostic::ConflictingImport { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ConflictingImport)
            }
            DeclDiagnostic::ModuleNotAccessible { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ModuleNotAccessible)
            }
            DeclDiagnostic::UnexpectedPackagePath { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnexpectedPackagePath)
            }
            DeclDiagnostic::DuplicatePackage { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::DuplicatePackage)
            }
            DeclDiagnostic::DuplicateClass { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::DuplicateClass)
            }
            DeclDiagnostic::ClassPublicShouldBeInFile { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ClassPublicShouldBeInFile)
            }
            DeclDiagnostic::AnnotationNotApplicable { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::AnnotationNotApplicable)
            }
            DeclDiagnostic::UnknownAnnotationMember { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnknownAnnotationMember)
            }
            DeclDiagnostic::DuplicateAnnotationMemberValue { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::DuplicateAnnotationMemberValue)
            }
            DeclDiagnostic::AnnotationElementTypeMismatch { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::AnnotationElementTypeMismatch)
            }
            DeclDiagnostic::UnknownAnnotationElementConstant { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnknownAnnotationElementConstant)
            }
            DeclDiagnostic::ConstructorNameMismatch { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::ConstructorNameMismatch)
            }
            DeclDiagnostic::IllegalModifierCombination { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::IllegalModifierCombination)
            }
            DeclDiagnostic::CannotInheritFromFinalClass { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::CannotInheritFromFinalClass)
            }
            DeclDiagnostic::CannotOverrideFinalMethod { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::CannotOverrideFinalMethod)
            }
            DeclDiagnostic::WeakerAccessPrivileges { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::WeakerAccessPrivileges)
            }
            DeclDiagnostic::UnimplementedAbstractMethod { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::UnimplementedAbstractMethod)
            }
            DeclDiagnostic::CyclicInheritance { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::CyclicInheritance)
            }
            DeclDiagnostic::NoDefaultConstructor { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::NoDefaultConstructor)
            }
            DeclDiagnostic::RecursiveConstructorInvocation { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::RecursiveConstructorInvocation)
            }
            DeclDiagnostic::DuplicateDeclaration { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::DuplicateDeclaration)
            }
            DeclDiagnostic::FinalFieldNotInitialized { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::FinalFieldNotInitialized)
            }
            DeclDiagnostic::NameClashSameErasure { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::NameClashSameErasure)
            }
            DeclDiagnostic::GenericCannotExtendThrowable { .. } => {
                DiagnosticCode::Java(JavaDiagnosticCode::GenericCannotExtendThrowable)
            }
        }
    }

    /// The human-readable message, written in the IntelliJ IDEA style (a
    /// single, capitalized sentence) with types rendered simple
    /// ([`Ty::display_simple`]). The structured fields keep the canonical FQN;
    /// the simple rendering happens only here, at display time.
    pub fn message(&self, db: &dyn TyDatabase) -> String {
        match self {
            DeclDiagnostic::IncompatibleOverride {
                found,
                expected_owner,
                expected_ret,
                ..
            } => {
                format!(
                    "Incompatible override: '{}' cannot override '{}' in '{}'",
                    found.display_simple(db),
                    expected_ret.display_simple(db),
                    expected_owner.simple_name()
                )
            }
            DeclDiagnostic::ConflictingDefaults { method } => {
                let name = method.as_str();
                format!(
                    "Class inherits unrelated default methods for '{}()'; it must be overridden",
                    name
                )
            }
            DeclDiagnostic::MethodDoesNotOverride { method } => {
                let name = method.as_str();
                format!(
                    "Method '{}()' annotated @Override does not override or implement a method from a supertype",
                    name
                )
            }
            DeclDiagnostic::CannotResolveType { name, .. } => {
                format!("Cannot resolve symbol '{}'", name.as_str())
            }
            DeclDiagnostic::AmbiguousName { name, .. } => {
                format!(
                    "Reference to '{}' is ambiguous; it is imported on demand from more than one type",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnresolvedImport { name, .. } => {
                format!(
                    "Cannot resolve symbol '{}' in the single-type import",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnresolvedImportPackage { name, .. } => {
                format!("Package '{}' does not exist", name.as_str())
            }
            DeclDiagnostic::UnresolvedStaticImport { name, .. } => {
                format!("Cannot resolve symbol '{}'", name.simple_name())
            }
            DeclDiagnostic::ConflictingImport { name, .. } => {
                format!(
                    "Import conflicts with another declaration of '{}'",
                    name.as_str()
                )
            }
            DeclDiagnostic::ModuleNotAccessible { name, .. } => {
                format!(
                    "Package in which '{}' is declared is not visible from the current module",
                    name.as_str()
                )
            }
            DeclDiagnostic::UnexpectedPackagePath { expected, dir, .. } => format!(
                "Package name '{}' does not correspond to the file path '{}'",
                expected.as_str(),
                dir
            ),
            DeclDiagnostic::DuplicatePackage { package, .. } => {
                format!("Duplicate package declaration '{}'", package.as_str())
            }
            DeclDiagnostic::DuplicateClass { fqn, .. } => {
                format!("Duplicate class: {fqn}")
            }
            DeclDiagnostic::ClassPublicShouldBeInFile { name, .. } => {
                let simple = name.simple_name();
                format!(
                    "Class '{simple}' is public; it should be declared in a file named '{simple}.java'"
                )
            }
            DeclDiagnostic::AnnotationNotApplicable {
                name, element_type, ..
            } => {
                format!(
                    "Annotation '@{}' is not applicable to '{}'",
                    name.as_str(),
                    element_type
                )
            }
            DeclDiagnostic::UnknownAnnotationMember { name, .. } => {
                format!("No annotation member named '{}'", name.as_str())
            }
            DeclDiagnostic::DuplicateAnnotationMemberValue { name, .. } => {
                format!("Duplicate annotation member '{}'", name.as_str())
            }
            DeclDiagnostic::AnnotationElementTypeMismatch {
                found, expected, ..
            } => format!(
                "Incompatible types. Found: '{}', required: '{}'",
                found.display_simple(db),
                expected.display_simple(db)
            ),
            DeclDiagnostic::UnknownAnnotationElementConstant { member, .. } => {
                format!("Cannot resolve symbol '{}'", member.simple_name())
            }
            DeclDiagnostic::ConstructorNameMismatch { name, class, .. } => {
                format!(
                    "Constructor name '{}' is different from the class name '{}'",
                    name.as_str(),
                    class.as_str()
                )
            }
            DeclDiagnostic::IllegalModifierCombination { first, second, .. } => {
                format!("Illegal combination of modifiers: '{first}' and '{second}'")
            }
            DeclDiagnostic::CannotInheritFromFinalClass { super_owner, .. } => {
                format!("Cannot inherit from '{}'", super_owner.simple_name())
            }
            DeclDiagnostic::CannotOverrideFinalMethod {
                method,
                super_owner,
                ..
            } => {
                format!(
                    "Cannot override final method '{}()' in '{}'",
                    method.as_str(),
                    super_owner.simple_name()
                )
            }
            DeclDiagnostic::WeakerAccessPrivileges {
                method,
                super_owner,
                ..
            } => {
                format!(
                    "Overrides '{}()' in '{}' with weaker access privilege",
                    method.as_str(),
                    super_owner.simple_name()
                )
            }
            DeclDiagnostic::UnimplementedAbstractMethod {
                class,
                method,
                owner,
                ..
            } => {
                format!(
                    "Class '{}' must either be declared abstract or implement abstract method '{}()' in '{}'",
                    class.simple_name(),
                    method.as_str(),
                    owner.simple_name()
                )
            }
            DeclDiagnostic::CyclicInheritance { class, .. } => {
                format!("Cyclic inheritance involving '{}'", class.simple_name())
            }
            DeclDiagnostic::NoDefaultConstructor { super_owner, .. } => {
                format!(
                    "There is no default constructor available in '{}'",
                    super_owner.simple_name()
                )
            }
            DeclDiagnostic::RecursiveConstructorInvocation { .. } => {
                "Recursive constructor invocation".to_owned()
            }
            DeclDiagnostic::DuplicateDeclaration { name, .. } => {
                format!(
                    "Variable '{}' is already defined in the scope",
                    name.as_str()
                )
            }
            DeclDiagnostic::FinalFieldNotInitialized { field, .. } => {
                format!(
                    "Variable '{}' might not have been initialized",
                    field.as_str()
                )
            }
            DeclDiagnostic::NameClashSameErasure {
                method,
                params,
                other_params,
            } => {
                let render = |ty: &Ty| ty.display_simple(db).to_string();
                let sig = |name: &str, params: &[Ty]| {
                    format!(
                        "{}({})",
                        name,
                        params.iter().map(render).collect::<Vec<_>>().join(", ")
                    )
                };
                format!(
                    "'{}' clashes with '{}'; both methods have same erasure",
                    sig(method.as_str(), params),
                    sig(method.as_str(), other_params)
                )
            }
            DeclDiagnostic::GenericCannotExtendThrowable { class, .. } => {
                format!(
                    "Generic class '{}' may not subclass java.lang.Throwable",
                    class.simple_name()
                )
            }
        }
    }

    /// The name of the offending method, for rendering.
    pub fn method_name(&self) -> &str {
        match self {
            DeclDiagnostic::IncompatibleOverride { method, .. }
            | DeclDiagnostic::ConflictingDefaults { method }
            | DeclDiagnostic::MethodDoesNotOverride { method }
            | DeclDiagnostic::CannotOverrideFinalMethod { method, .. }
            | DeclDiagnostic::WeakerAccessPrivileges { method, .. }
            | DeclDiagnostic::NameClashSameErasure { method, .. } => method.as_str(),
            DeclDiagnostic::CannotResolveType { .. }
            | DeclDiagnostic::AmbiguousName { .. }
            | DeclDiagnostic::UnresolvedImport { .. }
            | DeclDiagnostic::UnresolvedImportPackage { .. }
            | DeclDiagnostic::UnresolvedStaticImport { .. }
            | DeclDiagnostic::ConflictingImport { .. }
            | DeclDiagnostic::ModuleNotAccessible { .. }
            | DeclDiagnostic::UnexpectedPackagePath { .. }
            | DeclDiagnostic::DuplicatePackage { .. }
            | DeclDiagnostic::DuplicateClass { .. }
            | DeclDiagnostic::ClassPublicShouldBeInFile { .. }
            | DeclDiagnostic::AnnotationNotApplicable { .. }
            | DeclDiagnostic::UnknownAnnotationMember { .. }
            | DeclDiagnostic::DuplicateAnnotationMemberValue { .. }
            | DeclDiagnostic::AnnotationElementTypeMismatch { .. }
            | DeclDiagnostic::UnknownAnnotationElementConstant { .. }
            | DeclDiagnostic::ConstructorNameMismatch { .. }
            | DeclDiagnostic::IllegalModifierCombination { .. }
            | DeclDiagnostic::CannotInheritFromFinalClass { .. }
            | DeclDiagnostic::UnimplementedAbstractMethod { .. }
            | DeclDiagnostic::CyclicInheritance { .. }
            | DeclDiagnostic::NoDefaultConstructor { .. }
            | DeclDiagnostic::RecursiveConstructorInvocation { .. }
            | DeclDiagnostic::DuplicateDeclaration { .. }
            | DeclDiagnostic::FinalFieldNotInitialized { .. }
            | DeclDiagnostic::GenericCannotExtendThrowable { .. } => "",
        }
    }

    /// The source range of a reference-position diagnostic (unknown type,
    /// ambiguous name, import), when it has one.
    pub fn range(&self) -> Option<rowan::TextRange> {
        match self {
            DeclDiagnostic::CannotResolveType { range, .. }
            | DeclDiagnostic::AmbiguousName { range, .. }
            | DeclDiagnostic::UnresolvedImport { range, .. }
            | DeclDiagnostic::UnresolvedImportPackage { range, .. }
            | DeclDiagnostic::UnresolvedStaticImport { range, .. }
            | DeclDiagnostic::ConflictingImport { range, .. }
            | DeclDiagnostic::ModuleNotAccessible { range, .. } => *range,
            DeclDiagnostic::UnexpectedPackagePath { name_range, .. } => *name_range,
            DeclDiagnostic::DuplicatePackage { name_range, .. }
            | DeclDiagnostic::DuplicateClass { name_range, .. }
            | DeclDiagnostic::ClassPublicShouldBeInFile { name_range, .. }
            | DeclDiagnostic::AnnotationNotApplicable {
                range: name_range, ..
            }
            | DeclDiagnostic::UnknownAnnotationMember {
                range: name_range, ..
            }
            | DeclDiagnostic::DuplicateAnnotationMemberValue {
                range: name_range, ..
            }
            | DeclDiagnostic::AnnotationElementTypeMismatch {
                range: name_range, ..
            }
            | DeclDiagnostic::UnknownAnnotationElementConstant {
                range: name_range, ..
            }
            | DeclDiagnostic::ConstructorNameMismatch {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::IllegalModifierCombination {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::CannotInheritFromFinalClass {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::UnimplementedAbstractMethod {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::CyclicInheritance {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::NoDefaultConstructor {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::RecursiveConstructorInvocation {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::DuplicateDeclaration {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::FinalFieldNotInitialized {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::GenericCannotExtendThrowable {
                range: name_range, ..
            } => *name_range,
            _ => None,
        }
    }
}

/// The declaration-level diagnostics of every class-like declaration in
/// `file`, in source order.
pub fn class_diagnostics(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    crate::java::db::class_diagnostics_query(db, db.file_text(file))
}

/// Enumerates the class-like declarations of the file in source order and
/// checks each against its inheritance graph.
pub(crate) fn class_diagnostics_impl(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let tree = hir::file_item_tree(db, file);
    let scope = scope_for_file(db, file);
    let mut out = Vec::new();

    // §6.5.5.1/[§7.5.1]: the unknown-reference and import diagnostics of the
    // file's declarations (see [`crate::java::name_check`]).
    out.extend(crate::java::name_check::declaration_type_diagnostics(
        db, file, &tree,
    ));

    // §7.2.1: the file's package directory must match its declared package
    // (see [`crate::java::name_check::package_path_diagnostics`]).
    out.extend(crate::java::name_check::package_path_diagnostics(
        db, file, &tree,
    ));

    // §7.4.1: a compilation unit declares at most one package (see
    // [`crate::java::name_check::duplicate_package_diagnostics`]).
    out.extend(crate::java::name_check::duplicate_package_diagnostics(
        &tree,
    ));

    // §7.6: at most one public top-level type per file, named after the file.
    out.extend(public_type_diagnostics(db, file, &tree));

    // §7.6: no two class-like declarations share a fully qualified name,
    // across the source set (cross-file as well as same-file).
    out.extend(duplicate_class_diagnostics(db, file, &tree));

    // §8.1.1/[§8.4.3]: a declaration carries two or more modifiers the JLS
    // forbids from co-occurring (see [`modifier_combination_diagnostics`]).
    out.extend(modifier_combination_diagnostics(db, file, &tree));

    // §9.6.4.1/[§9.7.4]/[§9.7.1]: the `@Target` applicability and the
    // element-value arguments of every annotation, declaration and type-use
    // alike (see [`crate::java::annotation_check`]).
    out.extend(crate::java::annotation_check::annotation_diagnostics(
        db, file, &tree,
    ));

    fn walk(
        db: &dyn TyDatabase,
        file: FileId,
        scope: &hir::ResolutionScope,
        tree: &hir_def::java::item_tree::ItemTree,
        id: hir_def::java::item_tree::ItemId,
        out: &mut Vec<DeclDiagnostic>,
    ) {
        let data = tree.data(id);
        if data.is_type()
            && let Some(fqn) = hir::source_class_fqn(db, file, id)
        {
            out.extend(check_class(db, file, scope, tree, fqn.as_str(), id));
        }
        for &child in data.body() {
            walk(db, file, scope, tree, child, out);
        }
    }
    for top in &tree.top {
        walk(db, file, &scope, &tree, *top, &mut out);
    }
    out
}

/// Checks one class-like declaration against its inheritance graph.
fn check_class(
    db: &dyn TyDatabase,
    file: FileId,
    scope: &hir::ResolutionScope,
    tree: &hir_def::java::item_tree::ItemTree,
    fqn: &str,
    item: hir_def::java::item_tree::ItemId,
) -> Vec<DeclDiagnostic> {
    // The access-control context of the class itself ([§6.6.1]): the walk is
    // a member enumeration, not an invocation from outside.
    let ctx = crate::java::method::access_context(db, file, item);
    let mut out = Vec::new();
    // The name-resolution context of the declaration itself: its type
    // parameters and every enclosing class's ([§6.5.5.1], [§8.1.3]).
    let resolver = crate::java::resolve::Resolver::new(
        tree,
        crate::java::db::type_params_map_query(db, db.file_text(file)),
        item,
    );
    // Every member visible from the class, most-derived first ([§8.4.8.1]),
    // *without* the most-derived dedup: an override must still see the super
    // declaration it hides — both for the return-type-substitutability check
    // and for `@Override` ([§9.6.4.4]). Split into the class's own
    // declarations and the inherited set.
    let self_ty = Ty::reference(db, fqn, Vec::new());
    let all = method::all_methods_raw(db, scope, &self_ty, &ctx);
    let declared: Vec<&MethodData> = all.iter().filter(|m| m.owner == fqn).collect();
    let inherited: Vec<&MethodData> = all.iter().filter(|m| m.owner != fqn).collect();
    for method in &declared {
        for super_method in &inherited {
            if same_signature(db, method, super_method) {
                // §8.4.3.3: a final method of a superclass or superinterface
                // can neither be overridden (instance) nor hidden (static), so
                // a redeclaration of its signature is an error.
                if super_method.is_final {
                    out.push(DeclDiagnostic::CannotOverrideFinalMethod {
                        method: Name::new(&method.name),
                        super_owner: Name::new(&super_method.owner),
                    });
                }
                // §8.4.8.3: the access of an overriding or hiding method must
                // be at least as permissive as the access of the method it
                // overrides or hides (`public` > `protected` > package-private
                // > `private`). A static/instance signature clash is a
                // different error (neither overrides nor hides), so only
                // same-staticness pairs are compared.
                if method.is_static == super_method.is_static
                    && weaker_access(method.access, super_method.access)
                {
                    out.push(DeclDiagnostic::WeakerAccessPrivileges {
                        method: Name::new(&method.name),
                        super_owner: Name::new(&super_method.owner),
                    });
                }
                // §8.4.8.3: an overriding *instance* method must be
                // return-type-substitutable — its return type is a subtype of
                // the overridden return type. A static method hides (§8.4.8.2)
                // and its result type is unconstrained, so only instance pairs
                // are checked.
                if !method.is_static && !method.ret.is_void(db) {
                    // §8.4.8.3: the overriding return must be *substitutable*
                    // for the overridden one — `R1 <: R2`, or `R1 <: |R2|`
                    // against its ERASURE when the overridden return is a type
                    // variable ([§8.4.4] adaptation, [§4.6]).
                    let super_ret_erasure = super_method.ret.erasure(db);
                    if !super_method.ret.is_error(db)
                        && !subtyping::is_subtype(
                            db,
                            scope,
                            &method.ret.clone(),
                            &super_ret_erasure,
                        )
                    {
                        out.push(DeclDiagnostic::IncompatibleOverride {
                            method: Name::new(&method.name),
                            found: method.ret,
                            expected_owner: Name::new(&super_method.owner),
                            expected_ret: super_method.ret,
                        });
                    }
                }
                break;
            }
        }
    }

    // §8.1.1.2: a class declaration whose `extends` clause names a `final`
    // class — a final class can have no subclasses ([§8.1.1.2]). Interfaces
    // extend interfaces only (their `extends` clause is stored in
    // `ClassData::interfaces`, not `super_class`), so the check reads the
    // superclass reference of a *class* declaration.
    if let ItemData::Class(class) = tree.data(item)
        && let Some(super_ref) = &class.super_class
    {
        let super_ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref);
        if let Some((is_class_like, is_final)) =
            subtyping::class_like_and_final(db, scope, &super_ty)
            && is_class_like
            && is_final
        {
            let fqn = super_ty
                .as_reference(db)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| class.name.clone());
            out.push(DeclDiagnostic::CannotInheritFromFinalClass {
                super_owner: fqn,
                range: super_ref.first_ref().and_then(|r| r.range),
            });
        }
    }

    // §8.1.4/[§9.1.3]: a class or interface appears in its own inheritance
    // chain — `class A extends B` with `class B extends A`. Reported for every
    // class-like declaration, at its name.
    if in_own_supertype_cycle(db, scope, fqn) {
        out.push(DeclDiagnostic::CyclicInheritance {
            class: class_like_simple_name(tree.data(item)),
            range: Some(tree.data(item).name_range()),
        });
    }

    // §8.8.7: a class that declares no constructor has an implicit default
    // constructor whose body begins with `super()`; a direct superclass with
    // no *accessible* no-argument constructor makes that implicit call fail.
    // Enums and records have their own implicit superclass (`Enum`, `Record`),
    // so only plain class declarations are checked.
    if let ItemData::Class(class) = tree.data(item)
        && !class
            .body
            .iter()
            .any(|child| matches!(tree.data(*child), ItemData::Method(m) if m.is_constructor()))
        && let Some(super_ref) = &class.super_class
    {
        let super_ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref);
        let no_accessible_no_arg = has_no_accessible_no_arg_ctor(db, scope, &super_ty, &ctx);
        if no_accessible_no_arg == Some(true) {
            let super_owner = super_ty
                .as_reference(db)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| class.name.clone());
            out.push(DeclDiagnostic::NoDefaultConstructor {
                class: class.name.clone(),
                super_owner,
                range: Some(class.name_range),
            });
        }
    }

    // §8.1.1.1: a non-abstract class — a class without the `abstract`
    // modifier, a record [§8.10] or an enum [§8.9] — must implement every
    // abstract method it inherits (or declares itself) with a concrete method
    // of the same overriding signature. Interfaces and annotation types may
    // stay abstract, so they are exempt.
    if !declaring_interface_item(tree, item)
        && !class_like_modifiers(tree.data(item)).is_some_and(|m| m.is_abstract())
    {
        // The most-derived declaration of each overriding signature: the raw
        // member walk is derived-first, so the first occurrence of a
        // signature is its effective member. An abstract member whose
        // signature no concrete method (declared by the class itself or by a
        // *subtype of its declaring type*, i.e. one that actually overrides
        // it) implements is unimplemented.
        let mut seen = FxHashSet::default();
        for abstract_method in &all {
            let key = (abstract_method.name.clone(), abstract_method.params.clone());
            if abstract_method.abstract_ && !abstract_method.is_static && seen.insert(key) {
                let implemented = all.iter().any(|candidate| {
                    !candidate.abstract_
                        && !candidate.is_static
                        && same_signature(db, candidate, abstract_method)
                        && (candidate.owner == abstract_method.owner
                            || subtyping::is_subtype(
                                db,
                                scope,
                                &Ty::reference(db, candidate.owner.as_str(), Vec::new()),
                                &Ty::reference(db, abstract_method.owner.as_str(), Vec::new()),
                            ))
                });
                if !implemented {
                    out.push(DeclDiagnostic::UnimplementedAbstractMethod {
                        class: class_like_simple_name(tree.data(item)),
                        method: Name::new(&abstract_method.name),
                        owner: Name::new(&abstract_method.owner),
                        range: Some(tree.data(item).name_range()),
                    });
                }
            }
        }
    }

    // §8.8.7.1: a `this(...)` delegation cycle among the class's own
    // constructors ([`recursive_constructor_diagnostics`]).
    out.extend(recursive_constructor_diagnostics(db, file, tree, item));

    // §6.4: two members of one class-like declaration share a name — the later
    // declaration is reported ([§6.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4)).
    let mut member_names: Vec<Name> = Vec::new();
    for &child in tree.data(item).body() {
        let ItemData::Field(field) = tree.data(child) else {
            continue;
        };
        if member_names.iter().any(|seen| seen == &field.name) {
            out.push(DeclDiagnostic::DuplicateDeclaration {
                name: field.name.clone(),
                range: Some(field.name_range),
            });
        } else {
            member_names.push(field.name.clone());
        }
    }

    // §8.3.1.2/[§16]: a blank `final` field that no constructor path (or,
    // for a static field, no static initializer) assigns is never initialized.
    out.extend(final_field_diagnostics(db, file, tree, item));

    // §9.4.1.3: two default methods with the same signature whose declaring
    // interfaces are unrelated (neither a subtype of the other) conflict; the
    // class inherits them only if it overrides the signature itself. The
    // defaults are collected *without* the most-derived dedup — unrelated
    // defaults do not override each other, they conflict.
    let defaults = method::inherited_defaults(db, scope, &self_ty);
    let defaults: Vec<&MethodData> = defaults.iter().filter(|m| m.owner != fqn).collect();
    for (i, a) in defaults.iter().enumerate() {
        for b in &defaults[i + 1..] {
            if !same_signature(db, a, b) || related(db, scope, &a.owner, &b.owner) {
                continue;
            }
            let already_overridden = declared
                .iter()
                .any(|m| !m.is_static && same_signature(db, m, a));
            if !already_overridden {
                out.push(DeclDiagnostic::ConflictingDefaults {
                    method: Name::new(&a.name),
                });
            }
        }
    }

    // §9.6.4.4: a method annotated `@Override` must override or implement an
    // instance method declared in a supertype — otherwise the annotation is a
    // compile-time error. A `static` method never overrides ([§8.4.8.2]: it
    // *hides*), so its annotation always fails. An explicitly declared
    // record accessor is the accessor mandated by its component ([§8.10.3]),
    // so `@Override` is accepted on it ([§9.6.4.4]).
    let record_components: &[hir_def::java::item_tree::RecordComponent] = match tree.data(item) {
        ItemData::Record(record) => &record.components,
        _ => &[],
    };
    for &child in tree.data(item).body() {
        if let ItemData::Method(m) = tree.data(child)
            && !m.is_constructor()
            && m.annotations.iter().any(|annotation| {
                is_override_annotation(db, scope, &resolver, &annotation.name.name)
            })
        {
            let Some(method) = declared
                .iter()
                .find(|d| d.name == m.name.as_str() && d.params.len() == m.sig.params.len())
            else {
                continue;
            };
            let is_record_accessor = record_components
                .iter()
                .any(|component| component.name.as_str() == method.name);
            let overrides = is_record_accessor
                || inherited
                    .iter()
                    .any(|s| !s.is_static && same_signature(db, method, s));
            if method.is_static || !overrides {
                out.push(DeclDiagnostic::MethodDoesNotOverride {
                    method: Name::new(&method.name),
                });
            }
        }
    }

    // §8.4.2: two methods *declared by the class itself* whose erasures
    // ([§4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6))
    // are equal but whose parameterized signatures differ — the JVM cannot
    // load them both, and neither overrides the other. Each later method is
    // reported against each earlier clashing one.
    for (i, a) in declared.iter().enumerate() {
        for b in &declared[i + 1..] {
            if a.name != b.name || a.params.len() != b.params.len() {
                continue;
            }
            let same_erasure = a
                .params
                .iter()
                .zip(&b.params)
                .all(|(x, y)| x.erasure(db) == y.erasure(db));
            // Identical signatures are a *duplicate declaration*, not a name
            // clash — they collide even without generics.
            let identical = a.params == b.params;
            if same_erasure && !identical {
                out.push(DeclDiagnostic::NameClashSameErasure {
                    method: Name::new(&a.name),
                    params: a.params.clone(),
                    other_params: b.params.clone(),
                });
            }
        }
    }

    // §8.1.2: a generic class — one declaring type parameters — may not be a
    // direct or indirect subclass of `java.lang.Throwable`; an exception type
    // must be a concrete class. Interfaces are exempt (they never extend
    // classes).
    if let ItemData::Class(class) = tree.data(item)
        && !class.type_params.is_empty()
        && let Some(super_ref) = &class.super_class
    {
        let super_ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref);
        let throwable = Ty::reference(db, "java.lang.Throwable", Vec::new());
        if crate::java::subtyping::is_subtype(db, scope, &super_ty, &throwable) {
            out.push(DeclDiagnostic::GenericCannotExtendThrowable {
                class: class.name.clone(),
                range: Some(class.name_range),
            });
        }
    }

    // §8.8 ([§8.10.4] for records): the `SimpleTypeName` of every constructor
    // declaration must be the simple name of the class that contains it, or a
    // compile-time error occurs. The parser accepts any `Name(...)` member as
    // a constructor regardless of name (it has no class name at parse time),
    // so the match is checked here; a constructor whose name differs is how a
    // would-be method with a missing return type ([§8.4.5]) surfaces — javac
    // reports `invalid method declaration; return type required`. Only classes,
    // enum classes ([§8.9.2]) and record classes ([§8.10.4]) can declare
    // constructors; interfaces and annotation types ([§9]) cannot, so their
    // (syntactically parseable but semantically void) constructor-shaped
    // members are left to other checks.
    let class_simple = match tree.data(item) {
        ItemData::Class(class) => &class.name,
        ItemData::Enum(enum_) => &enum_.name,
        ItemData::Record(record) => &record.name,
        _ => return out,
    };
    for &child in tree.data(item).body() {
        let ItemData::Method(method) = tree.data(child) else {
            continue;
        };
        if method.is_constructor() && &method.name != class_simple {
            out.push(DeclDiagnostic::ConstructorNameMismatch {
                name: method.name.clone(),
                class: class_simple.clone(),
                range: Some(method.name_range),
            });
        }
    }
    out
}

/// Whether two methods have the same overriding signature
/// ([JLS §8.4.2]): identical name and *identical* parameter types. Widening
/// ([§5.1.2]) or boxing ([§5.1.7]) conversions apply to invocation, never to
/// overriding, so `f(int)` and `f(long)` are unrelated overloads. A parameter
/// that failed to resolve is treated as matching, so a broken classpath stays
/// conservative. The substitution of a supertype's type arguments into an
/// inherited method's parameters happens when the member set is built; the
/// substitution of a method's own type variables ([§8.4.4]) is not modelled.
fn same_signature(db: &dyn TyDatabase, a: &MethodData, b: &MethodData) -> bool {
    a.name == b.name
        && a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(x, y)| {
            x.is_error(db)
                || y.is_error(db)
                || x == y
                // [§8.4.8.1] with [§4.6]: a member inherited through a raw
                // supertype may arrive with its type variables unerased when
                // the stub record lacks the class `Signature`; the override
                // is still exact after erasure. Captured types (`CAP#n`)
                // never erase-match: they stand for unknown arguments.
                || (x.erasure(db) == y.erasure(db)
                    && !x.contains_type_var_named_capture(db)
                    && !y.contains_type_var_named_capture(db))
        })
}

/// §9.7.1/§6.5.5: whether an annotation name resolves to
/// `java.lang.Override`. The name is resolved in the file's scope like any
/// type reference, so a same-package `@interface Override` ([§6.5.5.1]) or a
/// single-type import shadows the JDK annotation and does not count. A name
/// that resolves nowhere falls back to its simple form, keeping broken or
/// partial classpaths conservative.
fn is_override_annotation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &crate::java::resolve::Resolver,
    name: &Name,
) -> bool {
    let resolved = crate::java::resolve::candidate_fqns(resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(db, scope, candidate.as_str()).is_some());
    match resolved {
        Some(fqn) => fqn.as_str() == "java.lang.Override",
        None => name.as_str().rsplit('.').next() == Some("Override"),
    }
}

/// Whether two declaring types are subtype-related in either direction, which
/// makes their default methods an override chain rather than a conflict
/// ([§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1),
/// [§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
/// ([§9.4.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.1),
/// [§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
fn related(db: &dyn TyDatabase, scope: &hir::ResolutionScope, a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_ty = Ty::reference(db, a, Vec::new());
    let b_ty = Ty::reference(db, b, Vec::new());
    subtyping::is_subtype(db, scope, &a_ty, &b_ty) || subtyping::is_subtype(db, scope, &b_ty, &a_ty)
}

/// §8.4.8.3: whether access `a` is strictly weaker than access `b` — the
/// ordering the JLS rules on overriding and hiding: `public` >
/// `protected` > package-private > `private` ([§6.6.1]).
fn weaker_access(a: Access, b: Access) -> bool {
    access_rank(a) < access_rank(b)
}

/// The numeric rank of an access level, `public` strongest
/// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
fn access_rank(access: Access) -> u8 {
    match access {
        Access::Public => 3,
        Access::Protected => 2,
        Access::Package => 1,
        Access::Private => 0,
    }
}

/// The simple name of a class-like declaration (class, interface, enum,
/// record or annotation), for a diagnostic header.
fn class_like_simple_name(data: &ItemData) -> Name {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => d.name.clone(),
        ItemData::Enum(d) => d.name.clone(),
        ItemData::Record(d) => d.name.clone(),
        ItemData::Annotation(d) => d.name.clone(),
        _ => Name::new(""),
    }
}

/// Whether the class-like declaration is an interface or annotation type —
/// either may stay abstract, so the §8.1.1.1 unimplemented-abstract-method
/// requirement does not apply.
fn declaring_interface_item(
    tree: &hir_def::java::item_tree::ItemTree,
    item: hir_def::java::item_tree::ItemId,
) -> bool {
    matches!(
        tree.data(item),
        ItemData::Interface(_) | ItemData::Annotation(_)
    )
}

/// §8.1.4/[§9.1.3]: whether the reference type `fqn` appears in its own
/// inheritance chain — a transitive-direct-supertype walk revisits the type
/// itself. Works for source and library types alike; the visited set keeps the
/// walk finite on cyclic graphs. `supertypes_impl` yields only the *direct*
/// supertypes, so one BFS level at a time.
fn in_own_supertype_cycle(db: &dyn TyDatabase, scope: &hir::ResolutionScope, fqn: &str) -> bool {
    let mut visited: FxHashSet<String> = FxHashSet::default();
    let mut stack: Vec<Ty> = vec![Ty::reference(db, fqn, Vec::new())];
    while let Some(ty) = stack.pop() {
        let Some((name, _)) = ty.as_reference(db) else {
            continue;
        };
        if !visited.insert(name.as_str().to_owned()) {
            continue;
        }
        for parent in subtyping::supertypes_impl(db, scope, &ty) {
            let Some((parent_name, _)) = parent.as_reference(db) else {
                continue;
            };
            if parent_name.as_str() == fqn {
                return true;
            }
            stack.push(parent);
        }
    }
    false
}

/// §8.8.7: whether the class `super_ty` demonstrably provides *no* accessible
/// no-argument constructor to the implicit `super()` of the class in `ctx` (a
/// `super` invocation mode, [§8.8.7.1]). Returns `Some(true)` when the
/// violation holds, `Some(false)` when there is an accessible one, and `None`
/// when the superclass's constructor set cannot be trusted — a *library* stub
/// that records no `<init>` at all is partial (a real classfile always has
/// one), so its absence proves nothing. Source superclasses name their
/// constructors after the class; library ones use the JVMS `<init>` name
/// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)),
/// and a constructor-less source class's *implicit* default constructor
/// ([§8.8.9]) is part of its member set.
fn has_no_accessible_no_arg_ctor(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    super_ty: &Ty,
    ctx: &InvocationContext,
) -> Option<bool> {
    use hir::ClassOrModuleStub;
    let (fqn, _) = super_ty.as_reference(db)?;
    let name = match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Library(library)) => {
            // A library stub with no `<init>` at all is a partial record; only
            // a declared constructor set is conclusive.
            let record = hir::class_record(db, &library)?;
            let ClassOrModuleStub::Class(class) = record.as_ref() else {
                return Some(false);
            };
            let interner = &db.hir_state().interner;
            let init_count = class
                .methods
                .iter()
                .filter(|m| interner.resolve(&m.name) == "<init>")
                .count();
            if init_count == 0 {
                return None;
            }
            "<init>"
        }
        Some(hir::Resolved::Source(_)) => fqn.as_str().rsplit('.').next().unwrap_or(fqn.as_str()),
        // An unresolvable superclass is already reported as a missing type by
        // the name check; whether it has a no-arg constructor is unknowable.
        None => return None,
    };
    let access = ctx.with_mode(InvocationMode::Super);
    let has_no_arg = method::member_set(db, scope, super_ty, name, &access)
        .iter()
        .any(|method| method.params.is_empty());
    Some(!has_no_arg)
}

/// §8.8.7.1: a constructor delegation (`this(...)`) cycle among the class's
/// own constructors — every path through the delegation graph must reach the
/// supertype constructor, so a cycle is a compile-time error. The delegation
/// target of each constructor's first explicit `this(...)` is resolved by
/// arity (the source is already broken, so full overload resolution is not
/// needed); the first explicit `this(...)` of every constructor on a cycle is
/// reported. javac: `recursive constructor invocation`.
fn recursive_constructor_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: hir_def::java::item_tree::ItemId,
) -> Vec<DeclDiagnostic> {
    use hir_def::java::item_tree::ItemData as I;
    use hir_expand::body::{CtorCallTarget, ExprData, StmtData};
    let item_data = tree.data(item);
    let ctors: Vec<ItemId> = item_data
        .body()
        .iter()
        .copied()
        .filter(|child| matches!(tree.data(*child), I::Method(m) if m.is_constructor()))
        .collect();
    if ctors.is_empty() {
        return Vec::new();
    }
    let bodies = hir::file_body_tree(db, file);
    // The arity of each constructor, for the delegation-target lookup.
    let arity = |id: ItemId| match tree.data(id) {
        I::Method(m) => m.sig.params.len(),
        _ => 0,
    };
    // §8.8.7.1: resolve a `this(...)` call's target by arity. When several
    // overloads share the arity the source is ambiguous; the edge is then
    // skipped (returns `None`) so a wrong toString resolve does not fabricate
    // a cycle in otherwise-legal chains through same-arity overloads.
    let unique_target_with_arity = |wanted: usize| {
        let mut it = ctors.iter().filter(|id| arity(**id) == wanted);
        let first = it.next()?;
        it.next()
            .is_none()
            .then_some(ctors.iter().position(|id| *id == *first).unwrap())
    };
    // The delegation edge of each constructor: its target's index and the
    // source range of the delegating `this(...)` call.
    let edges: Vec<Option<(usize, Option<rowan::TextRange>)>> = ctors
        .iter()
        .map(|id| {
            let I::Method(m) = tree.data(*id) else {
                return None;
            };
            let body_id = m.body()?;
            let call = bodies.body(body_id).stmts.iter().find_map(|&stmt| {
                if let StmtData::Expr(expr) = bodies.stmt(stmt)
                    && let ExprData::CtorCall {
                        target: CtorCallTarget::This,
                        args,
                    } = bodies.expr(*expr)
                {
                    return Some((args.len(), bodies.expr_range(*expr)));
                }
                None
            })?;
            let target = unique_target_with_arity(call.0)?;
            Some((target, call.1))
        })
        .collect();
    // Find every delegation cycle: a walk from `start` that revisits a node
    // already on its own path closes a cycle, whose members are the path tail.
    let mut reported: FxHashSet<usize> = FxHashSet::default();
    let mut out = Vec::new();
    for start in 0..edges.len() {
        let mut order: Vec<usize> = Vec::new();
        let mut cur = Some(start);
        let mut cycle: Option<Vec<usize>> = None;
        while let Some(i) = cur {
            if let Some(pos) = order.iter().position(|&node| node == i) {
                cycle = Some(order[pos..].to_vec());
                break;
            }
            order.push(i);
            cur = edges[i].as_ref().map(|(target, _)| *target);
        }
        if let Some(members) = cycle {
            for node in members {
                if reported.insert(node) {
                    out.push(DeclDiagnostic::RecursiveConstructorInvocation {
                        range: edges[node].and_then(|(_, range)| range),
                    });
                }
            }
        }
    }
    out
}

/// §7.6: the class-like declarations of a compilation unit that a package
/// may hold more than one of — class, interface, enum, record and annotation
/// ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6)).
fn is_class_like(data: &ItemData) -> bool {
    data.is_type()
}

/// The `public` modifier and simple name of a class-like top-level declaration
/// ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6),
/// [§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
fn class_like_modifiers(data: &ItemData) -> Option<&hir_def::java::modifiers::JavaModifiers> {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => Some(&d.modifiers),
        ItemData::Enum(d) => Some(&d.modifiers),
        ItemData::Record(d) => Some(&d.modifiers),
        ItemData::Annotation(d) => Some(&d.modifiers),
        _ => None,
    }
}

/// The source-root-relative file name *without* its extension of `file` (e.g.
/// `Zed` for `/src/com/example/Zed.java`), used to check the §7.6 rule that a
/// public top-level type must name its file. `None` for files with no source
/// root or a virtual (unsaved) path.
fn file_stem(db: &dyn TyDatabase, file: FileId) -> Option<String> {
    let root = db.source_root_for_file(file)?;
    let root = db.source_root(root);
    let path = root.source_root(db).path_for_file(&file)?;
    let abs = path.as_path()?;
    abs.file_stem().map(|stem| stem.to_owned())
}

/// §7.6: every `public` top-level class-like declaration must be declared in a
/// file named after its simple name ([JLS §7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6)).
/// Because two top-level declarations cannot share a simple name within one
/// compilation unit without duplicating their FQN ([§7.6] — caught separately
/// by [`duplicate_class_diagnostics`]), "the public type must name the file"
/// is exactly javac's "at most one public top-level type per file" rule. Files
/// without a resolvable real path (unsaved buffers) are skipped.
fn public_type_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
) -> Vec<DeclDiagnostic> {
    if tree.language != LanguageKind::Java {
        return Vec::new();
    }
    let Some(stem) = file_stem(db, file) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &top in &tree.top {
        let data = tree.data(top);
        if !is_class_like(data) {
            continue;
        }
        let Some(modifiers) = class_like_modifiers(data) else {
            continue;
        };
        if !modifiers.is_public() {
            continue;
        }
        let simple = match data {
            ItemData::Class(d) | ItemData::Interface(d) => d.name.clone(),
            ItemData::Enum(d) => d.name.clone(),
            ItemData::Record(d) => d.name.clone(),
            ItemData::Annotation(d) => d.name.clone(),
            _ => continue,
        };
        if simple.as_str() != stem {
            out.push(DeclDiagnostic::ClassPublicShouldBeInFile {
                name: simple,
                name_range: Some(data.name_range()),
            });
        }
    }
    out
}

/// §7.6: no two class-like declarations of one source set share a fully
/// qualified name ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7),
/// [§7.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.6)) —
/// javac's `duplicate class` error, which spans files as well as a single
/// file. For every top-level class-like declaration of `file`, the set of
/// declarations sharing its FQN is the per-(source set, package, FQN) symbol
/// bucket ([`hir::source_set_fqn_symbols`]) — an O(1) slice of the
/// salsa-tracked per-package symbol index — and the *non-first* occurrences
/// are reported, each in its own file, at the declaration's name range
/// (matching javac, which reports on the later declaration).
///
/// The bucket is tracked per FQN, so the check recomputes soundly when a peer
/// file is edited (its FQN's bucket re-derives) and short-circuits when an
/// edit lands in a different package *or* changes a different declaration —
/// the LSP layer re-pulls the affected file's diagnostics lazily
/// ([`ide_diagnostics::file_report`]).
fn duplicate_class_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
) -> Vec<DeclDiagnostic> {
    if tree.language != LanguageKind::Java {
        return Vec::new();
    }
    let Some(source_set) = hir::source_set_for_file(db, file) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &top in &tree.top {
        let data = tree.data(top);
        if !is_class_like(data) {
            continue;
        }
        let Some(fqn) = hir::source_class_fqn(db, file, top) else {
            continue;
        };
        // The declaring file's package is the FQN minus its last segment (a
        // top-level class is `package.Simple`); the unnamed package
        // ([JLS §7.4.2]) leaves a bare simple name.
        let package = fqn.as_str().rsplit_once('.').map(|(p, _)| p).unwrap_or("");
        // The per-(source set, package, FQN) symbol bucket
        // ([`hir::source_set_fqn_symbols`]) is salsa-tracked per FQN, so a text
        // edit that changes a *different* declaration leaves this file's check
        // memoized; only files declaring the edited FQN re-run it.
        let class_refs: Vec<hir::SourceSymbolRef> =
            hir::source_set_fqn_symbols(db, source_set.clone(), &Name::new(package), &fqn)
                .iter()
                .filter(|reference| {
                    matches!(
                        reference.symbol.kind,
                        hir::SourceSymbolKind::Class
                            | hir::SourceSymbolKind::Interface
                            | hir::SourceSymbolKind::Enum
                            | hir::SourceSymbolKind::Record
                            | hir::SourceSymbolKind::Annotation
                    )
                })
                .cloned()
                .collect();
        if class_refs.len() < 2 {
            continue;
        }
        // Deterministic first-occurrence: the smallest (file, item).
        let mut sorted = class_refs.clone();
        sorted.sort_by_key(|reference| (reference.file, reference.symbol.item));
        let first = &sorted[0];
        if first.file == file && first.symbol.item == top {
            continue;
        }
        out.push(DeclDiagnostic::DuplicateClass {
            fqn: fqn.as_str().to_owned(),
            name_range: Some(data.name_range()),
        });
    }
    out
}

/// §8.3.1.2/[§16]: a blank (initializer-less) `final` instance field must be
/// assigned on every supertype-constructor path of the class — i.e. by every
/// constructor that does not delegate with `this(...)` (delegation hands the
/// requirement to the target constructor), with an instance initializer
/// counting for all paths; a blank `final` static field must be assigned in
/// the static initializers. A field that no such construct assigns is never
/// initialized — javac: `variable {f} might not have been initialized`,
/// reported at the field's name.
fn final_field_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: hir_def::java::item_tree::ItemId,
) -> Vec<DeclDiagnostic> {
    use hir_def::java::item_tree::ItemData as I;
    use hir_expand::body::{CtorCallTarget, ExprData, StmtData};
    let I::Class(class) = tree.data(item) else {
        return Vec::new();
    };
    // The blank final fields of the class: (name, is_static, name range).
    let fields: Vec<(Name, bool, rowan::TextRange)> = class
        .body
        .iter()
        .filter_map(|child| match tree.data(*child) {
            I::Field(f) if f.modifiers.is_final() && f.initializer.is_none() => {
                Some((f.name.clone(), f.modifiers.is_static(), f.name_range))
            }
            _ => None,
        })
        .collect();
    if fields.is_empty() {
        return Vec::new();
    }
    let bodies = hir::file_body_tree(db, file);

    /// Whether `name` is assigned anywhere in the statement forest of a body:
    /// a write `name = …` (or `this.name = …`) with a **plain** assignment
    /// operator — the only form that initializes a blank final. Walks blocks,
    /// branches, loops, switches, `try` and nested lambdas/initializers.
    fn body_assigns_name(bodies: &BodyTree, body: hir_expand::body::BodyId, name: &str) -> bool {
        fn walk_stmt(
            bodies: &BodyTree,
            stmt: hir_expand::body::StmtId,
            name: &str,
            found: &mut bool,
        ) {
            if *found {
                return;
            }
            match bodies.stmt(stmt) {
                StmtData::Expr(expr) => walk_expr(bodies, *expr, name, found),
                StmtData::Block(inner) | StmtData::DeclGroup(inner) => {
                    for s in inner {
                        walk_stmt(bodies, *s, name, found);
                    }
                }
                StmtData::Labeled { stmt: s, .. } => walk_stmt(bodies, *s, name, found),
                StmtData::If {
                    cond, then, els, ..
                } => {
                    walk_expr(bodies, *cond, name, found);
                    walk_stmt(bodies, *then, name, found);
                    if let Some(els) = els {
                        walk_stmt(bodies, *els, name, found);
                    }
                }
                StmtData::While { cond, body, .. } => {
                    walk_expr(bodies, *cond, name, found);
                    walk_stmt(bodies, *body, name, found);
                }
                StmtData::DoWhile { body, cond, .. } => {
                    walk_stmt(bodies, *body, name, found);
                    walk_expr(bodies, *cond, name, found);
                }
                StmtData::For {
                    init,
                    cond,
                    step,
                    body,
                } => {
                    for s in init {
                        walk_stmt(bodies, *s, name, found);
                    }
                    if let Some(cond) = cond {
                        walk_expr(bodies, *cond, name, found);
                    }
                    for e in step {
                        walk_expr(bodies, *e, name, found);
                    }
                    walk_stmt(bodies, *body, name, found);
                }
                StmtData::ForEach { iterable, body, .. } => {
                    walk_expr(bodies, *iterable, name, found);
                    walk_stmt(bodies, *body, name, found);
                }
                StmtData::Switch {
                    scrutinee, arms, ..
                } => {
                    walk_expr(bodies, *scrutinee, name, found);
                    for arm in arms {
                        for label in &arm.labels {
                            if let hir_expand::body::SwitchLabel::Expr(e) = label {
                                walk_expr(bodies, *e, name, found);
                            }
                        }
                        for s in &arm.body {
                            walk_stmt(bodies, *s, name, found);
                        }
                    }
                }
                StmtData::Return(ret) => {
                    if let Some(ret) = ret {
                        walk_expr(bodies, *ret, name, found);
                    }
                }
                StmtData::Throw(ret) | StmtData::Yield(ret) => {
                    walk_expr(bodies, *ret, name, found);
                }
                StmtData::Synchronized { expr, body } => {
                    walk_expr(bodies, *expr, name, found);
                    walk_stmt(bodies, *body, name, found);
                }
                StmtData::Try {
                    resources,
                    body,
                    catches,
                    finally,
                } => {
                    for r in resources {
                        if let Some(init) = r.initializer {
                            walk_expr(bodies, init, name, found);
                        }
                    }
                    walk_stmt(bodies, *body, name, found);
                    for c in catches {
                        walk_stmt(bodies, c.body, name, found);
                    }
                    if let Some(finally) = finally {
                        walk_stmt(bodies, *finally, name, found);
                    }
                }
                StmtData::Assert { cond, msg, .. } => {
                    walk_expr(bodies, *cond, name, found);
                    if let Some(msg) = msg {
                        walk_expr(bodies, *msg, name, found);
                    }
                }
                StmtData::Empty
                | StmtData::Break(_)
                | StmtData::Continue(_)
                | StmtData::LocalClass { .. }
                | StmtData::Missing => {}
                StmtData::Decl { .. } => {
                    // A declarator's initializer may itself assign.
                    if let StmtData::Decl {
                        local: _,
                        initializer,
                    } = bodies.stmt(stmt)
                        && let Some(init) = initializer
                    {
                        walk_expr(bodies, *init, name, found);
                    }
                }
            }
        }
        fn walk_expr(
            bodies: &BodyTree,
            expr: hir_expand::body::ExprId,
            name: &str,
            found: &mut bool,
        ) {
            if *found {
                return;
            }
            match bodies.expr(expr) {
                ExprData::Assign { op, lhs, rhs, .. } => {
                    // A plain assignment whose left-hand side is the name
                    // (or `this.name`) initializes the field.
                    if matches!(op, hir_expand::body::AssignOp::Assign) {
                        match bodies.expr(*lhs) {
                            ExprData::Var(n) => {
                                if n.as_str() == name {
                                    *found = true;
                                }
                            }
                            ExprData::FieldAccess {
                                target, name: n, ..
                            } => {
                                let receiver_is_this = match target {
                                    None => true,
                                    Some(t) => matches!(bodies.expr(*t), ExprData::This { .. }),
                                };
                                if receiver_is_this && n.as_str() == name {
                                    *found = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    walk_expr(bodies, *lhs, name, found);
                    walk_expr(bodies, *rhs, name, found);
                }
                ExprData::Template { args }
                | ExprData::ArrayInit(args)
                | ExprData::New {
                    args,
                    receiver: _,
                    diamond: _,
                    members: _,
                    ty: _,
                }
                | ExprData::CtorCall {
                    args, target: _, ..
                } => {
                    for a in args {
                        walk_expr(bodies, *a, name, found);
                    }
                }
                ExprData::FieldAccess { target, .. }
                | ExprData::MethodCall {
                    receiver: target, ..
                } => {
                    if let Some(t) = target {
                        walk_expr(bodies, *t, name, found);
                    }
                }
                ExprData::ArrayAccess { array, index } => {
                    walk_expr(bodies, *array, name, found);
                    walk_expr(bodies, *index, name, found);
                }
                ExprData::NewArray {
                    dims, initializer, ..
                } => {
                    for d in dims {
                        walk_expr(bodies, *d, name, found);
                    }
                    if let Some(elems) = initializer {
                        for e in elems {
                            walk_expr(bodies, *e, name, found);
                        }
                    }
                }
                ExprData::Unary { expr: inner, .. }
                | ExprData::Postfix { expr: inner, .. }
                | ExprData::Cast { ty: _, expr: inner }
                | ExprData::Paren(inner) => walk_expr(bodies, *inner, name, found),
                ExprData::Binary { lhs, rhs, .. } => {
                    walk_expr(bodies, *lhs, name, found);
                    walk_expr(bodies, *rhs, name, found);
                }
                ExprData::InstanceOf { expr: inner, .. } => {
                    walk_expr(bodies, *inner, name, found);
                }
                ExprData::Conditional { cond, then, els } => {
                    walk_expr(bodies, *cond, name, found);
                    walk_expr(bodies, *then, name, found);
                    walk_expr(bodies, *els, name, found);
                }
                ExprData::Lambda { body, .. } => match body {
                    hir_expand::body::LambdaBody::Expr(inner) => {
                        walk_expr(bodies, *inner, name, found)
                    }
                    hir_expand::body::LambdaBody::Block(stmt) => {
                        walk_stmt(bodies, *stmt, name, found)
                    }
                },
                ExprData::MethodRef { qualifier, .. } => {
                    if let Some(q) = qualifier {
                        walk_expr(bodies, *q, name, found);
                    }
                }
                ExprData::Switch {
                    scrutinee, arms, ..
                } => {
                    walk_expr(bodies, *scrutinee, name, found);
                    for arm in arms {
                        for s in &arm.body {
                            walk_stmt(bodies, *s, name, found);
                        }
                    }
                }
                ExprData::Literal(_)
                | ExprData::Null
                | ExprData::This { .. }
                | ExprData::Super { .. }
                | ExprData::ClassLit(_)
                | ExprData::Var(_)
                | ExprData::NamePath(_)
                | ExprData::Missing => {}
            }
        }
        let mut found = false;
        for s in bodies.body(body).stmts.iter().copied() {
            walk_stmt(bodies, s, name, &mut found);
        }
        found
    }

    let mut out = Vec::new();
    for (field, is_static, range) in &fields {
        let name = field.as_str();
        if *is_static {
            // A static final field must be assigned in a static initializer.
            let assigned = class.body.iter().any(|child| {
                if let I::StaticInit(init) = tree.data(*child)
                    && let Some(body_id) = init.body
                {
                    return body_assigns_name(&bodies, body_id, name);
                }
                false
            });
            if !assigned {
                out.push(DeclDiagnostic::FinalFieldNotInitialized {
                    field: field.clone(),
                    range: Some(*range),
                });
            }
        } else {
            // An instance final field is assigned on every path iff every
            // non-this-delegating constructor assigns it, or an instance
            // initializer does (it runs on every path).
            let inits_assign = class.body.iter().any(|child| {
                if let I::InstanceInit(init) = tree.data(*child)
                    && let Some(body_id) = init.body
                {
                    return body_assigns_name(&bodies, body_id, name);
                }
                false
            });
            if inits_assign {
                continue;
            }
            let unassigned_ctor = class.body.iter().any(|child| {
                let I::Method(ctor) = tree.data(*child) else {
                    return false;
                };
                if !ctor.is_constructor() {
                    return false;
                }
                let Some(body_id) = ctor.body() else {
                    return true;
                };
                // A this(...) delegating constructor defers to its target.
                let delegates = bodies.body(body_id).stmts.iter().any(|&stmt| {
                    matches!(
                        bodies.stmt(stmt),
                        StmtData::Expr(expr)
                            if matches!(
                                bodies.expr(*expr),
                                ExprData::CtorCall {
                                    target: CtorCallTarget::This,
                                    ..
                                }
                            )
                    )
                });
                !delegates && !body_assigns_name(&bodies, body_id, name)
            });
            if unassigned_ctor {
                out.push(DeclDiagnostic::FinalFieldNotInitialized {
                    field: field.clone(),
                    range: Some(*range),
                });
            }
        }
    }
    out
}

/// §8.1.1/[§8.4.3]: a declaration whose source modifier list carries two or
/// more modifiers the JLS forbids from co-occurring — two access modifiers,
/// `abstract` with `final`/`static`/`private`/`default`/`native`/
/// `synchronized`/`strictfp`, `final` with `sealed` or `volatile`, `sealed`
/// with `non-sealed`. javac reports the offending pair in a canonical order
/// (`illegal combination of modifiers: abstract, final`); the message here is
/// IntelliJ-style.
///
/// The lowered [`JavaModifiers`] cannot detect this: a duplicate visibility
/// overwrites the first and the modality/general flag sets OR, so the
/// co-occurrence is lost at lowering time. The raw modifier keywords are
/// therefore re-read from the file's cached parse tree — of the same revision
/// the item tree was lowered from.
fn modifier_combination_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
) -> Vec<DeclDiagnostic> {
    use syntax::java::SourceFile as JavaSourceFile;
    if tree.language != LanguageKind::Java {
        return Vec::new();
    }
    let parse = base_db::parse(db, file, LanguageKind::Java);
    let syntax::SourceFile::Java(JavaSourceFile { syntax_node }) =
        parse.syntax_node(LanguageKind::Java)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_decl_modifiers(&syntax_node, &mut out);
    out
}

/// Recursively walks `node` for modifier-bearing declarations, pushing an
/// [`IllegalModifierCombination`] for every conflicting pair of their
/// modifier lists, at the declaration's whole source range.
fn walk_decl_modifiers(
    node: &rowan::SyntaxNode<syntax::java::Lang>,
    out: &mut Vec<DeclDiagnostic>,
) {
    use syntax::java::SyntaxKind as J;
    for child in node.children() {
        if is_modifier_bearing_decl(child.kind())
            && let Some(modifier_list) = child.children().find(|c| c.kind() == J::MODIFIER_LIST)
        {
            let names = modifier_keywords(&modifier_list);
            for (first, second) in conflicting_modifiers(&names) {
                out.push(DeclDiagnostic::IllegalModifierCombination {
                    first,
                    second,
                    range: Some(child.text_range()),
                });
            }
        }
        walk_decl_modifiers(&child, out);
    }
}

/// Whether the syntax node is a declaration kind that carries a
/// `MODIFIER_LIST` ([JLS §8.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1),
/// [§8.3.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3.1),
/// [§8.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3)).
fn is_modifier_bearing_decl(kind: syntax::java::SyntaxKind) -> bool {
    use syntax::java::SyntaxKind as J;
    matches!(
        kind,
        J::CLASS_DECL
            | J::INTERFACE_DECL
            | J::ENUM_DECL
            | J::RECORD_DECL
            | J::ANNOTATION_TYPE_DECL
            | J::METHOD_DECL
            | J::CONSTRUCTOR_DECL
            | J::COMPACT_CONSTRUCTOR_DECL
            | J::FIELD_DECL
            | J::ANNOTATION_TYPE_ELEMENT_DECL
    )
}

/// The recognized modifier keywords of a `MODIFIER_LIST` node, in source
/// order (annotations are child *nodes* and skipped; the restricted keywords
/// `sealed` and `non-sealed` are lexed as `IDENTIFIER` tokens, [JLS §3.9]).
fn modifier_keywords(node: &rowan::SyntaxNode<syntax::java::Lang>) -> Vec<&'static str> {
    use rowan::NodeOrToken;
    use syntax::java::SyntaxKind as J;
    node.children_with_tokens()
        .filter_map(|element| match element {
            NodeOrToken::Node(_) => None,
            NodeOrToken::Token(token) => match token.kind() {
                J::PUBLIC_KW => Some("public"),
                J::PROTECTED_KW => Some("protected"),
                J::PRIVATE_KW => Some("private"),
                J::ABSTRACT_KW => Some("abstract"),
                J::FINAL_KW => Some("final"),
                J::STATIC_KW => Some("static"),
                J::DEFAULT_KW => Some("default"),
                J::NATIVE_KW => Some("native"),
                J::SYNCHRONIZED_KW => Some("synchronized"),
                J::TRANSIENT_KW => Some("transient"),
                J::VOLATILE_KW => Some("volatile"),
                J::STRICTFP_KW => Some("strictfp"),
                J::IDENTIFIER => match token.text() {
                    "sealed" => Some("sealed"),
                    "non-sealed" => Some("non-sealed"),
                    _ => None,
                },
                _ => None,
            },
        })
        .collect()
}

/// The illegal modifier pairs declared by `names`, each once, in the canonical
/// javac order (e.g. `abstract, final` for `final abstract`).
fn conflicting_modifiers(names: &[&'static str]) -> Vec<(&'static str, &'static str)> {
    fn push_unique(
        pairs: &mut Vec<(&'static str, &'static str)>,
        first: &'static str,
        second: &'static str,
    ) {
        if !pairs.contains(&(first, second)) {
            pairs.push((first, second));
        }
    }
    let mut pairs: Vec<(&'static str, &'static str)> = Vec::new();
    // More than one access modifier: every pair, canonicalized to
    // `public` < `protected` < `private` ([§6.6.1]).
    let access: Vec<&'static str> = names
        .iter()
        .copied()
        .filter(|name| matches!(*name, "public" | "protected" | "private"))
        .collect();
    if access.len() > 1 {
        let mut sorted = access.clone();
        sorted.sort_by_key(|name| match *name {
            "public" => 0,
            "protected" => 1,
            _ => 2,
        });
        for pair in sorted.windows(2) {
            push_unique(&mut pairs, pair[0], pair[1]);
        }
    }
    let has = |name: &'static str| names.contains(&name);
    // §8.4.3: `abstract` excludes the modifiers that turn it into a
    // contradiction — a concrete body, a static receiver, a private
    // inheritance, or an implementation keyword.
    if has("abstract") {
        for other in [
            "final",
            "static",
            "private",
            "default",
            "native",
            "synchronized",
            "strictfp",
        ] {
            if has(other) {
                push_unique(&mut pairs, "abstract", other);
            }
        }
    }
    // §8.1.1: a sealed class must not be final ([§8.1.1.2]).
    if has("final") && has("sealed") {
        push_unique(&mut pairs, "final", "sealed");
    }
    // §8.3.1: a `final` field cannot also be `volatile`.
    if has("final") && has("volatile") {
        push_unique(&mut pairs, "final", "volatile");
    }
    // §8.1.1: `sealed` and `non-sealed` are mutually exclusive.
    if has("sealed") && has("non-sealed") {
        push_unique(&mut pairs, "sealed", "non-sealed");
    }
    pairs
}
