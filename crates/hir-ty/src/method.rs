//! Method resolution: member set, accessibility and applicability
//! ([JLS §15.12]).
//!
//! [`member_set`] computes the candidate methods for a name on a receiver
//! type ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
//! the methods of the receiver and, transitively, of all its superclasses and
//! superinterfaces, each instantiated with the receiver type's type arguments,
//! captured per [§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10)
//! so that wildcard arguments become fresh type variables. The [`InvocationContext`]
//! restricts the candidates to those allowed by the invocation mode
//! ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
//! [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
//! and accessible at the invocation site ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
//! [`pick_method`] then runs the overload resolution of
//! [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
//! the strict ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
//! loose ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3))
//! and variable-arity
//! ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4))
//! applicability phases, choosing the most specific applicable method
//! ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)).
//!
//! A generic method's invocation type ([§15.12.2.6]) is computed by the
//! method invocation type inference of [JLS §18.5.2] ([`crate::inference`]):
//! the method's own type parameters become fresh inference variables whose
//! constraints are solved against the actual argument types, so
//! `Collections.identity("s")` is applicable as `identity(String)`, not as the
//! erasure `identity(Object)`. The returned [`MethodData`] is the instantiated
//! invocation: its parameters and return type carry the inferred type
//! arguments. Inference-derived bounds and captured wildcard types
//! ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
//! are modelled; target-type compatibility of §18.5.2.4 is not.

use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::TypeParameter;

use hir_expand::{item_tree::ItemData, name::Name};

use crate::{
    db::{
        ItemKey, ScopeId, ScopeKind, TyDatabase, item_ty_query, method_params_query,
        type_params_map_query,
    },
    inference::{Constraint, Inference, InvocationPhase},
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file, ty_from_library},
    subtyping::{is_subtype, supertypes_query},
    ty::{Ty, TyData, TyKind, boxed_type, capture},
};

/// How the method name is qualified: the invocation mode of
/// [JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1):
/// a static invocation (`TypeName.m`, §15.12.1), a super invocation
/// (`super.m` or `TypeName.super.m`), or a virtual invocation (via an
/// expression). The mode restricts which members are candidates
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationMode {
    /// `TypeName.m(...)`: only static members are candidates.
    Static,
    /// `super.m(...)`: only instance members are candidates.
    Super,
    /// `InterfaceName.super.m(...)`: only instance members are candidates.
    Interface,
    /// `expression.m(...)`: all members except static methods declared in an
    /// interface are candidates.
    Virtual,
}

/// The context of a method invocation: how the name is qualified (the
/// invocation mode, JLS §15.12.1/§15.12.3) and the lexical context used for
/// access control ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
///
/// `None` context fields are treated permissively: a missing access
/// restriction does not filter candidates out. [`InvocationContext::unconstrained`]
/// performs no mode or access filtering.
#[derive(Debug, Clone)]
pub struct InvocationContext {
    /// The invocation mode.
    pub mode: InvocationMode,
    /// The fully qualified name of the class or interface in which the
    /// invocation appears, for `private` and `protected` access control
    /// ([§6.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.1),
    /// [§6.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6.2)).
    pub enclosing_class: Option<String>,
    /// The package of the compilation unit in which the invocation appears,
    /// for package and `protected` access control.
    pub package: Option<String>,
}

impl InvocationContext {
    /// A permissive default: a virtual invocation with no access filtering.
    pub fn unconstrained() -> Self {
        Self {
            mode: InvocationMode::Virtual,
            enclosing_class: None,
            package: None,
        }
    }
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
    fn from_flags(flags: u16) -> Access {
        if flags & 0x0002 != 0 {
            // ACC_PRIVATE
            Access::Private
        } else if flags & 0x0004 != 0 {
            // ACC_PROTECTED
            Access::Protected
        } else if flags & 0x0001 != 0 {
            // ACC_PUBLIC
            Access::Public
        } else {
            Access::Package
        }
    }
}

