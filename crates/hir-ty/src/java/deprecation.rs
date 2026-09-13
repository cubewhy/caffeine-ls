//! Deprecation detection
//! ([JLS §9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)).
//!
//! A *deprecated* element is a class or interface, method, constructor or
//! field whose declaration carries `@Deprecated`; with
//! `@Deprecated(forRemoval = true)` it is *terminally* deprecated. This module
//! answers, for a source or library element, whether it is deprecated and how
//! — the detection half of the rule. The report itself (the wording, the lint
//! key, the severity and the `@SuppressWarnings` scope) belongs to
//! `ide-diagnostics`, and the *use* sites that ask these questions live in
//! [`crate::java::infer`] and [`crate::java::name_check`].
//!
//! Library elements are read from the classfile `Deprecated` attribute
//! ([JVMS §4.7.15]) and from the `java.lang.Deprecated` annotation of the
//! `RuntimeVisibleAnnotations` attribute — javac writes both, so a
//! `forRemoval` library element is recognisable from either.
//!
//! Not covered, deliberately: a Javadoc-only `@deprecated` tag (§9.6.4.6
//! defines a deprecated element as one *annotated* `@Deprecated`; javac's
//! extra warning for the tag alone is its separate `[dep-ann]` lint, and the
//! crate models no doc-comment tags), deprecated packages and modules, an
//! `@Deprecated` *enum constant* (the lowered `EnumConstant` carries no
//! annotations) and one on a *record component* (its propagation to the
//! synthesized accessor has no item anchor), the implicit `super()` a
//! subclass's default constructor runs on a deprecated superclass constructor
//! (no call site exists to anchor it), and the preview-API warnings of §1.5,
//! whose rule set (reflective versus normal APIs, the module exemption,
//! disabled-means-error, the classfile `0xFFFF` minor version) is a distinct
//! one.

use hir_def::java::item_tree::{ItemAnnotationRef, ItemAnnotationValue, ItemData, ItemId};
use hir_expand::body::Literal;
use hir_expand::name::Name;
use syntax::stub::PrimitiveValue;
use vfs::FileId;

use crate::java::db::TyDatabase;
use crate::java::method::{MethodData, source_top_level, top_level_of};
use crate::java::resolve::{
    NameResolution, Resolver, item_data, resolve_name_checked, scope_for_file,
};
use crate::java::ty::Ty;

/// How a declaration is deprecated ([JLS §9.6.4.6]): a plain `@Deprecated`, or
/// `@Deprecated(forRemoval = true)` — *terminally* deprecated, whose use is a
/// warning javac reports by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deprecation {
    Ordinary,
    Terminal,
}

/// The one deprecated API a reference names, in the shape javac's message
/// quotes it: `{api} in {owner} has been deprecated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeprecatedApi {
    /// A class or interface. `name` is the class's simple name and `owner` is
    /// javac's *location* operand: the enclosing class's simple name for a
    /// nested class, the package for a top-level one (the empty name for the
    /// unnamed package).
    Class { name: Name, owner: Name },
    /// A method or constructor; `name` is `<init>` for a constructor and
    /// `varargs` records a variable-arity declaration, whose last parameter
    /// is the array its element type packs into.
    Method {
        owner: Name,
        name: Name,
        params: Vec<Ty>,
        varargs: bool,
    },
    /// A field.
    Field { owner: Name, name: Name },
}

impl DeprecatedApi {
    /// The declaring class of the member, or the class itself — the element
    /// whose *outermost class* the same-outermost-class exemption compares
    /// ([JLS §9.6.4.6]).
    pub(crate) fn owner(&self) -> &Name {
        match self {
            DeprecatedApi::Class { name, .. } => name,
            DeprecatedApi::Method { owner, .. } | DeprecatedApi::Field { owner, .. } => owner,
        }
    }
}

/// The `@Deprecated` annotation of `annotations`, resolved to
/// `java.lang.Deprecated` ([JLS §9.6.4.6]); `forRemoval = true` yields
/// [`Deprecation::Terminal`].
pub(crate) fn annotation_deprecation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    resolver: &Resolver,
    annotations: &[ItemAnnotationRef],
) -> Option<Deprecation> {
    for annotation in annotations {
        let NameResolution::Resolved(name) =
            resolve_name_checked(db, scope, resolver, &annotation.name)
        else {
            continue;
        };
        if name.as_str() != DEPRECATED {
            continue;
        }
        return Some(deprecation_of_args(&annotation.args));
    }
    None
}

