//! Java member-type lookup, separate from the language-neutral canonical-name
//! index: JLS §6.5.5.2 and §8.5 permit `Subclass.Member` to name a declaration
//! in a superclass. Kotlin nested classifiers instead belong to their declaring
//! classifier's path (https://kotlinlang.org/spec/declarations.html#nested-and-inner-classifiers).

use super::*;
use crate::jvm::member::{Access, ClassKey};
use crate::jvm::member_set::{InvocationContext, InvocationMode, source_top_level, top_level_of};
use hir_def::java::modifiers::JavaVisibility;
use rustc_hash::FxHashSet;

struct Member {
    name: Name,
    owner: Name,
    access: Access,
    package: Option<String>,
    top_level: String,
}

/// JLS §8.5: declarations hide inherited types; inheritance excludes private
/// types and package members across a package boundary. Deduplicate declaration
/// identities, not paths, so interface diamonds are not ambiguous.
/// https://docs.oracle.com/javase/specs/jls/se25/html/jls-8.html#jls-8.5
struct Lookup<'a> {
    db: &'a dyn TyDatabase,
    scope: &'a hir::ResolutionScope,
    active: FxHashSet<(Name, Name)>,
}

impl Lookup<'_> {
    fn qualified(&mut self, name: &Name, ctx: Option<&InvocationContext>) -> Vec<Name> {
        if let Some(name) = canonical_type_name(self.db, self.scope, name) {
            return vec![name];
        }
        let Some((prefix, simple)) = name.as_str().rsplit_once('.') else {
            return Vec::new();
        };
        let owners = self.qualified(&Name::new(prefix), ctx);
        // An ambiguous qualifier is not a type through which a later segment
        // can be selected (JLS §6.5.5.2).
        if owners.len() != 1 {
            return Vec::new();
        }
        let owner = &owners[0];
        let mut result = Vec::new();
        for member in self.members(owner, &Name::new(simple)) {
            if ctx.is_some_and(|ctx| !self.accessible(&member, owner, ctx)) {
                continue;
            }
            if !result.contains(&member.name) {
                result.push(member.name);
            }
        }
        result
    }

    fn accessible(&self, member: &Member, owner: &Name, ctx: &InvocationContext) -> bool {
        // §6.6.2.1 restricts protected instance fields and methods by receiver
        // type, not member types (including non-static member classes).
        crate::java::method::member_accessible(
            self.db,
            self.scope,
            member.access,
            Some(member.package.as_deref().unwrap_or("")),
            &ClassKey::Named(member.owner.clone()),
            Some(&member.top_level),
            &Ty::reference(self.db, owner.clone(), Vec::new()),
            true,
            ctx,
        )
    }

    fn members(&mut self, owner: &Name, simple: &Name) -> Vec<Member> {
        let key = (owner.clone(), simple.clone());
        if !self.active.insert(key.clone()) {
            return Vec::new();
        }
        let result = self.members_inner(owner, simple);
        self.active.remove(&key);
        result
    }

    fn members_inner(&mut self, owner: &Name, simple: &Name) -> Vec<Member> {
        let candidate = join(owner, simple.as_str());
        if let Some(member) = self.declared(owner, &candidate) {
            return vec![member];
        }
        let package = self.package(owner);
        let mut result: Vec<Member> = Vec::new();
        for parent in self.parents(owner) {
            for member in self.members(&parent, simple) {
                if member.access == Access::Private
                    || (member.access == Access::Package && member.package != package)
                    || result.iter().any(|existing| existing.name == member.name)
                {
                    continue;
                }
                result.push(member);
            }
        }
        result
    }

    fn declared(&self, owner: &Name, candidate: &Name) -> Option<Member> {
        let resolved = hir::fqn_resolve(self.db, self.scope, candidate.as_str())?;
        let (name, access, package, top_level) = match resolved {
            hir::Resolved::Library(class) => {
                let record = hir::class_record(self.db, &class)?;
                let hir::ClassOrModuleStub::Class(record) = &*record else {
                    return None;
                };
                let name = Name::new(self.db.hir_state().interner.resolve(&record.fqn));
                let package = name.as_str().rsplit_once('.').map(|(p, _)| p.to_owned());
                let top_level = top_level_of(name.as_str());
                (name, Access::from_flags(record.flags), package, top_level)
            }
            hir::Resolved::Source(source) => {
                let tree = hir_def::java::plugin::tree(self.db, source.file);
                let data = item_data(&tree, source.item)?;
                let visibility = match data {
                    ItemData::Class(d) | ItemData::Interface(d) => d.modifiers.visibility,
                    ItemData::Enum(d) => d.modifiers.visibility,
                    ItemData::Record(d) => d.modifiers.visibility,
                    ItemData::Annotation(d) => d.modifiers.visibility,
                    _ => return None,
                };
                let in_interface = tree.parent_of(source.item).is_some_and(|parent| {
                    matches!(
                        tree.data(parent),
                        ItemData::Interface(_) | ItemData::Annotation(_)
                    )
                });
                let access = if in_interface {
                    Access::Public
                } else {
                    match visibility {
                        JavaVisibility::Public => Access::Public,
                        JavaVisibility::Protected => Access::Protected,
                        JavaVisibility::Package => Access::Package,
                        JavaVisibility::Private => Access::Private,
                    }
                };
                let name = hir::source_class_fqn(self.db, source.file, source.item)?;
                let package = tree.package.as_ref().map(|p| p.as_str().to_owned());
                let top_level = source_top_level(package.as_deref(), name.as_str());
                (name, access, package, top_level)
            }
            hir::Resolved::Facade { .. } => return None,
        };
        Some(Member {
            name,
            owner: owner.clone(),
            access,
            package,
            top_level,
        })
    }

    fn package(&self, owner: &Name) -> Option<String> {
        match hir::fqn_resolve(self.db, self.scope, owner.as_str())? {
            hir::Resolved::Source(source) => hir_def::java::plugin::tree(self.db, source.file)
                .package
                .as_ref()
                .map(|p| p.as_str().to_owned()),
            _ => owner.as_str().rsplit_once('.').map(|(p, _)| p.to_owned()),
        }
    }

    // Resolve source supertype *names* using this same guarded walk, not the
    // typing query: resolving `class C extends C.Missing` otherwise re-enters
    // type resolution through supertypes indefinitely (JLS §8.1.4).
    fn parents(&mut self, owner: &Name) -> Vec<Name> {
        let Some(resolved) = hir::fqn_resolve(self.db, self.scope, owner.as_str()) else {
            return Vec::new();
        };
        match resolved {
            hir::Resolved::Library(_) => hir::super_types(self.db, &resolved)
                .iter()
                .map(|name| Name::new(self.db.hir_state().interner.resolve(name)))
                .collect(),
            hir::Resolved::Source(source) => {
                let tree = hir_def::java::plugin::tree(self.db, source.file);
                let Some(data) = item_data(&tree, source.item) else {
                    return Vec::new();
                };
                let refs: Vec<_> = match data {
                    ItemData::Class(d) | ItemData::Interface(d) => {
                        d.super_class.iter().chain(&d.interfaces).collect()
                    }
                    ItemData::Enum(d) => d.interfaces.iter().collect(),
                    ItemData::Record(d) => d.interfaces.iter().collect(),
                    _ => Vec::new(),
                };
                let resolver = Resolver::for_item(self.db, source.file, &tree, source.item);
                let mut parents = Vec::new();
                for reference in refs {
                    let TypeRef::Reference { name, .. } = &**reference else {
                        continue;
                    };
                    let candidates = candidate_fqns(&resolver, name);
                    // Direct names first: a superclass named in this package
                    // must not require looking through its subclass to find it.
                    if let Some(parent) = candidates
                        .iter()
                        .find_map(|name| canonical_type_name(self.db, self.scope, name))
                    {
                        parents.push(parent);
                    } else {
                        for candidate in candidates {
                            let found = self.qualified(&candidate, None);
                            if !found.is_empty() {
                                parents.extend(found);
                                break;
                            }
                        }
                    }
                }
                parents
            }
            hir::Resolved::Facade { .. } => Vec::new(),
        }
    }
}

/// Resolve an otherwise missing qualified type using inherited members. The
/// canonical declaration name is shared by typing and checked name resolution
/// (JLS §6.7), so parameters written through a subclass match binary signatures.
/// https://docs.oracle.com/javase/specs/jls/se25/html/jls-6.html#jls-6.5.5.2
pub(super) fn resolve(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    candidate: &Name,
) -> Vec<Name> {
    let mut lookup = Lookup {
        db,
        scope,
        active: FxHashSet::default(),
    };
    let ctx = InvocationContext {
        mode: InvocationMode::Static,
        enclosing_class: resolver.enclosing().first().cloned().map(ClassKey::Named),
        package: Some(resolver.package().map_or("", Name::as_str).to_owned()),
        subclass_of: None,
    };
    lookup.qualified(candidate, Some(&ctx))
}