/// A type parameter of a generic method
/// ([JLS §8.4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.4))
/// with its declared bounds ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4)),
/// kept so [`pick_method`] can run the invocation type inference of
/// [JLS §18.5.2].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodTypeParam {
    pub name: Name,
    pub bounds: Vec<Ty>,
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
    /// The fully qualified name of the declaring class or interface.
    pub owner: String,
    /// The parameter types, instantiated with the declaring type's type
    /// arguments; the method's own type parameters are not yet instantiated.
    pub params: Vec<Ty>,
    /// The return type, in the same partially instantiated form.
    pub ret: Ty,
    /// Whether the method is a variable-arity method
    /// ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)).
    pub varargs: bool,
    /// Whether the method is static.
    pub is_static: bool,
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
        write!(f, "{}.{}(", self.method.owner, self.method.name)?;
        for (i, param) in self.method.params.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", param.display(self.db))?;
        }
        write!(f, ")")
    }
}

/// The member set of a name on a receiver type
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
/// the methods named `name` of the receiver and, transitively, of all its
/// superclasses and superinterfaces, in an unspecified order. The receiver is
/// first captured ([§5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.10))
/// so that wildcard type arguments become fresh type variables, and the
/// candidates are restricted to those allowed by `ctx`'s invocation mode
/// ([§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3))
/// and accessible at the invocation site
/// ([§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// For a type variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. Primitive and array receivers yield only the array
/// supertypes' methods.
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    ctx: &InvocationContext,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let receiver = capture(db, *receiver);
    let mut stack = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } => bounds.to_vec(),
        _ => vec![receiver],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        out.extend(
            class_methods(db, &scope_id, &ty, name)
                .into_iter()
                .filter(|method| mode_allows(method, ctx) && is_accessible(db, scope, method, ctx)),
        );
        for parent in supertypes_query(db, scope_id, ty.id) {
            stack.push(parent);
        }
    }
    out
}

/// The methods of a single class or interface, instantiated with `ty`'s type
/// arguments.
fn class_methods(db: &dyn TyDatabase, scope_id: &ScopeId, ty: &Ty, name: &str) -> Vec<MethodData> {
    let TyKind::Reference {
        name: class_name,
        args,
    } = ty.kind(db)
    else {
        return Vec::new();
    };
    let Some(resolved) = hir::fqn_resolve(db, &scope_id.kind(db).to_scope(), class_name.as_str())
    else {
        return Vec::new();
    };
    let args = args.clone();
    match resolved {
        hir::Resolved::Library(class) => library_class_methods(db, class, args, name),
        hir::Resolved::Source(source) => source_class_methods(db, source, args, name),
    }
}

/// The package of a fully qualified class name, or `None` for the unnamed
/// package.
fn package_of(fqn: &str) -> Option<String> {
    fqn.rfind('.').map(|i| fqn[..i].to_owned())
}

/// The fully qualified name of the top-level class of `fqn`: the name up to
/// the first `$` (nested classes are named `Outer$Inner`).
fn top_level_of(fqn: &str) -> String {
    match fqn.find('$') {
        Some(i) => fqn[..i].to_owned(),
        None => fqn.to_owned(),
    }
}