/// The deprecation a `@Deprecated` argument list declares: `Terminal` when it
/// carries `forRemoval = true`, `Ordinary` otherwise
/// ([§9.6.4.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.6.4.6)).
fn deprecation_of_args(args: &[hir_def::java::item_tree::ItemAnnotationArg]) -> Deprecation {
    let for_removal = args.iter().any(|arg| {
        arg.name.as_str() == "forRemoval"
            && matches!(
                &arg.value,
                ItemAnnotationValue::Literal(Literal::Boolean(true))
            )
    });
    if for_removal {
        Deprecation::Terminal
    } else {
        Deprecation::Ordinary
    }
}

/// One reference to a deprecated element: the API javac's message names, how
/// it is deprecated, and the reference's own source range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeprecatedReference {
    pub api: DeprecatedApi,
    pub deprecation: Deprecation,
    pub range: Option<rowan::TextRange>,
}

/// Whether a reference to the deprecated `api` is exempt from the warning
/// ([JLS §9.6.4.6]): the use is inside a declaration that is itself
/// *ordinarily* deprecated, or the use and the element are within the same
/// outermost class.
///
/// `enclosing` is the deprecation in force at the use site (the enclosing
/// declaration's own `@Deprecated`, or the innermost enclosing declaration
/// that carries one) and `use_site_outermost` the outermost class of the
/// declaration the use sits in.
pub(crate) fn is_exempt(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    enclosing: Option<Deprecation>,
    use_site_outermost: Option<&Name>,
    api: &DeprecatedApi,
    deprecation: Deprecation,
) -> bool {
    // "The use is within an entity that is itself annotated with @Deprecated"
    // — javac exempts only the *ordinary* case: a `forRemoval` deprecation is
    // still reported inside a deprecated declaration.
    if deprecation == Deprecation::Ordinary && enclosing.is_some() {
        return true;
    }
    // "The use and declaration are both within the same outermost class."
    let Some(use_site) = use_site_outermost else {
        return false;
    };
    outermost_class(db, scope, api.owner()).as_ref() == Some(use_site)
}

/// The deprecation of one resolved member — a source declaration when
/// `declaration` names its item, a library member otherwise ([JLS §9.6.4.6]).
///
/// The *owner class*'s own deprecation is not reported here: javac reports it
/// from the written type reference at the site (the receiver's declared type,
/// the `new` expression's type, the `extends` clause), which the reference walk
/// already covers, and reports it once.
pub(crate) fn member_deprecation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    owner: &str,
    name: &str,
    declaration: Option<(FileId, ItemId)>,
    descriptor: Option<&str>,
) -> Option<Deprecation> {
    match declaration {
        Some((file, item)) => source_item(db, file, item),
        None => library_member(db, scope, owner, name, descriptor),
    }
}

/// The same test against a *library* annotation stub: its type name and its
/// `forRemoval` argument, in the `u32`-backed form `hir` exposes.
fn library_annotation_deprecation(
    db: &dyn TyDatabase,
    annotations: &[hir::AnnotationSig<hir::Symbol>],
) -> Option<Deprecation> {
    let interner = &db.hir_state().interner;
    for annotation in annotations {
        let is_deprecated = annotation
            .annotation_type
            .as_reference_name()
            .is_some_and(|name| interner.resolve(name) == DEPRECATED);
        if !is_deprecated {
            continue;
        }
        let for_removal = annotation.arguments.iter().any(|(name, value)| {
            interner.resolve(name) == "forRemoval"
                && matches!(
                    value,
                    hir::AnnotationValue::Primitive(PrimitiveValue::Boolean(true))
                )
        });
        return Some(if for_removal {
            Deprecation::Terminal
        } else {
            Deprecation::Ordinary
        });
    }
    None
}

/// The deprecation of the source declaration `item` of `file`, or `None` when
/// it is not deprecated ([JLS §9.6.4.6]) — and, for a snapshot of the
/// enclosing declarations, of every item of a file at once
/// ([`crate::java::db::deprecated_enclosing_query`]).
pub(crate) fn source_item(db: &dyn TyDatabase, file: FileId, item: ItemId) -> Option<Deprecation> {
    let tree = hir::file_item_tree(db, file);
    let data = item_data(&tree, item)?;
    let annotations = item_annotations(data);
    if annotations.is_empty() {
        return None;
    }
    let resolver = Resolver::for_item(db, file, &tree, item);
    annotation_deprecation(db, &scope_for_file(db, file), &resolver, annotations)
}

