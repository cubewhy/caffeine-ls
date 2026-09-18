//! The declaration-level checks of a Kotlin file: what `kotlinc` reports for a
//! declaration before any body is typed.
//!
//! The shape mirrors the Java checker ([`crate::java::decl_check`]): one
//! [`DeclDiagnostic`] per finding, the *compiler's* wording — captured from
//! kotlinc 2.4.20 on this machine — so a run of this server compares against a
//! compiler run, and one entry point per file ([`class_diagnostics`]). A
//! *typing* finding is [`crate::kotlin::diagnostics`]'s, and a body's is the
//! inference's.
//!
//! The checks are the ones the Kotlin language defines over a declaration:
//! the override rules of [KLS
//! `declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding),
//! abstract members
//! ([`#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)),
//! supertype initialization
//! ([`#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)),
//! duplicate declarations
//! ([`overload-resolution.html`](https://kotlinlang.org/spec/overload-resolution.html)),
//! the applicability of a modifier
//! ([`declarations.html#modifiers`](https://kotlinlang.org/spec/declarations.html#modifiers))
//! and `lateinit`
//! ([`declarations.html#lateinit-properties`](https://kotlinlang.org/spec/declarations.html#lateinit-properties)).
//!
//! Every check reads the *Kotlin-visible* member set the rest of the layer
//! resolves against ([`crate::kotlin::method::declared_members`]), so a Kotlin
//! class overriding a Java superclass member is checked against the Java
//! member — a `getFoo()` maps to the property `foo` there, exactly as a call
//! site sees it.
//!
//! # Recorded deviations
//!
//! * **A supertype with a constructor.** kotlinc 2.4.20 reports
//!   `this type has a constructor, so it must be initialized here.` for *every*
//!   classifier supertype written without a delegation call, even one with an
//!   implicit no-argument constructor — `abstract class BaseAddon` extended by
//!   `class JavaAgent : BaseAddon` is reported by it. KLS
//!   `declarations.html#supertype-specifiers` does not say that, and a real
//!   Kotlin project is all false positives under it, so
//!   [`check_supertype_initializers`] reports the case the two agree on: a
//!   supertype whose primary constructor declares a parameter the subclass
//!   cannot omit.
//! * **Ranges.** A finding anchors at the declaration's name, or at its whole
//!   declaration where it has none; the finer type-reference and supertype
//!   ranges belong to the navigation layer ([`hir_def::kotlin::ranges`]), and
//!   the checks need nothing finer.
//! * **A member of another language.** A Java or classfile supertype answers
//!   through its JVM view, so an abstract `method()` a classfile declares is an
//!   obligation of the same name; its parameters are compared as Kotlin types.

use base_db::LanguageKind;
use hir::hir_def::kotlin::item_tree::{
    ClassData, KotlinClassKind, KotlinItemData, KotlinItemTree, KotlinSuperType, PropertyData,
};
use hir::hir_def::kotlin::modifiers::{KotlinModality, KotlinModifierFlags, KotlinModifiers};
use hir_expand::{ids::ItemId, name::Name};
use rowan::TextRange;
use syntax::KotlinDiagnosticCode;
use vfs::FileId;

use crate::jvm::db::TyDatabase;
use crate::kotlin::method::{self, CallSite, Member, MemberKind, MemberTarget};
use crate::kotlin::resolve::KotlinResolver;
use crate::kotlin::ty::{display_kotlin, ty_from_java, ty_from_type_ref};
use crate::ty::{Ty, TyKind};

/// A declaration-level finding of a Kotlin file. Each carries the message
/// kotlinc 2.4.20 prints for the same source; [`DeclDiagnostic::message`]
/// renders it.
#[derive(Debug, Clone, PartialEq)]
pub enum DeclDiagnostic {
    /// KLS
    /// [`declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding):
    /// an `override` member overrides nothing. kotlinc: `'nothingHere'
    /// overrides nothing.` — with ` Potential signatures for overriding:` and
    /// the supertype declarations when a member of the same *name* exists with
    /// another signature ([`Self::OverridesNothing::candidates`]).
    OverridesNothing {
        name: Name,
        candidates: Vec<String>,
        range: Option<TextRange>,
    },
    /// A declared member has a supertype member's signature without writing
    /// `override`, and that member may be overridden. kotlinc: `'f' hides
    /// member of supertype 'A' and needs an 'override' modifier.`
    NeedsOverrideModifier {
        name: Name,
        supertype: Name,
        range: Option<TextRange>,
    },
    /// An `override` member overrides a `final` one — the default modality of a
    /// class member ([KLS
    /// `declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding)).
    /// kotlinc: `'g' in 'A' is final and cannot be overridden.`
    FinalMemberOverridden {
        name: Name,
        supertype: Name,
        range: Option<TextRange>,
    },
    /// A concrete classifier leaves an inherited abstract member
    /// unimplemented ([KLS
    /// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)).
    /// kotlinc: `class 'Missing1' is not abstract and does not implement
    /// abstract base class member:\nfun mustImplement(): Int` — and `… abstract
    /// member:` when the obligation comes from an interface.
    UnimplementedAbstractMember {
        class: Name,
        member: String,
        /// Whether the obligation is an *interface* member, which kotlinc words
        /// `abstract member` rather than `abstract base class member`.
        from_interface: bool,
        range: Option<TextRange>,
    },
    /// A supertype whose primary constructor declares a parameter the subclass
    /// cannot omit is written without a delegation call ([KLS
    /// `declarations.html#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)).
    /// kotlinc: `this type has a constructor, so it must be initialized here.`
    SupertypeNotInitialized { range: Option<TextRange> },
    /// Two declarations of one scope have the same signature ([KLS
    /// `overload-resolution.html`](https://kotlinlang.org/spec/overload-resolution.html)).
    /// kotlinc: `conflicting overloads:` for two functions, `conflicting
    /// declarations:` for two properties, each followed by the *other*
    /// declaration.
    ConflictingDeclarations {
        /// The prefix kotlinc uses for this pair: `conflicting overloads` or
        /// `conflicting declarations`.
        prefix: &'static str,
        signature: String,
        range: Option<TextRange>,
    },
    /// A modifier on a declaration that does not accept it ([KLS
    /// `declarations.html#modifiers`](https://kotlinlang.org/spec/declarations.html#modifiers)).
    /// kotlinc: `modifier 'data' is not applicable to 'top level function'.`
    ModifierNotApplicable {
        modifier: &'static str,
        target: &'static str,
        range: Option<TextRange>,
    },
    /// `lateinit` on a `val` — only a `var` can be initialized later ([KLS
    /// `declarations.html#lateinit-properties`](https://kotlinlang.org/spec/declarations.html#lateinit-properties)).
    /// kotlinc: `'lateinit' modifier is allowed only on mutable properties.`
    LateinitOnImmutableProperty { range: Option<TextRange> },
    /// `lateinit` on a property that writes an initializer. kotlinc:
    /// `'lateinit' modifier is not allowed on properties with initializer.`
    LateinitWithInitializer { range: Option<TextRange> },
    /// `lateinit` on a property of a primitive type, whose field cannot carry
    /// the null default `lateinit` relies on. kotlinc: `'lateinit' modifier is
    /// not allowed on properties of primitive types.`
    LateinitOnPrimitive { range: Option<TextRange> },
}

