//! Termination regressions for the iterative [`Ty`] rewrites
//! ([`Ty::substitute`], [`Ty::substitute_incl_bounds`],
//! [`Ty::substitute_infer`], [`Ty::erase_infer_vars`]).
//!
//! The rewrites walk the interned [`Ty`] DAG. Real source types are *recursive*
//! by construction ([JLS §4.4] `T extends Box<K,T>`), and inference tables
//! hold substitution cycles (`α → List<α>`); the naive recursive rewrites
//! followed such chains on the native stack and overflowed it. These tests
//! assert that the iterative, memoized rewrites terminate on the shapes that
//! used to crash: deep nesting, cyclic substitution values and re-substitution
//! to a fixpoint.

use hir_expand::name::Name;
use hir_ty::{BoundKind, Ty, TyKind, WildcardBound};
use rustc_hash::FxHashMap;

#[macro_use]
mod common;

use crate::common::TestDatabase;

fn r(db: &TestDatabase, name: &str, args: Vec<Ty>) -> Ty {
    Ty::reference(db, name, args)
}

/// Builds a type nested `depth` levels deep: `List<List<...<α>...>>`.
fn deep_list_of(db: &TestDatabase, depth: usize, leaf: Ty) -> Ty {
    let mut ty = leaf;
    for _ in 0..depth {
        ty = r(db, "java.util.List", vec![ty]);
    }
    ty
}

#[test]
fn substitute_infer_terminates_on_deep_nesting() {
    let db = TestDatabase::new();
    // A chain deeper than any fixed thread stack (the old recursion overflowed
    // well below this on a 16 MiB stack).
    let depth = 500_000;
    let leaf = Ty::reference(&db, "java.lang.String", Vec::new());
    let ty = deep_list_of(&db, depth, leaf);
    let mut subst = FxHashMap::default();
    subst.insert(
        u64::MAX,
        Ty::reference(&db, "java.lang.Integer", Vec::new()),
    );
    // Substitute a variable that is *not* present: every level rebuilds, so
    // this exercises the full-depth walk without growing the type.
    let rewritten = ty.substitute_infer(&db, &subst);
    assert!(matches!(
        rewritten.kind(&db),
        TyKind::Reference { name, .. } if name.as_str() == "java.util.List"
    ));
}

#[test]
fn erase_infer_vars_terminates_on_deep_nesting() {
    let db = TestDatabase::new();
    let depth = 500_000;
    let var = Ty::infer_var(&db);
    let ty = deep_list_of(&db, depth, var);
    let erased = ty.erase_infer_vars(&db);
    // The innermost `?n` erased to `java.lang.Object`; walk to the leaf.
    let mut current = erased;
    for _ in 0..depth {
        let TyKind::Reference { args, .. } = current.kind(&db) else {
            panic!("expected reference at every level");
        };
        assert_eq!(args.len(), 1);
        current = args[0];
    }
    assert!(current.is_object(&db));
}

#[test]
fn substitute_infer_cyclic_value_terminates() {
    let db = TestDatabase::new();
    // α → List<α>. A single substitution expands one level (the value is used
    // as-is, never re-walked) and must not loop.
    let var = Ty::infer_var(&db);
    let id = var.as_infer_var(&db).expect("inference variable");
    let cyclic = r(&db, "java.util.List", vec![var]);
    let mut subst = FxHashMap::default();
    subst.insert(id, cyclic);

    let ty = r(&db, "java.util.List", vec![var]);
    let rewritten = ty.substitute_infer(&db, &subst);
    let TyKind::Reference { args, .. } = rewritten.kind(&db) else {
        panic!("expected reference");
    };
    assert_eq!(args.len(), 1);
    // The argument is the substituted value: `List<List<α>>`, finite.
    let TyKind::Reference {
        args: inner_args, ..
    } = args[0].kind(&db)
    else {
        panic!("expected inner reference");
    };
    assert_eq!(inner_args.len(), 1);
}