/// The methods of a library class, whose `Signature` attribute
/// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1))
/// may declare type parameters. The class's parameters are bound to `args`;
/// the method's own parameters (generic methods) keep their type parameters —
/// [`pick_method`] instantiates them by invocation type inference
/// ([JLS §18.5.2]).
fn library_class_methods(
    db: &dyn TyDatabase,
    class: hir::ResolvedClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let Some(record) = hir::class_record(db, &class) else {
        return Vec::new();
    };
    let hir::ClassOrModuleStub::Class(class) = record.as_ref() else {
        return Vec::new();
    };
    let interner = &db.hir_state().interner;
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        class
            .type_params
            .iter()
            .zip(args.iter().copied())
            .map(|(tp, arg)| (Name::new(interner.resolve(&tp.name)), arg))
            .collect()
    };
    let fqn = interner.resolve(&class.fqn).to_owned();
    let declaring_package = package_of(&fqn);
    let declaring_top_level = Some(top_level_of(&fqn));
    let declaring_interface = matches!(
        hir::ClassKind::from_flags(class.flags, class.is_record),
        hir::ClassKind::Interface | hir::ClassKind::Annotation
    );
    let instantiate =
        |tyref: &hir::TypeRef<hir::Symbol>| ty_from_library(db, tyref).substitute(db, &binding);

    let mut out = Vec::new();
    for method in &class.methods {
        if interner.resolve(&method.name) != name {
            continue;
        }
        let type_params = method
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                name: Name::new(interner.resolve(&tp.name)),
                bounds: tp.bounds.iter().map(|b| ty_from_library(db, b)).collect(),
            })
            .collect();
        out.push(MethodData {
            name: name.to_owned(),
            owner: fqn.clone(),
            params: method
                .params
                .iter()
                .map(|param| instantiate(&param.param_type))
                .collect(),
            ret: instantiate(&method.return_type),
            varargs: method.flags & 0x0080 != 0,   // ACC_VARARGS
            is_static: method.flags & 0x0008 != 0, // ACC_STATIC
            access: Access::from_flags(method.flags),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
        });
    }
    out
}

/// The methods of a source class, resolved against the file's own scope and
/// instantiated with `args`. Class type parameters are bound to `args`; method
/// type parameters are kept — [`pick_method`] instantiates them.
fn source_class_methods(
    db: &dyn TyDatabase,
    source: hir::SourceClass,
    args: Vec<Ty>,
    name: &str,
) -> Vec<MethodData> {
    let tree = hir::file_item_tree(db, source.file);
    let Some(class_data) = item_data(&tree, source.item) else {
        return Vec::new();
    };
    let declared: &[TypeParameter<Name>] = match class_data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.type_params,
        ItemData::Record(d) => &d.type_params,
        _ => &[],
    };
    let binding: FxHashMap<Name, Ty> = if args.is_empty() {
        FxHashMap::default()
    } else {
        declared
            .iter()
            .map(|tp| tp.name.clone())
            .zip(args.iter().copied())
            .collect()
    };
    let scope = scope_for_file(db, source.file);
    let type_params = type_params_map_query(db, db.file_text(source.file));
    let resolver = Resolver::new(&tree, type_params, source.item);
    let fqn = hir::source_class_fqn(db, source.file, source.item)
        .map(|fqn| fqn.as_str().to_owned())
        .unwrap_or_default();
    let declaring_package = resolver.package().map(|p| p.as_str().to_owned());
    let declaring_top_level = (!fqn.is_empty()).then(|| top_level_of(&fqn));
    let declaring_interface =
        matches!(class_data, ItemData::Interface(_) | ItemData::Annotation(_));

    let mut out = Vec::new();
    for &item in class_data.body() {
        let Some(ItemData::Method(method)) = item_data(&tree, item) else {
            continue;
        };
        if method.name.as_str() != name {
            continue;
        }
        let method_resolver = Resolver::new(&tree, type_params, item);
        let type_params = method
            .sig
            .type_params
            .iter()
            .map(|tp| MethodTypeParam {
                name: tp.name.clone(),
                bounds: tp
                    .bounds
                    .iter()
                    .map(|bound| resolve_type_ref(db, &scope, &method_resolver, bound))
                    .collect(),
            })
            .collect();
        let key = ItemKey::new(db, source.file, item);
        let instantiate = |ty: &Ty| ty.substitute(db, &binding);
        let varargs = method.sig.params.last().is_some_and(|param| param.varargs);
        let mut params: Vec<Ty> = method_params_query(db, key)
            .iter()
            .map(instantiate)
            .collect();
        // A variable-arity parameter `T...` is lowered as the element type
        // `T`; its formal type is the array `T[]` ([JLS §8.4.1]).
        if varargs && let Some(last) = params.last_mut() {
            *last = Ty::array(db, *last);
        }
        let ret = instantiate(&item_ty_query(db, key));
        out.push(MethodData {
            name: name.to_owned(),
            owner: fqn.clone(),
            params,
            ret,
            varargs,
            is_static: method.modifiers.static_,
            access: access_of(&method.modifiers),
            declaring_package: declaring_package.clone(),
            declaring_top_level: declaring_top_level.clone(),
            declaring_interface,
            type_params,
        });
    }
    out
}