impl DeclDiagnostic {
    /// The message kotlinc 2.4.20 reports for this finding.
    pub fn message(&self, _db: &dyn TyDatabase) -> String {
        match self {
            DeclDiagnostic::OverridesNothing {
                name, candidates, ..
            } => {
                let mut message = format!("'{}' overrides nothing.", name.as_str());
                if !candidates.is_empty() {
                    message.push_str(" Potential signatures for overriding:\n");
                    message.push_str(&candidates.join("\n"));
                }
                message
            }
            DeclDiagnostic::NeedsOverrideModifier {
                name, supertype, ..
            } => format!(
                "'{}' hides member of supertype '{}' and needs an 'override' modifier.",
                name.as_str(),
                supertype.as_str()
            ),
            DeclDiagnostic::FinalMemberOverridden {
                name, supertype, ..
            } => format!(
                "'{}' in '{}' is final and cannot be overridden.",
                name.as_str(),
                supertype.as_str()
            ),
            DeclDiagnostic::UnimplementedAbstractMember {
                class,
                member,
                from_interface,
                ..
            } => {
                let obligation = match from_interface {
                    true => "abstract member",
                    false => "abstract base class member",
                };
                format!(
                    "class '{}' is not abstract and does not implement {obligation}:\n{member}",
                    class.as_str()
                )
            }
            DeclDiagnostic::SupertypeNotInitialized { .. } => {
                "this type has a constructor, so it must be initialized here.".to_owned()
            }
            DeclDiagnostic::ConflictingDeclarations {
                prefix, signature, ..
            } => format!("{prefix}:\n{signature}"),
            DeclDiagnostic::ModifierNotApplicable {
                modifier, target, ..
            } => format!("modifier '{modifier}' is not applicable to '{target}'."),
            DeclDiagnostic::LateinitOnImmutableProperty { .. } => {
                "'lateinit' modifier is allowed only on mutable properties.".to_owned()
            }
            DeclDiagnostic::LateinitWithInitializer { .. } => {
                "'lateinit' modifier is not allowed on properties with initializer.".to_owned()
            }
            DeclDiagnostic::LateinitOnPrimitive { .. } => {
                "'lateinit' modifier is not allowed on properties of primitive types.".to_owned()
            }
        }
    }

    /// The stable code of the finding ([`KotlinDiagnosticCode`]).
    pub fn code(&self) -> KotlinDiagnosticCode {
        match self {
            DeclDiagnostic::OverridesNothing { .. } => KotlinDiagnosticCode::OverridesNothing,
            DeclDiagnostic::NeedsOverrideModifier { .. } => {
                KotlinDiagnosticCode::NeedsOverrideModifier
            }
            DeclDiagnostic::FinalMemberOverridden { .. } => {
                KotlinDiagnosticCode::FinalMemberOverridden
            }
            DeclDiagnostic::UnimplementedAbstractMember { .. } => {
                KotlinDiagnosticCode::UnimplementedAbstractMember
            }
            DeclDiagnostic::SupertypeNotInitialized { .. } => {
                KotlinDiagnosticCode::SupertypeNotInitialized
            }
            DeclDiagnostic::ConflictingDeclarations { prefix, .. } => match *prefix {
                "conflicting declarations" => KotlinDiagnosticCode::ConflictingDeclarations,
                _ => KotlinDiagnosticCode::ConflictingOverloads,
            },
            DeclDiagnostic::ModifierNotApplicable { .. } => {
                KotlinDiagnosticCode::ModifierNotApplicable
            }
            DeclDiagnostic::LateinitOnImmutableProperty { .. } => {
                KotlinDiagnosticCode::LateinitOnImmutableProperty
            }
            DeclDiagnostic::LateinitWithInitializer { .. } => {
                KotlinDiagnosticCode::LateinitWithInitializer
            }
            DeclDiagnostic::LateinitOnPrimitive { .. } => KotlinDiagnosticCode::LateinitOnPrimitive,
        }
    }

    /// The source range the finding underlines: the declaration's name where it
    /// has one, its whole declaration otherwise.
    pub fn range(&self) -> Option<TextRange> {
        match self {
            DeclDiagnostic::OverridesNothing { range, .. }
            | DeclDiagnostic::NeedsOverrideModifier { range, .. }
            | DeclDiagnostic::FinalMemberOverridden { range, .. }
            | DeclDiagnostic::UnimplementedAbstractMember { range, .. }
            | DeclDiagnostic::SupertypeNotInitialized { range }
            | DeclDiagnostic::ConflictingDeclarations { range, .. }
            | DeclDiagnostic::ModifierNotApplicable { range, .. }
            | DeclDiagnostic::LateinitOnImmutableProperty { range }
            | DeclDiagnostic::LateinitWithInitializer { range }
            | DeclDiagnostic::LateinitOnPrimitive { range } => *range,
        }
    }
}

