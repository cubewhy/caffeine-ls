//! Method resolution: member set and applicability ([JLS §15.12]).
//!
//! [`member_set`] computes the candidate methods for a name on a receiver
//! type ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)):
//! the methods of the receiver and, transitively, of all its superclasses and
//! superinterfaces, each instantiated with the receiver type's type arguments.
//! [`pick_method`] then runs the overload resolution of
//! [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
//! the strict ([§15.12.2.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.2)),
//! loose ([§15.12.2.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.3))
//! and variable-arity
//! ([§15.12.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.4))
//! applicability phases, choosing the most specific applicable method
//! ([§15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)).
//!
//! Generic method *invocation* type inference ([JLS §18]) is not modelled: a
//! generic method's own type parameters are instantiated by their erasure
//! ([§4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.6)),
//! keeping applicability exact in arity and sound on the bounds. The class
//! type parameters of a parameterized receiver are substituted with its actual
//! arguments, so `List<String>.add` sees `add(String)`, not `add(E)`.

use rustc_hash::{FxHashMap, FxHashSet};
use syntax::stub::TypeParameter;

use hir_expand::{item_tree::ItemData, name::Name};

use crate::{
    db::{
        ItemKey, ScopeId, ScopeKind, TyDatabase, item_ty_query, method_params_query,
        type_params_map_query,
    },
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file, ty_from_library},
    subtyping::{is_assignable, is_subtype, supertypes_query, widening_primitive},
    ty::{Ty, TyData, TyKind},
};

/// A candidate method from the member set
/// ([JLS §15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)),
/// instantiated for its declaring type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodData {
    /// The simple name of the method.
    pub name: String,
    /// The fully qualified name of the declaring class or interface.
    pub owner: String,
    /// The parameter types, instantiated with the declaring type's type
    /// arguments.
    pub params: Vec<Ty>,
    /// The return type.
    pub ret: Ty,
    /// Whether the method is a variable-arity method
    /// ([JLS §8.4.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-8.html#jls-8.4.1)).
    pub varargs: bool,
    /// Whether the method is static.
    pub is_static: bool,
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
/// superclasses and superinterfaces, in an unspecified order. For a type
/// variable receiver the declared bounds
/// ([§4.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.4))
/// are searched instead. Primitive and array receivers yield only the array
/// supertypes' methods.
pub fn member_set(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
) -> Vec<MethodData> {
    let scope_id = ScopeId::new(db, ScopeKind::from_scope(scope));
    let mut stack = match receiver.kind(db) {
        TyKind::TypeVar { bounds, .. } => bounds.to_vec(),
        _ => vec![*receiver],
    };
    let mut seen: FxHashSet<TyData> = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty.id) {
            continue;
        }
        out.extend(class_methods(db, &scope_id, &ty, name));
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

/// The methods of a library class, whose `Signature` attribute
/// ([JVMS §4.7.9.1](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.9.1))
/// may declare type parameters. The class's parameters are bound to `args`;
/// the method's own parameters (generic methods) are instantiated by their
/// erasure, since invocation type inference ([JLS §18]) is not modelled.
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

    let mut out = Vec::new();
    for method in &class.methods {
        if interner.resolve(&method.name) != name {
            continue;
        }
        let mut method_binding = FxHashMap::default();
        for tp in &method.type_params {
            let var = Ty::type_var(
                db,
                Name::new(interner.resolve(&tp.name)),
                tp.bounds.iter().map(|b| ty_from_library(db, b)).collect(),
            );
            method_binding.insert(Name::new(interner.resolve(&tp.name)), var.erasure(db));
        }
        let instantiate = |tyref: &hir::TypeRef<hir::Symbol>| {
            ty_from_library(db, tyref)
                .substitute(db, &method_binding)
                .substitute(db, &binding)
        };
        out.push(MethodData {
            name: name.to_owned(),
            owner: interner.resolve(&class.fqn).to_owned(),
            params: method
                .params
                .iter()
                .map(|param| instantiate(&param.param_type))
                .collect(),
            ret: instantiate(&method.return_type),
            varargs: method.flags & 0x0080 != 0,   // ACC_VARARGS
            is_static: method.flags & 0x0008 != 0, // ACC_STATIC
        });
    }
    out
}

/// The methods of a source class, resolved against the file's own scope and
/// instantiated with `args`. Class type parameters are bound to `args`; method
/// type parameters are instantiated by their erasure.
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

    let mut out = Vec::new();
    for &item in class_data.body() {
        let Some(ItemData::Method(method)) = item_data(&tree, item) else {
            continue;
        };
        if method.name.as_str() != name {
            continue;
        }
        let resolver = Resolver::new(&tree, type_params, item);
        let mut method_binding = FxHashMap::default();
        for tp in &method.sig.type_params {
            let var = Ty::type_var(
                db,
                tp.name.clone(),
                tp.bounds
                    .iter()
                    .map(|bound| resolve_type_ref(db, &scope, &resolver, bound))
                    .collect(),
            );
            method_binding.insert(tp.name.clone(), var.erasure(db));
        }
        let key = ItemKey::new(db, source.file, item);
        let instantiate = |ty: &Ty| ty.substitute(db, &method_binding).substitute(db, &binding);
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
            owner: hir::source_class_fqn(db, source.file, source.item)
                .map(|fqn| fqn.as_str().to_owned())
                .unwrap_or_default(),
            params,
            ret,
            varargs,
            is_static: method.modifiers.static_,
        });
    }
    out
}

