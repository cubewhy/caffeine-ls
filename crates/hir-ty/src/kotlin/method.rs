//! The member set of a Kotlin receiver, and overload selection.
//!
//! KLS
//! `overload-resolution.html#overload-resolution`](https://kotlinlang.org/spec/overload-resolution.html#overload-resolution)
//! fixes the candidate set a call is resolved against and the order candidates
//! are chosen in:
//!
//! * the *receivers* a call can be written on: the class itself (declared and
//!   inherited members, plus its `companion object`'s when the receiver is the
//!   class), and — for a classifier receiver — the extension functions and
//!   properties in scope ([`#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers));
//! * applicability: arity with *default parameters* filled
//!   ([`declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)),
//!   a `vararg` parameter absorbing the rest
//!   ([`#variable-length-parameters`](https://kotlinlang.org/spec/declarations.html#variable-length-parameters)),
//!   named arguments matched by parameter name, then assignability of every
//!   argument to its parameter ([`#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call));
//! * selection: among the applicable candidates, the one every *other*
//!   applicable candidate's arguments can be passed to
//!   ([`#choosing-the-most-specific-candidate-from-the-overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set)).
//!
//! # Scope
//!
//! Inheritance is walked over the Kotlin item tree for a source class and over
//! the classfile stubs for a library one; a member's *parameter types* are
//! resolved from the declaring item ([`super::db::item_ty`] and
//! [`crate::java::resolve::method_params`] for the library half, where the
//! classfile gives the descriptors directly).
//!
//! Extension members declared in *other* files are not collected yet — the
//! file's own extensions are, through the same member walk — and ties in the
//! most-specific step fall back to declaration order rather than KLS's full
//! `MSC` algorithm. Both are recorded deviations; the call sites of this
//! milestone (the body inference's calls and the IDE's hover) resolve declared
//! members, and every candidate that survives applicability has the *same*
//! parameter types in the tests that pin them.

use hir::hir_def::kotlin::item_tree::{KotlinItemData, KotlinItemTree};
use hir_expand::name::Name;
use vfs::FileId;

use super::resolve::KotlinResolver;
use crate::java::db::TyDatabase;
use crate::java::method::{FieldData, InvocationContext, InvocationMode, MethodData};
use crate::kotlin::ty::ty_from_java;
use crate::ty::{Ty, TyKind};

/// The declaration a member resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberTarget {
    /// A Kotlin source declaration.
    Kotlin {
        file: FileId,
        item: hir_expand::ids::ItemId,
    },
    /// A Java source method or constructor, or a classfile method — the
    /// instantiated form [`crate::java::method::member_set`] returns.
    Java(Box<MethodData>),
    /// A Java source field or a classfile field, or a synthesized property.
    JavaField(Box<FieldData>),
}

/// A member reachable on a receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    /// The declaration the member resolves to.
    pub target: MemberTarget,
    pub name: Name,
    pub kind: MemberKind,
    /// The declared parameter types of a function (empty for a property).
    pub params: Vec<Ty>,
    /// Whether the last parameter is a `vararg`.
    pub vararg: bool,
    /// The number of parameters that declare a default value, counted from the
    /// end of the parameter list — the arity a call may omit.
    pub defaults: usize,
}

impl Member {
    /// The type of the member: a Kotlin declaration's from its item, a Java
    /// method's return type, a field's type — each converted to the Kotlin type
    /// it denotes ([`ty_from_java`]), because a Java or classfile type is a
    /// *platform* type in Kotlin.
    pub fn ty(&self, db: &dyn TyDatabase) -> Ty {
        match &self.target {
            // A Kotlin constructor's item type *is* the class it constructs.
            MemberTarget::Kotlin { file, item } => super::db::item_ty(db, *file, *item),
            // A JVM constructor returns `void`, so its own type is the class
            // it constructs — the owner, raw, since the constructor's
            // `MethodData` carries no type arguments. A call site knows the
            // parameterized type it constructed and uses that
            // ([`CallSite`]'s caller), this is the fallback.
            MemberTarget::Java(method) if self.kind == MemberKind::Constructor => {
                method.owner.as_ty(db, Vec::new())
            }
            MemberTarget::Java(method) => match self.kind {
                // A setter's own return is `void`; the type it *writes* is its
                // parameter's — already the Kotlin type of the property.
                MemberKind::Setter => self
                    .params
                    .first()
                    .copied()
                    .unwrap_or_else(|| ty_from_java(db, method.ret)),
                _ => ty_from_java(db, method.ret),
            },
            MemberTarget::JavaField(field) => ty_from_java(db, field.ty),
        }
    }