/// The declaration findings of `file`: every scope the file declares — its top
/// level, every classifier body and every function body — checked against the
/// members its supertypes declare.
pub fn class_diagnostics(db: &dyn TyDatabase, file: FileId) -> Vec<DeclDiagnostic> {
    let outer = hir::file_item_tree(db, file);
    let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
        return Vec::new();
    };
    let scope = match hir::source_set_for_file(db, file) {
        Some(source_set) => hir::ResolutionScope::SourceSet(source_set),
        None => hir::ResolutionScope::JdkBuiltins,
    };
    let mut out = Vec::new();
    let top = tree.top.clone();
    check_scope(db, file, tree, &scope, &top, Location::TopLevel, &mut out);
    out
}

/// Where a scope is: a file's top level, a classifier body, or a function body.
/// kotlinc names a declaration's kind by it (`top level function`, `member
/// function`, `local function`), which is what
/// [`DeclDiagnostic::ModifierNotApplicable`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    TopLevel,
    Member,
    Local,
}

/// The declaration checks over one *scope*.
fn check_scope(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    scope: &hir::ResolutionScope,
    items: &[ItemId],
    location: Location,
    out: &mut Vec<DeclDiagnostic>,
) {
    let declarations = declared_signatures(db, file, tree, items);
    report_conflicts(db, file, tree, &declarations, out);
    for declaration in &declarations {
        check_modifiers(db, file, tree, declaration, location, out);
    }
    for &item in items {
        match tree.data(item) {
            KotlinItemData::Class(class) => {
                check_class(db, file, tree, scope, item, class, location, out);
                let mut nested = class.body.clone();
                nested.extend(class.primary_constructor);
                check_scope(db, file, tree, scope, &nested, Location::Member, out);
            }
            KotlinItemData::Function(_) => {
                let locals: Vec<ItemId> = tree.local_types_of(item).collect();
                check_scope(db, file, tree, scope, &locals, Location::Local, out);
            }
            _ => {}
        }
    }
}

/// One declaration of a scope, reduced to what the signature checks compare.
struct Declaration {
    item: ItemId,
    kind: MemberKindClass,
    name: Name,
    params: Vec<Ty>,
    modifiers: KotlinModifiers,
    /// The declaration as kotlinc prints it in a conflict or an
    /// unimplemented-member message (`fun a(x: Int): Unit`).
    signature: String,
}

/// The class identity a signature comparison uses: a property (a `val`/`var`,
/// with its accessors) and a function of the same name are *different*
/// declarations, and a property read and a function call with no arguments are
/// written apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberKindClass {
    Function,
    Property,
}

/// The declarations of `items` a signature check applies to, in source order.
fn declared_signatures(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    items: &[ItemId],
) -> Vec<Declaration> {
    let mut out = Vec::new();
    for &item in items {
        let (name, modifiers, kind) = match tree.data(item) {
            KotlinItemData::Function(function) => (
                function.name.clone(),
                function.modifiers,
                MemberKindClass::Function,
            ),
            KotlinItemData::Property(property) => (
                property.name.clone(),
                property.modifiers,
                MemberKindClass::Property,
            ),
            _ => continue,
        };
        let resolver = KotlinResolver::for_item(db, file, tree, item);
        let params: Vec<Ty> = match tree.data(item) {
            KotlinItemData::Function(function) => function
                .params
                .iter()
                .map(|param| ty_from_type_ref(db, &resolver, &param.param.ty.ty))
                .collect(),
            _ => Vec::new(),
        };
        let signature = render_signature(db, file, tree, item);
        out.push(Declaration {
            item,
            kind,
            name,
            params,
            modifiers,
            signature,
        });
    }
    out
}