fn access_of(modifiers: &hir_expand::modifiers::Modifiers) -> Access {
    if modifiers.private {
        Access::Private
    } else if modifiers.protected {
        Access::Protected
    } else if modifiers.public {
        Access::Public
    } else {
        Access::Package
    }
}

/// Whether `method` is allowed by the invocation mode of `ctx`
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§15.12.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.3)).
fn mode_allows(method: &MethodData, ctx: &InvocationContext) -> bool {
    match ctx.mode {
        // A static invocation selects only static members.
        InvocationMode::Static => method.is_static,
        // Super and interface invocations select only instance members.
        InvocationMode::Super | InvocationMode::Interface => !method.is_static,
        // A virtual invocation must not select a static method declared in an
        // interface (§15.12.3); a static method of a class may be selected.
        InvocationMode::Virtual => !(method.is_static && method.declaring_interface),
    }
}

/// Whether `method` is accessible to the class in which the invocation appears
/// ([JLS §6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// A missing context field (`None`) is permissive: that restriction is not
/// enforced.
fn is_accessible(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    ctx: &InvocationContext,
) -> bool {
    match method.access {
        Access::Public => true,
        // §6.6.1: a private member is accessible throughout the top-level
        // class in which it is declared.
        Access::Private => match (&ctx.enclosing_class, &method.declaring_top_level) {
            (Some(enclosing), Some(declaring)) => enclosing == declaring,
            _ => true,
        },
        // §6.6.1: a package member is accessible only within its own package.
        Access::Package => match (&ctx.package, &method.declaring_package) {
            (Some(invocation), Some(declaring)) => invocation == declaring,
            _ => true,
        },
        // §6.6.2: a protected member is accessible within the declaring
        // package, or from a class that is a subclass of the declaring class.
        Access::Protected => {
            if let (Some(invocation), Some(declaring)) = (&ctx.package, &method.declaring_package)
                && invocation == declaring
            {
                return true;
            }
            match &ctx.enclosing_class {
                Some(enclosing) => {
                    let enclosing = Ty::reference(db, enclosing.as_str(), Vec::new());
                    let declaring = Ty::reference(db, method.owner.as_str(), Vec::new());
                    is_subtype(db, scope, &enclosing, &declaring)
                }
                None => true,
            }
        }
    }
}