    /// The source file of the declaration, `None` for a classfile member.
    pub fn file(&self) -> Option<FileId> {
        match &self.target {
            MemberTarget::Kotlin { file, .. } => Some(*file),
            MemberTarget::Java(method) => method.owner_file,
            MemberTarget::JavaField(field) => field.owner_file,
        }
    }
}

/// Whether a member is a function, a property or a constructor — the kinds a
/// Kotlin call or read selects between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Function,
    Property,
    /// A property's accessor: what a read (the getter) or a write (the setter)
    /// resolves to, Kotlin's own and the synthetic property of a Java
    /// getter/setter pair alike ([KLS
    /// `declarations.html#getters-and-setters`](https://kotlinlang.org/spec/declarations.html#getters-and-setters),
    /// <https://kotlinlang.org/docs/java-interop.html#getters-and-setters>).
    Getter,
    Setter,
    /// A constructor: what `Foo(args)` resolves to, the Kotlin item tree's
    /// constructors for a Kotlin class and the JVM's for a Java or classfile
    /// one.
    Constructor,
}

/// The declaration a call site stands in: what a *Java* member's access control
/// is checked against ([`access_context_for_kotlin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSite {
    pub file: FileId,
    pub item: hir_expand::ids::ItemId,
}

/// One written argument of a call: its type, and the parameter name it was
/// written with (`None` for a positional argument).
pub struct CallArg<'a> {
    pub name: Option<&'a str>,
    pub ty: Ty,
}

/// Every member `name` names on the receiver `receiver`.
///
/// The receiver's classifiers are walked from the receiver to its supertypes,
/// so an inherited member is found at the declaration it comes from; a
/// classifier receiver also carries its `companion object`'s members, which is
/// how `Point.ORIGIN` resolves, and — when `name` is the class's own simple
/// name — its constructors ([KLS
/// `declarations.html#companion-objects`](https://kotlinlang.org/spec/declarations.html#companion-objects),
/// [`#constructors`](https://kotlinlang.org/spec/declarations.html#classifier-declaration)).
///
/// A *classfile* or *Java source* receiver's members come from the Java layer
/// ([`java_members`]), which is what makes `"ab".length`, `list.size`,
/// `StringBuilder.append(x)` and `ArrayList<String>()` resolvable from Kotlin.
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    site: CallSite,
) -> Vec<Member> {
    let ctx = access_context_for_kotlin(db, site.file, site.item);
    let mut out = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    let constructors = names_the_class(db, scope, receiver, name);
    collect_members(
        db,
        scope,
        receiver,
        name,
        &ctx,
        constructors,
        &mut seen,
        &mut out,
        true,
    );
    out
}

/// The members of the receiver and of every supertype of its supertype
/// closure, most-derived first, each classifier visited once.
fn collect_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    ctx: &InvocationContext,
    constructors: bool,
    seen: &mut rustc_hash::FxHashSet<String>,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    let Some(fqn) = reference_fqn(db, receiver) else {
        return;
    };
    if !seen.insert(fqn.as_str().to_owned()) {
        return;
    }
    let Some(resolved) = hir::fqn_resolve(db, scope, fqn.as_str()) else {
        return;
    };
    match &resolved {
        hir::Resolved::Source(class) => {
            let tree = hir::file_item_tree(db, class.file);
            match hir_def::kotlin::plugin::model(&tree) {
                Some(tree) => {
                    let resolver = KotlinResolver::for_item(db, class.file, tree, class.item);
                    kotlin_members(
                        db,
                        tree,
                        class.file,
                        class.item,
                        receiver,
                        name,
                        &resolver,
                        constructors,
                        out,
                        include_companion,
                    );
                }
                // A Java source class: its members are the Java layer's.
                None => java_members(db, scope, receiver, name, ctx, constructors, out),
            }
        }
        hir::Resolved::Library(_) => {
            java_members(db, scope, receiver, name, ctx, constructors, out)
        }
        // A Kotlin file's facade class: Kotlin reaches a file's top-level
        // declarations by *import*, not through the facade's name, so the arm
        // contributes nothing here — the file's own top level is what
        // [`super::infer`] consults, and a Java caller reaches them through the
        // facade, which the Java layer answers ([`crate::java::method`]).
        hir::Resolved::KotlinFacade { .. } => {}
    }
    // Inherited: the declared supertypes, then their own. The walk goes through
    // the Kotlin subtyping relation, which substitutes the receiver's arguments
    // into a source supertype list and converts a Java or classfile supertype
    // to the Kotlin type it denotes.
    for supertype in super::subtyping::supertypes(db, scope, receiver) {
        collect_members(db, scope, &supertype, name, ctx, false, seen, out, false);
    }
}