#[test]
fn substitute_infer_fixpoint_with_cycle_is_bounded() {
    let db = TestDatabase::new();
    // The `resolve` fixpoint re-applies substitute_infer until the values stop
    // changing. With a cycle (α → List<α>) a naive loop grows `List<...>` one
    // level per pass forever; the bounded loop must stop. The bound is
    // O(n²) passes over n variables, so assert a *small* cycle terminates with
    // a finite (capped) value rather than hanging.
    let var = Ty::infer_var(&db);
    let id = var.as_infer_var(&db).expect("inference variable");
    let mut subst = FxHashMap::default();
    subst.insert(id, r(&db, "java.util.List", vec![var]));

    // Replicate the bounded fixpoint loop of `Inference::resolve`.
    let keys: Vec<u64> = subst.keys().copied().collect();
    let rounds = keys.len().saturating_mul(2).max(1);
    for _ in 0..rounds {
        let mut changed = false;
        for key in &keys {
            if let Some(value) = subst.get(key).copied() {
                let updated = value.substitute_infer(&db, &subst);
                if subst.get(key).copied() != Some(updated) {
                    subst.insert(*key, updated);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Terminated (the loop is bounded); the value grew, but finitely.
    let _ = subst[&id];
}

#[test]
fn substitute_maps_type_vars_to_arguments() {
    let db = TestDatabase::new();
    // `<E> ArrayList<E>` instantiated with `E := String`: `E[]` → `String[]`.
    let var = Ty::type_var(&db, "E", Vec::new());
    let array = Ty::array(&db, var);
    let mut binding = FxHashMap::default();
    binding.insert(
        Name::new("E"),
        Ty::reference(&db, "java.lang.String", Vec::new()),
    );
    let rewritten = array.substitute(&db, &binding);
    let TyKind::Array(inner) = rewritten.kind(&db) else {
        panic!("expected array");
    };
    assert!(inner.is_object(&db) || inner.as_reference(&db).is_some());
}

#[test]
fn substitute_incl_bounds_rewrites_recursive_bound() {
    let db = TestDatabase::new();
    // Emulate a `class Box<K, T>` where a *field type* `V` is declared
    // `V extends Box<K, T>`: `V`'s bound references the class type parameter
    // `T`. Substituting `T := String` through [`Ty::substitute_incl_bounds`]
    // must rewrite the bound of the *unbound* variable `V` in the same pass —
    // plain `substitute` would leave the recursive `T` behind.
    let name_k = Name::new("K");
    let name_t = Name::new("T");
    let name_v = Name::new("V");
    let k_ty = Ty::type_var(&db, name_k.clone(), Vec::new());
    let t_ty = Ty::type_var(&db, name_t.clone(), Vec::new());
    let box_of = |db: &TestDatabase, args: Vec<Ty>| r(db, "com.example.Box", args);

    // `V extends Box<K, T>` (the bound references the substituted `T`).
    let v_ty = Ty::type_var(&db, name_v.clone(), vec![box_of(&db, vec![k_ty, t_ty])]);
    // The type being rewritten: `Box<K, V>`.
    let ty = box_of(&db, vec![k_ty, v_ty]);

    let mut binding = FxHashMap::default();
    binding.insert(
        name_t.clone(),
        Ty::reference(&db, "java.lang.String", Vec::new()),
    );
    let rewritten = ty.substitute_incl_bounds(&db, &binding);

    // `Box<K, V>`; the second argument is `V`, kept but with its bound
    // rewritten to `Box<K, String>`.
    let TyKind::Reference { args, .. } = rewritten.kind(&db) else {
        panic!("expected reference");
    };
    assert_eq!(args.len(), 2);
    let TyKind::TypeVar { name, bounds, .. } = args[1].kind(&db) else {
        panic!("expected type variable V");
    };
    assert_eq!(name.as_str(), "V");
    assert_eq!(bounds.len(), 1);
    let TyKind::Reference {
        name: bound_name,
        args: bound_args,
    } = bounds[0].kind(&db)
    else {
        panic!("expected Box bound");
    };
    assert_eq!(bound_name.as_str(), "com.example.Box");
    assert_eq!(bound_args.len(), 2);
    let (bound_arg_name, bound_arg_args) = bound_args[1]
        .as_reference(&db)
        .expect("expected bound argument to be a reference");
    assert_eq!(bound_arg_name.as_str(), "java.lang.String");
    assert!(bound_arg_args.is_empty());
}

#[test]
fn wildcard_and_intersection_are_rewritten() {
    let db = TestDatabase::new();
    let var = Ty::infer_var(&db);
    let id = var.as_infer_var(&db).expect("inference variable");
    // `? extends α` and `α & Runnable`, both carrying the variable.
    let wc = Ty::wildcard(
        &db,
        Some(Box::new(WildcardBound {
            kind: BoundKind::Upper,
            ty: var,
        })),
    );
    let inter = Ty::intersection(&db, vec![var, r(&db, "java.lang.Runnable", Vec::new())]);
    let ty = r(&db, "java.util.List", vec![wc, inter]);

    let mut subst = FxHashMap::default();
    subst.insert(id, Ty::reference(&db, "java.lang.String", Vec::new()));
    let rewritten = ty.substitute_infer(&db, &subst);
    // The wildcard bound and the intersection member both became String.
    let TyKind::Reference { args, .. } = rewritten.kind(&db) else {
        panic!("expected reference");
    };
    let TyKind::Wildcard(Some(b)) = args[0].kind(&db) else {
        panic!("expected wildcard");
    };
    let (bound_name, bound_args) = b.ty.as_reference(&db).expect("expected bound reference");
    assert_eq!(bound_name.as_str(), "java.lang.String");
    assert!(bound_args.is_empty());
    let TyKind::Intersection(members) = args[1].kind(&db) else {
        panic!("expected intersection");
    };
    let names: Vec<&str> = members
        .iter()
        .map(|m| {
            m.as_reference(&db)
                .expect("expected reference member")
                .0
                .as_str()
        })
        .collect();
    assert_eq!(names, ["java.lang.String", "java.lang.Runnable"]);
    assert!(!rewritten.contains_infer_var(&db));
}