/// Instantiates `method` to its invocation type
/// ([JLS §15.12.2.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.6))
/// for a call with `args` in `phase`, running the method invocation type
/// inference of [JLS §18.5.2] against the actual argument types. `None` when
/// `method` is not applicable in this phase. `varargs` selects the variable-
/// arity invocation rules of [§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4).
fn instantiate(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[Ty],
    phase: InvocationPhase,
    varargs: bool,
) -> Option<MethodData> {
    let mut inference = Inference::new();

    // The method's own type parameters become fresh inference variables; their
    // declared bounds are the initial upper bounds (§18.5.2.2), with the type
    // parameter names substituted by the variables.
    let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
    for tp in &method.type_params {
        let var = inference.fresh_var(db);
        subst.insert(tp.name.clone(), var);
        let bounds: Vec<Ty> = tp.bounds.iter().map(|b| b.substitute(db, &subst)).collect();
        if bounds.is_empty() {
            inference.add_upper(db, var, Ty::reference(db, "java.lang.Object", Vec::new()));
        } else {
            for bound in bounds {
                inference.add_upper(db, var, bound);
            }
        }
    }
    let formals: Vec<Ty> = method
        .params
        .iter()
        .map(|p| p.substitute(db, &subst))
        .collect();

    // In the loose phase (and for variable-arity invocation, §15.12.2.4)
    // primitive arguments are boxed, so `⟨int → α⟩` yields the boxed lower
    // bound.
    let args: Vec<Ty> = match phase {
        InvocationPhase::Strict => args.to_vec(),
        InvocationPhase::Loose => args
            .iter()
            .map(|arg| match arg.kind(db) {
                TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                _ => *arg,
            })
            .collect(),
    };

    let mut constraints = Vec::new();
    if varargs {
        if !method.varargs || args.len() + 1 < formals.len() {
            return None;
        }
        let (fixed, last) = formals.split_at(formals.len() - 1);
        for (formal, arg) in fixed.iter().zip(&args) {
            constraints.push(Constraint::Sub(*arg, *formal));
        }
        let rest = &args[fixed.len()..];
        if !rest.is_empty() {
            if rest.len() == 1 && rest[0].is_array(db) {
                // A single trailing actual of the array type is used as-is.
                constraints.push(Constraint::Sub(rest[0], last[0]));
            } else {
                // Otherwise the trailing actuals are packed into the array: each
                // is related to the element type.
                let element = last[0].element(db)?;
                for arg in rest {
                    constraints.push(Constraint::Sub(*arg, *element));
                }
            }
        }
    } else {
        if formals.len() != args.len() {
            return None;
        }
        for (formal, arg) in formals.iter().zip(&args) {
            constraints.push(Constraint::Sub(*arg, *formal));
        }
    }

    let resolved = inference.solve(db, scope, phase, constraints)?;

    Some(MethodData {
        name: method.name.clone(),
        owner: method.owner.clone(),
        params: formals
            .iter()
            .map(|p| p.substitute_infer(db, &resolved))
            .collect(),
        ret: method
            .ret
            .substitute(db, &subst)
            .substitute_infer(db, &resolved),
        varargs: method.varargs,
        is_static: method.is_static,
        access: method.access,
        declaring_package: method.declaring_package.clone(),
        declaring_top_level: method.declaring_top_level.clone(),
        declaring_interface: method.declaring_interface,
        type_params: method.type_params.clone(),
    })
}

/// Whether `m1` is more specific than `m2`
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// a non-variable-arity method beats a variable-arity one, a non-generic
/// method beats a generic one, and otherwise every formal parameter of `m1` is
/// a subtype of the corresponding formal of `m2`. For two generic methods the
/// type-parameter-relative signatures are compared: `m2`'s type parameters are
/// substituted by `m1`'s (by position), and `m1`'s declared bounds must be at
/// least as restrictive as `m2`'s, so `<T extends String>` is more specific
/// than `<T>`.
fn more_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    m1: &MethodData,
    m2: &MethodData,
) -> bool {
    if m1.params.len() != m2.params.len() {
        return false;
    }
    if m1.varargs != m2.varargs {
        return !m1.varargs;
    }
    // §15.12.2.5: a non-generic method is more specific than a generic one.
    if m1.type_params.is_empty() != m2.type_params.is_empty() {
        return m1.type_params.is_empty();
    }
    if !m1.type_params.is_empty() {
        // Both generic: compare the type-parameter-relative signatures. m2's
        // type parameters are substituted by m1's (excess become `Object`),
        // and m1 must be at least as restrictive: its declared bounds are
        // subtypes of m2's, and its parameters are subtypes of m2's.
        let object = Ty::reference(db, "java.lang.Object", Vec::new());
        let mut subst: FxHashMap<Name, Ty> = FxHashMap::default();
        for (i, tp2) in m2.type_params.iter().enumerate() {
            let t1 = match m1.type_params.get(i) {
                Some(tp1) => Ty::type_var(db, tp1.name.clone(), tp1.bounds.clone()),
                None => object,
            };
            subst.insert(tp2.name.clone(), t1);
        }
        let m2_params: Vec<Ty> = m2.params.iter().map(|p| p.substitute(db, &subst)).collect();
        let bounds_ok = m1
            .type_params
            .iter()
            .zip(&m2.type_params)
            .all(|(tp1, tp2)| {
                let b1: Vec<Ty> = if tp1.bounds.is_empty() {
                    vec![object]
                } else {
                    tp1.bounds.clone()
                };
                let b2: Vec<Ty> = if tp2.bounds.is_empty() {
                    vec![object]
                } else {
                    tp2.bounds.clone()
                };
                b1.iter()
                    .all(|x| b2.iter().any(|y| is_subtype(db, scope, x, y)))
            });
        return bounds_ok
            && m1
                .params
                .iter()
                .zip(&m2_params)
                .all(|(param1, param2)| is_subtype(db, scope, param1, param2));
    }
    m1.params
        .iter()
        .zip(&m2.params)
        .all(|(param1, param2)| is_subtype(db, scope, param1, param2))
}