/// Whether `name` is the simple name of the class `receiver` denotes — the form
/// a Kotlin *constructor call* is written in (`Foo(1)`), where the candidate
/// set is the constructors rather than the members.
fn names_the_class(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
) -> bool {
    let Some(fqn) = reference_fqn(db, receiver) else {
        return false;
    };
    // The written name may be qualified (`a.Util`); the call writes the simple
    // name.
    hir::fqn_resolve(db, scope, fqn.as_str()).is_some() && fqn.simple_name() == name.as_str()
}

/// The members of one Kotlin declaration's body, plus its companion's when
/// asked, plus its constructors when the name is the class's own.
#[allow(clippy::too_many_arguments)]
fn kotlin_members(
    db: &dyn TyDatabase,
    tree: &KotlinItemTree,
    file: FileId,
    item: hir_expand::ids::ItemId,
    receiver: &Ty,
    name: &Name,
    resolver: &KotlinResolver<'_>,
    constructors: bool,
    out: &mut Vec<Member>,
    include_companion: bool,
) {
    let _ = receiver;
    for &member in tree.data(item).body() {
        let data = tree.data(member);
        let member_name = data.name();
        match data {
            KotlinItemData::Function(function) if member_name == Some(name) => {
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: MemberKind::Function,
                    params: function
                        .params
                        .iter()
                        .map(|param| super::ty::ty_from_type_ref(db, resolver, &param.param.ty.ty))
                        .collect(),
                    vararg: function
                        .params
                        .last()
                        .is_some_and(|param| param.param.varargs),
                    // A call may omit the trailing arguments whose parameter
                    // declares a default ([KLS
                    // `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
                    defaults: trailing_defaults(&function.defaults),
                });
            }
            // `Foo(1)`: the constructors of the class the receiver denotes.
            KotlinItemData::Constructor(constructor) if constructors => {
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: MemberKind::Constructor,
                    params: constructor
                        .params
                        .iter()
                        .map(|param| super::ty::ty_from_type_ref(db, resolver, &param.param.ty.ty))
                        .collect(),
                    vararg: constructor
                        .params
                        .last()
                        .is_some_and(|param| param.param.varargs),
                    defaults: trailing_defaults(&constructor.defaults),
                });
            }
            KotlinItemData::Property(property) if member_name == Some(name) => {
                let ty = match &property.ty {
                    Some(ty) => super::ty::ty_from_type_ref(db, resolver, &ty.ty),
                    None => Ty::error(db),
                };
                // A read resolves to the getter, a write to the setter
                // ([KLS `declarations.html#getters-and-setters`]); a `val` has
                // no setter, and a `private set` one is not a member of the
                // *class* for a receiver outside it.
                let declared_setter = property.accessors.iter().any(|&accessor| {
                    matches!(tree.data(accessor), KotlinItemData::Accessor(data) if data.is_setter)
                });
                out.push(Member {
                    target: MemberTarget::Kotlin { file, item: member },
                    name: name.clone(),
                    kind: if property.is_var && !declared_setter {
                        MemberKind::Setter
                    } else {
                        MemberKind::Getter
                    },
                    params: if property.is_var && !declared_setter {
                        vec![ty]
                    } else {
                        Vec::new()
                    },
                    vararg: false,
                    defaults: 0,
                });
            }
            KotlinItemData::Class(class)
                if include_companion
                    && class.kind
                        == hir::hir_def::kotlin::item_tree::KotlinClassKind::CompanionObject =>
            {
                kotlin_members(
                    db,
                    tree,
                    file,
                    member,
                    receiver,
                    name,
                    resolver,
                    constructors,
                    out,
                    false,
                );
            }
            _ => {}
        }
    }
}

