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

use hir_def::java::item_tree::{ItemData, ItemId, ItemTree};
use hir_def::jvm::decl::ItemTypeRef;
use hir_expand::body::BodyTree;
use hir_expand::name::Name;
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;
use vfs::FileId;

use crate::java::method::{InvocationContext, InvocationMode};
use crate::java::range_ctx::range_ctx;
use crate::java::release_api::ReleaseApi;
use crate::java::resolve::scope_for_file;
use crate::java::subtyping;
use crate::java::ty::{Ty, TyKind, TypeVarScope};
use crate::jvm::db::TyDatabase;
use crate::jvm::member::{Access, MethodData};
use crate::jvm::member_set::{all_methods_raw, inherited_defaults, member_set};
use base_db::LanguageKind;
use hir_def::java::ranges;

/// The source range of an item's declared name, resolved on demand from the
/// file's parse (mirror of the old `ItemData::name_range`).
fn item_name_range(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
    id: ItemId,
) -> Option<rowan::TextRange> {
    let (map, source) = range_ctx(db, file, tree.language)?;
    hir_def::java::ranges::item_name_range(map, &source, tree, id)
}

/// The source range of the first reference name of a declaration type
/// reference (mirror of the old `SpannedTypeRef::first_ref`).
fn first_type_ref_range(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
    tyref: &ItemTypeRef,
) -> Option<rowan::TextRange> {
    let (map, source) = range_ctx(db, file, tree.language)?;
    hir_def::java::ranges::type_ref_occurrences(map, &source, tyref)
        .into_iter()
        .next()
        .and_then(|(_, range)| range)
}

/// The source range of a declaration type reference as written — the whole
/// `TYPE` node, so qualifiers and type-use annotations are covered
/// ([`hir_def::java::ranges::type_ref_range`]).
fn type_ref_range(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &ItemTree,
    tyref: &ItemTypeRef,
) -> Option<rowan::TextRange> {
    let (map, source) = range_ctx(db, file, tree.language)?;
    hir_def::java::ranges::type_ref_range(map, &source, tyref)
}

