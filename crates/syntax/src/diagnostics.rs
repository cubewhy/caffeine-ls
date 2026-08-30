//! Language-tagged diagnostic codes shared across the parser layer
//! ([crate::java], [`crate::kotlin`]) and the type layer (`hir-ty`), so the
//! IDE/LSP layer can attach a stable `code` to every diagnostic it surfaces
//! and key code actions off it (the LSP `Diagnostic.code` field and
//! `CodeAction.data`).
//!
//! Codes are kebab-case strings scoped per language (each document is a
//! single language, so no language prefix is needed), distinct from the
//! human-readable message.

/// The code of a diagnostic, tagged by language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCode {
    /// A Java diagnostic.
    Java(JavaDiagnosticCode),
    /// A Kotlin diagnostic. The enum is currently empty; Kotlin diagnostics
    /// carry no code today.
    Kotlin(KotlinDiagnosticCode),
}

/// Java diagnostic codes, spanning the lexer, the parser and the type layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaDiagnosticCode {
    // Type layer (`hir-ty`).
    /// §14.4.1: a `var` declaration must have an initializer.
    VarWithoutInitializer,
    /// §6.5: a simple name resolves to nothing.
    CannotResolveName,
    /// §6.5.5.1: a reference type name resolves to nothing on the classpath.
    CannotResolveType,
    /// §6.5.5.1/[§7.5.2]: a simple type name is ambiguous between on-demand
    /// imports.
    AmbiguousName,
    /// §7.5.1: a single-type import names a class that cannot be found.
    UnresolvedImport,
    /// §7.5.1/[§7.5.2]: an on-demand import (`import pkg.*;`) names a package
    /// that is not observable on the classpath.
    UnresolvedImportPackage,
    /// §7.5.4/[§7.5.2]: a static on-demand import (`import static pkg.Type.*;`)
    /// names a class that cannot be found.
    UnresolvedStaticImport,
    /// §7.5.1: two imports conflict for the same simple name.
    ConflictingImport,
    /// §7.4.3/[§7.7.2]: a package is not visible from the current module.
    ModuleNotAccessible,
    /// §15.11: no field of the name on the receiver.
    NoSuchField,
    /// §15.12.1: no method of the name on the receiver.
    NoSuchMethod,
    /// §15.9/[§8.8.7.1]: no constructor of the name on the class — an
    /// unqualified `new`/`this(...)`/`super(...)` of a class that declares
    /// none of that signature.
    NoSuchConstructor,
    /// §15.12.2: no member is applicable to the actual arguments.
    WrongArity,
    /// §14.18: a `throw` operand is not assignable to `Throwable`; also used
    /// for a declaration initializer, an assignment or a returned expression
    /// not assignable to its target ([§5.2], [§14.17]).
    IncompatibleTypes,
    /// §14.9/§14.11/§14.12.1/§14.16/§15.25: a condition position requires a
    /// `boolean`.
    NonBooleanCondition,
    /// §15.15/§15.17/§15.18/§15.22: a numeric operand of a unary, binary or
    /// shift operator is not numeric.
    IncompatibleOperand,
    /// §15.21/§15.20: the operands of an equality or relational operator are
    /// not comparable.
    IncomparableTypes,
    /// §14.14.2: the expression of a for-each loop is not an array or
    /// `Iterable`.
    NonIterableForEach,
    /// §5.5/§15.16: a cast is not a casting conversion.
    BadCast,
    /// §15.10: generic array creation (`new List<String>[3]`).
    GenericArrayCreation,
    /// §15.9: instantiating a type variable, interface, abstract class or enum
    /// with `new`.
    CannotInstantiateTypeVar,
    /// §14.4: a `var` initializer cannot be an array initializer.
    VarArrayInitializer,
    /// §14.11.1: the selector of a `switch` is not one of the allowed types
    /// (`char`, `byte`, `short`, `int` or their boxes, `String`, an enum).
    SwitchSelectorType,
    /// §8.4.8.3: an override's return type is not return-type-substitutable.
    IncompatibleOverride,
    /// §9.4.1.3: two unrelated superinterfaces declare matching default
    /// methods and the class does not override them.
    ConflictingDefaults,
    /// §9.8/§15.27.3: a lambda or method reference target is not a functional
    /// interface.
    NotAFunctionalInterface,
    /// §11.2: a checked exception is thrown but neither caught nor declared.
    UnreportedException,
    /// §11.2.3: a catch clause is shadowed by an earlier superclass catch.
    AlreadyCaught,
    /// §8.3.3: a field initializer reads a same-class field declared later
    /// by simple name.
    IllegalForwardReference,
    /// §16: a local variable's value is read before it is definitely
    /// assigned.
    VariableMightNotHaveBeenInitialized,
    /// §14.11.1/§15.28: a switch expression does not cover all possible
    /// selector values.
    NotExhaustive,
    /// §14.11.1/§15.28: a `case` label of a primitive- or `String`-selector
    /// switch is not a constant expression.
    NonConstantCaseLabel,
    /// §14.11.1: two `case` labels of one `switch` declare the same constant
    /// value.
    DuplicateCaseLabel,
    /// §4.12.2: a generic class is used without type arguments — a raw type.
    RawTypeUse,
    /// §5.1.9/§5.2: a raw value converts to a parameterized type without a
    /// static guarantee — unchecked conversion.
    UncheckedConversion,
    /// §9.6.4.4: a method annotated `@Override` overrides or implements no
    /// supertype method.
    MethodDoesNotOverride,
    /// §14.22: a statement is unreachable — the preceding one cannot
    /// complete normally.
    UnreachableStatement,
    /// §8.4.7: a method with a non-`void` return type can complete normally
    /// without a `return`.
    MissingReturnValue,
    /// §11.2.3: a `catch` clause names a checked exception that the `try`
    /// block cannot throw.
    CatchNeverThrown,
    /// §7.2.1 (javadoc/classpath convention, no javac `compiler.*` twin): the
    /// file's package directory under its source root does not equal its
    /// declared package — the class will not resolve by FQN on a conventional
    /// classpath.
    UnexpectedPackagePath,
    /// §15.12.3: an unqualified invocation of an instance method from a static
    /// context ([§8.1.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.1.3)).
    NonStaticMethodFromStaticContext,
    /// §15.8.3/[§15.8.4]: the `this` or `super` keyword is used in a static
    /// context ([§8.1.3]) — a static method body, a static field initializer,
    /// a static initializer or an enum constant, where no enclosing instance
    /// exists. javac reports `non-static variable this cannot be referenced
    /// from a static context` for both keywords.
    NonStaticThisFromStaticContext,
    /// §15.11: a simple-name read or write of an *instance* field of the
    /// implicit receiver in a static context ([§8.1.3]) — the field can only
    /// be reached through `this`, which does not exist there.
    NonStaticFieldFromStaticContext,
    /// §7.4.1: a compilation unit declares more than one `package` declaration
    /// — the second and later are errors.
    DuplicatePackage,
    /// §7.6: two or more class-like declarations in the same package (across
    /// one or several files) share a fully qualified name.
    DuplicateClass,
    /// §7.6: a `public` top-level class-like declaration must be declared in a
    /// file named after its simple name — which also means at most one `public`
    /// top-level type per file.
    ClassPublicShouldBeInFile,
    /// §9.6.4.1: an annotation is used on a declaration or type whose element
    /// type is not in its `@Target` set (or, for a type-use annotation, the
    /// target includes neither `TYPE_USE` nor the declaration's element type).
    AnnotationNotApplicable,
    /// §9.7.1: an annotation element-value pair names an element the annotation
    /// type does not declare.
    UnknownAnnotationMember,
    /// §9.7.1: the same annotation element is given a value twice.
    DuplicateAnnotationMemberValue,
    /// §9.7.1/[§5.2]: an annotation element value is not assignable to its
    /// element's declared type ([§9.6.1]).
    AnnotationElementTypeMismatch,
    /// §9.7.1/[§8.9]: an enum-constant element value names a constant that the
    /// element's (enum) type does not declare.
    UnknownAnnotationElementConstant,
    /// §8.8: a constructor declaration's name is not the simple name of the
    /// class that contains it. javac reports `invalid method declaration;
    /// return type required` ([JLS §8.8](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.8)):
    /// a constructor-shaped declaration (`Name(...)`) whose name matches
    /// neither the class nor any return type is a method with a missing
    /// result.
    ConstructorNameMismatch,
    /// §8.1.1/[§8.4.3]: a declaration carries two or more modifiers that the
    /// JLS forbids from co-occurring — two access modifiers, `abstract` with
    /// `final`/`static`/`private`/`default`/`native`/`synchronized`/`strictfp`,
    /// `final` with `sealed` or `volatile`, `sealed` with `non-sealed`. javac
    /// reports the pair as `illegal combination of modifiers`.
    IllegalModifierCombination,
    /// §8.1.1.2: a class or interface declaration whose direct superclass
    /// (in `extends`) is a `final` class — a final class cannot have
    /// subclasses. javac: `cannot inherit from final {F}`.
    CannotInheritFromFinalClass,
    /// §8.4.3.3: a declaration of a method with the same signature as a
    /// `final` method inherited from a superclass or superinterface — a final
    /// method can neither be overridden (instance) nor hidden (static). javac:
    /// `{m} in {D} cannot override {m} in {S}; overridden method is final`.
    CannotOverrideFinalMethod,
    /// §8.4.8.3: an override or implementation whose access is weaker than
    /// the access of the method it overrides — `public` > `protected` >
    /// package-private > `private`. javac: `{m} in {D} cannot override {m} in
    /// {S}; attempting to assign weaker access privileges`.
    WeakerAccessPrivileges,
    /// §8.1.1.1: a non-abstract class (or record, or enum) inherits an
    /// abstract method and does not implement it with a concrete method of the
    /// same signature. javac: `{C} is not abstract and does not override
    /// abstract method {m} in {A}`.
    UnimplementedAbstractMethod,
    /// §8.1.4/[§9.1.3]: a class or interface appears in its own inheritance
    /// chain — `class A extends B` with `class B extends A`. javac: `cyclic
    /// inheritance involving {C}`.
    CyclicInheritance,
    /// §6.6: a field, method or constructor exists on the receiver type with
    /// the referenced name but is not accessible from the enclosing class —
    /// its access is more restrictive than [§6.6.1]/[§6.6.2] allows at the
    /// access site. javac: `{member} has {access} access in {owner}`.
    IllegalAccess,
    /// §8.8.7: a class with no declared constructor has an implicit default
    /// constructor that invokes `super()`; a direct superclass with no
    /// accessible no-argument constructor makes that implicit call fail. javac:
    /// `implicit super constructor {S}() is undefined`.
    NoDefaultConstructor,
    /// §8.8.7.1: the `this(...)` delegation chain between a class's own
    /// constructors cycles, so no path ever reaches the supertype constructor.
    /// javac: `recursive constructor invocation`.
    RecursiveConstructorInvocation,
    /// §8.8.7.1: an explicit constructor invocation `this(...)`/`super(...)`
    /// that is not the first statement of the constructor body. javac: `call
    /// to {this|super} must be first statement in constructor`.
    ConstructorCallNotFirst,
    /// §8.8.7.1: a reference to `this`, `super` or an instance member of the
    /// class being constructed before the supertype constructor has been
    /// called. javac: `cannot reference {x} before supertype constructor has
    /// been called`.
    CannotReferenceBeforeSuper,
    /// §8.3.1.2/[§16]: an assignment (or increment) whose target is a `final`
    /// variable or field. javac: `cannot assign a value to final variable
    /// {x}`.
    CannotAssignToFinalVariable,
    /// §8.3.1.2/[§16]: a blank `final` instance field that is not assigned in
    /// every constructor, or a blank `final` static field not assigned in the
    /// static initializers. javac: `variable {x} might not have been
    /// initialized`.
    FinalFieldNotInitialized,
    /// §6.5.6.1/[§15.27.2]: a local variable captured by a lambda expression
    /// (or local class, or a method reference receiver) that is not effectively
    /// final — it is reassigned after its initializer. javac: `local variables
    /// referenced from a lambda expression must be final or effectively final`.
    VariableMustBeEffectivelyFinal,
    /// §6.4: a variable, parameter or member is already defined in the scope —
    /// two fields of one class, two locals/parameters of one method scope, or a
    /// nested-block local shadowing an enclosing local. javac: `{x} is already
    /// defined in {y}`.
    DuplicateDeclaration,
    /// §4.5.1: a type argument of a parameterized type is not within the bounds
    /// of the type parameter it fills — `class M<T extends Number>` used as
    /// `M<String>`. javac: `type argument {a} is not within bounds of
    /// type-variable {T}`.
    TypeArgumentOutOfBounds,
    /// §8.4.2: two methods of one class have the same erasure
    /// ([§4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6))
    /// but different parameterized signatures — `void m(List<String>)` and
    /// `void m(List<Integer>)` — and neither overrides the other. javac: `name
    /// clash: {m1} and {m2} have the same erasure, yet neither overrides the
    /// other`.
    NameClashSameErasure,
    /// §15.9: a class instance creation whose type argument is a wildcard
    /// (`new ArrayList<?>()`) — a wildcard never names a concrete type. javac:
    /// `unexpected type; required: exact type, found: ?`.
    CannotInstantiateWildcard,
    /// §8.1.2/[§8.1.1.1]: a *generic* class may not be a direct or indirect
    /// subclass of `java.lang.Throwable`. javac: `generic class {C} may not
    /// subclass java.lang.Throwable`.
    GenericCannotExtendThrowable,
    /// §14.20: a catch clause parameter declared with a type variable — an
    /// exception type must be a concrete class, not a type parameter. javac:
    /// `unexpected type; required: class, found: type parameter T`.
    CannotCatchTypeVariable,
    /// §4.7/[§15.20.2]: the reference type of an `instanceof` check must be
    /// reifiable — a type variable or a parameterized type with a non-wildcard
    /// (or bounded-wildcard) argument is not. javac: `inconvertible types;
    /// cannot cast {o} to {T}` / `cannot perform instanceof check against
    /// non-reifiable type`.
    IllegalGenericInstanceOf,
    /// §14.16: a `continue` statement with no target loop — it appears
    /// outside any `while`, `do`, `for` or enhanced `for` statement. javac:
    /// `continue outside of loop`.
    ContinueOutsideLoop,
    /// §14.15: an unlabeled `break` statement outside any `switch`, `while`,
    /// `do`, `for` or enhanced `for` statement. javac: `break outside switch
    /// or loop`.
    BreakOutsideSwitchOrLoop,
    /// §14.15/[§14.16]: a `break label;` or `continue label;` whose label is
    /// not the label of an enclosing statement — no labeled statement of that
    /// name is in scope. javac: `undefined label: {label}`.
    UndefinedLabel,
    /// §14.16: a `continue label;` whose label names an enclosing statement
    /// that is not a loop — a `continue` can only target a labeled loop.
    /// javac: `not a loop label: {label}`.
    NotALoopLabel,
    /// §14.30.2: a record pattern declares a different number of nested
    /// patterns than the record has components. javac: `incorrect number of
    /// nested patterns`.
    IncorrectNumberOfPatternComponents,
    /// §8.1.1.2: a class directly extends (or a class or interface directly
    /// implements/extends) a `sealed` supertype without being named in its
    /// `permits` clause. javac: `class is not allowed to extend sealed class
    /// {S}` / `sealed class {S} not permitted to extend`.
    CantInheritFromSealed,
    /// §8.1.1.2: a permitted direct subclass of a `sealed` supertype is
    /// itself neither `sealed`, `non-sealed` nor `final` — it must be one of
    /// the three, or the hierarchy is not closed. javac: `{sealed, non-sealed
    /// or final} expected`.
    SealedSealedOrFinalExpected,
    /// §8.1.1.2: a `sealed` class or interface has no direct subclasses —
    /// a sealed type must have at least one. javac: `sealed class must have
    /// subclasses`.
    SealedClassMustHaveSubclasses,
    /// §8.4.3/[§9.4: a modifier on a member declaration that the JLS forbids
    /// for that member's kind — a `protected` interface method, for example.
    /// javac: `modifier {m} not allowed here`.
    ModifierNotAllowedHere,
    /// §8.4.5/[§9.4: a method with no body that is neither `abstract` nor
    /// `native` (an interface `private`/`static`/`default` method without a
    /// body, or a class method without `abstract`). javac: `missing method
    /// body, or declare abstract`.
    MissingMethodBodyOrDeclareAbstract,
    // Lexical ([JLS §3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html)).
    UnexpectedChar,
    UnterminatedString,
    UnterminatedComment,
    InvalidChar,
    IllegalTextBlockOpen,
    UnterminatedTextBlock,
    InvalidNumber,
    InvalidUnicodeEscape,
    UnterminatedChar,
    InvalidEscapeSequence,
    UnterminatedTemplate,
    // Parse.
    ExpectedToken,
    ExpectedKeyword,
    ExpectedConstruct,
    NotAStatement,
}

