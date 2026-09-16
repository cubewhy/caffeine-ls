//! The JVM member vocabulary every language's type layer speaks (IntelliJ:
//! `JvmClass`, `JvmMethod`, `JvmField`).
//!
//! A class is identified by [`ClassKey`], a candidate method by [`MethodData`]
//! with its instantiated signature and its [`Access`], a field by
//! [`FieldData`] — the shapes the shared call-site member set
//! ([`crate::jvm::member_set`]) produces and every language's resolution
//! consumes. Java-specific selection machinery (the JLS §15.12 phases, the
//! invocation context, access control over *source* modifiers) stays with the
//! Java layer; this module carries no language concept.

use smol_str::SmolStr;
use vfs::FileId;

use hir_def::java::item_tree::ItemId;
use hir_def::jvm::access::{JvmAccessFlags, JvmVisibility};
use hir_expand::name::Name;

use crate::jvm::db::TyDatabase;
use crate::ty::{Ty, TypeVarScope};

/// The declaring class of a member, or the class a member access is made
/// from: a classpath or source class carrying a canonical fully qualified name
/// ([JLS §6.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.7)),
/// or a declaration with no canonical name — a *local* class-like declaration
/// ([JLS §14.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.3)),
/// or a member type of one — which is identified by its declaration instead.
///
/// The distinction is load-bearing wherever a class is compared with another:
/// two same-named local declarations in different methods are different
/// classes, and neither is the class of that simple name elsewhere in the
/// file, so every comparison keys on this value rather than on a name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClassKey {
    /// A class named by its canonical fully qualified name ([§6.7]): a source
    /// class of the workspace, or a classpath (binary) name.
    Named(Name),
    /// A declaration with no canonical name ([§6.7]).
    Local(hir::SourceClass),
}
/// The type parameters a *class* declares, with their bounds — the JVM view's
/// answer, distinct from [`MethodTypeParam`], which carries a *method*'s own
/// parameters that invocation type inference instantiates ([JLS §18.5.2]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParamData {
    pub scope: TypeVarScope,
    pub bounds: Vec<Ty>,
}

/// The JVM kind of a class ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1)
/// `ACC_INTERFACE`/`ACC_ANNOTATION`/`ACC_ENUM` and the `ClassFile` superclass
/// `java/lang/Enum`/`java/lang/Record`): what the class *is*, as opposed to how
/// the source spells it. A language with a declaration form that has no one
/// classfile shape reports the shape it compiles to
/// ([`JvmClassKind::Facade`] for the class a compiler synthesizes for a file's
/// top-level declarations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmClassKind {
    Class,
    Interface,
    Enum,
    Annotation,
    /// The synthetic class of a file's top-level declarations (`FooKt`).
    Facade,
}

/// The access of a member ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)),
/// derived from the classfile access flags (ACC_PUBLIC, ACC_PRIVATE,
/// ACC_PROTECTED, [JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
/// or the source modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    Protected,
    Package,
    Private,
}

impl Access {
    /// The access derived from the classfile access flags
    /// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
    /// via the canonical [`JvmAccessFlags`] model.
    pub(crate) fn from_flags(flags: u16) -> Access {
        match JvmVisibility::from_access_flags(JvmAccessFlags::from_bits_retain(flags)) {
            JvmVisibility::Private => Access::Private,
            JvmVisibility::Protected => Access::Protected,
            JvmVisibility::Public => Access::Public,
            JvmVisibility::Package => Access::Package,
        }
    }
}

/// A type parameter of a generic method
/// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4))
/// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// kept so [`pick_method`] can run the invocation type inference of
/// [JLS §18.5.2].
///
/// The parameter is identified by its [`TypeVarScope`] — the declaration that
/// introduces it ([§4.4], [§6.3]) — so the invocation's substitution
/// ([§18.5.2.2]) instantiates exactly the method's own variables and never a
/// same-named variable of the declaring class ([§6.4.1]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodTypeParam {
    pub scope: TypeVarScope,
    pub bounds: Vec<Ty>,
}

impl MethodTypeParam {
    /// The parameter's own name within its declaration, as javac renders it.
    pub fn name(&self) -> &Name {
        self.scope.name()
    }
}