/// The declaration as kotlinc prints it in a conflict message
/// (`fun a(x: Int): Unit`, `val dupProp: Int`), resolved through the
/// declaration's own scope.
fn render_signature(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    item: ItemId,
) -> String {
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    let render = |ty: &hir::hir_def::jvm::decl::ItemTypeRef| {
        display_kotlin(db, ty_from_type_ref(db, &resolver, &ty.ty)).to_string()
    };
    match tree.data(item) {
        KotlinItemData::Function(function) => {
            let params = function
                .params
                .iter()
                .map(|param| format!("{}: {}", param.param.name, render(&param.param.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            let ret = function
                .ret
                .as_ref()
                .map(render)
                .unwrap_or_else(|| "Unit".to_owned());
            format!("fun {}({params}): {ret}", function.name)
        }
        KotlinItemData::Property(property) => {
            let ty = property.ty.as_ref().map(render).unwrap_or_else(|| {
                display_kotlin(db, super::db::item_ty(db, file, item)).to_string()
            });
            let keyword = match property.is_var {
                true => "var",
                false => "val",
            };
            format!("{keyword} {}: {ty}", property.name)
        }
        _ => String::new(),
    }
}

/// Reports every declaration whose signature another declaration of the same
/// scope already wrote, naming the other one
/// ([KLS
/// `overload-resolution.html#overload-candidate-set`](https://kotlinlang.org/spec/overload-resolution.html#overload-candidate-set)
/// makes two declarations conflict exactly when their names and parameter types
/// agree; kotlinc words two functions `conflicting overloads` and two
/// properties `conflicting declarations`).
fn report_conflicts(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    declarations: &[Declaration],
    out: &mut Vec<DeclDiagnostic>,
) {
    for (index, declaration) in declarations.iter().enumerate() {
        let partner = declarations.iter().enumerate().find(|(other, candidate)| {
            *other != index
                && candidate.kind == declaration.kind
                && candidate.name == declaration.name
                && same_params(db, &candidate.params, &declaration.params)
        });
        let Some((_, partner)) = partner else {
            continue;
        };
        out.push(DeclDiagnostic::ConflictingDeclarations {
            prefix: match declaration.kind {
                MemberKindClass::Function => "conflicting overloads",
                MemberKindClass::Property => "conflicting declarations",
            },
            signature: partner.signature.clone(),
            range: name_range(db, file, tree, declaration.item),
        });
    }
}

/// The override rules of one classifier ([KLS
/// `declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding)),
/// the abstract-member obligation
/// ([`#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes))
/// and the supertype-initialization rule
/// ([`#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)).
fn check_class(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    scope: &hir::ResolutionScope,
    item: ItemId,
    class: &ClassData,
    location: Location,
    out: &mut Vec<DeclDiagnostic>,
) {
    let site = CallSite { file, item };
    let class_ty = super::db::item_ty(db, file, item);
    check_class_modifiers(db, file, tree, item, class, location, out);
    check_supertype_initializers(db, file, tree, scope, item, class, out);
    let declarations = declared_signatures(db, file, tree, &class.body);
    for declaration in &declarations {
        check_override(db, file, tree, scope, &class_ty, declaration, site, out);
    }
    if concrete(class) {
        check_abstract_members(
            db,
            file,
            tree,
            scope,
            item,
            class,
            &class_ty,
            declarations.as_slice(),
            site,
            out,
        );
    }
}

/// Whether the classifier must implement everything it inherits: an `abstract`
/// or `sealed` classifier, an `interface` and an `annotation class` (whose
/// members are abstract by declaration) need not ([KLS
/// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)).
fn concrete(class: &ClassData) -> bool {
    !matches!(
        class.modifiers.modality,
        KotlinModality::Abstract | KotlinModality::Sealed
    ) && !matches!(
        class.kind,
        KotlinClassKind::Interface | KotlinClassKind::Annotation
    )
}

/// The override checks of one declared member ([KLS
/// `declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding)).
fn check_override(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    scope: &hir::ResolutionScope,
    class_ty: &Ty,
    declaration: &Declaration,
    site: CallSite,
    out: &mut Vec<DeclDiagnostic>,
) {
    // A `private` member is invisible to a subclass and never an override
    // ([KLS
    // `declarations.html#declaration-visibility`](https://kotlinlang.org/spec/declarations.html#declaration-visibility)).
    if declaration.modifiers.visibility.is_private() {
        return;
    }
    let range = name_range(db, file, tree, declaration.item);
    let mut matches: Vec<(Ty, Member)> = Vec::new();
    let mut same_named: Vec<String> = Vec::new();
    for supertype in super::subtyping::supertypes(db, scope, class_ty) {
        for candidate in method::declared_members(db, scope, &supertype, &declaration.name, site) {
            if member_kind_class(&candidate) != Some(declaration.kind) {
                continue;
            }
            if signature_matches(db, &candidate, &declaration.params) {
                matches.push((supertype, candidate));
            } else {
                same_named.push(member_signature(db, &candidate));
            }
        }
    }
    let overrides = declaration
        .modifiers
        .flags
        .contains(KotlinModifierFlags::OVERRIDE);
    if overrides {
        if let Some((supertype, _)) = matches.iter().find(|(_, member)| !overridable(db, member)) {
            out.push(DeclDiagnostic::FinalMemberOverridden {
                name: declaration.name.clone(),
                supertype: supertype_name(db, supertype),
                range,
            });
        } else if matches.is_empty() {
            out.push(DeclDiagnostic::OverridesNothing {
                name: declaration.name.clone(),
                candidates: same_named,
                range,
            });
        }
        return;
    }
    if let Some((supertype, member)) = matches.first()
        && overridable(db, member)
    {
        out.push(DeclDiagnostic::NeedsOverrideModifier {
            name: declaration.name.clone(),
            supertype: supertype_name(db, supertype),
            range,
        });
    }
}

/// The members the hierarchy still requires of a concrete classifier
/// ([KLS
/// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)):
/// every abstract member of its supertype closure for which it declares no
/// implementation.
#[allow(clippy::too_many_arguments)]
fn check_abstract_members(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    scope: &hir::ResolutionScope,
    item: ItemId,
    class: &ClassData,
    class_ty: &Ty,
    declarations: &[Declaration],
    site: CallSite,
    out: &mut Vec<DeclDiagnostic>,
) {
    let _ = declarations;
    let range = declaration_range(db, file, tree, item);
    let mut seen: rustc_hash::FxHashSet<Ty> = rustc_hash::FxHashSet::default();
    let mut frontier = super::subtyping::supertypes(db, scope, class_ty);
    let mut reported: Vec<(Name, Vec<Ty>)> = Vec::new();
    while let Some(supertype) = frontier.pop() {
        if !seen.insert(supertype) {
            continue;
        }
        for obligation in abstract_members_of(db, scope, &supertype) {
            if reported
                .iter()
                .any(|(name, params)| *name == obligation.name && *params == obligation.params)
            {
                continue;
            }
            // The classifier's *own* member set answers with its most derived
            // member of the name: an implementation is one that is not abstract.
            let implemented = method::declared_members(db, scope, class_ty, &obligation.name, site)
                .iter()
                .any(|member| {
                    member_kind_class(member) == Some(obligation.kind)
                        && signature_matches(db, member, &obligation.params)
                        && !member_is_abstract(db, member)
                });
            if implemented {
                continue;
            }
            reported.push((obligation.name.clone(), obligation.params.clone()));
            out.push(DeclDiagnostic::UnimplementedAbstractMember {
                class: class.name.clone(),
                member: obligation.signature,
                from_interface: obligation.from_interface,
                range,
            });
        }
        frontier.extend(super::subtyping::supertypes(db, scope, &supertype));
    }
}

/// An abstract member one supertype declares, as the checks compare it.
struct Obligation {
    name: Name,
    kind: MemberKindClass,
    params: Vec<Ty>,
    signature: String,
    from_interface: bool,
}

/// The abstract members `supertype` declares: a Kotlin source supertype answers
/// from its own item tree (a member written `abstract`), a Java or classfile one
/// from the members its classfile records `ACC_ABSTRACT` ([KLS
/// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)).
fn abstract_members_of(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    supertype: &Ty,
) -> Vec<Obligation> {
    let mut out = Vec::new();
    let TyKind::Reference { name, .. } = supertype.kind(db) else {
        return out;
    };
    let Some(resolved) = hir::fqn_resolve(db, scope, name.as_str()) else {
        return out;
    };
    match &resolved {
        hir::Resolved::Source(class) => {
            let outer = hir::file_item_tree(db, class.file);
            let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
                return out;
            };
            let KotlinItemData::Class(inner) = tree.data(class.item) else {
                return out;
            };
            let from_interface = matches!(
                inner.kind,
                KotlinClassKind::Interface | KotlinClassKind::Annotation
            );
            for &member in &inner.body {
                let (member_name, modifiers, kind) = match tree.data(member) {
                    KotlinItemData::Function(function) => (
                        function.name.clone(),
                        function.modifiers,
                        MemberKindClass::Function,
                    ),
                    KotlinItemData::Property(property) => (
                        property.name.clone(),
                        property.modifiers,
                        MemberKindClass::Property,
                    ),
                    _ => continue,
                };
                // A member written `abstract`, or one an *interface* declares
                // without a body — its members are abstract by declaration
                // ([KLS
                // `declarations.html#interface-declaration`](https://kotlinlang.org/spec/declarations.html#interface-declaration)).
                if !(is_abstract(&modifiers) || (from_interface && !has_body(tree, member))) {
                    continue;
                }
                let resolver = KotlinResolver::for_item(db, class.file, tree, member);
                let params: Vec<Ty> = match tree.data(member) {
                    KotlinItemData::Function(function) => function
                        .params
                        .iter()
                        .map(|param| ty_from_type_ref(db, &resolver, &param.param.ty.ty))
                        .collect(),
                    _ => Vec::new(),
                };
                let signature = render_signature(db, class.file, tree, member);
                out.push(Obligation {
                    name: member_name,
                    kind,
                    params,
                    signature,
                    from_interface,
                });
            }
        }
        hir::Resolved::Library(_) => {
            let Some(source) = crate::lang::member_source(db, &resolved) else {
                return out;
            };
            let args = match supertype.kind(db) {
                TyKind::Reference { args, .. } => args.to_vec(),
                _ => Vec::new(),
            };
            for member in source.methods(db, &resolved, &args, "") {
                if !member.abstract_ {
                    continue;
                }
                let params: Vec<Ty> = member
                    .params
                    .iter()
                    .map(|param| ty_from_java(db, *param))
                    .collect();
                let rendered = params
                    .iter()
                    .map(|param| display_kotlin(db, *param).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = display_kotlin(db, ty_from_java(db, member.ret));
                out.push(Obligation {
                    name: Name::new(&member.name),
                    kind: MemberKindClass::Function,
                    params,
                    signature: format!("fun {}({rendered}): {ret}", member.name),
                    // A classfile declares no interface kind for the *member*'s
                    // owner here beyond the owner's own record.
                    from_interface: member.declaring_interface,
                });
            }
        }
        hir::Resolved::Facade { .. } => {}
    }
    out
}

/// The supertype-initialization rule ([KLS
/// `declarations.html#supertype-specifiers`](https://kotlinlang.org/spec/declarations.html#supertype-specifiers)):
/// a supertype whose primary constructor declares a parameter the subclass
/// cannot omit needs a delegation call. See the module docs for the recorded
/// deviation from this machine's kotlinc, which reports the missing call for
/// every classifier supertype.
fn check_supertype_initializers(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    scope: &hir::ResolutionScope,
    item: ItemId,
    class: &ClassData,
    out: &mut Vec<DeclDiagnostic>,
) {
    let resolver = KotlinResolver::for_item(db, file, tree, item);
    let missing = class.super_types.iter().any(|super_type| {
        super_type.args.is_empty() && declares_required_parameters(db, scope, &resolver, super_type)
    });
    if missing {
        out.push(DeclDiagnostic::SupertypeNotInitialized {
            range: declaration_range(db, file, tree, item),
        });
    }
}

/// Whether the supertype a delegation specifier names is a Kotlin *class* whose
/// primary constructor declares a parameter the subclass cannot omit — one
/// without a default that is not a `vararg` ([KLS
/// `declarations.html#named-positional-and-default-parameters`](https://kotlinlang.org/spec/declarations.html#named-positional-and-default-parameters)).
///
/// A supertype of another language answers `false`: its constructors are the
/// classpath's, whose parameter lists the Kotlin rule about a *primary*
/// constructor does not speak about.
fn declares_required_parameters(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &KotlinResolver<'_>,
    super_type: &KotlinSuperType,
) -> bool {
    let ty = ty_from_type_ref(db, resolver, &super_type.ty.ty);
    let TyKind::Reference { name, .. } = ty.kind(db) else {
        return false;
    };
    let Some(hir::Resolved::Source(class)) = hir::fqn_resolve(db, scope, name.as_str()) else {
        return false;
    };
    let outer = hir::file_item_tree(db, class.file);
    let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
        return false;
    };
    let KotlinItemData::Class(inner) = tree.data(class.item) else {
        return false;
    };
    let Some(constructor) = inner.primary_constructor else {
        return false;
    };
    let KotlinItemData::Constructor(data) = tree.data(constructor) else {
        return false;
    };
    data.params.iter().enumerate().any(|(index, param)| {
        param.param.varargs == false && data.defaults.get(index) == Some(&None)
    })
}

/// The modifier rules of a class declaration ([KLS
/// `declarations.html#modifiers`](https://kotlinlang.org/spec/declarations.html#modifiers)).
fn check_class_modifiers(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    item: ItemId,
    class: &ClassData,
    location: Location,
    out: &mut Vec<DeclDiagnostic>,
) {
    let _ = location;
    let target = class_target(class);
    let mut modifiers: Vec<&'static str> =
        class.modifiers.flags.iter().filter_map(flag_name).collect();
    if class.modifiers.modality != KotlinModality::Final {
        modifiers.push(match class.modifiers.modality {
            KotlinModality::Open => "open",
            KotlinModality::Abstract => "abstract",
            KotlinModality::Sealed => "sealed",
            KotlinModality::Final => unreachable!("filtered above"),
        });
    }
    for modifier in modifiers {
        if modifier_applies(modifier, Target::Class(class.kind), location) {
            continue;
        }
        out.push(DeclDiagnostic::ModifierNotApplicable {
            modifier,
            target,
            range: name_range(db, file, tree, item),
        });
    }
}

/// The modifiers of a `fun`/`val`/`var` ([KLS
/// `declarations.html#modifiers`](https://kotlinlang.org/spec/declarations.html#modifiers)),
/// and `lateinit`'s own rules for a property.
fn check_modifiers(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    declaration: &Declaration,
    location: Location,
    out: &mut Vec<DeclDiagnostic>,
) {
    let range = name_range(db, file, tree, declaration.item);
    let kind = match declaration.kind {
        MemberKindClass::Function => Target::Function,
        MemberKindClass::Property => Target::Property {
            backing_field: has_backing_field(tree, declaration.item),
            mutable: matches!(tree.data(declaration.item), KotlinItemData::Property(p) if p.is_var),
        },
    };
    // `open` and `abstract` are the declaration's *modality*, not a flag of the
    // general set: a top-level or local declaration has no supertype and
    // accepts neither ([KLS
    // `declarations.html#inheritance`](https://kotlinlang.org/spec/declarations.html#inheritance)).
    let modality = match declaration.modifiers.modality {
        KotlinModality::Open => Some("open"),
        KotlinModality::Abstract => Some("abstract"),
        KotlinModality::Final | KotlinModality::Sealed => None,
    };
    let modifiers = declaration
        .modifiers
        .flags
        .iter()
        .filter_map(flag_name)
        .chain(modality);
    for modifier in modifiers {
        if modifier_applies(modifier, kind, location) {
            continue;
        }
        out.push(DeclDiagnostic::ModifierNotApplicable {
            modifier,
            target: target_name(kind, location),
            range,
        });
    }
    if let KotlinItemData::Property(property) = tree.data(declaration.item) {
        check_lateinit(db, file, tree, declaration.item, property, out);
    }
}

/// `lateinit`'s own rules ([KLS
/// `declarations.html#lateinit-properties`](https://kotlinlang.org/spec/declarations.html#lateinit-properties)):
/// only a `var`, only without an initializer, and never of a primitive type —
/// each of which is a message of its own in kotlinc.
fn check_lateinit(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    item: ItemId,
    property: &PropertyData,
    out: &mut Vec<DeclDiagnostic>,
) {
    if !property
        .modifiers
        .flags
        .contains(KotlinModifierFlags::LATEINIT)
    {
        return;
    }
    let range = name_range(db, file, tree, item);
    if !property.is_var {
        out.push(DeclDiagnostic::LateinitOnImmutableProperty { range });
        return;
    }
    if property.initializer_expr.is_some() || property.delegate_expr.is_some() {
        out.push(DeclDiagnostic::LateinitWithInitializer { range });
        return;
    }
    // A primitive-typed property's field cannot carry the null default
    // `lateinit` relies on: the type is the declared one, or the inferred one
    // ([`Self::check_lateinit`] reads what the type layer answers for the item).
    let ty = super::db::item_ty(db, file, item).flexible_lower(db);
    if let TyKind::Reference { name, .. } = ty.kind(db)
        && crate::kotlin::builtins::primitive_of(name.as_str()).is_some()
    {
        out.push(DeclDiagnostic::LateinitOnPrimitive { range });
    }
}

/// Where a declaration sits, for the modifier table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Function,
    Property { backing_field: bool, mutable: bool },
    Class(KotlinClassKind),
}

/// The declaration kind a class is, for [`Target`].
fn class_target(class: &ClassData) -> &'static str {
    match class.kind {
        KotlinClassKind::Class => "class",
        KotlinClassKind::Interface => "interface",
        KotlinClassKind::Enum => "enum class",
        KotlinClassKind::Annotation => "annotation class",
        KotlinClassKind::Object => "object",
        KotlinClassKind::CompanionObject => "companion object",
    }
}