/// Kotlin diagnostic codes — none defined yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KotlinDiagnosticCode {}

impl DiagnosticCode {
    /// The stable machine-readable code string, without a language prefix.
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticCode::Java(code) => code.as_str(),
            DiagnosticCode::Kotlin(code) => match *code {},
        }
    }
}

impl JavaDiagnosticCode {
    /// The javac `compiler.*` code this diagnostic maps to, when the
    /// underlying construct has a 1:1 javac twin ([`crate::java::JavacCode`]).
    /// `None` for recovery-only or preview-restricted diagnostics that javac
    /// never emits in this form — those keep the custom code from
    /// [`Self::as_str`].
    pub fn javac_code(&self) -> Option<&'static str> {
        use JavaDiagnosticCode::*;
        match self {
            VarWithoutInitializer => Some("compiler.err.cant.infer.local.var.type"),
            VarArrayInitializer => Some("compiler.err.cant.infer.local.var.type"),
            CannotResolveName => Some("compiler.err.cant.resolve.location"),
            CannotResolveType => Some("compiler.err.cant.resolve.location"),
            AmbiguousName => Some("compiler.err.ref.ambiguous"),
            UnresolvedImport => Some("compiler.err.doesnt.exist"),
            UnresolvedImportPackage => Some("compiler.err.doesnt.exist"),
            UnresolvedStaticImport => Some("compiler.err.cant.resolve.location"),
            ConflictingImport => Some("compiler.err.already.defined.single.import"),
            ModuleNotAccessible => Some("compiler.err.package.not.visible"),
            NoSuchField => Some("compiler.err.cant.resolve.location"),
            NoSuchMethod => Some("compiler.err.cant.resolve.location"),
            NoSuchConstructor => Some("compiler.err.cant.resolve.location"),
            WrongArity => Some("compiler.err.cant.apply.symbol"),
            IncompatibleTypes => Some("compiler.err.prob.found.req"),
            NonBooleanCondition => Some("compiler.err.prob.found.req"),
            IncompatibleOperand => Some("compiler.err.operator.cant.be.applied.1"),
            IncomparableTypes => Some("compiler.err.operator.cant.be.applied.1"),
            NonIterableForEach => Some("compiler.err.foreach.not.applicable.to.type"),
            BadCast => Some("compiler.err.prob.found.req"),
            GenericArrayCreation => Some("compiler.err.generic.array.creation"),
            CannotInstantiateTypeVar => Some("compiler.err.abstract.cant.be.instantiated"),
            SwitchSelectorType => None,
            IncompatibleOverride => Some("compiler.err.override.incompatible.ret"),
            ConflictingDefaults => Some("compiler.err.types.incompatible"),
            NotAFunctionalInterface => Some("compiler.err.prob.found.req"),
            UnreportedException => Some("compiler.err.unreported.exception.need.to.catch.or.throw"),
            AlreadyCaught => Some("compiler.err.except.already.caught"),
            IllegalForwardReference => Some("compiler.err.illegal.forward.ref"),
            VariableMightNotHaveBeenInitialized => {
                Some("compiler.err.var.might.not.have.been.initialized")
            }
            NotExhaustive => Some("compiler.err.not.exhaustive"),
            NonConstantCaseLabel => Some("compiler.err.const.expr.req"),
            DuplicateCaseLabel => Some("compiler.err.duplicate.case.label"),
            RawTypeUse => Some("compiler.warn.raw.class.use"),
            UncheckedConversion => Some("compiler.warn.unchecked.assign"),
            MethodDoesNotOverride => Some("compiler.err.method.does.not.override.superclass"),
            UnreachableStatement => Some("compiler.err.unreachable.stmt"),
            MissingReturnValue => Some("compiler.err.missing.ret.stmt"),
            CatchNeverThrown => Some("compiler.err.except.never.thrown.in.try"),
            NonStaticMethodFromStaticContext => Some("compiler.err.non-static.cant.be.ref"),
            NonStaticThisFromStaticContext => Some("compiler.err.non-static.cant.be.ref"),
            NonStaticFieldFromStaticContext => Some("compiler.err.non-static.cant.be.ref"),
            UnexpectedPackagePath => None,
            DuplicatePackage => None,
            DuplicateClass => Some("compiler.err.duplicate.class"),
            ClassPublicShouldBeInFile => Some("compiler.err.class.public.should.be.in.file"),
            AnnotationNotApplicable => Some("compiler.err.annotation.not.applicable"),
            UnknownAnnotationMember => Some("compiler.err.no.annotation.member"),
            DuplicateAnnotationMemberValue => {
                Some("compiler.err.duplicate.annotation.member.value")
            }
            AnnotationElementTypeMismatch => Some("compiler.err.prob.found.req"),
            UnknownAnnotationElementConstant => Some("compiler.err.cant.resolve.location"),
            ConstructorNameMismatch => Some("compiler.err.invalid.meth.decl.ret.type.req"),
            IllegalModifierCombination => Some("compiler.err.illegal.combination.of.modifiers"),
            CannotInheritFromFinalClass => Some("compiler.err.cant.inherit.from.final"),
            CannotOverrideFinalMethod => Some("compiler.err.override.meth"),
            WeakerAccessPrivileges => Some("compiler.err.override.weaker.access"),
            UnimplementedAbstractMethod => Some("compiler.err.does.not.override.abstract"),
            CyclicInheritance => Some("compiler.err.cyclic.inheritance"),
            IllegalAccess => Some("compiler.err.report.access"),
            NoDefaultConstructor => Some("compiler.err.implicit.super.constructor.undefined"),
            RecursiveConstructorInvocation => Some("compiler.err.recursive.ctor.invocation"),
            ConstructorCallNotFirst => Some("compiler.err.call.to.super.must.be.first.stmt"),
            CannotReferenceBeforeSuper => Some("compiler.err.cant.ref.before.ctor.called"),
            CannotAssignToFinalVariable => Some("compiler.err.cant.assign.val.to.var"),
            FinalFieldNotInitialized => Some("compiler.err.var.might.not.have.been.initialized"),
            VariableMustBeEffectivelyFinal => {
                Some("compiler.err.cant.ref.non.effectively.final.var")
            }
            DuplicateDeclaration => Some("compiler.err.already.defined"),
            TypeArgumentOutOfBounds => Some("compiler.err.not.within.bounds"),
            NameClashSameErasure => Some("compiler.err.name.clash.same.erasure"),
            CannotInstantiateWildcard => Some("compiler.err.type.found.req"),
            GenericCannotExtendThrowable => Some("compiler.err.generic.throwable"),
            CannotCatchTypeVariable => Some("compiler.err.type.found.req"),
            IllegalGenericInstanceOf => Some("compiler.err.instanceof.reifiable.not.safe"),
            ContinueOutsideLoop => Some("compiler.err.cont.outside.loop"),
            BreakOutsideSwitchOrLoop => Some("compiler.err.break.outside.switch.loop"),
            UndefinedLabel => Some("compiler.err.undef.label"),
            NotALoopLabel => Some("compiler.err.not.loop.label"),
            IncorrectNumberOfPatternComponents => {
                Some("compiler.err.incorrect.number.of.nested.patterns")
            }
            CantInheritFromSealed => Some("compiler.err.cant.inherit.from.sealed"),
            SealedSealedOrFinalExpected => Some("compiler.err.non.sealed.sealed.or.final.expected"),
            SealedClassMustHaveSubclasses => Some("compiler.err.sealed.class.must.have.subclasses"),
            ModifierNotAllowedHere => Some("compiler.err.mod.not.allowed.here"),
            MissingMethodBodyOrDeclareAbstract => {
                Some("compiler.err.missing.meth.body.or.decl.abstract")
            }
            UnexpectedChar | InvalidChar => Some("compiler.err.illegal.char"),
            UnterminatedString => Some("compiler.err.unclosed.str.lit"),
            UnterminatedComment => Some("compiler.err.unclosed.comment"),
            IllegalTextBlockOpen => Some("compiler.err.illegal.text.block.open"),
            UnterminatedTextBlock => Some("compiler.err.unclosed.text.block"),
            InvalidNumber => None,
            InvalidUnicodeEscape => Some("compiler.err.illegal.unicode.esc"),
            UnterminatedChar => Some("compiler.err.unclosed.char.lit"),
            InvalidEscapeSequence => Some("compiler.err.illegal.esc.char"),
            UnterminatedTemplate => None,
            ExpectedToken | ExpectedKeyword | ExpectedConstruct => None,
            NotAStatement => Some("compiler.err.not.stmt"),
        }
    }

    /// The stable machine-readable code string; kept `'static` so it can be
    /// cheaply embedded in an LSP `code`/code-action `data` payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            JavaDiagnosticCode::VarWithoutInitializer => "var-without-initializer",
            JavaDiagnosticCode::CannotResolveName => "cannot-resolve-symbol",
            JavaDiagnosticCode::CannotResolveType => "cannot-resolve-type",
            JavaDiagnosticCode::AmbiguousName => "ambiguous-name",
            JavaDiagnosticCode::UnresolvedImport => "unresolved-import",
            JavaDiagnosticCode::UnresolvedImportPackage => "unresolved-import-package",
            JavaDiagnosticCode::UnresolvedStaticImport => "unresolved-static-import",
            JavaDiagnosticCode::ConflictingImport => "conflicting-import",
            JavaDiagnosticCode::ModuleNotAccessible => "module-not-accessible",
            JavaDiagnosticCode::NoSuchField => "no-such-field",
            JavaDiagnosticCode::NoSuchMethod => "no-such-method",
            JavaDiagnosticCode::NoSuchConstructor => "no-such-constructor",
            JavaDiagnosticCode::WrongArity => "wrong-argument-count",
            JavaDiagnosticCode::IncompatibleTypes => "incompatible-types",
            JavaDiagnosticCode::NonBooleanCondition => "non-boolean-condition",
            JavaDiagnosticCode::IncompatibleOperand => "incompatible-operand",
            JavaDiagnosticCode::IncomparableTypes => "incomparable-types",
            JavaDiagnosticCode::NonIterableForEach => "non-iterable-for-each",
            JavaDiagnosticCode::BadCast => "bad-cast",
            JavaDiagnosticCode::GenericArrayCreation => "generic-array-creation",
            JavaDiagnosticCode::CannotInstantiateTypeVar => "cannot-instantiate-type-var",
            JavaDiagnosticCode::VarArrayInitializer => "var-array-initializer",
            JavaDiagnosticCode::SwitchSelectorType => "switch-selector-type",
            JavaDiagnosticCode::IncompatibleOverride => "incompatible-override",
            JavaDiagnosticCode::ConflictingDefaults => "conflicting-defaults",
            JavaDiagnosticCode::NotAFunctionalInterface => "not-a-functional-interface",
            JavaDiagnosticCode::UnreportedException => "unreported-exception",
            JavaDiagnosticCode::AlreadyCaught => "already-caught",
            JavaDiagnosticCode::IllegalForwardReference => "illegal-forward-reference",
            JavaDiagnosticCode::VariableMightNotHaveBeenInitialized => {
                "variable-might-not-have-been-initialized"
            }
            JavaDiagnosticCode::NotExhaustive => "not-exhaustive",
            JavaDiagnosticCode::NonConstantCaseLabel => "non-constant-case-label",
            JavaDiagnosticCode::DuplicateCaseLabel => "duplicate-case-label",
            JavaDiagnosticCode::RawTypeUse => "raw-type-use",
            JavaDiagnosticCode::UncheckedConversion => "unchecked-conversion",
            JavaDiagnosticCode::MethodDoesNotOverride => "method-does-not-override",
            JavaDiagnosticCode::UnreachableStatement => "unreachable-statement",
            JavaDiagnosticCode::MissingReturnValue => "missing-return-value",
            JavaDiagnosticCode::CatchNeverThrown => "catch-never-thrown",
            JavaDiagnosticCode::UnexpectedPackagePath => "package-path-mismatch",
            JavaDiagnosticCode::NonStaticMethodFromStaticContext => {
                "non-static-method-from-static-context"
            }
            JavaDiagnosticCode::NonStaticThisFromStaticContext => {
                "non-static-this-from-static-context"
            }
            JavaDiagnosticCode::NonStaticFieldFromStaticContext => {
                "non-static-field-from-static-context"
            }
            JavaDiagnosticCode::DuplicatePackage => "duplicate-package",
            JavaDiagnosticCode::DuplicateClass => "duplicate-class",
            JavaDiagnosticCode::ClassPublicShouldBeInFile => "class-public-should-be-in-file",
            JavaDiagnosticCode::AnnotationNotApplicable => "annotation-not-applicable",
            JavaDiagnosticCode::UnknownAnnotationMember => "unknown-annotation-member",
            JavaDiagnosticCode::DuplicateAnnotationMemberValue => {
                "duplicate-annotation-member-value"
            }
            JavaDiagnosticCode::AnnotationElementTypeMismatch => "annotation-element-type-mismatch",
            JavaDiagnosticCode::UnknownAnnotationElementConstant => {
                "unknown-annotation-element-constant"
            }
            JavaDiagnosticCode::ConstructorNameMismatch => "constructor-name-mismatch",
            JavaDiagnosticCode::IllegalModifierCombination => "illegal-combination-of-modifiers",
            JavaDiagnosticCode::CannotInheritFromFinalClass => "cannot-inherit-from-final-class",
            JavaDiagnosticCode::CannotOverrideFinalMethod => "cannot-override-final-method",
            JavaDiagnosticCode::WeakerAccessPrivileges => "weaker-access-privileges",
            JavaDiagnosticCode::UnimplementedAbstractMethod => "unimplemented-abstract-method",
            JavaDiagnosticCode::CyclicInheritance => "cyclic-inheritance",
            JavaDiagnosticCode::IllegalAccess => "illegal-access",
            JavaDiagnosticCode::NoDefaultConstructor => "no-default-constructor",
            JavaDiagnosticCode::RecursiveConstructorInvocation => {
                "recursive-constructor-invocation"
            }
            JavaDiagnosticCode::ConstructorCallNotFirst => "constructor-call-not-first",
            JavaDiagnosticCode::CannotReferenceBeforeSuper => "cannot-reference-before-super",
            JavaDiagnosticCode::CannotAssignToFinalVariable => "cannot-assign-to-final-variable",
            JavaDiagnosticCode::FinalFieldNotInitialized => "final-field-not-initialized",
            JavaDiagnosticCode::VariableMustBeEffectivelyFinal => {
                "variable-must-be-effectively-final"
            }
            JavaDiagnosticCode::DuplicateDeclaration => "duplicate-declaration",
            JavaDiagnosticCode::TypeArgumentOutOfBounds => "type-argument-not-within-bounds",
            JavaDiagnosticCode::NameClashSameErasure => "name-clash-same-erasure",
            JavaDiagnosticCode::CannotInstantiateWildcard => "cannot-instantiate-wildcard",
            JavaDiagnosticCode::GenericCannotExtendThrowable => "generic-cannot-extend-throwable",
            JavaDiagnosticCode::CannotCatchTypeVariable => "cannot-catch-type-variable",
            JavaDiagnosticCode::IllegalGenericInstanceOf => "illegal-generic-instanceof",
            JavaDiagnosticCode::ContinueOutsideLoop => "continue-outside-of-loop",
            JavaDiagnosticCode::BreakOutsideSwitchOrLoop => "break-outside-of-switch-or-loop",
            JavaDiagnosticCode::UndefinedLabel => "undefined-label",
            JavaDiagnosticCode::NotALoopLabel => "not-a-loop-label",
            JavaDiagnosticCode::IncorrectNumberOfPatternComponents => {
                "incorrect-number-of-pattern-components"
            }
            JavaDiagnosticCode::CantInheritFromSealed => "cannot-inherit-from-sealed",
            JavaDiagnosticCode::SealedSealedOrFinalExpected => {
                "sealed-non-sealed-or-final-expected"
            }
            JavaDiagnosticCode::SealedClassMustHaveSubclasses => {
                "sealed-class-must-have-subclasses"
            }
            JavaDiagnosticCode::ModifierNotAllowedHere => "modifier-not-allowed-here",
            JavaDiagnosticCode::MissingMethodBodyOrDeclareAbstract => {
                "missing-method-body-or-declare-abstract"
            }
            JavaDiagnosticCode::UnexpectedChar => "unexpected-char",
            JavaDiagnosticCode::UnterminatedString => "unterminated-string",
            JavaDiagnosticCode::UnterminatedComment => "unterminated-comment",
            JavaDiagnosticCode::InvalidChar => "invalid-char",
            JavaDiagnosticCode::IllegalTextBlockOpen => "illegal-text-block-open",
            JavaDiagnosticCode::UnterminatedTextBlock => "unterminated-text-block",
            JavaDiagnosticCode::InvalidNumber => "invalid-number",
            JavaDiagnosticCode::InvalidUnicodeEscape => "invalid-unicode-escape",
            JavaDiagnosticCode::UnterminatedChar => "unterminated-char",
            JavaDiagnosticCode::InvalidEscapeSequence => "invalid-escape-sequence",
            JavaDiagnosticCode::UnterminatedTemplate => "unterminated-template",
            JavaDiagnosticCode::ExpectedToken => "expected-token",
            JavaDiagnosticCode::ExpectedKeyword => "expected-keyword",
            JavaDiagnosticCode::ExpectedConstruct => "expected-construct",
            JavaDiagnosticCode::NotAStatement => "not-a-statement",
        }
    }
}