/// A candidate method from the member set
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
/// instantiated for its declaring type: the class type parameters are
/// substituted with the receiver's actual type arguments, while the method's
/// own type parameters remain as type variables ([`TyKind::TypeVar`]) —
/// [`pick_method`] instantiates them by invocation type inference
/// ([JLS §18.5.2]). The [`MethodData`] returned by [`pick_method`] is the
/// fully instantiated invocation: parameters and return type carry the
/// inferred type arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodData {
    /// The simple name of the method.
    pub name: String,
    /// The declaring class or interface ([`ClassKey`]).
    pub owner: ClassKey,
    /// The workspace source file declaring this method, when it is a source
    /// declaration (including the synthesized implicit constructors, enum
    /// members and record accessors of a source class). `None` for library
    /// members and the synthetic `Object.clone` of array types.
    pub owner_file: Option<FileId>,
    /// The item id of the source declaration, when there is one — the anchor
    /// the declaration-level checks ([§8.4.2] duplicate methods) report at.
    pub decl_item: Option<ItemId>,
    /// The parameter types, instantiated with the declaring type's type
    /// arguments; the method's own type parameters are not yet instantiated.
    pub params: Vec<Ty>,
    /// The formal parameter *names* of a source method ([§8.4.1]), in order.
    /// `None` for library members (classfiles do not record them without a
    /// `MethodParameters` attribute) and synthesized implicit members; the
    /// record canonical-constructor parameter-name rule
    /// ([§8.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.10.4))
    /// is a source-only check.
    pub param_names: Option<Vec<String>>,
    /// The return type, in the same partially instantiated form.
    pub ret: Ty,
    /// The thrown exceptions ([JLS §8.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.6)),
    /// instantiated with the declaring type's type arguments; the method's own
    /// type parameters are not yet instantiated.
    pub throws: Vec<Ty>,
    /// Whether the method is a variable-arity method
    /// ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)).
    pub varargs: bool,
    /// Whether the method is static.
    pub is_static: bool,
    /// Whether the method is abstract
    /// ([JLS §8.4.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3)):
    /// the ACC_ABSTRACT flag of the classfile
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
    /// or the `abstract` modifier of the source. The single abstract method of
    /// a functional interface ([JLS §9.8]) is found from these.
    pub abstract_: bool,
    /// Whether the method is `final`
    /// ([JLS §8.4.3.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.3.3)):
    /// the ACC_FINAL flag of the classfile
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6))
    /// or the `final` modifier of the source. A final instance method cannot be
    /// overridden and a final static method cannot be hidden by a subclass; the
    /// declaration-level checks ([JLS §8.4.3.3]) report the violation.
    pub is_final: bool,
    /// The access of the method
    /// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    pub access: Access,
    /// The package of the declaring class, or `None` for the unnamed package.
    pub declaring_package: Option<String>,
    /// The fully qualified name of the top-level class of the declaring class
    /// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    pub declaring_top_level: Option<String>,
    /// Whether the declaring type is an interface (or annotation).
    pub declaring_interface: bool,
    /// The method's own type parameters
    /// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4)).
    pub type_params: Vec<MethodTypeParam>,
    /// Whether this member's signature was *erased* because it was reached
    /// through a raw receiver ([JLS §4.8]) while its declaration mentions the
    /// declaring class's type parameters — the condition under which an
    /// invocation of it is an unchecked call
    /// ([§5.1.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.9),
    /// javac's `unchecked call to … as a member of the raw type …`).
    ///
    /// A member whose declared signature mentions no type parameter (`void
    /// m(String)`) keeps a fully-checked invocation even on a raw receiver,
    /// which is why the flag records the *declaration*, not merely the raw
    /// receiver.
    pub raw_erased: bool,
    /// The classfile descriptor of a library member
    /// ([JVMS §4.6](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.6) for a
    /// method, [§4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5) for a
    /// field). `None` for a source declaration and for a member this crate
    /// synthesizes (`Object.clone` on an array type, the implicit members of a
    /// source class).
    pub descriptor: Option<SmolStr>,
}

impl MethodData {
    /// Formats this method as a callable signature, e.g.
    /// `java.util.List.add(java.lang.String)`.
    pub fn display<'a>(&'a self, db: &'a dyn TyDatabase) -> MethodDisplay<'a> {
        MethodDisplay { method: self, db }
    }
}

/// A displayable view of a [`MethodData`], produced by [`MethodData::display`].
pub struct MethodDisplay<'a> {
    method: &'a MethodData,
    db: &'a dyn TyDatabase,
}

impl std::fmt::Display for MethodDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}(",
            self.method.owner.display_name(self.db),
            self.method.name
        )?;
        for (i, param) in self.method.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", param.display(self.db))?;
        }
        write!(f, ")")?;
        if !self.method.throws.is_empty() {
            write!(f, " throws ")?;
            for (i, thrown) in self.method.throws.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", thrown.display(self.db))?;
            }
        }
        Ok(())
    }
}

/// A field resolved through the member set of a field access
/// ([JLS §15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)),
/// instantiated with the receiver type's type arguments (type variables are not
/// yet instantiated — fields carry no type parameters of their own, so the
/// field type is the declaration type with the receiver's type arguments
/// substituted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldData {
    /// The simple name of the field.
    pub name: String,
    /// The declaring class or interface ([`ClassKey`]).
    pub owner: ClassKey,
    /// The workspace source file declaring this field, when it is a source
    /// declaration (including the implicit enum-constant and record-component
    /// fields of a source class). `None` for library members.
    pub owner_file: Option<FileId>,
    /// The item id of the source declaration, when there is one — the anchor
    /// the declaration-level checks report at (the deprecated-use warning
    /// reads the declaration's own `@Deprecated` from it).
    pub decl_item: Option<ItemId>,
    /// The field's type, instantiated with the declaring type's type arguments.
    pub ty: Ty,
    /// The classfile descriptor of a library field
    /// ([JVMS §4.5](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.5)). `None`
    /// for a source declaration and for a field this crate synthesizes.
    pub descriptor: Option<SmolStr>,
    /// Whether the field is static.
    pub is_static: bool,
    /// The access of the field
    /// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
    pub access: Access,
    /// Whether the field is `final`
    /// ([JLS §8.3.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.3.1.2)):
    /// the ACC_FINAL flag of the classfile
    /// ([JVMS §4.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.1))
    /// or the `final` modifier of the source. A final field cannot be assigned
    /// after initialization ([§16]).
    pub is_final: bool,
    /// The package of the declaring class, or `None` for the unnamed package.
    pub declaring_package: Option<String>,
    /// The fully qualified name of the top-level class of the declaring class
    /// ([JLS §6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1)).
    pub declaring_top_level: Option<String>,
}