/// The most specific method among `candidates`, all applicable by the same
/// phase ([JLS §15.12.2.5]). Each candidate is the `(generic member, inferred
/// invocation)` pair produced by [`instantiate`]; specificity is decided on the
/// generic member. `None` when no candidate is strictly more specific than
/// every other (an ambiguity error), or when the set is empty.
fn choose_most_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: &[(MethodData, MethodData)],
) -> Option<MethodData> {
    if candidates.len() == 1 {
        return candidates.first().map(|(_, invocation)| invocation.clone());
    }
    let mut best: Option<usize> = None;
    for (i, (candidate, _)) in candidates.iter().enumerate() {
        let wins = candidates
            .iter()
            .all(|(other, _)| other == candidate || more_specific(db, scope, candidate, other));
        if wins {
            if best.is_some() {
                return None;
            }
            best = Some(i);
        }
    }
    best.map(|i| candidates[i].1.clone())
}

/// Resolves a method call `receiver.name(args)` by the applicability phases of
/// [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
/// strict invocation ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
/// then loose invocation ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3)),
/// then variable arity ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4));
/// the most specific applicable method
/// ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5))
/// wins. `None` when no method is applicable or the applicable ones are
/// ambiguous. The candidate set is restricted by the invocation mode and
/// access of `ctx` ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1),
/// [§6.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6)).
/// The returned [`MethodData`] is the inferred invocation type
/// ([JLS §18.5.2]).
pub fn pick_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    args: &[Ty],
    ctx: &InvocationContext,
) -> Option<MethodData> {
    let members = member_set(db, scope, receiver, name, ctx);

    // Phase 1: strict invocation (§15.12.2.2) — no boxing or unboxing, fixed
    // arity.
    let strict: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(db, scope, method, args, InvocationPhase::Strict, false)
                .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !strict.is_empty() {
        return choose_most_specific(db, scope, &strict);
    }

    // Phase 2: loose invocation (§15.12.2.3) — boxing and unboxing allowed.
    let loose: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(db, scope, method, args, InvocationPhase::Loose, false)
                .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !loose.is_empty() {
        return choose_most_specific(db, scope, &loose);
    }

    // Phase 3: variable arity (§15.12.2.4).
    let varargs: Vec<(MethodData, MethodData)> = members
        .iter()
        .filter_map(|method| {
            instantiate(db, scope, method, args, InvocationPhase::Loose, true)
                .map(|invocation| (method.clone(), invocation))
        })
        .collect();
    if !varargs.is_empty() {
        return choose_most_specific(db, scope, &varargs);
    }

    None
}