/// The `@Deprecated` annotations of one lowered declaration.
pub(crate) fn item_annotations(data: &ItemData) -> &[ItemAnnotationRef] {
    match data {
        ItemData::Class(d) | ItemData::Interface(d) => &d.annotations,
        ItemData::Enum(d) => &d.annotations,
        ItemData::Record(d) => &d.annotations,
        ItemData::Annotation(d) => &d.annotations,
        ItemData::Method(d) => &d.annotations,
        ItemData::Field(d) => &d.annotations,
        ItemData::Module(_) | ItemData::EnumConstant(_) => &[],
        ItemData::StaticInit(_) | ItemData::InstanceInit(_) => &[],
    }
}

/// The deprecation of the class `fqn` — a source class or a library class —
/// resolved in `scope` ([JLS §9.6.4.6]).
pub(crate) fn class_deprecation(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Option<Deprecation> {
    match hir::fqn_resolve(db, scope, fqn.as_str())? {
        hir::Resolved::Source(class) => source_item(db, class.file, class.item),
        hir::Resolved::Library(class) => {
            let record = hir::class_record(db, &class)?;
            let hir::ClassOrModuleRecord::Class(stub) = record.as_ref() else {
                return None;
            };
            if stub.deprecated {
                // The attribute carries no `forRemoval`; a terminal
                // deprecation is only visible through the annotation, which
                // javac writes alongside it.
                return Some(
                    library_annotation_deprecation(db, &stub.annotations)
                        .unwrap_or(Deprecation::Ordinary),
                );
            }
            library_annotation_deprecation(db, &stub.annotations)
        }
    }
}

/// Every deprecated class a written reference name denotes, innermost first
/// ([JLS §9.6.4.6]) — the class itself and each of its enclosing classes,
/// because javac reports the deprecated member types of a written qualified
/// name individually (`new Outer.Inner()` names both).
pub(crate) fn class_hits(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Vec<(Deprecation, DeprecatedApi)> {
    let mut out = Vec::new();
    for candidate in class_and_enclosing(fqn.as_str()) {
        let name = Name::new(candidate);
        let Some(deprecation) = class_deprecation(db, scope, &name) else {
            continue;
        };
        out.push((
            deprecation,
            DeprecatedApi::Class {
                name: Name::new(simple_class_name(candidate)),
                owner: class_owner(db, scope, &name),
            },
        ));
    }
    out
}

/// A class name and every name enclosing it, innermost first: `p.Outer.Inner`
/// yields itself and `p.Outer`; a library binary name nests with `$`
/// ([JVMS §4.2](https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.2)),
/// so `p.Outer$Inner` yields itself and `p.Outer`.
fn class_and_enclosing(fqn: &str) -> Vec<&str> {
    let mut out = vec![fqn];
    let mut current = fqn;
    loop {
        let next = match (current.rfind('$'), current.rfind('.')) {
            // A `$` after the last `.` is a nesting separator; one before it
            // belongs to the name itself and is not a class boundary.
            (Some(dollar), Some(dot)) if dollar > dot => &current[..dollar],
            (_, Some(dot)) => &current[..dot],
            _ => break,
        };
        if next.is_empty() {
            break;
        }
        out.push(next);
        current = next;
    }
    out
}

/// The deprecation of the library member `name` of `owner` (a binary FQN) —
/// `descriptor` the classfile descriptor the member was resolved by, or `None`
/// to match by name alone ([JVMS §4.5]/[§4.6]).
pub(crate) fn library_member(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    owner: &str,
    name: &str,
    descriptor: Option<&str>,
) -> Option<Deprecation> {
    let hir::Resolved::Library(class) = hir::fqn_resolve(db, scope, owner)? else {
        return None;
    };
    let record = hir::class_record(db, &class)?;
    let hir::ClassOrModuleRecord::Class(stub) = record.as_ref() else {
        return None;
    };
    let interner = &db.hir_state().interner;
    let matches = |member_name: &hir::Symbol, member_descriptor: &hir::Symbol| {
        interner.resolve(member_name) == name
            && descriptor.is_none_or(|want| interner.resolve(member_descriptor) == want)
    };
    let member_deprecation = |deprecated: bool,
                              annotations: &[hir::AnnotationSig<hir::Symbol>]|
     -> Option<Deprecation> {
        if deprecated {
            return Some(
                library_annotation_deprecation(db, annotations).unwrap_or(Deprecation::Ordinary),
            );
        }
        library_annotation_deprecation(db, annotations)
    };
    for method in &stub.methods {
        if matches(&method.name, &method.descriptor) {
            return member_deprecation(method.deprecated, &method.annotations);
        }
    }
    for field in &stub.fields {
        if matches(&field.name, &field.descriptor) {
            return member_deprecation(field.deprecated, &field.annotations);
        }
    }
    None
}

/// Javac's "in" operand for a class ([JLS §9.6.4.6]): the enclosing class's
/// simple name for a nested class, the package for a top-level one — the empty
/// name for the unnamed package.
pub(crate) fn class_owner(db: &dyn TyDatabase, scope: &hir::ResolutionScope, fqn: &Name) -> Name {
    let text = fqn.as_str();
    match hir::fqn_resolve(db, scope, text) {
        Some(hir::Resolved::Source(class)) => {
            let tree = hir::file_item_tree(db, class.file);
            source_owner(tree.package.as_ref(), text)
        }
        // A library class's *binary* name nests with `$` ([JVMS §4.2]), while
        // the reference may write the nesting with dots (`Legacy.Nested`), so
        // the resolution's own FQN — not the written name — decides the
        // "in" operand.
        Some(hir::Resolved::Library(class)) => {
            let binary = db.hir_state().interner.resolve(&class.entry.fqn);
            library_owner(binary)
        }
        // Unresolvable: the written name is all there is, and a dotted tail is
        // as good a nesting guess as any.
        None => library_owner(text),
    }
}

/// Javac's "in" operand for a *source* class name: `pkg.Outer.Inner` is owned
/// by `Outer`, `pkg.Top` by the package `pkg` (the empty name when unnamed).
fn source_owner(package: Option<&Name>, fqn: &str) -> Name {
    let rest = match package {
        Some(package) => fqn
            .strip_prefix(package.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            .unwrap_or(fqn),
        None => fqn,
    };
    match rest.split('.').collect::<Vec<_>>().as_slice() {
        [.., enclosing, _outermost] if rest.contains('.') => Name::new(enclosing),
        _ => package.cloned().unwrap_or_else(|| Name::new("")),
    }
}

/// Javac's "in" operand for a *library* binary name (`p.Outer$Inner` is owned
/// by `Outer`; `p.Top` by the package `p`).
fn library_owner(binary: &str) -> Name {
    let class_part = binary.rsplit('.').next().unwrap_or(binary);
    let segments: Vec<&str> = class_part.split('$').collect();
    if segments.len() > 1 {
        return Name::new(segments[segments.len() - 2]);
    }
    Name::new(binary.rsplit_once('.').map(|(pkg, _)| pkg).unwrap_or(""))
}

/// The simple name of a class in either naming: the last segment of a source
/// FQN or of a library binary name (`p.Outer$Inner` is `Inner`).
pub(crate) fn simple_class_name(fqn: &str) -> &str {
    let class_part = fqn.rsplit('.').next().unwrap_or(fqn);
    class_part.rsplit('$').next().unwrap_or(class_part)
}

/// The outermost class of the class `fqn` — the unit the same-outermost-class
/// exemption of [JLS §9.6.4.6] compares.
pub(crate) fn outermost_class(
    db: &dyn TyDatabase,
    scope: &hir::ResolutionScope,
    fqn: &Name,
) -> Option<Name> {
    match hir::fqn_resolve(db, scope, fqn.as_str()) {
        Some(hir::Resolved::Source(class)) => {
            let tree = hir::file_item_tree(db, class.file);
            let top = source_top_level(tree.package.as_ref().map(Name::as_str), fqn.as_str());
            Some(Name::new(&top))
        }
        _ => Some(Name::new(&top_level_of(fqn.as_str()))),
    }
}

/// The `DeprecatedApi` of a resolved method or constructor.
pub(crate) fn method_api(db: &dyn TyDatabase, data: &MethodData) -> DeprecatedApi {
    DeprecatedApi::Method {
        owner: data.owner.display_name(db),
        name: Name::new(&data.name),
        params: data.params.clone(),
        varargs: data.varargs,
    }
}

/// The `DeprecatedApi` of a resolved field.
pub(crate) fn field_api(
    db: &dyn TyDatabase,
    data: &crate::java::method::FieldData,
) -> DeprecatedApi {
    DeprecatedApi::Field {
        owner: data.owner.display_name(db),
        name: Name::new(&data.name),
    }
}

/// The fully qualified name of `java.lang.Deprecated`.
const DEPRECATED: &str = "java.lang.Deprecated";