/// How kotlinc names a declaration in a modifier message.
fn target_name(target: Target, location: Location) -> &'static str {
    match (target, location) {
        (Target::Function, Location::TopLevel) => "top level function",
        (Target::Function, Location::Member) => "member function",
        (Target::Function, Location::Local) => "local function",
        (
            Target::Property {
                backing_field: true,
                ..
            },
            Location::TopLevel,
        ) => "top level property with backing field",
        (
            Target::Property {
                backing_field: true,
                ..
            },
            Location::Member,
        ) => "member property with backing field",
        (
            Target::Property {
                backing_field: true,
                ..
            },
            Location::Local,
        ) => "local variable",
        (
            Target::Property {
                backing_field: false,
                ..
            },
            Location::TopLevel,
        ) => "top level property",
        (
            Target::Property {
                backing_field: false,
                ..
            },
            Location::Member,
        ) => "member property",
        (Target::Property { .. }, Location::Local) => "local variable",
        (Target::Class(KotlinClassKind::Class), _) => "class",
        (Target::Class(KotlinClassKind::Interface), _) => "interface",
        (Target::Class(KotlinClassKind::Enum), _) => "enum class",
        (Target::Class(KotlinClassKind::Annotation), _) => "annotation class",
        (Target::Class(KotlinClassKind::Object), _) => "object",
        (Target::Class(KotlinClassKind::CompanionObject), _) => "companion object",
    }
}