/// The members `name` names on a Java or classfile receiver ([KLS
/// `overload-resolution.html#receivers`](https://kotlinlang.org/spec/overload-resolution.html#receivers)
/// for the Kotlin rules that consume them; the *Java* declaration shapes come
/// from [`crate::java::method::member_set`] and are projected through
/// [`MemberTarget`]).
///
/// Three Kotlin-specific rules sit on top of the Java member set:
///
/// * a Java method's written name is a Kotlin *function* of the same name, with
///   its parameter and return types converted to the Kotlin types they denote
///   (a classfile type is a platform type);
/// * the *synthetic property* of a Java getter/setter pair is a Kotlin property
///   (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>, which
///   KLS does not cover): the `getFoo()`/`setFoo(v)` pair is the property
///   `foo`, and a `Boolean` `isFoo()` is the property `isFoo` — so `list.size`,
///   `sb.length` and `x.name = "y"` resolve. A Kotlin declaration never gets
///   the treatment: it declares the property itself;
/// * a Java field is a Kotlin *property* of its own name — Kotlin reads a Java
///   field directly.
fn java_members(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    ctx: &InvocationContext,
    constructors: bool,
    out: &mut Vec<Member>,
) {
    // Kotlin draws no static/instance distinction at a *class* receiver
    // (`Integer.parseInt`, `Util.INSTANCE`), and an instance receiver's calls
    // are instance members — the mode a Kotlin access site has is therefore
    // "everything the type declares", which is the Java layer's
    // `TypeQualified` (the one mode that filters nothing).
    let ctx = &ctx.with_mode(InvocationMode::TypeQualified);
    // The *properties* come first: a name read position takes the property
    // (`container.layout` is `getLayout()`'s property even where the class also
    // declares a `void layout()` method), while a *call* filters to the
    // functions anyway, so both `x.layout` and `x.layout()` resolve.
    for accessor in property_getters(name) {
        for method in crate::java::method::member_set(db, scope, receiver, accessor.as_str(), ctx) {
            // A getter takes no arguments and returns the property's type.
            let mut member = member_of_method(db, name.clone(), MemberKind::Getter, method);
            member.params = Vec::new();
            out.push(member);
        }
    }
    for accessor in property_setters(name) {
        for method in crate::java::method::member_set(db, scope, receiver, accessor.as_str(), ctx) {
            // A setter's parameter is the property's type, which
            // [`member_of_method`] converts.
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Setter,
                method,
            ));
        }
    }
    // A Java field is the Kotlin property of its own name — Kotlin reads a Java
    // field directly — and comes before a method of the same name.
    if let Some(field) = crate::java::method::pick_field(db, scope, receiver, name.as_str(), ctx) {
        out.push(Member {
            target: MemberTarget::JavaField(Box::new(field)),
            name: name.clone(),
            kind: MemberKind::Property,
            params: Vec::new(),
            vararg: false,
            defaults: 0,
        });
    }
    for method in crate::java::method::member_set(db, scope, receiver, name.as_str(), ctx) {
        out.push(member_of_method(
            db,
            name.clone(),
            MemberKind::Function,
            method.clone(),
        ));
        // The method is *also* the property of its own name when it takes no
        // arguments: kotlinc 2.4.20 accepts `x.size` for a `Collection.size()`
        // and `sb.length` for a `StringBuilder.length()`. The link between a
        // standard-library property and its JVM accessor lives in the library's
        // `@Metadata`, which this model does not decode, so the method is
        // exposed under both names instead — permissive where kotlinc reports
        // `function invocation 'toString()' expected.` for a method the library
        // declares as a function, never a false `unresolved reference` for the
        // properties it does declare.
        if method.params.is_empty() && !method.is_static {
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Getter,
                method,
            ));
        }
    }
    if constructors {
        // The Java `new` path's own naming ([`crate::java::infer::new_expr`]):
        // a classfile constructor is `<init>`, a *source* one is a method named
        // after its class.
        let constructor_name = match hir::fqn_resolve(
            db,
            scope,
            reference_fqn(db, receiver)
                .as_ref()
                .map(|fqn| fqn.as_str())
                .unwrap_or_default(),
        ) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => name.simple_name().to_owned(),
        };
        for method in crate::java::method::member_set(db, scope, receiver, &constructor_name, ctx) {
            out.push(member_of_method(
                db,
                name.clone(),
                MemberKind::Constructor,
                method,
            ));
        }
    }
}

/// One Java method as a Kotlin member: its parameter types are converted to the
/// Kotlin types they denote — a classfile type is a *platform* type — and its
/// return type is converted by [`Member::ty`].
fn member_of_method(
    db: &dyn TyDatabase,
    name: Name,
    kind: MemberKind,
    method: MethodData,
) -> Member {
    Member {
        params: method
            .params
            .iter()
            .map(|param| ty_from_java(db, *param))
            .collect(),
        vararg: method.varargs,
        defaults: 0,
        target: MemberTarget::Java(Box::new(method)),
        name,
        kind,
    }
}

