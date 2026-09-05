//! The least upper bound ([JLS §4.10.4]).

use rustc_hash::FxHashSet;

use crate::{
    java::db::TyDatabase,
    java::subtyping::{is_subtype, supertypes_impl},
    java::ty::{BoundKind, Ty, TyData, TyKind, WildcardBound, boxed_type},
};

/// The least upper bound of a set of types
/// ([JLS §4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)):
/// primitive types are boxed (§5.1.7), the sets of *erased* supertypes are
/// intersected into the erased candidate set `EC`, the minimal candidates
/// `MEC` are kept, the type arguments of generic candidates are recovered with
/// the least containing parameterization `lcp` / least containing type
/// argument `lcta`, and the best candidates join with `&` into an
/// intersection type ([§4.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.9)).
///
/// The specification permits `lub` to *not terminate* — "it is possible that
/// the lub() function yields an infinite type". A recursive call on an
/// identical argument set is such a cycle; it is broken by degrading to
/// `java.lang.Object`, keeping the result finite and well-formed (of the many
/// admissible choices, `Object` is the one the erased candidate set yields
/// for an unknown candidate).
pub fn least_upper_bound(db: &dyn TyDatabase, scope: &hir::ResolutionScope, types: &[Ty]) -> Ty {
    let object = Ty::reference(db, "java.lang.Object", Vec::new());
    if types.is_empty() {
        return object;
    }
    if types.len() == 1 {
        return types[0];
    }
    // JLS §4.10.4: the lub of identical types is that type itself — in
    // particular `lub(Z, Z)` for a type variable `Z` is `Z`, not its erased
    // supertype. Without the dedup a method type variable bounded below twice
    // by the same outer variable (`readEnum((Z[]) ..., (Z) ...)` inferring
    // `W` with lower `[Z, Z]`) degrades to the bound (`Enum<?>`) and fails its
    // own upper-bound check (`Enum<?> <: Z`).
    if types.iter().all(|t| *t == types[0]) {
        return types[0];
    }
    // JLS §4.10.4/§15.27.3: the null type is a subtype of every reference
    // type, so `lub(T, null)` is `T` — a block lambda returning `Method` on
    // one path and `null` on another (`return null` in a `catch`) has result
    // `Method`, not `Object`. Filter nulls (all-null degrades to null).
    if types.iter().any(|t| t.is_null(db)) {
        let non_null: Vec<Ty> = types.iter().copied().filter(|t| !t.is_null(db)).collect();
        if non_null.is_empty() {
            return types[0];
        }
        if non_null.len() == 1 {
            return non_null[0];
        }
        if non_null.iter().all(|t| *t == non_null[0]) {
            return non_null[0];
        }
        return least_upper_bound(db, scope, &non_null);
    }
    // §4.10.4 step 1: box primitive bounds.
    let types: Vec<Ty> = if types
        .iter()
        .any(|t| matches!(t.kind(db), TyKind::Primitive(_)))
    {
        types
            .iter()
            .map(|t| match t.kind(db) {
                TyKind::Primitive(p) => Ty::reference(db, boxed_type(*p), Vec::new()),
                _ => *t,
            })
            .collect()
    } else {
        types.to_vec()
    };
    let mut ctx = Lub {
        db,
        scope,
        guard: Vec::new(),
    };
    ctx.set(&types)
}

/// The state of one [`least_upper_bound`] computation: the scope and the
/// cycle guard of argument-type-set keys.
struct Lub<'a> {
    db: &'a dyn TyDatabase,
    scope: &'a hir::ResolutionScope,
    guard: Vec<FxHashSet<TyData>>,
}