/// Whether `arg` is convertible to `param` by a strict invocation conversion
/// ([JLS §5.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.3)):
/// identity, widening primitive
/// ([§5.1.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.2))
/// or widening reference
/// ([§5.1.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.5)).
/// Unlike assignment conversion there is no boxing or unboxing.
fn strict_conversion(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    arg: &Ty,
    param: &Ty,
) -> bool {
    if arg == param {
        return true;
    }
    match (arg.kind(db), param.kind(db)) {
        (TyKind::Primitive(src), TyKind::Primitive(dst)) => widening_primitive(*src, *dst),
        _ => is_subtype(db, scope, arg, param),
    }
}

/// Whether `method` is applicable to `args` by strict invocation
/// ([JLS §15.12.2.2]).
fn strict_applicable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[Ty],
) -> bool {
    method.params.len() == args.len()
        && method
            .params
            .iter()
            .zip(args)
            .all(|(param, arg)| strict_conversion(db, scope, arg, param))
}

/// Whether `method` is applicable to `args` by loose invocation
/// ([JLS §15.12.2.3]): strict plus boxing and unboxing, i.e. assignment
/// conversion ([§5.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2)).
fn loose_applicable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[Ty],
) -> bool {
    method.params.len() == args.len()
        && method
            .params
            .iter()
            .zip(args)
            .all(|(param, arg)| is_assignable(db, scope, arg, param))
}

/// Whether `method` is applicable to `args` by variable-arity invocation
/// ([JLS §15.12.2.4]): the last formal is the array `T[]`, the preceding
/// formals are checked by loosening invocation, and the trailing actuals are
/// packed into the array (or supplied as the array itself).
fn varargs_applicable(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    method: &MethodData,
    args: &[Ty],
) -> bool {
    if !method.varargs || args.len() + 1 < method.params.len() {
        return false;
    }
    let (fixed, last) = method.params.split_at(method.params.len() - 1);
    if !fixed
        .iter()
        .zip(&args[..fixed.len()])
        .all(|(param, arg)| is_assignable(db, scope, arg, param))
    {
        return false;
    }
    let Some(array) = last[0].element(db) else {
        return false;
    };
    let rest = &args[fixed.len()..];
    // An empty varargs argument list.
    if rest.is_empty() {
        return true;
    }
    // A single trailing actual of the array type itself is used as-is.
    if rest.len() == 1 && is_assignable(db, scope, &rest[0], &last[0]) {
        return true;
    }
    rest.iter().all(|arg| is_assignable(db, scope, arg, array))
}

/// Whether `m1` is more specific than `m2`
/// ([JLS §15.12.2.5](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2.5)):
/// a non-variable-arity method beats a variable-arity one, otherwise every
/// formal parameter of `m1` is a subtype of the corresponding formal of `m2`.
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
    m1.params
        .iter()
        .zip(&m2.params)
        .all(|(param1, param2)| is_subtype(db, scope, param1, param2))
}

/// The most specific method among `candidates`, all applicable by the same
/// phase ([JLS §15.12.2.5]). `None` when no candidate is strictly more
/// specific than every other (an ambiguity error), or when the set is empty.
fn choose_most_specific(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    candidates: &[MethodData],
) -> Option<MethodData> {
    if candidates.len() == 1 {
        return candidates.first().cloned();
    }
    let mut best: Option<&MethodData> = None;
    for candidate in candidates {
        let wins = candidates
            .iter()
            .all(|other| other == candidate || more_specific(db, scope, candidate, other));
        if wins {
            if best.is_some() {
                return None;
            }
            best = Some(candidate);
        }
    }
    best.cloned()
}

/// Resolves a method call `receiver.name(args)` by the applicability phases of
/// [JLS §15.12.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.2):
/// strict invocation ([§15.12.2.2]), then loose invocation ([§15.12.2.3]),
/// then variable arity ([§15.12.2.4]); the most specific applicable method
/// ([§15.12.2.5]) wins. `None` when no method is applicable or the applicable
/// ones are ambiguous. The static/instance context of the receiver
/// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
/// is not modelled, and generic method invocation inference ([JLS §18]) is
/// approximated by erasure of the method type parameters ([§4.6]).
pub fn pick_method(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    receiver: &Ty,
    name: &str,
    args: &[Ty],
) -> Option<MethodData> {
    let members = member_set(db, scope, receiver, name);

    // Phase 1: strict invocation (§15.12.2.2) — no boxing or unboxing.
    let strict: Vec<MethodData> = members
        .iter()
        .filter(|method| strict_applicable(db, scope, method, args))
        .cloned()
        .collect();
    if !strict.is_empty() {
        return choose_most_specific(db, scope, &strict);
    }

    // Phase 2: loose invocation (§15.12.2.3) — boxing and unboxing allowed.
    let loose: Vec<MethodData> = members
        .iter()
        .filter(|method| loose_applicable(db, scope, method, args))
        .cloned()
        .collect();
    if !loose.is_empty() {
        return choose_most_specific(db, scope, &loose);
    }

    // Phase 3: variable arity (§15.12.2.4).
    let varargs: Vec<MethodData> = members
        .iter()
        .filter(|method| varargs_applicable(db, scope, method, args))
        .cloned()
        .collect();
    if !varargs.is_empty() {
        return choose_most_specific(db, scope, &varargs);
    }

    None
}