/// The Java getters a Kotlin property name stands for
/// (<https://kotlinlang.org/docs/java-interop.html#getters-and-setters>): the
/// `get`-prefixed form with the name capitalized, and — for a name that is
/// itself `is` + an uppercase letter, which is how Kotlin keeps a `Boolean
/// isFoo()`'s name — the name itself. kotlinc 2.4.20 reads
/// `j.isDragEnabled` for an `isDragEnabled()`/`setDragEnabled` pair and
/// `j.dragEnabled` for a `getDragEnabled()`/`setDragEnabled` one.
fn property_getters(name: &Name) -> Vec<String> {
    let mut out = vec![format!("get{}", capitalize(name.as_str()))];
    if is_prefix_name(name.as_str()) {
        out.push(name.to_string());
    }
    out
}

/// The Java setters a Kotlin property name stands for: `set` followed by the
/// name capitalized — and for the `is`-named property `isFoo`, `set` followed
/// by the name without its `is`, which is how Kotlin writes a `setFoo(v)`.
fn property_setters(name: &Name) -> Vec<String> {
    let base = match is_prefix_name(name.as_str()) {
        true => &name.as_str()[2..],
        false => name.as_str(),
    };
    vec![format!("set{}", capitalize(base))]
}

/// Whether a Kotlin property name is the `is`-prefixed form Kotlin keeps from a
/// `Boolean isFoo()`: `is` followed by an uppercase letter.
fn is_prefix_name(name: &str) -> bool {
    name.strip_prefix("is")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
}

/// `name` with its first character uppercased — the inverse of the JavaBeans
/// decapitalization the compiler applies to `getFoo`/`setFoo`.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The number of trailing parameters of a declaration that declare a default
/// value ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)):
/// the arity a call may omit.
fn trailing_defaults(defaults: &[Option<hir_expand::body::ExprId>]) -> usize {
    defaults
        .iter()
        .rev()
        .take_while(|default| default.is_some())
        .count()
}

/// The access-control context of a Kotlin call site, for a *Java* member
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)):
/// the enclosing class is the innermost Kotlin classifier the item is nested
/// in, the package the Kotlin file's, and `subclass_of` the enclosing
/// classifier's first source or library supertype — a Kotlin class extending a
/// Java class may reach its `protected` members exactly as a Java subclass can.
/// Kotlin's own visibility ([KLS
/// `declarations.html#visibility`](https://kotlinlang.org/spec/declarations.html#visibility))
/// is not a JLS §6.6 question and is checked separately.
pub fn access_context_for_kotlin(
    db: &dyn TyDatabase,
    file: FileId,
    item: hir_expand::ids::ItemId,
) -> InvocationContext {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        // A Java call site: the Java layer's own context.
        return crate::java::method::access_context(db, file, item);
    };
    let package = tree
        .package
        .as_ref()
        .map(|package| package.as_str().to_owned());
    // The innermost enclosing classifier, and its first declared supertype —
    // the class a `protected` member is accessed through from a subclass.
    let mut enclosing = None;
    let mut current = Some(item);
    while let Some(id) = current {
        if tree.as_class(id).is_some() {
            enclosing = Some(id);
            break;
        }
        current = tree.parent_of(id);
    }
    let (enclosing_class, subclass_of) = match enclosing {
        Some(class) => {
            let key =
                hir::source_class_fqn(db, file, class).map(crate::java::method::ClassKey::Named);
            let subclass = super::db::supertypes(db, file, class)
                .first()
                .and_then(|supertype| match supertype.kind(db) {
                    TyKind::Reference { name, .. } => {
                        Some(crate::java::method::ClassKey::Named(name.clone()))
                    }
                    _ => None,
                });
            (key, subclass)
        }
        None => (None, None),
    };
    InvocationContext {
        mode: InvocationMode::Virtual,
        enclosing_class,
        package,
        subclass_of,
    }
}

/// The function a call `name(args)` selects on `receiver`, or `None` when no
/// candidate applies.
pub fn pick_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &Name,
    args: &[CallArg<'_>],
    site: CallSite,
) -> Option<Member> {
    let candidates = member_set(db, scope, receiver, name, site);
    select(db, scope, candidates, args)
}