impl std::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for JavaDiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl DiagnosticCode {
    /// The Java code for a lexical error kind, when the kind is structured
    /// enough to carry one (free-text `Message` kinds have none).
    pub(crate) fn from_java_syntax(kind: &crate::java::SyntaxErrorKind) -> Option<DiagnosticCode> {
        use crate::java::{LexicalErrorKind, ParseErrorKind};
        let code = match kind {
            crate::java::SyntaxErrorKind::Lexer(kind) => match kind {
                LexicalErrorKind::UnexpectedChar(_) => JavaDiagnosticCode::UnexpectedChar,
                LexicalErrorKind::UnterminatedString => JavaDiagnosticCode::UnterminatedString,
                LexicalErrorKind::UnterminatedComment => JavaDiagnosticCode::UnterminatedComment,
                LexicalErrorKind::InvalidChar => JavaDiagnosticCode::InvalidChar,
                LexicalErrorKind::IllegalTextBlockOpen => JavaDiagnosticCode::IllegalTextBlockOpen,
                LexicalErrorKind::UnterminatedTextBlock => {
                    JavaDiagnosticCode::UnterminatedTextBlock
                }
                LexicalErrorKind::InvalidNumber => JavaDiagnosticCode::InvalidNumber,
                LexicalErrorKind::InvalidUnicodeEscape => JavaDiagnosticCode::InvalidUnicodeEscape,
                LexicalErrorKind::UnterminatedChar => JavaDiagnosticCode::UnterminatedChar,
                LexicalErrorKind::InvalidEscapeSequence => {
                    JavaDiagnosticCode::InvalidEscapeSequence
                }
                LexicalErrorKind::UnterminatedTemplate => JavaDiagnosticCode::UnterminatedTemplate,
            },
            crate::java::SyntaxErrorKind::Parser(kind) => match kind {
                ParseErrorKind::ExpectedToken { .. } => JavaDiagnosticCode::ExpectedToken,
                ParseErrorKind::ExpectedContextualKeyword { .. } => {
                    JavaDiagnosticCode::ExpectedKeyword
                }
                ParseErrorKind::ExpectedConstruct(_) => JavaDiagnosticCode::ExpectedConstruct,
                ParseErrorKind::NotAStatement => JavaDiagnosticCode::NotAStatement,
                ParseErrorKind::Message(_) => return None,
            },
        };
        Some(DiagnosticCode::Java(code))
    }

    pub(crate) fn from_kotlin_syntax(
        _kind: &crate::kotlin::SyntaxErrorKind,
    ) -> Option<DiagnosticCode> {
        // TODO: add kotlin syntax code
        None
    }
}