/// The declaration a *duplicate local class* re-declares within
/// ([JLS §6.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.4),
/// [§8.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1)):
/// the *enclosing* declaration whose scope already holds the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateContainer {
    /// §6.4's case: a member body — a method, constructor or initializer body
    /// — in whose scope the name is already declared. `noun` is
    /// `method`/`constructor`/`static initializer`/`instance initializer` and
    /// `name` the declaration's own name (`None` for an initializer).
    Member {
        noun: &'static str,
        name: Option<Name>,
    },
    /// §8.1/[§9.1]'s case: the local declaration has the same simple name as
    /// an enclosing class or interface.
    EnclosingType { name: Name },
}

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
    /// `static` (static methods hide, they never override). `range` is the
    /// annotated method's name range, so the diagnostic stays on the right
    /// overload when several methods share the name.
    MethodDoesNotOverride {
        method: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.6.4.4/[§8.4.8.2]: an `@Override` annotation on a `static` method —
    /// a static method never overrides (it hides), so the annotation is
    /// always an error. javac keeps a dedicated message for this shape
    /// (`static methods cannot be annotated with @Override`), distinct from
    /// the generic does-not-override error of an instance method that
    /// matches no supertype method.
    MethodDoesNotOverrideStatic {
        method: Name,
        range: Option<rowan::TextRange>,
    },
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
    /// §9.6.4.1: an annotation is applied in a *declaration context* whose
    /// element type is not in its `@Target` set, and the use site is not a
    /// type context the annotation may attach to instead ([§9.7.4]). javac:
    /// `annotation interface not applicable to this kind of declaration`
    /// (`compiler.err.annotation.type.not.applicable`); IntelliJ:
    /// `'@X' not applicable to {target}`. `name` is the annotation's
    /// (possibly qualified) name; `element_type` is the `ElementType`
    /// constant of the annotated declaration — the target IntelliJ names in
    /// its message ([`element_type_display`]).
    AnnotationNotApplicable {
        name: Name,
        element_type: &'static str,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.4: an annotation is applied in a *type context* — a type
    /// argument, an array dimension, a cast, a class literal, a `new`, an
    /// `instanceof` type, ... — but its `@Target` does not contain
    /// `TYPE_USE`, which every type context requires ([§9.6.4.1]). javac:
    /// `annotation @X not applicable in this type context`
    /// (`compiler.err.annotation.type.not.applicable.to.type`); IntelliJ:
    /// `'@X' not applicable to type use`.
    AnnotationNotApplicableToType {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.4: an annotation that is applicable only in type contexts is
    /// written before a type that is not written in source at all — a `var`
    /// variable declaration or `var` lambda parameter ([§14.4], [§15.27.1])
    /// — so the annotation has no closest type to apply to, which is a
    /// compile-time error. IntelliJ: `'var' type may not be annotated`; javac
    /// reports the declaration-context code
    /// `compiler.err.annotation.type.not.applicable`.
    AnnotatedVar {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1: an annotation element-value pair names an element the annotation
    /// type does not declare, but the name *did* resolve to a member of the
    /// annotation interface ([§9.2]) — a method inherited from another type,
    /// `java.lang.Object.toString` above all. javac reports this sub-case as
    /// `no annotation member named {name}`
    /// (`compiler.err.no.annotation.member`). `range` is the source range of
    /// the offending value expression.
    UnknownAnnotationMember {
        name: Name,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1/[§6.5.5.1]: an annotation element-value pair names no member of
    /// the annotation interface at all, so the *name* does not resolve — javac
    /// reports the failed resolution of the name,
    /// `compiler.err.cant.resolve.location.args`
    /// (`cannot find symbol … kindname.method, {name}, …`), not
    /// [`UnknownAnnotationMember`](Self::UnknownAnnotationMember), whose key it
    /// prints only for a name owned by another type
    /// (`Annotate.attributeAnnotationNameValuePair`). `range` is the value, the
    /// position the client underlines for both.
    UnresolvedAnnotationMember {
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
    /// §9.7.1: a normal annotation must contain an element-value pair for
    /// every element of its annotation interface except those with a default
    /// value. javac: `annotation @X is missing a default value for the element
    /// 'y'` (`compiler.err.annotation.missing.default.value`); IntelliJ:
    /// `'x' missing but required` — for several elements, `'x', 'y' missing but
    /// required`. `names` are the elements without a pair, in declaration
    /// order; `range` is the annotation's *name*, where IntelliJ anchors the
    /// report.
    MissingAnnotationElement {
        names: Vec<Name>,
        range: Option<rowan::TextRange>,
    },
    /// §9.7.1: an element value for a primitive- or `String`-typed element must
    /// be a constant expression ([§15.29]). javac: `element value must be a
    /// constant expression` (`compiler.err.attribute.value.must.be.constant`);
    /// IntelliJ: `Attribute value must be constant`. `range` is the value.
    NonConstantAnnotationElement { range: Option<rowan::TextRange> },
    /// §9.7.1: an element value for a `Class`-typed element must be a class
    /// literal ([§15.8.2]). javac: `element value must be a class literal`
    /// (`compiler.err.annotation.value.must.be.class.literal`); IntelliJ:
    /// `Attribute value must be a class literal`. `range` is the value — a
    /// parenthesized class literal is not one, parentheses included.
    AnnotationElementNotClassLiteral { range: Option<rowan::TextRange> },
    /// §9.7.1: an element value for an enum-typed element must be an enum
    /// constant ([§8.9.1]). javac: `annotation value must be an enum constant`
    /// (`compiler.err.enum.annotation.must.be.enum.constant`); IntelliJ:
    /// `Attribute value must be an enum constant`. `range` is the value.
    AnnotationElementNotEnumConstant { range: Option<rowan::TextRange> },
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
    /// §8.1.5/[§9.1.3]: a type named by an `implements` clause (of a class,
    /// enum or record) or by an interface's `extends` clause is not an
    /// interface. javac: `interface expected here`; the message is IntelliJ's
    /// `Interface expected here`. `range` spans the written type reference
    /// (qualified name and type-use annotations included).
    InterfaceExpectedHere { range: Option<rowan::TextRange> },
    /// §8.1.4: a type named by a class declaration's `extends` clause is an
    /// interface, not a class — an interface can never be a superclass. javac:
    /// `no interface expected here`; the message is IntelliJ's `No interface
    /// expected here`. `range` spans the written type reference (qualified
    /// name and type-use annotations included).
    NoInterfaceExpectedHere { range: Option<rowan::TextRange> },
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
    /// §8.4.8.1/[§8.4.8.2]: a method whose signature is that of an inherited
    /// *instance* method, declared `static` (javac `overriding method is
    /// static`), or whose signature is that of an inherited *static* method,
    /// declared as an instance method (javac `overridden method is static`).
    /// A static method can never override an instance method — it can only
    /// *hide* a static one ([§8.4.8.2]) — and an instance method can never
    /// hide a static one, so either mixed-staticness redeclaration of an
    /// inherited signature is an error. `super_owner` is the declaring class
    /// of the inherited method; `overriding_is_static` says which direction
    /// the clash runs: `true` when the *new* declaration is the static one.
    StaticInstanceClash {
        method: Name,
        super_owner: Name,
        overriding_is_static: bool,
    },
    /// §8.4.8.3: an override or implementation declares a `throws` clause
    /// naming a checked exception type that is not a subtype of one the
    /// overridden method throws — the override may only *narrow* the checked
    /// exceptions, never broaden them ([§8.4.8.3]). Unchecked additions
    /// (`RuntimeException`, `Error`, their subtypes) are always allowed.
    /// `super_owner` is the declaring class of the overridden method and
    /// `thrown` the offending added checked type, rendered simple.
    IncompatibleThrows {
        method: Name,
        super_owner: Name,
        thrown: Ty,
    },
    /// §9.4.1.2/[§8.4.8.2]: an interface `default` or `static` method whose
    /// signature matches a `public` (or `protected` *final*) method of
    /// `java.lang.Object`. `Object` methods are *not* abstract interface
    /// members: a class's implementation always comes from `Object` itself,
    /// so an interface cannot override them — a `default` declaration is an
    /// error ([§9.4.1.2], javac `default method {m} in {I} overrides a
    /// member of java.lang.Object`), and a `static` one would have to be an
    /// override too (javac `overriding method is static`), which is likewise
    /// impossible. Abstract redeclarations of those signatures stay legal —
    /// they merely restate the inherited `Object` contract
    /// ([§9.4.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.4.1.2)).
    /// `method` is the offending method name and `is_static` its modifier.
    CannotOverrideObjectMethod { method: Name, is_static: bool },
    /// §8.9.2: an enum constructor's first statement may not be an explicit
    /// `super(...)` invocation — the implicit superclass `java.lang.Enum`
    /// has no constructor accessible to the enum (the compiler supplies the
    /// superclass arguments itself, [§8.9.2]). javac: `call to super not
    /// allowed in enum constructor`; the message here is javac's,
    /// IntelliJ-style. `range` is the `super(...)` call.
    EnumCtorSuperCall { range: Option<rowan::TextRange> },
    /// §8.10.4: a record's *canonical* constructor — the constructor whose
    /// parameter types mirror the record's components ([§8.10.4]) — must
    /// declare its parameters with exactly the component *names*. A normal
    /// constructor `R(int z)` next to `record R(int x)` is not canonical
    /// (javac: `invalid canonical constructor in record R … invalid
    /// parameter names in canonical constructor`). Only the *first*
    /// constructor with the component types counts (the canonical
    /// constructor); a later one with the same types is a duplicate. The
    /// parameters are matched positionally to the components.
    RecordCtorParamNameMismatch {
        record: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.9.1: an enum body's members must follow all its constants — the
    /// first non-constant member before a `;` ends the constant section, and
    /// any constant after that `;` is an error ([§8.9.1]). javac:
    /// `enum constant expected here` (the member that ended the constant
    /// section early, or the parser recovery at its place); the message here
    /// is javac's, IntelliJ-style. A constant after the `;` is the separate
    /// [`EnumConstantNotExpected`](Self::EnumConstantNotExpected). `range`
    /// is the offending member's name.
    EnumMemberBeforeConstants { range: Option<rowan::TextRange> },
    /// §8.9.1: an enum constant declared *after* the separating `;` of the
    /// constant section — javac: `enum constant not expected here`, at the
    /// constant.
    EnumConstantNotExpected { range: Option<rowan::TextRange> },
    /// §8.4.2/[§8.4.1]: two methods declared by one class have the *same*
    /// signature (identical parameter types and name) — the later
    /// declaration is an error. javac: `method {m} is already defined in
    /// class {C}` (constructors, whose signature is the parameter list
    /// alone, are reported as `constructor {m} is already defined`). (With
    /// the signature differing only in parameterized types, the erasure is
    /// shared but the signatures differ — that is the separate
    /// [`NameClashSameErasure`] error, [§8.4.2].)
    DuplicateMethod {
        method: Name,
        is_constructor: bool,
        range: Option<rowan::TextRange>,
    },
    /// §8.4.1/[§4.6]: one method's last formal parameter is a fixed array
    /// (`m(String[])`) and another's is the variable arity of the same
    /// component type (`m(String...)`) — the two signatures have the same
    /// erasure ([§8.4.2]) and the JVM cannot load both. javac: `cannot
    /// declare both {m}(String[]) and {m}(String...) in {C}`; the message
    /// here is javac's, IntelliJ-style. `method` is the common name and
    /// `array` the shared last-parameter array type, from which the message
    /// renders the fixed-array spelling (as lowered) and the varargs
    /// spelling (its element with `...`).
    CannotDeclareBothVarargsAndArray { method: Name, array: Ty },
    /// §8.4.5/[§9.4]: an `abstract` or `native` method carries a body — an
    /// abstract method declares behavior for its subtypes to provide, a
    /// native method declares a platform implementation, so neither may
    /// define a Java body. javac: `abstract methods cannot have a body` /
    /// `interface abstract methods cannot have a body` /
    /// `native methods cannot have a body`; the message here is javac's,
    /// IntelliJ-style. `method` is the method's name; `abstract_` says which
    /// modifier is violated.
    AbstractOrNativeMethodWithBody {
        method: Name,
        abstract_: bool,
        range: Option<rowan::TextRange>,
    },
    /// §8.8.9/[§11.2]: a class declares no constructor and inherits its
    /// implicit default constructor, whose body is exactly `super()` — a
    /// direct superclass constructor that throws a checked exception makes
    /// that implicit call an unhandled throw, and the default constructor
    /// declares no `throws` clause, so the exception is unreported (javac:
    /// `unreported exception {E} in default constructor`). An abstract
    /// subclass is exempt — it need not be instantiable
    /// ([§8.1.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1.1)),
    /// and javac checks the liability only at the first concrete
    /// descendant. `class` is the reported subclass, `super_owner` the
    /// declaring class of the throwing no-argument constructor, and `thrown`
    /// the offending checked type.
    DefaultCtorUnreportedException {
        class: Name,
        super_owner: Name,
        thrown: Ty,
        range: Option<rowan::TextRange>,
    },
    /// §11.2.2 (with §8.8.7): a declared constructor whose body contains no
    /// explicit constructor invocation implicitly calls `super()` before every
    /// statement, so a checked exception the superclass's no-argument
    /// constructor declares can neither be caught by a `try` in the body nor
    /// left discharged by anything but the constructor's own `throws` clause.
    /// javac: `unreported exception {E}; must be caught or declared to be
    /// thrown`; the message here is IntelliJ's `Unhandled exception: {E}`.
    /// `thrown` is the offending checked type, `range` the constructor's name.
    CtorUnreportedException {
        thrown: Ty,
        range: Option<rowan::TextRange>,
    },
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
    /// §8.1.1.2: a class directly extends — or a class or interface directly
    /// implements/extends — a `sealed` supertype without being named in its
    /// `permits` clause. javac: `class is not allowed to extend sealed class
    /// {S}`; the message is IntelliJ's `Cannot inherit from sealed
    /// 'Shape'`. `super_owner` is the sealed supertype (rendered simple) and
    /// `range` the subclass's name range.
    CantInheritFromSealed {
        super_owner: Name,
        range: Option<rowan::TextRange>,
    },
    /// §8.1.1.2: a permitted direct subclass of a `sealed` supertype is
    /// itself neither `sealed`, `non-sealed` nor `final` — it must be one of
    /// the three so the hierarchy stays closed. javac: `{sealed, non-sealed
    /// or final} expected`; the message is javac's, IntelliJ-style. `range`
    /// is the subclass's name range.
    SealedSealedOrFinalExpected { range: Option<rowan::TextRange> },
    /// §8.1.1.2: a `sealed` class or interface has no direct subclasses — a
    /// sealed type must have at least one. javac: `sealed class must have
    /// subclasses`; the message is javac's, IntelliJ-style. `range` is the
    /// sealed declaration's name range. Conservative: only reported when the
    /// sealed type declares no `permits` clause and no direct subclass
    /// appears in the same file (cross-file hierarchies are not provably
    /// subclass-less).
    SealedClassMustHaveSubclasses { range: Option<rowan::TextRange> },
    /// A construct whose Java *source* level is newer than the level the file's
    /// source set is compiled at. javac reports
    /// `compiler.err.feature.not.supported.in.source` (or
    /// `compiler.err.preview.feature.disabled` for a preview feature used
    /// without `--enable-preview`). The construct is still typed at the newest
    /// level, so the IDE keeps working inside it.
    FeatureRequiresNewerSourceLevel {
        /// The construct newer than the source level.
        feature: crate::java::level_check::JavaFeature,
        /// The project's source level.
        found: u8,
        /// The level at which the feature is standard.
        required: u8,
        /// Use the preview wording instead of the source-level wording.
        preview_disabled: bool,
        range: Option<rowan::TextRange>,
    },
    /// §9.4: a modifier on an interface member declaration that the JLS
    /// forbids for that member's kind — a `protected` interface method, for
    /// example ([§9.4]). javac: `modifier {m} not allowed here`; the message
    /// is javac's, IntelliJ-style. `range` is the offending modifier's source
    /// range.
    ModifierNotAllowedHere {
        modifier: &'static str,
        range: Option<rowan::TextRange>,
    },
    /// §14.3: a `sealed` or `non-sealed` modifier on a local class or
    /// interface declaration. javac: `sealed or non-sealed local classes are
    /// not allowed`; the message is IntelliJ's `Sealed or non-sealed local
    /// classes are not allowed`, at the modifier's own range.
    SealedOrNonSealedLocalClass {
        modifier: &'static str,
        range: Option<rowan::TextRange>,
    },
    /// §14.3: the direct superclass or a direct superinterface of a local
    /// class declaration — or a direct superinterface of a local interface
    /// declaration — is `sealed`. javac: `local classes must not extend
    /// sealed classes`; the message is the sibling `Cannot inherit from
    /// sealed 'S'`, at the written supertype reference.
    LocalClassCantExtendSealed {
        super_owner: Name,
        range: Option<rowan::TextRange>,
    },
    /// §6.4/[§8.1]/[§9.1]: a local class or interface declaration re-declares
    /// a name already in scope as a local declaration, or has the same simple
    /// name as an enclosing class or interface. javac reports both under
    /// `compiler.err.already.defined` (`…in method m()`, `…in {package}`), or
    /// `compiler.err.already.defined.in.clinit` when the enclosing
    /// declaration is an initializer; the message is the sibling
    /// `DuplicateMethod`'s shape. `range` is the redeclaring declaration's
    /// name.
    DuplicateLocalClass {
        name: Name,
        /// The declaration's noun: `Class`, `Interface`, `Enum` or `Record`
        /// ([§14.3]).
        kind: &'static str,
        container: DuplicateContainer,
        range: Option<rowan::TextRange>,
    },
    /// §8.4.5/[§9.4: a method with no body that is neither `abstract` nor
    /// `native` — an interface `private`/`static`/`default` method without a
    /// body, or a class method that should be `abstract`. javac: `missing
    /// method body, or declare abstract`; the message is javac's,
    /// IntelliJ-style. `method` is the method name and `range` its name
    /// range.
    MissingMethodBodyOrDeclareAbstract {
        method: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.7.1: a `requires` directive names a module that is not on the
    /// module path — no source module and no classpath library declares it.
    /// javac: `module not found: {m}`; the message is javac's,
    /// IntelliJ-style. `range` is the required module name's source range.
    ModuleNotFound {
        module: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.7.1: an `exports`/`opens` directive names a package that has no
    /// source files in the module — the package is empty or does not exist.
    /// javac: `package is empty or does not exist: {p}`; the message is
    /// javac's, IntelliJ-style. `range` is the package name's source range.
    PackageEmptyOrNotFound {
        package: Name,
        range: Option<rowan::TextRange>,
    },
    /// §7.7.2: a `provides` directive names an implementation type that is
    /// not a subtype of the service interface. javac: `service implementation
    /// must be a subtype of service interface`; the message is javac's,
    /// IntelliJ-style. `range` is the implementation type's name range.
    ServiceImplementationNotSubtype {
        service: Ty,
        implementation: Ty,
        range: Option<rowan::TextRange>,
    },
    /// §4.8/§4.12.2: a *declaration* type reference names a generic class
    /// without its type arguments. javac: `found raw type: {ty}`; the message
    /// is javac's, IntelliJ-style. `range` spans the type reference.
    RawTypeUse {
        ty: Ty,
        range: Option<rowan::TextRange>,
    },
    /// §4.5: a *declaration* type reference carries the wrong number of type
    /// arguments for the class it names. javac: `wrong number of type
    /// arguments; required {n}` (`type {C} does not take parameters` for a
    /// non-generic class). `range` spans the type reference.
    WrongTypeArgumentCount {
        ty: Ty,
        expected: usize,
        range: Option<rowan::TextRange>,
    },
    /// §9.6.4.7: `@SafeVarargs` on a declaration that cannot suppress heap
    /// pollution — anything but a variable-arity `static`, `final` or
    /// `private` method. javac: `Invalid SafeVarargs annotation. …`.
    InvalidSafeVarargs {
        reason: SafeVarargsRejection,
        range: Option<rowan::TextRange>,
    },
    /// §9.6.4.9: `@FunctionalInterface` on something that is not a functional
    /// interface — not an interface, or one whose abstract-method count
    /// ([§9.8]) is not one. javac: `Unexpected @FunctionalInterface
    /// annotation`.
    NotAFunctionalInterfaceAnnotation { range: Option<rowan::TextRange> },
    /// JEP 247: the platform API this reference resolved to is not part of the
    /// release the compilation unit targets, so `javac --release` rejects it
    /// ([JLS §7.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.3),
    /// [JLS §13.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-13.html#jls-13.1)).
    NotSupportedInRelease {
        api: ReleaseApi,
        /// The release the source set compiles against.
        found: u8,
        /// The earliest release whose platform view provides the API.
        added: u8,
        range: Option<rowan::TextRange>,
    },
    /// §9.6.4.6: a declaration-position reference to an element declared
    /// `@Deprecated` — a supertype, a member's written type, an annotation
    /// type, or an overriding method whose overridden declaration is
    /// deprecated.
    DeprecatedUse {
        api: crate::java::deprecation::DeprecatedApi,
        deprecation: crate::java::deprecation::Deprecation,
        range: Option<rowan::TextRange>,
    },
}

/// Why `@SafeVarargs` was rejected ([JLS §9.6.4.7]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeVarargsRejection {
    /// The annotation is not on a method or constructor at all.
    NotAMethod,
    /// Not a variable-arity method ([§8.4.1]).
    NotVarargs,
    /// An instance method that is neither `final` nor `private` — a subclass
    /// may override it with a different arity.
    Instance,
}

impl DeclDiagnostic {
    /// The name of the offending method, for rendering.
    pub fn method_name(&self) -> &str {
        match self {
            DeclDiagnostic::IncompatibleOverride { method, .. }
            | DeclDiagnostic::ConflictingDefaults { method }
            | DeclDiagnostic::MethodDoesNotOverride { method, .. }
            | DeclDiagnostic::MethodDoesNotOverrideStatic { method, .. }
            | DeclDiagnostic::CannotOverrideFinalMethod { method, .. }
            | DeclDiagnostic::WeakerAccessPrivileges { method, .. }
            | DeclDiagnostic::StaticInstanceClash { method, .. }
            | DeclDiagnostic::IncompatibleThrows { method, .. }
            | DeclDiagnostic::CannotOverrideObjectMethod { method, .. }
            | DeclDiagnostic::DuplicateMethod { method, .. }
            | DeclDiagnostic::CannotDeclareBothVarargsAndArray { method, .. }
            | DeclDiagnostic::AbstractOrNativeMethodWithBody { method, .. }
            | DeclDiagnostic::NameClashSameErasure { method, .. } => method.as_str(),
            DeclDiagnostic::DefaultCtorUnreportedException { .. }
            | DeclDiagnostic::CtorUnreportedException { .. }
            | DeclDiagnostic::EnumCtorSuperCall { .. }
            | DeclDiagnostic::RecordCtorParamNameMismatch { .. }
            | DeclDiagnostic::EnumMemberBeforeConstants { .. }
            | DeclDiagnostic::EnumConstantNotExpected { .. }
            | DeclDiagnostic::CannotResolveType { .. }
            | DeclDiagnostic::AmbiguousName { .. }
            | DeclDiagnostic::UnresolvedImport { .. }
            | DeclDiagnostic::UnresolvedImportPackage { .. }
            | DeclDiagnostic::UnresolvedStaticImport { .. }
            | DeclDiagnostic::ConflictingImport { .. }
            | DeclDiagnostic::ModuleNotAccessible { .. }
            | DeclDiagnostic::DeprecatedUse { .. }
            | DeclDiagnostic::RawTypeUse { .. }
            | DeclDiagnostic::WrongTypeArgumentCount { .. }
            | DeclDiagnostic::InvalidSafeVarargs { .. }
            | DeclDiagnostic::NotAFunctionalInterfaceAnnotation { .. }
            | DeclDiagnostic::UnexpectedPackagePath { .. }
            | DeclDiagnostic::DuplicatePackage { .. }
            | DeclDiagnostic::DuplicateClass { .. }
            | DeclDiagnostic::ClassPublicShouldBeInFile { .. }
            | DeclDiagnostic::AnnotationNotApplicable { .. }
            | DeclDiagnostic::AnnotationNotApplicableToType { .. }
            | DeclDiagnostic::AnnotatedVar { .. }
            | DeclDiagnostic::UnknownAnnotationMember { .. }
            | DeclDiagnostic::UnresolvedAnnotationMember { .. }
            | DeclDiagnostic::DuplicateAnnotationMemberValue { .. }
            | DeclDiagnostic::AnnotationElementTypeMismatch { .. }
            | DeclDiagnostic::UnknownAnnotationElementConstant { .. }
            | DeclDiagnostic::MissingAnnotationElement { .. }
            | DeclDiagnostic::NonConstantAnnotationElement { .. }
            | DeclDiagnostic::AnnotationElementNotClassLiteral { .. }
            | DeclDiagnostic::AnnotationElementNotEnumConstant { .. }
            | DeclDiagnostic::ConstructorNameMismatch { .. }
            | DeclDiagnostic::IllegalModifierCombination { .. }
            | DeclDiagnostic::CannotInheritFromFinalClass { .. }
            | DeclDiagnostic::InterfaceExpectedHere { .. }
            | DeclDiagnostic::NoInterfaceExpectedHere { .. }
            | DeclDiagnostic::UnimplementedAbstractMethod { .. }
            | DeclDiagnostic::CyclicInheritance { .. }
            | DeclDiagnostic::NoDefaultConstructor { .. }
            | DeclDiagnostic::RecursiveConstructorInvocation { .. }
            | DeclDiagnostic::DuplicateDeclaration { .. }
            | DeclDiagnostic::FinalFieldNotInitialized { .. }
            | DeclDiagnostic::GenericCannotExtendThrowable { .. }
            | DeclDiagnostic::CantInheritFromSealed { .. }
            | DeclDiagnostic::SealedSealedOrFinalExpected { .. }
            | DeclDiagnostic::SealedClassMustHaveSubclasses { .. }
            | DeclDiagnostic::FeatureRequiresNewerSourceLevel { .. }
            | DeclDiagnostic::ModifierNotAllowedHere { .. }
            | DeclDiagnostic::SealedOrNonSealedLocalClass { .. }
            | DeclDiagnostic::LocalClassCantExtendSealed { .. }
            | DeclDiagnostic::DuplicateLocalClass { .. }
            | DeclDiagnostic::MissingMethodBodyOrDeclareAbstract { .. }
            | DeclDiagnostic::ModuleNotFound { .. }
            | DeclDiagnostic::PackageEmptyOrNotFound { .. }
            | DeclDiagnostic::ServiceImplementationNotSubtype { .. }
            | DeclDiagnostic::NotSupportedInRelease { .. } => "",
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
            DeclDiagnostic::RawTypeUse { range, .. }
            | DeclDiagnostic::WrongTypeArgumentCount { range, .. }
            | DeclDiagnostic::InvalidSafeVarargs { range, .. }
            | DeclDiagnostic::NotAFunctionalInterfaceAnnotation { range } => *range,
            DeclDiagnostic::UnexpectedPackagePath { name_range, .. } => *name_range,
            DeclDiagnostic::DuplicatePackage { name_range, .. }
            | DeclDiagnostic::DuplicateClass { name_range, .. }
            | DeclDiagnostic::ClassPublicShouldBeInFile { name_range, .. }
            | DeclDiagnostic::AnnotationNotApplicable {
                range: name_range, ..
            }
            | DeclDiagnostic::AnnotationNotApplicableToType {
                range: name_range, ..
            }
            | DeclDiagnostic::AnnotatedVar {
                range: name_range, ..
            }
            | DeclDiagnostic::UnknownAnnotationMember {
                range: name_range, ..
            }
            | DeclDiagnostic::UnresolvedAnnotationMember {
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
            | DeclDiagnostic::MissingAnnotationElement {
                range: name_range, ..
            }
            | DeclDiagnostic::NonConstantAnnotationElement {
                range: name_range, ..
            }
            | DeclDiagnostic::AnnotationElementNotClassLiteral {
                range: name_range, ..
            }
            | DeclDiagnostic::AnnotationElementNotEnumConstant {
                range: name_range, ..
            }
            | DeclDiagnostic::ConstructorNameMismatch {
                range: name_range, ..
            }
            | DeclDiagnostic::MethodDoesNotOverride {
                range: name_range, ..
            }
            | DeclDiagnostic::DuplicateMethod {
                range: name_range, ..
            }
            | DeclDiagnostic::AbstractOrNativeMethodWithBody {
                range: name_range, ..
            }
            | DeclDiagnostic::DefaultCtorUnreportedException {
                range: name_range, ..
            }
            | DeclDiagnostic::EnumCtorSuperCall {
                range: name_range, ..
            }
            | DeclDiagnostic::RecordCtorParamNameMismatch {
                range: name_range, ..
            }
            | DeclDiagnostic::EnumMemberBeforeConstants {
                range: name_range, ..
            }
            | DeclDiagnostic::EnumConstantNotExpected {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::StaticInstanceClash { .. }
            | DeclDiagnostic::IncompatibleThrows { .. }
            | DeclDiagnostic::CannotOverrideObjectMethod { .. }
            | DeclDiagnostic::CannotDeclareBothVarargsAndArray { .. } => None,
            DeclDiagnostic::IllegalModifierCombination {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::CannotInheritFromFinalClass {
                range: name_range, ..
            }
            | DeclDiagnostic::InterfaceExpectedHere {
                range: name_range, ..
            }
            | DeclDiagnostic::NoInterfaceExpectedHere {
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
            DeclDiagnostic::CtorUnreportedException {
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
            DeclDiagnostic::CantInheritFromSealed {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::SealedSealedOrFinalExpected {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::SealedClassMustHaveSubclasses {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::FeatureRequiresNewerSourceLevel {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::ModifierNotAllowedHere {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::SealedOrNonSealedLocalClass {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::LocalClassCantExtendSealed {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::DuplicateLocalClass {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::MissingMethodBodyOrDeclareAbstract {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::ModuleNotFound {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::PackageEmptyOrNotFound {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::ServiceImplementationNotSubtype {
                range: name_range, ..
            } => *name_range,
            DeclDiagnostic::NotSupportedInRelease {
                range: name_range, ..
            }
            | DeclDiagnostic::DeprecatedUse {
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

/// The module-directive diagnostics of the `module-info.java` in `file`
/// ([JLS §7.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.7)):
/// a `requires` of an unknown module, an `exports`/`opens` of a package with
/// no source files in the module, and a `provides` implementation that is not
/// a subtype of its service. Empty for files without a module declaration.
pub fn module_diagnostics(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    crate::java::db::module_diagnostics_query(db, db.file_text(file))
}

/// §7.7.1/[§7.7.2: the module-directive checks of `file`'s `module-info.java`.
pub(crate) fn module_diagnostics_impl(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let tree = hir::java_item_tree(db, file);
    let Some(top) = tree
        .top
        .iter()
        .copied()
        .find(|&t| matches!(tree.data(t), ItemData::Module(_)))
    else {
        return Vec::new();
    };
    let ItemData::Module(data) = tree.data(top) else {
        return Vec::new();
    };
    let scope = scope_for_file(db, file);
    let ctx = hir::module_ctx_for_scope(db, &scope);
    let interner = &db.hir_state().interner;
    let resolver = crate::java::resolve::Resolver::for_item(db, file, &tree, top);
    let mut out = Vec::new();

    // §7.7.1: a `requires` directive must name a module on the module path —
    // a source module or a classpath library.
    for req in &data.requires {
        let symbol = interner.get_or_intern(req.name.as_str());
        if ctx.graph.module(symbol).is_none() {
            let range = range_ctx(db, file, tree.language)
                .and_then(|(map, source)| ranges::requires_name_range(map, &source, req));
            out.push(DeclDiagnostic::ModuleNotFound {
                module: req.name.clone(),
                range,
            });
        }
    }

    // §7.7.1: an `exports`/`opens` directive must name a package that has
    // source files in the module — a package with no files is empty or does
    // not exist ([§7.7.1]).
    if let Some(source_set) = hir::source_set_for_file(db, file) {
        for export in data.exports.iter().chain(data.opens.iter()) {
            if hir::source_set_package_files(db, source_set.clone(), &export.package).is_empty() {
                let range = range_ctx(db, file, tree.language).and_then(|(map, source)| {
                    ranges::module_exports_package_range(map, &source, export)
                });
                out.push(DeclDiagnostic::PackageEmptyOrNotFound {
                    package: export.package.clone(),
                    range,
                });
            }
        }
    }

    // §7.7.2: a `provides` directive's implementation type must be a subtype
    // of its service interface ([§7.7.2]).
    for provide in &data.provides {
        let service =
            crate::java::resolve::resolve_type_ref(db, &scope, &resolver, &provide.service);
        for implementation in &provide.implementations {
            let implementation_ty =
                crate::java::resolve::resolve_type_ref(db, &scope, &resolver, implementation);
            if !service.is_error(db)
                && !implementation_ty.is_error(db)
                && !crate::java::subtyping::is_subtype(db, &scope, &implementation_ty, &service)
            {
                let range = first_type_ref_range(db, file, &tree, implementation);
                out.push(DeclDiagnostic::ServiceImplementationNotSubtype {
                    service,
                    implementation: implementation_ty,
                    range,
                });
            }
        }
    }
    out
}

/// Enumerates the class-like declarations of the file in source order and
/// checks each against its inheritance graph.
pub(crate) fn class_diagnostics_impl(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let tree = hir::java_item_tree(db, file);
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
        db, file, &tree,
    ));

    // §7.6: at most one public top-level type per file, named after the file.
    out.extend(public_type_diagnostics(db, file, &tree));

    // §7.6: no two class-like declarations share a fully qualified name,
    // across the source set (cross-file as well as same-file).
    out.extend(duplicate_class_diagnostics(db, file, &tree));

    // §14.3: the local class-like declarations of the file — which modifiers
    // they may carry, a sealed direct supertype, a name already in scope as a
    // local declaration, and a name an enclosing class or interface already
    // has.
    out.extend(local_modifier_diagnostics(db, file, &tree));
    out.extend(local_class_diagnostics(db, file, &tree, &scope));

    // §8.1.1/[§8.4.3]: a declaration carries two or more modifiers the JLS
    // forbids from co-occurring (see [`modifier_combination_diagnostics`]).
    out.extend(modifier_combination_diagnostics(db, file, &tree));

    // §8.9.1: an enum body's members must follow all its constants — the
    // first non-constant member ends the constant section, and any constant
    // after the separating `;` is an error. The lowered enum body drops the
    // `;` boundary, so the ordering is read from the file's parse tree.
    out.extend(enum_ordering_diagnostics(db, file, &tree));

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
        if data.is_type() {
            // §6.7: a class-like declaration is checked against its own key —
            // its canonical name, or its declaration when it has none.
            let key = crate::jvm::member::ClassKey::of(tree, file, id);
            out.extend(check_class(db, file, scope, tree, &key, id));
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
    key: &crate::jvm::member::ClassKey,
    item: hir_def::java::item_tree::ItemId,
) -> Vec<DeclDiagnostic> {
    // The access-control context of the class itself ([§6.6.1]): the walk is
    // a member enumeration, not an invocation from outside.
    let ctx = crate::java::method::access_context(db, file, item);
    let mut out = Vec::new();
    // The name-resolution context of the declaration itself: its type
    // parameters and every enclosing class's ([§6.5.5.1], [§8.1.3]).
    let resolver = crate::java::resolve::Resolver::for_item(db, file, tree, item);
    // Every member visible from the class, most-derived first ([§8.4.8.1]),
    // *without* the most-derived dedup: an override must still see the super
    // declaration it hides — both for the return-type-substitutability check
    // and for `@Override` ([§9.6.4.4]). Split into the class's own
    // declarations and the inherited set.
    let self_ty = key.as_ty(db, Vec::new());
    let all = all_methods_raw(db, scope, &self_ty, &ctx);
    let declared: Vec<&MethodData> = all.iter().filter(|m| m.owner == *key).collect();
    let inherited: Vec<&MethodData> = all.iter().filter(|m| m.owner != *key).collect();
    // §9.6.4.6: *overriding* a deprecated method is a use of it, reported at
    // the overriding method's own name (javac's caret). The exemption is
    // decided per pair, from the deprecation in force at the overriding
    // declaration and from the shared-outermost-class rule.
    let enclosing = crate::java::db::deprecated_enclosing_query(db, db.file_text(file));
    // §9.6.4.6's exemption compares the *outermost* class: for a local
    // declaration ([JLS §14.3]) that is the class enclosing it.
    let overriding_outermost = key
        .top_level(db, tree.package.as_ref().map(Name::as_str))
        .unwrap_or_else(|| Name::new(""));
    for method in &declared {
        for super_method in &inherited {
            if !same_signature(db, method, super_method) {
                continue;
            }
            let Some(deprecation) = crate::java::deprecation::member_deprecation(
                db,
                scope,
                super_method
                    .owner
                    .as_fqn()
                    .map(|fqn| fqn.as_str())
                    .unwrap_or(""),
                &super_method.name,
                super_method.owner_file.zip(super_method.decl_item),
                super_method.descriptor.as_deref(),
            ) else {
                continue;
            };
            let api = crate::java::deprecation::method_api(db, super_method);
            let in_force = method
                .decl_item
                .and_then(|item| enclosing.get(&item))
                .and_then(|info| info.enclosing);
            if crate::java::deprecation::is_exempt(
                db,
                scope,
                in_force,
                Some(&overriding_outermost),
                &api,
                deprecation,
            ) {
                continue;
            }
            let range = method
                .decl_item
                .and_then(|item| item_name_range(db, file, tree, item));
            if out.iter().any(|existing| {
                matches!(
                    existing,
                    DeclDiagnostic::DeprecatedUse {
                        api: seen,
                        range: seen_range,
                        ..
                    } if *seen == api && *seen_range == range
                )
            }) {
                continue;
            }
            out.push(DeclDiagnostic::DeprecatedUse {
                api,
                deprecation,
                range,
            });
        }
        for super_method in &inherited {
            if same_signature(db, method, super_method) {
                // The class's *own* redeclaration of an inherited signature:
                // the override/hide obligations of [§8.4.8.1]/[§8.4.8.2] —
                // a same-signature instance/instance pair must override, a
                // static/static pair must hide, and a *mixed* pair is an
                // error outright ([§8.4.8.1] instance-over-static and
                // [§8.4.8.2] static-over-instance: neither overrides nor
                // hides).
                let interface_default_rule =
                    declaring_interface_item(tree, item) && !method.is_static && !method.abstract_;
                if method.is_static != super_method.is_static {
                    // §8.4.8.1/[§8.4.8.2]: a static declaration cannot
                    // override an inherited instance method, and an instance
                    // declaration cannot hide an inherited static one. (An
                    // *interface* method is implicitly instance — `static`
                    // only via the keyword — so a subinterface redeclaring an
                    // inherited signature abstract is an override, never a
                    // clash.) The clash is reported once, at the redeclaring
                    // method, against the nearest inherited declaration.
                    out.push(DeclDiagnostic::StaticInstanceClash {
                        method: Name::new(&method.name),
                        super_owner: super_method.owner.display_name(db),
                        overriding_is_static: method.is_static,
                    });
                }
                // §8.4.3.3: a final method of a superclass or superinterface
                // can neither be overridden (instance) nor hidden (static), so
                // a redeclaration of its signature is an error.
                if super_method.is_final {
                    out.push(DeclDiagnostic::CannotOverrideFinalMethod {
                        method: Name::new(&method.name),
                        super_owner: super_method.owner.display_name(db),
                    });
                }
                // §9.4.1.2/[§8.4.8.2]: a `default` interface method whose
                // signature matches a member of `java.lang.Object` — the
                // class's implementation of every `Object` method comes from
                // `Object` itself, so the interface cannot override it. An
                // interface `static` method with such a signature would have
                // to *override* (never hide, §8.4.8.2), which is equally
                // impossible ([§9.4.1.2]); javac reports `default method {m}
                // in {I} overrides a member of java.lang.Object` /
                // `overriding method is static`. An *abstract* interface
                // redeclaration is legal — it only restates the inherited
                // contract ([§9.4.1.2]) — so `default`/`static` bodies (a
                // body implies concrete) are the only violations.
                if interface_default_rule
                    && matches!(
                        super_method.owner.as_fqn().map(|fqn| fqn.as_str()),
                        Some("java.lang.Object" | "java.lang.Record")
                    )
                    && !method.abstract_
                {
                    out.push(DeclDiagnostic::CannotOverrideObjectMethod {
                        method: Name::new(&method.name),
                        is_static: method.is_static,
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
                        super_owner: super_method.owner.display_name(db),
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
                            expected_owner: super_method.owner.display_name(db),
                            expected_ret: super_method.ret,
                        });
                    }
                }
                // §8.4.8.3: the `throws` clause of an overriding or hiding
                // method may not name a checked exception type that the
                // overridden method does not throw — it may only *narrow* the
                // thrown checked set ([§8.4.8.3]); `RuntimeException`, `Error`
                // and their subtypes may always be added ([§11.1.1]). An
                // unchecked addition is no liability. Same-staticness pairs
                // only: a mixed-staticness redeclaration is the separate
                // [`StaticInstanceClash`] error and cannot override or hide
                // ([§8.4.8.1]/[§8.4.8.2]).
                if method.is_static == super_method.is_static
                    && !method.abstract_
                    && !super_method.abstract_
                {
                    // javac applies the throws rule to concrete
                    // implementations and to abstract declarations alike; an
                    // abstract pair is a *clash* report (`m() in IB clashes
                    // with m() in IA`). Skip the abstract/abstract pair here:
                    // the checker's missing-body/does-not-override machinery
                    // keeps the class honest, and an abstract redeclaration
                    // that widens throws is javac's separate clash — the
                    // widening is still unlawful ([§8.4.8.3]) only when the
                    // pair is concrete. (D2c probe: javac flags
                    // interface-abstract `IB extends IA` widening with
                    // `clashes with`.)
                    for thrown in &method.throws {
                        let covered = super_method
                            .throws
                            .iter()
                            .any(|declared| subtyping::is_assignable(db, scope, thrown, declared));
                        if !covered && is_checked(db, scope, thrown) {
                            out.push(DeclDiagnostic::IncompatibleThrows {
                                method: Name::new(&method.name),
                                super_owner: super_method.owner.display_name(db),
                                thrown: *thrown,
                            });
                            break;
                        }
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
                range: first_type_ref_range(db, file, tree, super_ref),
            });
        }
        // §8.1.4: a class declaration's `extends` clause must name a *class* —
        // an interface (or annotation type) is never a superclass, so naming
        // one is a compile-time error (javac: `no interface expected here`;
        // IntelliJ: `No interface expected here`), reported at the written type
        // reference. `is_interface_type` yields `None` for a reference it
        // cannot classify — an unresolved name or a type variable — so only a
        // *resolved* interface is reported.
        if subtyping::is_interface_type(db, scope, &super_ty) == Some(true) {
            out.push(DeclDiagnostic::NoInterfaceExpectedHere {
                range: type_ref_range(db, file, tree, super_ref),
            });
        }
    }

    // §8.1.5/[§9.1.3]: each *InterfaceType* named by the `implements` clause of
    // a class declaration — a normal class, an enum ([§8.9]) or a record
    // ([§8.10]), all of which are class declarations ([§8.1]) — and each one
    // named by an interface declaration's `extends` clause must name an
    // interface, or a compile-time error occurs. Reported once per offending
    // reference, at the written type (IntelliJ: `Interface expected here`;
    // javac: `interface expected here`). `is_interface_type` yields `None` for
    // a reference it cannot classify — an unresolved name (`CannotResolveType`
    // already reports it) or a type variable (javac reports that with its own
    // `compiler.err.type.found.req`) — so only a *resolved* class-like type is
    // reported. The rest of the sentence (*accessible* interface, javac
    // `compiler.err.not.def.public.cant.access`), the duplicate-superinterface
    // rule of §8.1.5 (`compiler.err.repeated.interface`) and §8.1.4's
    // class-required rule for a class's `extends` clause
    // (`compiler.err.no.intf.expected.here`) are separate rules with their own
    // codes and stay unchanged.
    let superinterfaces: &[ItemTypeRef] = match tree.data(item) {
        ItemData::Class(data) | ItemData::Interface(data) => &data.interfaces,
        ItemData::Enum(data) => &data.interfaces,
        ItemData::Record(data) => &data.interfaces,
        _ => &[],
    };
    for interface_ref in superinterfaces {
        let interface_ty =
            crate::java::resolve::resolve_type_ref(db, scope, &resolver, interface_ref);
        if subtyping::is_interface_type(db, scope, &interface_ty) == Some(false) {
            out.push(DeclDiagnostic::InterfaceExpectedHere {
                range: type_ref_range(db, file, tree, interface_ref),
            });
        }
    }

    // §8.1.4/[§9.1.3]: a class or interface appears in its own inheritance
    // chain — `class A extends B` with `class B extends A`. Reported for every
    // class-like declaration, at its name.
    if key
        .as_fqn()
        .is_some_and(|fqn| in_own_supertype_cycle(db, scope, fqn.as_str()))
    {
        out.push(DeclDiagnostic::CyclicInheritance {
            class: class_like_simple_name(tree.data(item)),
            range: item_name_range(db, file, tree, item),
        });
    }

    // §8.8.7: a class that declares no constructor has an implicit default
    // constructor whose body begins with `super()`; a direct superclass with
    // no *accessible* no-argument constructor makes that implicit call fail.
    // §8.8.9/[§11.2]: the same implicit `super()` must *handle* the checked
    // exceptions the superclass constructor throws — the implicit default
    // constructor declares no `throws` clause, so an unchecked-off liability
    // is `unreported exception {E} in default constructor`. Enums and records
    // have their own implicit superclass (`Enum`, `Record`), so only plain
    // class declarations are checked. An abstract subclass is exempt from
    // both — javac reports the missing no-arg constructor and the
    // unreported-exception liability only at the first *concrete* class in
    // the chain ([§8.1.1.1]: an abstract class need not be instantiable), so
    // this walks to the nearest concrete descendant.
    if let ItemData::Class(class) = tree.data(item)
        && !class
            .body
            .iter()
            .any(|child| matches!(tree.data(*child), ItemData::Method(m) if m.is_constructor()))
        && let Some(super_ref) = &class.super_class
        && let Some(super_ty) = first_concrete_descendant_super(
            db,
            scope,
            hir::java_item_tree(db, file),
            item,
            &resolver,
            super_ref,
        )
        // §8.1.4: a superclass that names an interface is not a class — the
        // class-required check reports the `extends` clause, and an interface
        // declares no constructors, so the implicit `super()` has nothing to
        // resolve and the construction diagnostics stay silent.
        && subtyping::is_interface_type(db, scope, &super_ty) != Some(true)
    {
        let super_owner = super_ty
            .as_reference(db)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| class.name.clone());
        // §8.8.7: the implicit super() of the (possibly distant) concrete
        // descendant resolves the direct superclass's no-argument
        // constructor.
        let no_accessible_no_arg = has_no_accessible_no_arg_ctor(db, scope, &super_ty, &ctx);
        if no_accessible_no_arg == Some(true) {
            out.push(DeclDiagnostic::NoDefaultConstructor {
                class: class.name.clone(),
                super_owner: super_owner.clone(),
                range: item_name_range(db, file, tree, item),
            });
        }
        // §8.8.9/[§11.2]: every checked exception the reachable no-argument
        // constructor declares is unreported in the implicit default
        // constructor of the concrete descendant. The constructor's own
        // throws are already instantiated with its class's type arguments
        // ([§8.4.6]), so a subtype check against each declared exception
        // settles coverage.
        let super_class = super_ty
            .as_reference(db)
            .map(|(name, _)| name.as_str().to_owned());
        let accessible_no_arg_thrown: Vec<Ty> = match &super_class {
            Some(fqn) => member_set(db, scope, &super_ty, &fqn_ctor_name(db, scope, fqn), &ctx)
                .iter()
                .find(|ctor| ctor.params.is_empty())
                .map(|ctor| ctor.throws.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        for thrown in accessible_no_arg_thrown {
            if is_checked(db, scope, &thrown) {
                out.push(DeclDiagnostic::DefaultCtorUnreportedException {
                    class: class.name.clone(),
                    super_owner: super_owner.clone(),
                    thrown,
                    range: item_name_range(db, file, tree, item),
                });
            }
        }
    }

    // §8.8.7: a constructor body that contains no explicit constructor
    // invocation "implicitly begins with a superclass constructor invocation
    // `super();`, an implicit invocation of the constructor of the direct
    // superclass that takes no arguments". A real invocation, so §15.12.2.2
    // requires that constructor to exist and be applicable (access: §6.6).
    // javac reports `constructor {S} in class {S} cannot be applied to given
    // types; ... found: no arguments` at the constructor body; the report here
    // is IntelliJ's `There is no default constructor available in '{S}'` and
    // reuses the identity of the synthesized-constructor report above. A
    // declared constructor is checked whether or not the class is abstract:
    // javac reports it for an abstract class's own constructors too.
    if let ItemData::Class(class) = tree.data(item)
        && let Some(super_ref) = &class.super_class
        && let super_ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref)
        // §8.1.4: a superclass that names an interface is not a class — the
        // class-required check reports the `extends` clause, and an interface
        // declares no constructors, so neither the implicit `super()` nor its
        // checked exceptions have anything to resolve here.
        && subtyping::is_interface_type(db, scope, &super_ty) != Some(true)
    {
        let super_owner = super_ty
            .as_reference(db)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| class.name.clone());
        let bodies = hir::file_body_tree(db, file);
        for child in &class.body {
            let ItemData::Method(method) = tree.data(*child) else {
                continue;
            };
            if !method.is_constructor() {
                continue;
            }
            let Some(body_id) = method.body() else {
                continue;
            };
            // §8.8.7: an explicit constructor invocation replaces the implicit
            // `super()` — `this(...)` delegates to another constructor of the
            // class and `super(...)` names the superclass constructor itself.
            if body_has_ctor_call(&bodies, body_id) {
                continue;
            }
            // The implicit `super()` cannot be applied when the superclass
            // offers no accessible no-argument constructor; that is the
            // whole report for this constructor.
            if has_no_accessible_no_arg_ctor(db, scope, &super_ty, &ctx) == Some(true) {
                out.push(DeclDiagnostic::NoDefaultConstructor {
                    class: class.name.clone(),
                    super_owner: super_owner.clone(),
                    range: item_name_range(db, file, tree, *child),
                });
                continue;
            }
            // §8.8.7/[§11.2.2]: the accessible no-argument superclass
            // constructor the implicit `super()` resolves to declares its
            // checked exceptions; the body cannot catch them (the implicit
            // invocation precedes every statement), so only the constructor's
            // own `throws` clause can discharge them. A constructor that
            // cannot resolve its superclass stays silent — the missing type
            // reports itself (see `has_no_accessible_no_arg_ctor`).
            let Some(super_fqn) = super_ty
                .as_reference(db)
                .map(|(name, _)| name.as_str().to_owned())
            else {
                continue;
            };
            let declared: Vec<Ty> = method
                .sig
                .throws
                .iter()
                .map(|ex| crate::java::resolve::resolve_type_ref(db, scope, &resolver, ex))
                .collect();
            let range = item_name_range(db, file, tree, *child);
            for thrown in member_set(
                db,
                scope,
                &super_ty,
                &fqn_ctor_name(db, scope, &super_fqn),
                &ctx,
            )
            .iter()
            .find(|ctor| ctor.params.is_empty())
            .map(|ctor| ctor.throws.clone())
            .unwrap_or_default()
            {
                if is_checked(db, scope, &thrown)
                    && !declared
                        .iter()
                        .any(|target| subtyping::is_assignable(db, scope, &thrown, target))
                {
                    out.push(DeclDiagnostic::CtorUnreportedException { thrown, range });
                }
            }
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
        // §8.9.1: an enum's abstract methods are implemented by the bodies of
        // its constants — each constant body is the class body of an anonymous
        // subclass, so a constant carrying a matching method discharges the
        // enum's own obligation. javac reports the does-not-override-abstract
        // error on the enum only when *no* constant body implements the method.
        let is_enum = matches!(tree.data(item), ItemData::Enum(_));
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
                    // §8.4.8.1: a concrete instance member of the class's own
                    // member set satisfies the abstract method when it has the
                    // same overriding signature — the class inherits the
                    // concrete member (every member of `all` is declared by
                    // the class or one of its supertypes). No owner
                    // relationship is required: `Object.equals` (owner
                    // `Object`) satisfies `Comparator.equals` (owner
                    // `Comparator`) because the class inherits both and
                    // `Object` is its superclass; a class-declared `compare`
                    // satisfies the interface's `compare` the same way. Only
                    // an abstract member with no same-signature concrete
                    // inheritor is reported ([§8.4.8.1]).
                }) || (is_enum
                    && enum_constant_body_implements(db, file, tree, item, abstract_method));
                if !implemented {
                    out.push(DeclDiagnostic::UnimplementedAbstractMethod {
                        class: class_like_simple_name(tree.data(item)),
                        method: Name::new(&abstract_method.name),
                        owner: abstract_method.owner.display_name(db),
                        range: item_name_range(db, file, tree, item),
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
                range: item_name_range(db, file, tree, child),
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
    let defaults = inherited_defaults(db, scope, &self_ty);
    let defaults: Vec<&MethodData> = defaults.iter().filter(|m| m.owner != *key).collect();
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
            && m.annotations
                .iter()
                .any(|annotation| is_override_annotation(db, scope, &resolver, &annotation.name))
        {
            // §8.4.2: the annotated declaration must be matched to its own
            // [`MethodData`] by *signature*, not just name and arity — a class
            // may declare two same-arity overloads (`static void T(C[])` and
            // `@Override void T(Buffer)`) and the declared list is walked in
            // body order, so a name+arity match could land on the wrong
            // overload and misreport a correct `@Override` as orphaned.
            let Some(method) = declared.iter().find(|d| {
                d.name == m.name.as_str()
                    && d.params.len() == m.sig.params.len()
                    && d.params.iter().zip(&m.sig.params).all(|(ty, param)| {
                        let declared_ty =
                            crate::java::resolve::resolve_type_ref(db, scope, &resolver, &param.ty);
                        ty.is_error(db)
                            || declared_ty.is_error(db)
                            || ty.same_shape(db, &declared_ty)
                            || ty.erasure(db) == declared_ty.erasure(db)
                    })
            }) else {
                continue;
            };
            let is_record_accessor = record_components
                .iter()
                .any(|component| component.name.as_str() == method.name);
            let overrides = is_record_accessor
                || inherited
                    .iter()
                    .any(|s| !s.is_static && same_signature(db, method, s));
            if method.is_static {
                // §9.6.4.4: a static method never overrides — it hides
                // ([§8.4.8.2]) — so `@Override` on one is always an error.
                // javac's message for the static case is its own:
                // `static methods cannot be annotated with @Override`
                // (the generic does-not-override message is reserved for an
                // instance method that matches no supertype method).
                out.push(DeclDiagnostic::MethodDoesNotOverrideStatic {
                    method: Name::new(&method.name),
                    range: item_name_range(db, file, tree, child),
                });
            } else if !overrides {
                out.push(DeclDiagnostic::MethodDoesNotOverride {
                    method: Name::new(&method.name),
                    range: item_name_range(db, file, tree, child),
                });
            }
        }
    }

    // signature is the parameter list alone) — *declared by the class
    // itself* whose erasures
    // ([§4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6))
    // are equal but whose parameterized signatures differ — the JVM cannot
    // load them both, and neither overrides the other. Each later
    // declaration is reported against each earlier clashing one. The
    // *identical* pair (equal parameterized signatures, including two
    // `m(String[])`s) is the §8.4.2 duplicate-declaration error (`method m
    // is already defined` / `constructor C is already defined`), and the
    // same erasure with one varargs and one fixed-array parameter — the same
    // erasure ([§4.6]) under the array lowering of `T...` ([§8.4.1]) — is
    // javac's `cannot declare both m(String[]) and m(String...)`.
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
            if !same_erasure {
                continue;
            }
            // Identical signatures are a *duplicate declaration*, not a name
            // clash — they collide even without generics ([§8.4.2]). Two
            // methods whose type parameters differ only by name
            // (`<T> void m(List<T>)`, `<U> void m(List<U>)`) collapse to
            // identical parameter types after erasure ([§4.6]) — javac
            // reports them as `method <T>m(List<T>) is already defined`,
            // counting them duplicates, not name clashes. The *source-level*
            // raw parameters (each kept as its own type variable) differ by
            // variable identity, so the comparison runs on the erased
            // parameters. `varargs` is not part of the signature: the pair
            // differs only in the last parameter's varargs-ness.
            let identical = same_declared_params(db, a, b);
            if identical && a.varargs == b.varargs {
                // The duplicate is *each* later method against each earlier
                // identical one (javac reports `m() is already defined` at
                // every redeclaration beyond the first). Both are the
                // class's own declarations; anchor the report at `b` — the
                // later of the pair — which is the redeclaration.
                let range = b.decl_item.and_then(|decl| {
                    item_name_range(db, file, tree, decl).or_else(|| {
                        // The synthesized forms (an implicit record canonical
                        // next to the compact constructor) have no item; fall
                        // back to the class name.
                        item_name_range(db, file, tree, item)
                    })
                });
                out.push(DeclDiagnostic::DuplicateMethod {
                    method: Name::new(&a.name),
                    is_constructor: a.name == key.simple_name(db).as_str(),
                    range,
                });
            } else if identical {
                // One varargs, one fixed array of the same component type —
                // the erasure twin ([§8.4.1], [§4.6]). Both last parameters
                // are the same array type after erasure (the varargs
                // parameter was lowered to the array of its element); the
                // fixed-array one is that array already. Report the shared
                // array type, spelled with the fixed brackets in the message.
                let array = &a.params[a.params.len() - 1];
                out.push(DeclDiagnostic::CannotDeclareBothVarargsAndArray {
                    method: Name::new(&a.name),
                    array: *array,
                });
            } else {
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
                range: item_name_range(db, file, tree, item),
            });
        }
    }

    // §8.1.1.2: sealed hierarchies. A class may directly extend — and a class
    // or interface directly implement/extend — a `sealed` supertype only when
    // it is named in its `permits` clause (or, without one, is its
    // same-module direct subclass), and a permitted direct subclass must
    // itself be `final`, `sealed` or `non-sealed` so the hierarchy closes.
    sealed_subclass_diagnostics(db, file, tree, scope, &resolver, item, key, &mut out);

    // §8.1.1.2: a `sealed` type must have at least one direct subclass.
    if class_like_modifiers(tree.data(item)).is_some_and(|m| m.is_sealed())
        && !has_permits_clause(tree.data(item))
        && !key
            .as_fqn()
            .is_some_and(|fqn| file_has_direct_subclass(db, file, tree, scope, fqn.as_str()))
    {
        out.push(DeclDiagnostic::SealedClassMustHaveSubclasses {
            range: item_name_range(db, file, tree, item),
        });
    }

    // §8.4.5/[§9.4: interface member-shape rules: a non-`abstract`,
    // non-`native` method without a body — an interface `private`/`static`/
    // `default` method that never got its body, or a class method that should
    // be `abstract` — is a compile-time error; and `protected` is not a legal
    // interface method modifier ([§9.4]).
    if matches!(tree.data(item), ItemData::Interface(_)) {
        for &child in tree.data(item).body() {
            if let ItemData::Method(method) = tree.data(child) {
                // §9.4: interface methods may be `public`, `abstract`,
                // `static`, `default`, `private` or `strictfp` — but never
                // `protected` (javac points at the method name).
                if method.modifiers.is_protected() {
                    out.push(DeclDiagnostic::ModifierNotAllowedHere {
                        modifier: "protected",
                        range: item_name_range(db, file, tree, child),
                    });
                }
            }
        }
    }
    // §8.4.5: a non-abstract, non-native method without a body is an error.
    // In an interface a body-less method is *implicitly abstract* ([§9.4]),
    // so only the explicitly non-abstract forms (`private`/`static`/
    // `default`) must carry a body. Annotation type elements are exempt — an
    // element without a `default` value is implicitly abstract ([§9.6.1]).
    if !matches!(tree.data(item), ItemData::Annotation(_)) {
        let is_interface = matches!(tree.data(item), ItemData::Interface(_));
        for &child in tree.data(item).body() {
            if let ItemData::Method(method) = tree.data(child)
                && let Some(_body) = method.body()
                && !method.is_constructor()
                && (method.modifiers.is_abstract() || method.modifiers.is_native())
            {
                // §8.4.5/[§9.4: an `abstract` or `native` method cannot
                // carry a body — an abstract method leaves the behavior to
                // its subtypes, a native method to a platform
                // implementation, so neither may define a Java body. javac:
                // `abstract methods cannot have a body` (in an interface:
                // `interface abstract methods cannot have a body`) / `native
                // methods cannot have a body`. Abstract methods with bodies
                // are already `IllegalModifierCombination`-flagged only when
                // another modifier contradicts (`abstract` + `final`/
                // `static`/`private`/`default`/`native`/`synchronized`/
                // `strictfp`, [§8.4.3]); a *bare* `abstract void m() {}`
                // combines nothing and must be caught here.
                out.push(DeclDiagnostic::AbstractOrNativeMethodWithBody {
                    method: method.name.clone(),
                    abstract_: method.modifiers.is_abstract(),
                    range: item_name_range(db, file, tree, child),
                });
            }
            if let ItemData::Method(method) = tree.data(child)
                && method.body().is_none()
                && !method.is_constructor()
                && !method.modifiers.is_abstract()
                && !method.modifiers.is_native()
                && (!is_interface
                    || method.modifiers.is_static()
                    || method.modifiers.is_private()
                    || method.modifiers.is_default())
            {
                out.push(DeclDiagnostic::MissingMethodBodyOrDeclareAbstract {
                    method: method.name.clone(),
                    range: item_name_range(db, file, tree, child),
                });
            }
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
    // §8.9.2: an enum constructor may not be `public` or `protected` — enum
    // constructors are private by nature (the constants are the only
    // instances, so no outside caller may create one), and only `private`
    // (or nothing) may be written. javac: `modifier public not allowed
    // here` / `modifier protected not allowed here`, at the modifier. The
    // `enum` modifier diagnostics of [`modifier_combination_diagnostics`]
    // only cover the pair-combination walk, so the enum-ctor access rule is
    // checked here per constructor declaration.
    let is_enum = matches!(tree.data(item), ItemData::Enum(_));
    // §8.10.4: a record's *canonical* constructor is the constructor whose
    // parameter types equal the record's component types, in order
    // ([§8.10.4]); its parameters must be named exactly as the components
    // (javac: `invalid canonical constructor in record R (invalid parameter
    // names in canonical constructor)`). A record constructor with the
    // component types but different names is reported — the first such
    // constructor is the canonical one, and later same-typed ones duplicate
    // it ([§8.4.2] duplicate check) or delegate.
    let record_components: &[hir_def::java::item_tree::RecordComponent] = match tree.data(item) {
        ItemData::Record(record) => &record.components,
        _ => &[],
    };
    let mut canonical_seen = false;
    for &child in tree.data(item).body() {
        let ItemData::Method(method) = tree.data(child) else {
            continue;
        };
        if !method.is_constructor() || method.is_compact_constructor() {
            continue;
        }
        // Match the constructor's declared parameter types (lowered by the
        // item tree) against the component types — the canonical test is
        // *type equality in order* ([§8.10.4]). Varargs components are
        // array-typed in the canonical signature ([§8.4.1]).
        if method.sig.params.len() != record_components.len() {
            continue;
        }
        let is_canonical_shape =
            method
                .sig
                .params
                .iter()
                .zip(record_components)
                .all(|(param, component)| {
                    let declared =
                        crate::java::resolve::resolve_type_ref(db, scope, &resolver, &param.ty);
                    let mut component_ty =
                        crate::java::resolve::resolve_type_ref(db, scope, &resolver, &component.ty);
                    if component.varargs {
                        component_ty = Ty::array(db, component_ty);
                    }
                    declared.is_error(db) || component_ty.is_error(db) || declared == component_ty
                });
        if !is_canonical_shape {
            continue;
        }
        // The first constructor with the canonical shape is the canonical
        // constructor ([§8.10.4]); it must use the component names.
        if !canonical_seen {
            canonical_seen = true;
            let names_match = method
                .sig
                .params
                .iter()
                .zip(record_components)
                .all(|(param, component)| param.name == component.name);
            if !names_match {
                out.push(DeclDiagnostic::RecordCtorParamNameMismatch {
                    record: class_simple.clone(),
                    range: item_name_range(db, file, tree, child),
                });
            }
        }
    }
    for &child in tree.data(item).body() {
        let ItemData::Method(method) = tree.data(child) else {
            continue;
        };
        if method.is_constructor()
            && is_enum
            && (method.modifiers.is_public() || method.modifiers.is_protected())
        {
            out.push(DeclDiagnostic::ModifierNotAllowedHere {
                modifier: if method.modifiers.is_public() {
                    "public"
                } else {
                    "protected"
                },
                range: item_name_range(db, file, tree, child),
            });
        }
        // §8.9.2: an enum constructor may not invoke `super()` — its implicit
        // superclass `java.lang.Enum` has no constructor accessible to it
        // (the compiler supplies the arguments itself). javac rejects the
        // explicit `super()` with `call to super not allowed in enum
        // constructor`, at the invocation.
        if method.is_constructor()
            && is_enum
            && let Some(body_id) = method.body()
        {
            let bodies = hir::file_body_tree(db, file);
            let body = bodies.body(body_id);
            let super_call = body.stmts.iter().find_map(|&stmt| {
                let hir_expand::body::StmtData::Expr(expr) = bodies.stmt(stmt) else {
                    return None;
                };
                match bodies.expr(*expr) {
                    hir_expand::body::ExprData::CtorCall {
                        target: hir_expand::body::CtorCallTarget::Super,
                        ..
                    } => Some(*expr),
                    _ => None,
                }
            });
            if let Some(expr) = super_call {
                out.push(DeclDiagnostic::EnumCtorSuperCall {
                    range: bodies.expr_range(expr),
                });
            }
        }
    }
    for &child in tree.data(item).body() {
        let ItemData::Method(method) = tree.data(child) else {
            continue;
        };
        if method.is_constructor() && &method.name != class_simple {
            out.push(DeclDiagnostic::ConstructorNameMismatch {
                name: method.name.clone(),
                class: class_simple.clone(),
                range: item_name_range(db, file, tree, child),
            });
        }
    }
    out
}

/// §8.4.2: whether two methods declared by the same class have *identical*
/// parameter types — the duplicate-declaration test. Parameterized types
/// compare exactly (`m(List<String>)` vs `m(List<Integer>)` are *not*
/// identical; they share an erasure and are the [`NameClashSameErasure`]
/// error instead). Each method's own type parameters are anonymous — a
/// generic method's parameterized signature is its *declaration form*
/// (`<T> void m(List<T>)`), which does not change when the variable is
/// renamed to `<U>` — so two methods whose type variables appear at the same
/// positions, erasing to equal shapes, are duplicates (javac reports
/// `method <T>m(List<T>) is already defined`). A type variable that reaches
/// the parameter types only through *substitution* (never renamed in the
/// declaration) is structural too. The comparison therefore strips the
/// interned variable identities by erasure ([§4.6]) and requires the erasures
/// to be equal *and* the parameter-type shapes (modulo each method's own
/// variables) to coincide — the variables are compared by the shape of their
/// bounds, not their names ([§6.4.1] scoping makes the names irrelevant).
fn same_declared_params(db: &dyn TyDatabase, a: &MethodData, b: &MethodData) -> bool {
    if a.params.len() != b.params.len() {
        return false;
    }
    // §8.4.2: the duplicate-declaration test is *identical* declared
    // parameter types. Parameterized types compare exactly (`m(List<String>)`
    // vs `m(List<Integer>)` are not identical — they share an erasure and are
    // the [`NameClashSameErasure`] error instead). Each method's *own* type
    // parameters are anonymous: a generic declaration's signature is its
    // declaration form, so renaming the variable (`<T>` to `<U>`) does not
    // change it — javac reports `method <T>m(T) is already defined` for the
    // pair, and a variable whose declared bound is exactly its erasure
    // (`<T extends Object>` ≡ `<T>`) is likewise the same signature. javac
    // erases each method's own variables to their *bounds* and compares the
    // resulting structural parameter lists:
    //
    // - `<T> m(T)` vs `<U> m(U)` → `m(Object)` both — duplicate;
    // - `<T extends Number> m(T)` vs `<U extends Number> m(U)` →
    //   `m(Number)` both — duplicate;
    // - `<T extends Number> m(T)` vs `<T> m(Object)` → `m(Number)` vs
    //   `m(Object)` — *different*, a name clash only if some erasure equals;
    // - `<T extends Number> m(T)` vs `<T> m(T)` → `m(Number)` vs `m(Object)`
    //   — different (GTS5/GT10 probes: no error — the erasures differ too);
    // - `m(List<String>)` vs `m(List<Integer>)` → erasures equal but the
    //   declared forms differ — clash.
    //
    // The crate keeps each method's own variables un-erased in `params` (the
    // invocation-type inference of [§18.5.2] needs them), so the comparison
    // erases every occurrence of a *method* type variable to its effective
    // bound ([§4.4]) — which is what javac's signature erasure does — and
    // leaves class arguments (`String` vs `Integer`) intact, then requires
    // the two structural forms to coincide.
    let substitute_bounds = |params: &[Ty], method: &MethodData| -> Vec<Ty> {
        // Build the declaring parameter -> bound map (its declared first
        // bound, or Object for an unbounded variable, [§4.4]). Keyed by the
        // parameter's scope ([§6.3]) so only this method's own variables are
        // erased and a same-named class parameter of the declaring class is
        // left alone ([§6.4.1], [§4.4] capture-avoidance).
        let binding: FxHashMap<TypeVarScope, Ty> = method
            .type_params
            .iter()
            .map(|tp| {
                let bound = tp
                    .bounds
                    .first()
                    .cloned()
                    .unwrap_or_else(|| Ty::reference(db, "java.lang.Object", Vec::new()));
                (tp.scope.clone(), bound)
            })
            .collect();
        params
            .iter()
            .map(|param| param.substitute(db, &binding))
            .collect()
    };
    let a_erased = substitute_bounds(&a.params, a);
    let b_erased = substitute_bounds(&b.params, b);
    a_erased == b_erased
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
fn related(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    a: &crate::jvm::member::ClassKey,
    b: &crate::jvm::member::ClassKey,
) -> bool {
    if a == b {
        return true;
    }
    let a_ty = a.as_ty(db, Vec::new());
    let b_ty = b.as_ty(db, Vec::new());
    subtyping::is_subtype(db, scope, &a_ty, &b_ty) || subtyping::is_subtype(db, scope, &b_ty, &a_ty)
}

/// §8.4.8.3: whether access `a` is strictly weaker than access `b` — the
/// ordering the JLS rules on overriding and hiding: `public` >
/// `protected` > package-private > `private` ([§6.6.1]).
fn weaker_access(a: Access, b: Access) -> bool {
    access_rank(a) < access_rank(b)
}

/// §11.1.1: whether `ty` is a *checked* exception type — a subtype of
/// `Throwable` that is not a subtype of `RuntimeException` or `Error`. The
/// §8.4.8.3 throws rule only constrains checked additions: an override may
/// always declare `RuntimeException`/`Error` (and their subtypes) that the
/// overridden method does not.
fn is_checked(db: &dyn TyDatabase, scope: &hir::ResolutionScope, ty: &Ty) -> bool {
    let throwable = Ty::reference(db, "java.lang.Throwable", Vec::new());
    if !subtyping::is_assignable(db, scope, ty, &throwable) {
        return false;
    }
    let unchecked = ["java.lang.RuntimeException", "java.lang.Error"];
    !unchecked.iter().any(|name| {
        let supertype = Ty::reference(db, *name, Vec::new());
        subtyping::is_assignable(db, scope, ty, &supertype)
    })
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

/// §8.8.7/[§8.1.3]: the implicit default constructor of `item` (a class
/// declaring no constructor) is inherited by an abstract chain until the
/// first *concrete* descendant, whose own implicit default constructor's
/// `super()` must resolve. Returns the direct superclass of that nearest
/// concrete descendant — the type whose accessible no-argument constructor
/// the implicit `super()` invokes. `item` itself may be abstract (or its
/// direct superclass abstract); the walk skips abstract classes, each
/// declaring no constructor, until a concrete one appears — javac reports
/// `implicit super constructor {S}() is undefined` and the
/// unreported-exception liability at that concrete descendant, not at the
/// abstract intermediates ([§8.1.1.1]: an abstract class need not be
/// instantiable). A class whose own declared constructor ends the chain, or
/// an unresolvable supertype reference, returns `None`.
fn first_concrete_descendant_super(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    tree: Arc<ItemTree>,
    item: hir_def::java::item_tree::ItemId,
    resolver: &crate::java::resolve::Resolver,
    super_ref: &ItemTypeRef,
) -> Option<Ty> {
    let mut current_item = item;
    let mut current_tree: Arc<ItemTree> = tree;
    let mut super_ty = crate::java::resolve::resolve_type_ref(db, scope, resolver, super_ref);
    loop {
        let has_ctor = matches!(current_tree.data(current_item), ItemData::Class(d) if
        d.body.iter().any(|child| {
            matches!(current_tree.data(*child), ItemData::Method(m) if m.is_constructor())
        }));
        if has_ctor {
            return None;
        }
        let abstract_ = class_like_modifiers(current_tree.data(current_item))
            .is_some_and(|modifiers| modifiers.is_abstract());
        // A concrete class's implicit default constructor invokes the
        // no-argument constructor of this direct superclass.
        if !abstract_ {
            return Some(super_ty);
        }
        // An abstract class's own implicit default constructor would invoke
        // its superclass's — walk up, reporting nothing until a concrete
        // descendant is reached ([§8.8.9]).
        let (fqn, _) = super_ty.as_reference(db)?;
        let resolved = hir::fqn_resolve(db, scope, fqn.as_str())?;
        let hir::Resolved::Source(next) = resolved else {
            return None;
        };
        let next_tree: Arc<ItemTree> = hir::java_item_tree(db, next.file);
        let Some(ItemData::Class(next_class)) =
            crate::java::resolve::item_data(&next_tree, next.item)
        else {
            return None;
        };
        let super_ref = next_class.super_class.as_ref()?;
        let next_resolver =
            crate::java::resolve::Resolver::for_item(db, next.file, &next_tree, next.item);
        super_ty = crate::java::resolve::resolve_type_ref(db, scope, &next_resolver, super_ref);
        drop(next_resolver);
        current_item = next.item;
        current_tree = next_tree;
    }
}

/// §8.8.9/[§8.1.3]: the member-set name of a class's constructor: the class's
/// simple name for a source class, `<init>` for a library classfile
/// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6)).
fn fqn_ctor_name(db: &dyn TyDatabase, scope: &hir::ResolutionScope, fqn: &str) -> String {
    match hir::fqn_resolve(db, scope, fqn) {
        Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
        _ => fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
    }
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
        // A Kotlin file's facade: the class's *simple* name is what its
        // `<init>` is looked up under (the Java `new` path's own naming).
        Some(hir::Resolved::KotlinFacade { .. }) => {
            fqn.as_str().rsplit('.').next().unwrap_or(fqn.as_str())
        }
        // An unresolvable superclass is already reported as a missing type by
        // the name check; whether it has a no-arg constructor is unknowable.
        None => return None,
    };
    let access = ctx.with_mode(InvocationMode::Super);
    let has_no_arg = member_set(db, scope, super_ty, name, &access)
        .iter()
        .any(|method| method.params.is_empty());
    Some(!has_no_arg)
}

/// §8.8.7: whether the constructor body `body_id` contains an explicit
/// constructor invocation `this(...)`/`super(...)` — the *ConstructorBody*
/// rule is that a body either *has* one (and it must then be the first
/// statement, [§8.8.7.1]) or has none, in which case it implicitly begins with
/// `super();`. A body that begins with something else and mentions one further
/// down is a placement error ([§8.8.7.1]) — javac reports only
/// `constructor calls not allowed here` for it (`javac: compiler.err.ctor.calls.not.allowed.here`)
/// and adds *no* implicit `super()`, so the search covers the whole statement
/// forest rather than just the top-level statements. Blocks, branches, loops,
/// switches, `try` clauses and lambda bodies are walked.
fn body_has_ctor_call(bodies: &BodyTree, body_id: hir_expand::body::BodyId) -> bool {
    use hir_expand::body::{ExprData, StmtData};
    let mut found = false;
    fn walk_stmt(bodies: &BodyTree, stmt: hir_expand::body::StmtId, found: &mut bool) {
        if *found {
            return;
        }
        match bodies.stmt(stmt) {
            StmtData::Empty | StmtData::Break(_) | StmtData::Continue(_) => {}
            // A `this(...)`/`super(...)` invocation is an expression statement;
            // any other expression form is walked for completeness (a nested
            // invocation is invalid Java but still lowers).
            StmtData::Expr(expr) => walk_expr(bodies, *expr, found),
            StmtData::Decl { initializer, .. } => {
                if let Some(init) = initializer {
                    walk_expr(bodies, *init, found);
                }
            }
            StmtData::Block(inner) | StmtData::DeclGroup(inner) => {
                for s in inner {
                    walk_stmt(bodies, *s, found);
                }
            }
            StmtData::Labeled { stmt: s, .. } => walk_stmt(bodies, *s, found),
            StmtData::If {
                cond, then, els, ..
            } => {
                walk_expr(bodies, *cond, found);
                walk_stmt(bodies, *then, found);
                if let Some(els) = els {
                    walk_stmt(bodies, *els, found);
                }
            }
            StmtData::While { cond, body } => {
                walk_expr(bodies, *cond, found);
                walk_stmt(bodies, *body, found);
            }
            StmtData::DoWhile { body, cond } => {
                walk_stmt(bodies, *body, found);
                walk_expr(bodies, *cond, found);
            }
            StmtData::For {
                init,
                cond,
                step,
                body,
            } => {
                for s in init {
                    walk_stmt(bodies, *s, found);
                }
                if let Some(cond) = cond {
                    walk_expr(bodies, *cond, found);
                }
                for e in step {
                    walk_expr(bodies, *e, found);
                }
                walk_stmt(bodies, *body, found);
            }
            StmtData::ForEach { iterable, body, .. } => {
                walk_expr(bodies, *iterable, found);
                walk_stmt(bodies, *body, found);
            }
            StmtData::Switch {
                scrutinee, arms, ..
            } => {
                walk_expr(bodies, *scrutinee, found);
                for arm in arms {
                    for label in &arm.labels {
                        if let hir_expand::body::SwitchLabel::Expr(e) = label {
                            walk_expr(bodies, *e, found);
                        }
                    }
                    for s in &arm.body {
                        walk_stmt(bodies, *s, found);
                    }
                }
            }
            StmtData::Return(ret) => {
                if let Some(ret) = ret {
                    walk_expr(bodies, *ret, found);
                }
            }
            StmtData::Throw(expr) | StmtData::Yield(expr) => walk_expr(bodies, *expr, found),
            StmtData::Synchronized { expr, body } => {
                walk_expr(bodies, *expr, found);
                walk_stmt(bodies, *body, found);
            }
            StmtData::Try {
                resources,
                body,
                catches,
                finally,
            } => {
                for r in resources {
                    if let Some(init) = r.initializer {
                        walk_expr(bodies, init, found);
                    }
                }
                walk_stmt(bodies, *body, found);
                for c in catches {
                    walk_stmt(bodies, c.body, found);
                }
                if let Some(finally) = finally {
                    walk_stmt(bodies, *finally, found);
                }
            }
            StmtData::Assert { cond, msg } => {
                walk_expr(bodies, *cond, found);
                if let Some(msg) = msg {
                    walk_expr(bodies, *msg, found);
                }
            }
            // A local class's own constructor bodies are separate bodies of
            // the file, not part of this one.
            StmtData::LocalClass { .. } | StmtData::Missing => {}
            // A Kotlin local function — unreachable from a Java body.
            StmtData::LocalFunction { .. }
            | StmtData::Destructuring { .. }
            | StmtData::DeclDelegated { .. } => {}
        }
    }
    fn walk_expr(bodies: &BodyTree, expr: hir_expand::body::ExprId, found: &mut bool) {
        if *found {
            return;
        }
        match bodies.expr(expr) {
            ExprData::CtorCall { .. } => *found = true,
            ExprData::Literal(_)
            | ExprData::Null
            | ExprData::This { .. }
            | ExprData::Super { .. }
            | ExprData::ClassLit(_)
            | ExprData::Var(_)
            | ExprData::NamePath(_)
            | ExprData::Missing => {}
            ExprData::Template { args } | ExprData::ArrayInit(args) => {
                for e in args {
                    walk_expr(bodies, *e, found);
                }
            }
            ExprData::FieldAccess { target, .. } => {
                if let Some(target) = target {
                    walk_expr(bodies, *target, found);
                }
            }
            ExprData::ArrayAccess { array, index } => {
                walk_expr(bodies, *array, found);
                walk_expr(bodies, *index, found);
            }
            ExprData::MethodCall { receiver, args, .. } => {
                if let Some(receiver) = receiver {
                    walk_expr(bodies, *receiver, found);
                }
                for arg in args {
                    walk_expr(bodies, *arg, found);
                }
            }
            ExprData::New { args, receiver, .. } => {
                for arg in args {
                    walk_expr(bodies, *arg, found);
                }
                if let Some(receiver) = receiver {
                    walk_expr(bodies, *receiver, found);
                }
            }
            ExprData::NewArray {
                dims, initializer, ..
            } => {
                for dim in dims {
                    walk_expr(bodies, *dim, found);
                }
                if let Some(elems) = initializer {
                    for elem in elems {
                        walk_expr(bodies, *elem, found);
                    }
                }
            }
            ExprData::Unary { expr: inner, .. }
            | ExprData::Postfix { expr: inner, .. }
            | ExprData::Cast { expr: inner, .. }
            | ExprData::Paren(inner) => walk_expr(bodies, *inner, found),
            ExprData::Binary { lhs, rhs, .. } | ExprData::Assign { lhs, rhs, .. } => {
                walk_expr(bodies, *lhs, found);
                walk_expr(bodies, *rhs, found);
            }
            ExprData::InstanceOf { expr: inner, .. } => walk_expr(bodies, *inner, found),
            ExprData::Conditional { cond, then, els } => {
                walk_expr(bodies, *cond, found);
                walk_expr(bodies, *then, found);
                walk_expr(bodies, *els, found);
            }
            ExprData::Lambda { body, .. } => match body {
                hir_expand::body::LambdaBody::Expr(inner) => walk_expr(bodies, *inner, found),
                hir_expand::body::LambdaBody::Block(stmt) => walk_stmt(bodies, *stmt, found),
            },
            ExprData::MethodRef { qualifier, .. } => {
                if let Some(qualifier) = qualifier {
                    walk_expr(bodies, *qualifier, found);
                }
            }
            ExprData::Switch { scrutinee, arms } => {
                walk_expr(bodies, *scrutinee, found);
                for arm in arms {
                    for label in &arm.labels {
                        if let hir_expand::body::SwitchLabel::Expr(e) = label {
                            walk_expr(bodies, *e, found);
                        }
                    }
                    for s in &arm.body {
                        walk_stmt(bodies, *s, found);
                    }
                }
            }
            // Kotlin-only expressions — unreachable from a Java body.
            ExprData::Block(..)
            | ExprData::When { .. }
            | ExprData::Try { .. }
            | ExprData::Elvis { .. }
            | ExprData::SafeAccess { .. }
            | ExprData::NullAssert { .. }
            | ExprData::Range { .. }
            | ExprData::InfixCall { .. }
            | ExprData::ObjectLiteral { .. }
            | ExprData::CallableReference { .. }
            | ExprData::Spread { .. }
            | ExprData::Jump { .. } => {}
        }
    }
    for &stmt in &bodies.body(body_id).stmts {
        walk_stmt(bodies, stmt, &mut found);
    }
    found
}

/// §8.9.1: whether the abstract method `method` of the enum `item` is
/// implemented by the body of any of its constants. Each enum constant with a
/// body is an anonymous subclass of the enum type
/// ([§8.9.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.9.1)),
/// and it is there — not in the enum's own member list — that the abstract
/// methods of the enum are overridden. A constant body declaring a method of
/// the same name and arity discharges the enum's obligation; javac reports
/// the does-not-override-abstract error on the enum only when no constant
/// body implements the method. The constant bodies are not lowered into the
/// item tree (their methods live in the anonymous subclass), so the match is
/// read from the file's class bodies by name and formal-parameter count — a
/// deliberately conservative test: it cannot fabricate an implementation, and
/// a same-name same-arity method of a different parameter type is a
/// per-constant error javac reports separately.
fn enum_constant_body_implements(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    item: hir_def::java::item_tree::ItemId,
    method: &MethodData,
) -> bool {
    use syntax::java::{SourceFile as JavaSourceFile, SyntaxKind as J};
    let hir_def::java::item_tree::ItemData::Enum(data) = tree.data(item) else {
        return false;
    };
    let text = db.file_text(file).text(db);
    let Some((map, source)) = range_ctx(db, file, tree.language) else {
        return false;
    };
    for &constant in &data.body {
        let hir_def::java::item_tree::ItemData::EnumConstant(constant) = tree.data(constant) else {
            continue;
        };
        // The class body range is re-derived from the constant's syntax node.
        let Some(class_body) = ranges::enum_constant_class_body_range(map, &source, constant)
        else {
            continue;
        };
        // Re-parse the constant's class body as an anonymous class of the
        // enum, and look for a method declaration matching by name and arity.
        let body_text: String = text[class_body].to_string();
        let wrapped = format!("class __EnumConstant__ {body_text}");
        let parse = JavaSourceFile::parse(&wrapped);
        let root = parse.syntax_node();
        let mut found = false;
        for node in root.descendants() {
            if node.kind() != J::METHOD_DECL {
                continue;
            }
            let name = node
                .children_with_tokens()
                .find_map(|element| match element {
                    rowan::NodeOrToken::Token(token) if token.kind() == J::IDENTIFIER => {
                        Some(token.text().to_owned())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            if name != method.name {
                continue;
            }
            let arity = node
                .children()
                .find(|child| child.kind() == J::FORMAL_PARAMETERS)
                .map(|params| {
                    params
                        .children()
                        .filter(|child| {
                            matches!(child.kind(), J::FORMAL_PARAMETER | J::SPREAD_PARAMETER)
                        })
                        .count()
                })
                .unwrap_or(0);
            if arity == method.params.len() {
                found = true;
                break;
            }
        }
        if found {
            return true;
        }
    }
    false
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
    // The arity of each constructor: the declared formal-parameter count, or —
    // for a record *compact* constructor ([§8.10.4]) — the record's component
    // count. A compact constructor is lowered without a formal parameter list
    // ([`crate::hir_def`]), so its true signature (the component list it
    // assigns) must be recovered from the record declaration; without it a
    // delegating `this(...)` resolves against the compact constructor's
    // empty declared signature and fabricates a self-cycle.
    let arity = |id: ItemId| match tree.data(id) {
        I::Method(m) if m.is_compact_constructor() => match tree.data(item) {
            I::Record(record) => record.components.len(),
            _ => 0,
        },
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

/// §8.1.1.2: the canonical fully qualified names named in the `permits`
/// clause of the sealed class or interface `fqn`, resolved in its declaring
/// scope; `Some(vec)` for a sealed type (empty when it declares no `permits`
/// clause — the permitted set is then its same-module direct subclasses).
/// `None` when `fqn` is not sealed or its declaration cannot be recovered
/// (conservative — the sealed checks then stay silent).
fn sealed_permits(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> Option<Vec<Name>> {
    let resolved = hir::fqn_resolve(db, scope, fqn)?;
    match resolved {
        // A Kotlin file's facade is neither sealed nor a permitted subclass.
        hir::Resolved::KotlinFacade { .. } => None,
        hir::Resolved::Source(source) => {
            let tree = hir::java_item_tree(db, source.file);
            let (permits, sealed) = match tree.data(source.item) {
                ItemData::Class(d) | ItemData::Interface(d) => {
                    (&d.permits, d.modifiers.is_sealed())
                }
                ItemData::Record(d) => (&d.permits, d.modifiers.is_sealed()),
                _ => return None,
            };
            if !sealed {
                return None;
            }
            let file_scope = scope_for_file(db, source.file);
            let resolver =
                crate::java::resolve::Resolver::for_item(db, source.file, &tree, source.item);
            Some(
                permits
                    .iter()
                    .map(|ty| {
                        let ty =
                            crate::java::resolve::resolve_type_ref(db, &file_scope, &resolver, ty);
                        ty.as_reference(db)
                            .map(|(name, _)| name.clone())
                            .unwrap_or_else(|| Name::new(""))
                    })
                    .collect(),
            )
        }
        hir::Resolved::Library(resolved_class) => {
            let record = hir::class_record(db, &resolved_class)?;
            let hir::ClassOrModuleRecord::Class(class) = record.as_ref() else {
                return None;
            };
            // A library class is sealed exactly when its classfile carries a
            // `PermittedSubclasses` attribute; an empty one is not provably
            // sealed, so it is skipped.
            if class.permitted_subclasses.is_empty() {
                return None;
            }
            Some(
                class
                    .permitted_subclasses
                    .iter()
                    .map(|tyref| {
                        crate::java::resolve::ty_from_library(db, tyref)
                            .as_reference(db)
                            .map(|(name, _)| name.clone())
                            .unwrap_or_else(|| Name::new(""))
                    })
                    .collect(),
            )
        }
    }
}

/// §8.1.1.2: the sealed-hierarchy diagnostics of the class-like declaration
/// `item` (whose canonical FQN is `fqn`): a direct subclass of a `sealed`
/// supertype that is not named in its `permits` clause
/// ([`DeclDiagnostic::CantInheritFromSealed`]), and a permitted direct
/// subclass that is itself neither `sealed`, `non-sealed` nor `final`
/// ([`DeclDiagnostic::SealedSealedOrFinalExpected`]).
#[allow(clippy::too_many_arguments)]
fn sealed_subclass_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    scope: &hir::ResolutionScope,
    resolver: &crate::java::resolve::Resolver,
    item: hir_def::java::item_tree::ItemId,
    key: &crate::jvm::member::ClassKey,
    out: &mut Vec<DeclDiagnostic>,
) {
    // §14.3: a *local* class-like declaration can never appear in a `permits`
    // clause ([§8.1.1.2] names a top-level or member class), so it can neither
    // be reported as an unnamed direct subclass of a sealed type nor close a
    // hierarchy.
    let Some(fqn) = key.as_fqn() else {
        return;
    };
    let data = tree.data(item);
    let mods = class_like_modifiers(data);
    let final_ = mods.is_some_and(|m| m.is_final());
    let sealed = mods.is_some_and(|m| m.is_sealed());
    let non_sealed = mods.is_some_and(|m| m.is_non_sealed());
    // §8.1.1.2: an enum and a record are implicitly final ([§8.9], [§8.10]),
    // so they may always be a permitted subclass.
    let closed =
        final_ || sealed || non_sealed || matches!(data, ItemData::Enum(_) | ItemData::Record(_));
    // The direct supertypes of the declaration: a class's superclass and the
    // implemented interfaces; an interface's extended interfaces.
    let super_refs: Vec<&hir_def::jvm::decl::ItemTypeRef> = match data {
        ItemData::Class(d) => d.super_class.iter().chain(d.interfaces.iter()).collect(),
        ItemData::Interface(d) => d.interfaces.iter().collect(),
        ItemData::Record(d) => d.interfaces.iter().collect(),
        _ => return,
    };
    for super_ref in super_refs {
        let super_ty = crate::java::resolve::resolve_type_ref(db, scope, resolver, super_ref);
        let TyKind::Reference { name, .. } = super_ty.kind(db) else {
            continue;
        };
        let Some(permits) = sealed_permits(db, scope, name.as_str()) else {
            continue;
        };
        if permits.is_empty() || permits.iter().any(|p| p == fqn) {
            // §8.1.1.2: a permitted (or implicitly permitted) direct subclass
            // must be `final`, `sealed` or `non-sealed`.
            if !closed {
                out.push(DeclDiagnostic::SealedSealedOrFinalExpected {
                    range: item_name_range(db, file, tree, item),
                });
            }
        } else {
            // §8.1.1.2: extending a sealed supertype without being named in
            // its `permits` clause is an error.
            out.push(DeclDiagnostic::CantInheritFromSealed {
                super_owner: name.clone(),
                range: item_name_range(db, file, tree, item),
            });
        }
    }
}

/// Whether the class-like declaration `data` declares a `permits` clause
/// ([§8.1.1.2]).
fn has_permits_clause(data: &ItemData) -> bool {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => !d.permits.is_empty(),
        ItemData::Record(d) => !d.permits.is_empty(),
        _ => false,
    }
}

/// Whether any class-like declaration of `file` directly extends or
/// implements the type `fqn` — its direct-supertype walk, used by the
/// §8.1.1.2 "sealed class must have subclasses" check. Only the *same file*
/// is scanned: a sealed type whose subclasses live in another file is never
/// provably subclass-less, so the check stays silent for it.
fn file_has_direct_subclass(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    scope: &hir::ResolutionScope,
    fqn: &str,
) -> bool {
    fn walk(
        db: &dyn TyDatabase,
        file: FileId,
        tree: &hir_def::java::item_tree::ItemTree,
        scope: &hir::ResolutionScope,
        id: hir_def::java::item_tree::ItemId,
        fqn: &str,
    ) -> bool {
        let data = tree.data(id);
        let resolver = crate::java::resolve::Resolver::for_item(db, file, tree, id);
        let super_refs: Vec<&ItemTypeRef> = match data {
            ItemData::Class(d) => d.super_class.iter().chain(d.interfaces.iter()).collect(),
            ItemData::Interface(d) => d.interfaces.iter().collect(),
            ItemData::Record(d) => d.interfaces.iter().collect(),
            _ => Vec::new(),
        };
        for super_ref in super_refs {
            let ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref);
            if let Some((name, _)) = ty.as_reference(db)
                && name.as_str() == fqn
            {
                return true;
            }
        }
        for &child in data.body() {
            if walk(db, file, tree, scope, child, fqn) {
                return true;
            }
        }
        // A local class-like declaration ([JLS §14.3]) is not a member, so it
        // is not in any `body()`: it can extend the sealed type too.
        for local in tree.local_types_of(id) {
            if walk(db, file, tree, scope, local, fqn) {
                return true;
            }
        }
        false
    }
    for &top in &tree.top {
        if walk(db, file, tree, scope, top, fqn) {
            return true;
        }
    }
    false
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
                name_range: item_name_range(db, file, tree, top),
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
            name_range: item_name_range(db, file, tree, top),
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
            I::Field(f) if f.modifiers.is_final() && !f.has_initializer => {
                let name_range = item_name_range(db, file, tree, *child).unwrap_or_default();
                Some((f.name.clone(), f.modifiers.is_static(), name_range))
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
                // [JLS §14.3]: a local class's own bodies — its members', its
                // initializers' — are separate bodies of the file, not part of
                // this one.
                StmtData::Empty
                | StmtData::Break(_)
                | StmtData::Continue(_)
                | StmtData::LocalClass { .. }
                | StmtData::Missing => {}
                // A Kotlin local function — unreachable from a Java body.
                StmtData::LocalFunction { .. }
                | StmtData::Destructuring { .. }
                | StmtData::DeclDelegated { .. } => {}
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
                    anonymous: _,
                    ty: _,
                }
                | ExprData::CtorCall { args, .. } => {
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
                | ExprData::Cast {
                    ty: _, expr: inner, ..
                }
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
                // Kotlin-only expressions — unreachable from a Java body.
                ExprData::Block(..)
                | ExprData::When { .. }
                | ExprData::Try { .. }
                | ExprData::Elvis { .. }
                | ExprData::SafeAccess { .. }
                | ExprData::NullAssert { .. }
                | ExprData::Range { .. }
                | ExprData::InfixCall { .. }
                | ExprData::ObjectLiteral { .. }
                | ExprData::CallableReference { .. }
                | ExprData::Spread { .. }
                | ExprData::Jump { .. } => {}
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

/// §14.3: the modifiers a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// may carry. The grammar admits the whole `{ClassModifier}` prefix, and §14.3
/// makes the access modifiers, `static`, `sealed` and `non-sealed`
/// compile-time errors:
///
/// - "It is a compile-time error if a local class or interface declaration has
///   any of the access modifiers `public`, `protected`, or `private`" — and
///   the same sentence for `static`;
/// - "It is a compile-time error if a local class declaration has the modifier
///   `sealed` or `non-sealed`".
///
/// javac rejects the first group in its *parser*
/// (`compiler.err.illegal.start.of.expr`); the rule is reported here at the
/// offending modifier, which is where the IDE anchors it.
fn local_modifier_diagnostics(
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
    walk_local_modifiers(&syntax_node, &mut out);
    out
}

/// Recursively walks `node` for the class-like declarations a *block* declares
/// ([§14.3]) — a declaration whose parent is a `BLOCK` is a local one — and
/// inspects every modifier of each such declaration's modifier list.
fn walk_local_modifiers(
    node: &rowan::SyntaxNode<syntax::java::Lang>,
    out: &mut Vec<DeclDiagnostic>,
) {
    use syntax::java::SyntaxKind as J;
    for child in node.children() {
        let local = matches!(
            child.kind(),
            J::CLASS_DECL | J::INTERFACE_DECL | J::ENUM_DECL | J::RECORD_DECL
        ) && child
            .parent()
            .is_some_and(|parent| parent.kind() == J::BLOCK);
        if local
            && let Some(modifier_list) = child.children().find(|c| c.kind() == J::MODIFIER_LIST)
        {
            for (modifier, range) in modifier_tokens(&modifier_list) {
                match modifier {
                    // §14.3: the access modifiers and `static`.
                    "public" | "protected" | "private" | "static" => {
                        out.push(DeclDiagnostic::ModifierNotAllowedHere {
                            modifier,
                            range: Some(range),
                        });
                    }
                    // §14.3: `sealed` / `non-sealed`.
                    "sealed" | "non-sealed" => {
                        out.push(DeclDiagnostic::SealedOrNonSealedLocalClass {
                            modifier,
                            range: Some(range),
                        });
                    }
                    _ => {}
                }
            }
        }
        walk_local_modifiers(&child, out);
    }
}

/// The modifier keywords of a modifier list with their own source ranges, in
/// written order — the per-modifier ranges the §14.3 checks report at.
fn modifier_tokens(
    node: &rowan::SyntaxNode<syntax::java::Lang>,
) -> Vec<(&'static str, rowan::TextRange)> {
    use rowan::NodeOrToken;
    use syntax::java::SyntaxKind as J;
    let mut out: Vec<(&'static str, rowan::TextRange)> = Vec::new();
    let mut elements: Vec<_> = node.children_with_tokens().collect();
    elements.reverse();
    while let Some(element) = elements.pop() {
        let NodeOrToken::Token(token) = element else {
            continue;
        };
        let name = match token.kind() {
            J::PUBLIC_KW => "public",
            J::PROTECTED_KW => "protected",
            J::PRIVATE_KW => "private",
            J::ABSTRACT_KW => "abstract",
            J::FINAL_KW => "final",
            J::STATIC_KW => "static",
            J::DEFAULT_KW => "default",
            J::NATIVE_KW => "native",
            J::SYNCHRONIZED_KW => "synchronized",
            J::TRANSIENT_KW => "transient",
            J::VOLATILE_KW => "volatile",
            J::STRICTFP_KW => "strictfp",
            J::IDENTIFIER => match token.text() {
                // `non-sealed` lexes as `non - sealed` ([§8.1.1.2]): the
                // modifier's range covers all three tokens.
                "non" => {
                    let minus = elements.pop();
                    let sealed = elements.pop();
                    let end = match (&minus, &sealed) {
                        (Some(NodeOrToken::Token(minus)), Some(NodeOrToken::Token(sealed)))
                            if minus.kind() == J::MINUS && sealed.text() == "sealed" =>
                        {
                            sealed.text_range()
                        }
                        _ => token.text_range(),
                    };
                    out.push((
                        "non-sealed",
                        rowan::TextRange::new(token.text_range().start(), end.end()),
                    ));
                    continue;
                }
                "sealed" => "sealed",
                _ => continue,
            },
            _ => continue,
        };
        out.push((name, token.text_range()));
    }
    out
}

/// §14.3/[§6.4]/[§8.1]/[§9.1]: the checks a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3))
/// is subject to beyond the ones a declaration of that kind always is: its
/// direct supertypes may not be `sealed` (it can never be named in a `permits`
/// clause), its name may not already be in scope as another local declaration
/// (§6.4), and it may not repeat the simple name of an enclosing class or
/// interface ([§8.1], [§9.1]).
fn local_class_diagnostics(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &hir_def::java::item_tree::ItemTree,
    scope: &hir::ResolutionScope,
) -> Vec<DeclDiagnostic> {
    if tree.local_types.is_empty() {
        return Vec::new();
    }
    let Some((map, source)) = range_ctx(db, file, tree.language) else {
        return Vec::new();
    };
    let sites = crate::java::db::local_decl_sites_query(db, db.file_text(file));
    let mut out = Vec::new();
    for &item in &tree.local_types {
        let Some(declared_name) = tree.data(item).name().cloned() else {
            continue;
        };
        // §14.3: the direct superclass and direct superinterfaces of a local
        // class — and a local interface's direct superinterfaces — must not be
        // `sealed`: a local declaration can never be named in a `permits`
        // clause ([§8.1.1.2]), so no sealed supertype can admit it.
        let resolver = crate::java::resolve::Resolver::for_item(db, file, tree, item);
        let super_refs: Vec<&ItemTypeRef> = match tree.data(item) {
            ItemData::Class(d) => d.super_class.iter().chain(d.interfaces.iter()).collect(),
            ItemData::Interface(d) => d.interfaces.iter().collect(),
            ItemData::Record(d) => d.interfaces.iter().collect(),
            ItemData::Enum(d) => d.interfaces.iter().collect(),
            _ => continue,
        };
        for super_ref in super_refs {
            let ty = crate::java::resolve::resolve_type_ref(db, scope, &resolver, super_ref);
            let Some(resolved) = crate::java::resolve::reference_class(db, scope, &ty) else {
                continue;
            };
            if !class_is_sealed(db, &resolved) {
                continue;
            }
            out.push(DeclDiagnostic::LocalClassCantExtendSealed {
                super_owner: class_simple_name(db, &resolved),
                range: ranges::type_ref_range(map, &source, super_ref),
            });
        }

        // §6.4: a local declaration may not re-declare a name that is already
        // in scope as another local declaration, unless it is declared within
        // a *class or interface declaration* appearing within that scope.
        let redeclared = sites
            .get(&item)
            .and_then(|site| {
                site.local_types
                    .iter()
                    .rev()
                    .skip(1)
                    .find(|local| local.class.item != item && local.name == declared_name)
            })
            .map(|local| local.class.item);
        match redeclared {
            Some(earlier) if !local_redeclaration_is_legal(tree, item, earlier) => {
                out.push(DeclDiagnostic::DuplicateLocalClass {
                    name: declared_name.clone(),
                    kind: local_decl_kind(tree.data(item)),
                    container: local_container(tree, earlier),
                    range: item_name_range(db, file, tree, item),
                });
            }
            _ => {
                // §8.1/[§9.1]: a class may not have the same simple name as an
                // enclosing class or interface. A local declaration's
                // enclosing *local* declaration is the §6.4 case above, which
                // javac reports for that declaration instead — the enclosing
                // *named* classes are what this rule adds.
                if let Some(enclosing) = enclosing_class_with_name(tree, item, &declared_name) {
                    out.push(DeclDiagnostic::DuplicateLocalClass {
                        name: declared_name.clone(),
                        kind: local_decl_kind(tree.data(item)),
                        container: DuplicateContainer::EnclosingType { name: enclosing },
                        range: item_name_range(db, file, tree, item),
                    });
                }
            }
        }
    }
    out
}

/// Whether the resolved class is `sealed` ([JLS §8.1.1.2]): a source class with
/// the modifier, or a classpath class whose classfile carries a
/// `PermittedSubclasses` attribute ([JVMS §4.7.31]).
fn class_is_sealed(db: &dyn TyDatabase, resolved: &hir::Resolved) -> bool {
    match resolved {
        // A Kotlin file's facade carries no Kotlin `sealed` modifier.
        hir::Resolved::KotlinFacade { file, .. } => {
            let _ = file;
            false
        }
        hir::Resolved::Source(source) => {
            let tree = hir::java_item_tree(db, source.file);
            class_like_modifiers(tree.data(source.item)).is_some_and(|m| m.is_sealed())
        }
        hir::Resolved::Library(class) => hir::class_record(db, class)
            .and_then(|record| match record.as_ref() {
                hir::ClassOrModuleRecord::Class(class) => {
                    Some(!class.permitted_subclasses.is_empty())
                }
                hir::ClassOrModuleRecord::Module(_) => None,
            })
            .unwrap_or(false),
    }
}

/// §14.3: the noun a local declaration is reported under — `Class`,
/// `Interface`, `Enum` or `Record`.
fn local_decl_kind(data: &ItemData) -> &'static str {
    match data {
        ItemData::Class(_) => "Class",
        ItemData::Interface(_) => "Interface",
        ItemData::Enum(_) => "Enum",
        ItemData::Record(_) => "Record",
        _ => "Class",
    }
}

/// §6.4: the member body whose scope already holds the re-declared name — the
/// declaration the *earlier* local declaration belongs to, which is what javac
/// names in `class {A} is already defined in method {m}()` (an initializer
/// container is javac's separate `already.defined.in.clinit` key).
fn local_container(
    tree: &hir_def::java::item_tree::ItemTree,
    earlier: hir_def::java::item_tree::ItemId,
) -> DuplicateContainer {
    let Some(owner) = tree.parent_of(earlier) else {
        return DuplicateContainer::Member {
            noun: "method",
            name: None,
        };
    };
    match tree.data(owner) {
        ItemData::Method(method) => DuplicateContainer::Member {
            noun: if method.is_constructor() {
                "constructor"
            } else {
                "method"
            },
            name: Some(method.name.clone()),
        },
        ItemData::StaticInit(_) => DuplicateContainer::Member {
            noun: "static initializer",
            name: None,
        },
        ItemData::InstanceInit(_) => DuplicateContainer::Member {
            noun: "instance initializer",
            name: None,
        },
        _ => DuplicateContainer::Member {
            noun: "method",
            name: None,
        },
    }
}

/// §6.4's exception: the new local declaration is "declared within a class or
/// interface declaration appearing within the scope of" the earlier one, so
/// the re-declaration is legal. Probed against javac, the interface of that
/// sentence is: the class-like declaration that *directly* encloses the new
/// declaration must appear within the earlier declaration's scope — in the
/// body that declares it, but outside the earlier declaration itself. So
/// `class A {} class B { void n() { class A {} } }` is legal (the second `A`
/// is inside the local `B`, a sibling of the first), while a new `A` inside
/// `A`'s own body — a member method of it included — and a new `A` in the same
/// block as the first are not.
fn local_redeclaration_is_legal(
    tree: &hir_def::java::item_tree::ItemTree,
    redeclaring: hir_def::java::item_tree::ItemId,
    earlier: hir_def::java::item_tree::ItemId,
) -> bool {
    // The class-like declaration that directly encloses the redeclaration.
    let mut current = tree.parent_of(redeclaring);
    let nearest = loop {
        match current {
            Some(id) if tree.data(id).is_type() => break id,
            Some(id) => current = tree.parent_of(id),
            None => return false,
        }
    };
    let Some(scope_owner) = tree.parent_of(earlier) else {
        return false;
    };
    nearest != earlier && encloses(tree, scope_owner, nearest) && !encloses(tree, earlier, nearest)
}

/// The simple name a diagnostic about a resolved class renders ([§6.7]): the
/// source declaration's own name, or the last segment of a classpath name.
fn class_simple_name(db: &dyn TyDatabase, resolved: &hir::Resolved) -> Name {
    match resolved {
        // A facade is named after the file it belongs to.
        hir::Resolved::KotlinFacade { fqn, .. } => Name::new(fqn.simple_name()),
        hir::Resolved::Source(source) => hir::java_item_tree(db, source.file)
            .data(source.item)
            .name()
            .cloned()
            .unwrap_or_else(|| Name::new("")),
        hir::Resolved::Library(_) => Name::new(resolved.fqn(db).as_name().simple_name()),
    }
}

/// Whether the declaration `ancestor` encloses `item` (or is `item`).
fn encloses(
    tree: &hir_def::java::item_tree::ItemTree,
    ancestor: hir_def::java::item_tree::ItemId,
    item: hir_def::java::item_tree::ItemId,
) -> bool {
    let mut current = Some(item);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = tree.parent_of(id);
    }
    false
}

/// The canonically named enclosing class or interface of the local declaration
/// `item` that has the simple name `name`, if any ([§8.1], [§9.1]).
fn enclosing_class_with_name(
    tree: &hir_def::java::item_tree::ItemTree,
    item: hir_def::java::item_tree::ItemId,
    name: &Name,
) -> Option<Name> {
    let mut current = tree.parent_of(item);
    while let Some(id) = current {
        if let Some(declared) = tree.data(id).name()
            && declared == name
            && let Some(fqn) = crate::java::resolve::canonical_class_fqn(tree, id)
        {
            return Some(fqn);
        }
        current = tree.parent_of(id);
    }
    None
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
            // §8.4.3: which modifiers `abstract` contradicts depends on the
            // *kind* of declaration. On a method (or annotation element) an
            // abstract method cannot be `static`, `private`, `native`,
            // `synchronized`, `strictfp` or `default`; on a *class* those
            // combinations are ordinary — a nested `abstract static class`,
            // `private abstract class` or `abstract strictfp class` is legal
            // ([§8.1.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.1)),
            // so only `abstract` + `final` is rejected there.
            let is_method = matches!(
                child.kind(),
                J::METHOD_DECL | J::ANNOTATION_TYPE_ELEMENT_DECL
            );
            for (first, second) in conflicting_modifiers(&names, is_method) {
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

/// §8.9.1: an enum body must declare its constants first — the constant
/// section runs from the `{` to the first non-constant member or the `;`,
/// whichever comes first ([§8.9.1]). A member before the first constant is
/// javac's `enum constant expected here`; a constant after the separating
/// `;` is `enum constant not expected here`. The lowering drops the `;`
/// boundary (an empty `{ ; A }` constant section and a member-first body
/// lower alike), so the ordering is read from the file's parse tree — of the
/// same revision the item tree was lowered from.
fn enum_ordering_diagnostics(
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
    walk_enum_bodies(&syntax_node, &mut out);
    out
}

/// Walks `node` for `ENUM_BODY`s and reports the §8.9.1 constant-ordering
/// violations of each.
fn walk_enum_bodies(node: &rowan::SyntaxNode<syntax::java::Lang>, out: &mut Vec<DeclDiagnostic>) {
    use rowan::NodeOrToken;
    use syntax::java::SyntaxKind as J;
    for child in node.children() {
        if child.kind() == J::ENUM_BODY {
            // §8.9.1: the grammar is `{ [EnumConstantList] [,] [;] {ClassBodyDeclaration} }` —
            // constants, an optional `;`, then ordinary members. A member
            // before the first constant is javac's `enum constant expected
            // here`; a constant (or constant-section recovery) after the `;`
            // is `enum constant not expected here`. The lowered enum body
            // drops the `;` boundary, so the ordering is read from the
            // parse tree.
            let mut first_semi = None;
            let mut first_member_before_semi = None;
            let mut first_after_semi_const = None;
            let mut seen_semi = false;
            for element in child.children_with_tokens() {
                let range = element.text_range();
                match &element {
                    NodeOrToken::Token(token) if token.kind() == J::SEMICOLON && !seen_semi => {
                        seen_semi = true;
                        first_semi = Some(range);
                    }
                    NodeOrToken::Node(grandchild) => {
                        let kind = grandchild.kind();
                        if !seen_semi {
                            // Before the `;`: only enum constants belong. A
                            // class-body member — or the recovery `ERROR`
                            // node the parser produced for a member the
                            // grammar would not accept as a constant — ends
                            // the constant section illegally.
                            if kind == J::ENUM_CONSTANT {
                                continue;
                            }
                            // A `MISSING` node is the parser's
                            // "enum constant expected" recovery — the body
                            // began with a member the grammar would not
                            // accept as a constant and the parser skipped
                            // out of the constant section. Report at the
                            // member the recovery swallowed, when its source
                            // is visible, else at the recovery point.
                            if kind == J::MISSING || kind == J::ERROR {
                                first_member_before_semi.get_or_insert(range);
                                continue;
                            }
                            first_member_before_semi.get_or_insert_with(|| {
                                first_identifier_range(grandchild).unwrap_or(range)
                            });
                        } else if kind == J::ENUM_CONSTANT || kind == J::ERROR {
                            // After the `;` only ordinary members belong. A
                            // constant, or the recovery `ERROR` node holding
                            // the misplaced constants (`{ ; A, B; }` parses
                            // `A` as a missing declaration), is javac's
                            // `enum constant not expected here`.
                            first_after_semi_const.get_or_insert_with(|| {
                                first_identifier_range(grandchild).unwrap_or(range)
                            });
                        }
                    }
                    NodeOrToken::Token(_) => {}
                }
            }
            // §8.9.1: a *member* appearing before any `;` — the constant
            // section is over but never terminated. javac: `enum constant
            // expected here`, at the member.
            if let Some(range) = first_member_before_semi {
                out.push(DeclDiagnostic::EnumMemberBeforeConstants { range: Some(range) });
            }
            // A `;` that ends the constant section may not be followed by
            // constants ([§8.9.1]).
            if seen_semi && let Some(range) = first_after_semi_const {
                out.push(DeclDiagnostic::EnumConstantNotExpected { range: Some(range) });
            }
            let _ = first_semi;
        }
        walk_enum_bodies(&child, out);
    }
}

/// The source range of the first `IDENTIFIER` token under `node` — a
/// declaration's name, for anchoring an enum-ordering diagnostic.
fn first_identifier_range(
    node: &rowan::SyntaxNode<syntax::java::Lang>,
) -> Option<rowan::TextRange> {
    use syntax::java::SyntaxKind as J;
    node.children_with_tokens()
        .find_map(|element| match element {
            rowan::NodeOrToken::Token(token) if token.kind() == J::IDENTIFIER => {
                Some(token.text_range())
            }
            rowan::NodeOrToken::Token(_) => None,
            rowan::NodeOrToken::Node(_) => None,
        })
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
/// javac order (e.g. `abstract, final` for `final abstract`). `is_method`
/// selects whether the declaration is a method-like construct whose `abstract`
/// forbids the implementation keywords ([§8.4.3]) — a *class* may legally
/// combine `abstract` with `static`/`private`/`native`/`synchronized`/
/// `strictfp` ([§8.1.1]).
fn conflicting_modifiers(
    names: &[&'static str],
    is_method: bool,
) -> Vec<(&'static str, &'static str)> {
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
    // inheritance, or an implementation keyword. On a class-like declaration
    // these do not contradict: a nested class may be `abstract static`,
    // `private abstract`, `abstract strictfp`, and so on ([§8.1.1]).
    if has("abstract") {
        if is_method {
            for other in [
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
        if has("final") {
            push_unique(&mut pairs, "abstract", "final");
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