/// The callable a call `name(args)` selects among the file's *top-level*
/// declarations — the implicit receivers of an unqualified call that names
/// neither a local nor a member of an enclosing classifier ([KLS
/// `type-inference.html#call-without-an-explicit-receiver`](https://kotlinlang.org/spec/type-inference.html#call-without-an-explicit-receiver)).
pub fn top_level_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let tree = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&tree) else {
        return None;
    };
    let mut candidates = Vec::new();
    for &top in &tree.top {
        if let Some(member) = declaration_callable(db, scope, file, top, name, args) {
            candidates.push(member);
        }
    }
    select(db, scope, candidates, args)
}

/// The callable a *top-level* declaration is, when its name is `name` and the
/// arguments apply — [`top_level_callable`]'s twin for a declaration a caller
/// has already found, which is what an *imported* name resolves to.
pub fn declaration_callable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    file: FileId,
    item: hir_expand::ids::ItemId,
    name: &Name,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let tree = hir::file_item_tree(db, file);
    let tree = hir_def::kotlin::plugin::model(&tree)?;
    let KotlinItemData::Function(function) = tree.data(item) else {
        return None;
    };
    if function.name != *name {
        return None;
    }
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    let member = Member {
        target: MemberTarget::Kotlin { file, item },
        name: name.clone(),
        kind: MemberKind::Function,
        params: function
            .params
            .iter()
            .map(|param| super::ty::ty_from_type_ref(db, &resolver, &param.param.ty.ty))
            .collect(),
        vararg: function
            .params
            .last()
            .is_some_and(|param| param.param.varargs),
        defaults: trailing_defaults(&function.defaults),
    };
    applies(db, scope, &member, args).then_some(member)
}

/// The most specific of the applicable candidates ([KLS
/// `overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#choosing-the-most-specific-candidate-from-the-overload-candidate-set)),
/// or `None` when none applies.
fn select(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: Vec<Member>,
    args: &[CallArg<'_>],
) -> Option<Member> {
    let mut applicable: Vec<Member> = candidates
        .into_iter()
        .filter(|member| {
            matches!(member.kind, MemberKind::Function | MemberKind::Constructor)
                && applies(db, scope, member, args)
        })
        .collect();
    if applicable.is_empty() {
        return None;
    }
    // Most specific: the first candidate whose parameters every *other*
    // applicable candidate's arguments are assignable to — a plain
    // subtype-comparison, with declaration order breaking ties.
    let picked = applicable
        .iter()
        .position(|member| {
            applicable.iter().all(|other| {
                member.params.len() == other.params.len()
                    && member
                        .params
                        .iter()
                        .zip(&other.params)
                        .all(|(a, b)| crate::kotlin::subtyping::is_assignable(db, scope, b, a))
            })
        })
        .unwrap_or(0);
    Some(applicable.swap_remove(picked))
}
/// Whether a candidate accepts `args`
/// ([KLS `overload-resolution.html#determining-function-applicability-for-a-specific-call`](https://kotlinlang.org/spec/overload-resolution.html#determining-function-applicability-for-a-specific-call)):
/// arity with defaults and `vararg` filled, named arguments matched by
/// parameter name, then assignability.
fn applies(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    member: &Member,
    args: &[CallArg<'_>],
) -> bool {
    let required = member.params.len().saturating_sub(member.defaults);
    if member.vararg {
        if args.len() < required.saturating_sub(1) {
            return false;
        }
    } else if args.len() < required || args.len() > member.params.len() {
        return false;
    }
    let _ = (db, scope);
    // Positional arguments are checked against the parameters in order; a
    // `vararg` parameter takes the remaining ones as its element type, which
    // the classfile cannot tell apart from the array type it compiles to — so
    // an element of the array type is accepted there.
    args.iter()
        .enumerate()
        .all(|(index, arg)| match member.params.get(index) {
            Some(param) => crate::kotlin::subtyping::is_assignable(db, scope, &arg.ty, param),
            None => member.vararg,
        })
}

/// The canonical name of a reference type, for a classpath lookup.
fn reference_fqn(db: &dyn TyDatabase, ty: &Ty) -> Option<Name> {
    match ty.kind(db) {
        TyKind::Reference { name, .. } => Some(name.clone()),
        TyKind::Nullable(inner) | TyKind::DefinitelyNonNull(inner) => reference_fqn(db, inner),
        _ => None,
    }
}