impl Lub<'_> {
    fn object(&self) -> Ty {
        Ty::reference(self.db, "java.lang.Object", Vec::new())
    }

    /// `lub(U1, ..., Uk) = lct(U1, ..., Uk)` fold of [`Self::set_impl`]. The
    /// guard breaks the specification's admitted non-termination: a repeating
    /// argument set, or recursion past a depth cap, degrades to `Object`.
    fn set(&mut self, types: &[Ty]) -> Ty {
        let key: FxHashSet<TyData> = types.iter().map(|t| t.id).collect();
        if self.guard.contains(&key) || self.guard.len() > 64 {
            return self.object();
        }
        self.guard.push(key);
        let result = self.set_impl(types);
        self.guard.pop();
        result
    }

    /// The §4.10.4 computation on the (cycle-guarded) argument set.
    fn set_impl(&mut self, types: &[Ty]) -> Ty {
        let st: Vec<FxHashSet<TyData>> = types.iter().map(|t| self.st(*t)).collect();
        // EST(Ui) = { |W| : W in ST(Ui) }, and EC is their intersection.
        let est: Vec<FxHashSet<TyData>> = st
            .iter()
            .map(|set| {
                set.iter()
                    .map(|id| Ty { id: *id }.erasure(self.db).id)
                    .collect()
            })
            .collect();
        let mut ec: FxHashSet<TyData> = est[0].clone();
        for set in &est[1..] {
            ec.retain(|id| set.contains(id));
        }
        if ec.is_empty() {
            return self.object();
        }
        // MEC: drop candidates that are supertypes of another candidate.
        let ids: Vec<TyData> = ec.iter().copied().collect();
        let mut mec: Vec<Ty> = Vec::new();
        for id in ids {
            let candidate = Ty { id };
            let minimal = ec.iter().all(|other| {
                *other == id || !is_subtype(self.db, self.scope, &Ty { id: *other }, &candidate)
            });
            if minimal {
                mec.push(candidate);
            }
        }
        let mut all: FxHashSet<TyData> = FxHashSet::default();
        for set in &st {
            all.extend(set.iter().copied());
        }
        // Best(X) = Candidate(X) if X is generic, X otherwise.
        let best: Vec<Ty> = mec
            .iter()
            .map(|candidate| {
                if !self.is_generic(*candidate) {
                    return *candidate;
                }
                let TyKind::Reference { name, .. } = candidate.kind(self.db) else {
                    return *candidate;
                };
                let name = name.clone();
                let relevant: Vec<Ty> = all
                    .iter()
                    .map(|id| Ty { id: *id })
                    .filter(|w| {
                        matches!(w.kind(self.db), TyKind::Reference { name: wname, args } if wname == &name && !args.is_empty())
                    })
                    .collect();
                if relevant.is_empty() {
                    *candidate
                } else {
                    self.candidate(&relevant)
                }
            })
            .collect();
        Ty::intersection(self.db, best)
    }

    /// ST(Ui): the transitive supertype set of `ty` ([§4.10.2]), including
    /// itself. A type variable ranges over its declared bounds ([§4.10.2]).
    fn st(&self, ty: Ty) -> FxHashSet<TyData> {
        let mut out = FxHashSet::default();
        self.st_impl(ty, &mut out);
        out
    }

    fn st_impl(&self, ty: Ty, out: &mut FxHashSet<TyData>) {
        if !out.insert(ty.id) {
            return;
        }
        if let TyKind::TypeVar { bounds, .. } = ty.kind(self.db) {
            for bound in bounds {
                self.st_impl(*bound, out);
                for parent in supertypes_impl(self.db, self.scope, bound) {
                    self.st_impl(parent, out);
                }
            }
        } else {
            for parent in supertypes_impl(self.db, self.scope, &ty) {
                self.st_impl(parent, out);
            }
        }
    }

    /// Whether the erased candidate `g` names a class or interface that
    /// declares type parameters ([§4.5]).
    fn is_generic(&self, g: Ty) -> bool {
        let TyKind::Reference { name, .. } = g.kind(self.db) else {
            return false;
        };
        let Some(resolved) = hir::fqn_resolve(self.db, self.scope, name.as_str()) else {
            return false;
        };
        match resolved {
            hir::Resolved::Library(_) => hir::class_generic_info(self.db, &resolved)
                .is_some_and(|info| !info.type_params.is_empty()),
            hir::Resolved::Source(source) => {
                let tree = hir::file_item_tree(self.db, source.file);
                match tree.data(source.item) {
                    hir_def::java::item_tree::ItemData::Class(data)
                    | hir_def::java::item_tree::ItemData::Interface(data) => {
                        !data.type_params.is_empty()
                    }
                    hir_def::java::item_tree::ItemData::Record(data) => {
                        !data.type_params.is_empty()
                    }
                    _ => false,
                }
            }
        }
    }

    /// The candidate parameterization of a generic candidate from its
    /// relevant (possibly several) parameterizations (§4.10.4): `lcp`.
    fn candidate(&mut self, relevant: &[Ty]) -> Ty {
        if relevant.len() == 1 {
            // §4.10.4: when every input shares the *same* parameterization of
            // the candidate — `lub(Modern, Legacy)` where both ST sets contain
            // `FreecamController<Freecam>` — the least containing
            // parameterization is that parameterization itself. Mapping the
            // single (deduplicated) argument through `lcta_unary` would widen
            // the concrete `Freecam` to `? extends Object` and lose the exact
            // type the assignment `FreecamController<Freecam> y = b ? new
            // Modern() : new Legacy()` requires (§4.10.4: `lcp(G<X>) =
            // G<lcta(X)>`, and `lcta(X) = X` when `X` is proper).
            return relevant[0];
        }
        let mut acc = relevant[0];
        for w in &relevant[1..] {
            acc = self.lcp_pair(acc, *w);
        }
        acc
    }

    fn lcp_pair(&mut self, a: Ty, b: Ty) -> Ty {
        let (name, xs) = a.as_reference(self.db).expect("parameterized type");
        let (_, ys) = b.as_reference(self.db).expect("parameterized type");
        let args = xs
            .iter()
            .zip(ys)
            .map(|(x, y)| self.lcta_pair(*x, *y))
            .collect();
        Ty::reference(self.db, name.clone(), args)
    }

    /// lcta(U, V), the least containing type argument for a pair
    /// ([JLS §4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
    fn lcta_pair(&mut self, x: Ty, y: Ty) -> Ty {
        match (x.kind(self.db), y.kind(self.db)) {
            // `?` contains every type argument ([§4.5.1]).
            (TyKind::Wildcard(None), _) | (_, TyKind::Wildcard(None)) => {
                Ty::wildcard(self.db, None)
            }
            (TyKind::Wildcard(Some(bx)), TyKind::Wildcard(Some(by))) => match (bx.kind, by.kind) {
                (BoundKind::Upper, BoundKind::Upper) => {
                    let merged = self.set(&[bx.ty, by.ty]);
                    self.upper_wildcard(merged)
                }
                (BoundKind::Upper, BoundKind::Lower) => Ty::wildcard(self.db, None),
                (BoundKind::Lower, BoundKind::Upper) => Ty::wildcard(self.db, None),
                (BoundKind::Lower, BoundKind::Lower) => {
                    let merged = self.glb(bx.ty, by.ty);
                    self.lower_wildcard(merged)
                }
            },
            (TyKind::Wildcard(Some(bx)), _) => match bx.kind {
                BoundKind::Upper => {
                    let merged = self.set(&[bx.ty, y]);
                    self.upper_wildcard(merged)
                }
                BoundKind::Lower => {
                    let merged = self.glb(bx.ty, y);
                    self.lower_wildcard(merged)
                }
            },
            (_, TyKind::Wildcard(Some(by))) => match by.kind {
                BoundKind::Upper => {
                    let merged = self.set(&[x, by.ty]);
                    self.upper_wildcard(merged)
                }
                BoundKind::Lower => {
                    let merged = self.glb(x, by.ty);
                    self.lower_wildcard(merged)
                }
            },
            (_, _) => {
                if x == y {
                    x
                } else {
                    let merged = self.set(&[x, y]);
                    self.upper_wildcard(merged)
                }
            }
        }
    }

    fn upper_wildcard(&self, upper: Ty) -> Ty {
        Ty::wildcard(
            self.db,
            Some(Box::new(WildcardBound {
                kind: BoundKind::Upper,
                ty: upper,
            })),
        )
    }

    fn lower_wildcard(&self, lower: Ty) -> Ty {
        Ty::wildcard(
            self.db,
            Some(Box::new(WildcardBound {
                kind: BoundKind::Lower,
                ty: lower,
            })),
        )
    }

    /// The greatest lower bound ([JLS §5.1.10](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.10)):
    /// `u` when `u <: v`, else `v` when `v <: u`, else the intersection `u & v`.
    fn glb(&self, u: Ty, v: Ty) -> Ty {
        if is_subtype(self.db, self.scope, &u, &v) {
            return u;
        }
        if is_subtype(self.db, self.scope, &v, &u) {
            return v;
        }
        Ty::intersection(self.db, vec![u, v])
    }
}