/// Whether a modifier is applicable to a declaration
/// ([KLS
/// `declarations.html#modifiers`](https://kotlinlang.org/spec/declarations.html#modifiers)
/// lists the `classModifier`/`memberModifier`/`functionModifier`/
/// `propertyModifier` sets each declaration form accepts).
///
/// The table is the subset each check was verified against with kotlinc 2.4.20,
/// so no message is invented: `data`/`value` are the `classModifier`s of a
/// plain `class`, `open`/`abstract`/`override` are declaration modifiers that
/// need a supertype, and `inline`/`suspend` belong to a function.
fn modifier_applies(modifier: &'static str, target: Target, location: Location) -> bool {
    match modifier {
        // A `classModifier`: `data`/`value`/`inner` are a plain `class`'s
        // ([KLS
        // `declarations.html#class-declaration`](https://kotlinlang.org/spec/declarations.html#class-declaration)
        // is the `classModifier` set), `enum`/`annotation` are the two forms
        // that spell them in the declaration itself (`enum class`,
        // `annotation class`) and are not applicable to any other classifier,
        // and `sealed` also applies to an `interface` (Kotlin 1.5's
        // `sealed interface`).
        "data" | "value" | "inner" => matches!(target, Target::Class(KotlinClassKind::Class)),
        "enum" => matches!(target, Target::Class(KotlinClassKind::Enum)),
        "annotation" => matches!(target, Target::Class(KotlinClassKind::Annotation)),
        "sealed" => matches!(
            target,
            Target::Class(KotlinClassKind::Class | KotlinClassKind::Interface)
        ),
        // `open`/`abstract` need a *supertype*: a top-level or local *member*
        // has none, while a classifier is extended from anywhere
        // ([KLS
        // `declarations.html#inheritance`](https://kotlinlang.org/spec/declarations.html#inheritance)).
        // `override` needs a supertype that declares the member, which the
        // override checks report the absence of.
        "open" | "abstract" => match target {
            Target::Class(KotlinClassKind::Class | KotlinClassKind::Interface) => true,
            Target::Class(_) => false,
            _ => matches!(location, Location::Member),
        },
        "override" => matches!(location, Location::Member),
        // A `functionModifier`.
        "inline" | "suspend" | "tailrec" | "operator" | "infix" | "external" => {
            matches!(target, Target::Function)
        }
        // A `propertyModifier` of a top-level or `object`-member `val`.
        "const" => {
            matches!(target, Target::Property { mutable: false, .. })
                && matches!(location, Location::TopLevel | Location::Member)
        }
        // `lateinit` has messages of its own ([`check_lateinit`]).
        "lateinit" => true,
        // Every other modifier (visibility, `vararg` on a parameter) is
        // accepted where it is written: the set is a subset, never a guess.
        _ => true,
    }
}

/// The modifier keyword a flag stands for, for the modifiers the table
/// recognizes.
fn flag_name(flag: KotlinModifierFlags) -> Option<&'static str> {
    Some(match flag {
        KotlinModifierFlags::DATA => "data",
        KotlinModifierFlags::VALUE => "value",
        KotlinModifierFlags::ENUM => "enum",
        KotlinModifierFlags::ANNOTATION => "annotation",
        KotlinModifierFlags::INNER => "inner",
        KotlinModifierFlags::OVERRIDE => "override",
        KotlinModifierFlags::INLINE => "inline",
        KotlinModifierFlags::SUSPEND => "suspend",
        KotlinModifierFlags::TAILREC => "tailrec",
        KotlinModifierFlags::OPERATOR => "operator",
        KotlinModifierFlags::INFIX => "infix",
        KotlinModifierFlags::EXTERNAL => "external",
        KotlinModifierFlags::CONST => "const",
        KotlinModifierFlags::LATEINIT => "lateinit",
        _ => return None,
    })
}

/// Whether a declaration writes a body: a function or accessor with one, a
/// property with an initializer or a delegate (`None` is a declaration the
/// interface's contract leaves to its implementors).
fn has_body(tree: &KotlinItemTree, item: ItemId) -> bool {
    match tree.data(item) {
        KotlinItemData::Function(function) => function.body.is_some(),
        KotlinItemData::Property(property) => {
            property.initializer_expr.is_some()
                || property.delegate_expr.is_some()
                || property
                    .accessors
                    .iter()
                    .any(|&accessor| match tree.data(accessor) {
                        KotlinItemData::Accessor(data) => data.body.is_some(),
                        _ => false,
                    })
        }
        KotlinItemData::Accessor(data) => data.body.is_some(),
        _ => false,
    }
}

/// Whether a property declares a *backing field*: an initializer, or a
/// delegated one ([KLS
/// `declarations.html#backing-fields`](https://kotlinlang.org/spec/declarations.html#backing-fields)
/// makes the field exist unless an accessor writes the value itself).
fn has_backing_field(tree: &KotlinItemTree, item: ItemId) -> bool {
    match tree.data(item) {
        KotlinItemData::Property(property) => {
            property.initializer_expr.is_some() || property.delegate_expr.is_some()
        }
        _ => false,
    }
}

/// Whether `modifiers` say the declaration is abstract ([KLS
/// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)).
fn is_abstract(modifiers: &KotlinModifiers) -> bool {
    matches!(modifiers.modality, KotlinModality::Abstract)
}

/// Whether a supertype member may be overridden. A Kotlin source member with no
/// written modality takes its container's — an interface member is `open`, a
/// class member `final` (<https://kotlinlang.org/docs/interfaces.html>) — and a
/// classfile member answers its own `ACC_FINAL`.
///
/// The model does not record whether a modality was *written*, so a `final`
/// spelled out on an interface member is read as `open`: permissive, never a
/// false `is final and cannot be overridden`.
fn overridable(db: &dyn TyDatabase, member: &Member) -> bool {
    match &member.target {
        MemberTarget::Kotlin { file, item } => {
            // The member's *own* file: an item id indexes the arena of the file
            // its declaration lowered in, and a supertype's member usually
            // lives in another one.
            let outer = hir::file_item_tree(db, *file);
            let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
                return false;
            };
            let modifiers = match tree.data(*item) {
                KotlinItemData::Function(function) => function.modifiers,
                KotlinItemData::Property(property) => property.modifiers,
                _ => return false,
            };
            match modifiers.modality {
                KotlinModality::Open | KotlinModality::Abstract => true,
                KotlinModality::Final => container_is_interface(tree, *item),
                KotlinModality::Sealed => false,
            }
        }
        MemberTarget::Java(method) => !method.is_final,
        MemberTarget::Builtin { .. } | MemberTarget::JavaField(_) => false,
    }
}

/// Whether the container of `item` is an interface (or an annotation class).
fn container_is_interface(tree: &KotlinItemTree, item: ItemId) -> bool {
    let Some(parent) = tree.parent_of(item) else {
        return false;
    };
    match tree.data(parent) {
        KotlinItemData::Class(class) => matches!(
            class.kind,
            KotlinClassKind::Interface | KotlinClassKind::Annotation
        ),
        _ => false,
    }
}

/// Whether a member is abstract: a Kotlin source declaration written `abstract`,
/// or a classfile member the classpath records `ACC_ABSTRACT` ([KLS
/// `declarations.html#abstract-classes`](https://kotlinlang.org/spec/declarations.html#abstract-classes)).
fn member_is_abstract(db: &dyn TyDatabase, member: &Member) -> bool {
    match &member.target {
        MemberTarget::Kotlin { file, item } => {
            let outer = hir::file_item_tree(db, *file);
            let Some(tree) = hir_def::kotlin::plugin::model(&outer) else {
                return false;
            };
            let modifiers = match tree.data(*item) {
                KotlinItemData::Function(function) => function.modifiers,
                KotlinItemData::Property(property) => property.modifiers,
                _ => return false,
            };
            // A member an *interface* declares without a body is abstract by
            // declaration ([KLS
            // `declarations.html#interface-declaration`](https://kotlinlang.org/spec/declarations.html#interface-declaration)).
            is_abstract(&modifiers)
                || (container_is_interface(tree, *item) && !has_body(tree, *item))
        }
        MemberTarget::Java(method) => method.abstract_,
        MemberTarget::Builtin { .. } | MemberTarget::JavaField(_) => false,
    }
}

/// The class a member's kind belongs to, for a signature comparison: a function
/// is a function, a property (with either accessor) a property. `None` for a
/// member no override can concern.
fn member_kind_class(member: &Member) -> Option<MemberKindClass> {
    match member.kind {
        MemberKind::Function => Some(MemberKindClass::Function),
        MemberKind::Property | MemberKind::Getter | MemberKind::Setter => {
            Some(MemberKindClass::Property)
        }
        MemberKind::Constructor => None,
    }
}

/// Whether a supertype member has the declaration's own signature — the same
/// name and the same parameter types ([KLS
/// `declarations.html#overriding`](https://kotlinlang.org/spec/declarations.html#overriding)).
fn signature_matches(db: &dyn TyDatabase, member: &Member, params: &[Ty]) -> bool {
    same_params(db, &member.params, params)
}

/// Whether two parameter lists are the same for a signature: the types compare
/// through the *lower* half of a platform type, so a Java member's `String!` is
/// the `String` a Kotlin declaration writes, and an unresolved type is
/// compatible with every other (a name this model cannot resolve must not
/// *also* report a mismatch).
fn same_params(db: &dyn TyDatabase, a: &[Ty], b: &[Ty]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_type(db, *a, *b))
}

/// [`same_params`] for one pair. A *platform* type stands for either half,
/// exactly as it does in a subtyping question: a classfile `equals(Object)` is
/// the `Any?` a Kotlin declaration writes, so `override fun equals(other:
/// Any?)` overrides it.
fn same_type(db: &dyn TyDatabase, a: Ty, b: Ty) -> bool {
    let halves = |ty: Ty| [ty, ty.flexible_lower(db), ty.flexible_upper(db)];
    if halves(a).iter().any(|a| halves(b).iter().any(|b| a == b)) {
        return true;
    }
    if matches!(a.kind(db), TyKind::Error) || matches!(b.kind(db), TyKind::Error) {
        return true;
    }
    // A type written in one scope and the same type written in another may
    // resolve to differently *scoped* variables; the rendered form is what a
    // signature comparison is about.
    display_kotlin(db, a).to_string() == display_kotlin(db, b).to_string()
}

/// The Kotlin spelling of a member, as kotlinc prints it in an override
/// message ([`DeclDiagnostic::OverridesNothing`]).
fn member_signature(db: &dyn TyDatabase, member: &Member) -> String {
    let params = member
        .params
        .iter()
        .map(|param| display_kotlin(db, *param).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "fun {}({params}): {}",
        member.name,
        display_kotlin(db, member.ty(db))
    )
}

/// The name a supertype is reported by: its simple one ([KLS
/// `packages-and-imports.html#qualified-name`](https://kotlinlang.org/spec/packages-and-imports.html#qualified-name)
/// makes the last segment the simple one).
fn supertype_name(db: &dyn TyDatabase, ty: &Ty) -> Name {
    match ty.kind(db) {
        TyKind::Reference { name, .. } => Name::new(name.simple_name()),
        _ => Name::new("<missing>"),
    }
}

/// The file's parse and id map, for a range lookup.
fn range_ctx(
    db: &dyn TyDatabase,
    file: FileId,
) -> Option<(&hir_expand::ast_id_map::AstIdMap, syntax::SourceFile)> {
    crate::java::range_ctx::range_ctx(db, file, LanguageKind::Kotlin)
}

/// The range of a declaration's own name.
fn name_range(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    item: ItemId,
) -> Option<TextRange> {
    let (map, source) = range_ctx(db, file)?;
    hir_def::kotlin::ranges::item_name_range(map, &source, tree, item)
}

/// The range of a whole declaration.
fn declaration_range(
    db: &dyn TyDatabase,
    file: FileId,
    tree: &KotlinItemTree,
    item: ItemId,
) -> Option<TextRange> {
    let (map, source) = range_ctx(db, file)?;
    hir_def::kotlin::ranges::item_range(map, &source, tree, item)
}
