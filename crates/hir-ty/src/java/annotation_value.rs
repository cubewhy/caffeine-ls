//! The [§15.29] verdict of an annotation element value
//! ([JLS §9.7.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-9.html#jls-9.7.1)).
//!
//! §9.7.1 makes an element value a `ConditionalExpression`, and what the value
//! must *be* depends on the element's declared type: a constant expression
//! ([§15.29]) for a primitive or `String` element, a class literal ([§15.8.2])
//! for a `Class` element, an enum constant ([§8.9.1]) for an enum element, and
//! never `null`. This module decides the constant-expression half: whether an
//! expression of the file's [`BodyTree`] is a constant expression, and — for a
//! name denoting a variable — whether it denotes a *constant variable*
//! ([§4.12.4]).
//!
//! The layer deliberately stops where it cannot see: a name it cannot resolve
//! to a field, a source field whose initializer it has no body for, a library
//! constant whose classfile carries no `ConstantValue` attribute, a cycle of
//! field initializers. Those are [`ConstKind::Unknown`] — possibly constant,
//! never reported — so a check built on this module can only ever *miss*,
//! never falsely accuse.
//!
//! [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
//! [§15.29]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.29

use hir_def::java::item_tree::{ItemAnnotationValue, ItemData, ItemId};
use hir_expand::body::{BinaryOp, BodyTree, ExprData, ExprId, Literal, UnaryOp};
use hir_expand::name::Name;
use rustc_hash::FxHashSet;
use syntax::stub::{AnnotationValue as ClassfileValue, PrimitiveType, PrimitiveValue};
use vfs::FileId;

use crate::java::const_eval::{ArithOp, shift_mask, wrap_arith, wrap_divrem, wrap_ushr};
use crate::java::db::{TyDatabase, type_params_map_query};
use crate::java::method::{FieldData, access_context, pick_field};
use crate::java::resolve::{Resolver, candidate_fqns, resolve_type_ref, scope_for_file};
use crate::java::ty::{Ty, TyKind};

/// The deepest chain of field definitions a [§4.12.4] verdict follows before
/// giving up (a `final` field initialized by another `final` field, and so
/// on) — a cycle guard as much as a depth bound.
///
/// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
const MAX_FIELD_DEPTH: usize = 32;

/// The [§15.29] verdict of an annotation element value.
///
/// [§15.29]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.29
#[derive(Debug, Clone)]
pub(crate) enum ConstKind {
    /// A constant expression: its type, and the value of an integral
    /// constant, which the [§5.2] constant narrowing range-checks.
    ///
    /// `ty` is `None` for the rare conditional expression whose [§15.25] type
    /// this layer does not determine ([§15.29] still makes it a constant); the
    /// type is then not checked.
    ///
    /// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
    /// [§15.25]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25
    Constant { ty: Option<Ty>, int: Option<i64> },
    /// Not a constant expression ([§15.29]); `ty` is the value's type when
    /// this layer can determine it.
    NotConstant { ty: Option<Ty> },
    /// A value this layer cannot decide: a name it cannot resolve, a library
    /// constant without a `ConstantValue` attribute, a field-initializer
    /// cycle. Possibly constant, never reported.
    Unknown,
}

impl ConstKind {
    /// The value's type, when the layer determined one.
    pub(crate) fn ty(&self) -> Option<&Ty> {
        match self {
            ConstKind::Constant { ty, .. } | ConstKind::NotConstant { ty } => ty.as_ref(),
            ConstKind::Unknown => None,
        }
    }

    /// Whether the value is a constant expression ([§15.29]).
    ///
    /// [§15.29]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.29
    pub(crate) fn is_constant(&self) -> bool {
        matches!(self, ConstKind::Constant { .. })
    }

    /// The value §5.2's constant narrowing conversion works from: `Some` for a
    /// constant of type `byte`, `short`, `char` or `int` — exactly the source
    /// types the conversion accepts — and `None` otherwise, whatever the
    /// constant's integral value.
    ///
    /// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
    pub(crate) fn narrowing_value(&self, db: &dyn TyDatabase) -> Option<i64> {
        let ConstKind::Constant { ty, int } = self else {
            return None;
        };
        let value = (*int)?;
        let ty = ty.as_ref()?;
        is_narrowing_source(ty, db).then_some(value)
    }
}

/// The site an element value is evaluated at: the file and owning item its
/// expressions were lowered into, plus the resolution context of that item.
pub(crate) struct ValueCtx<'a> {
    pub db: &'a dyn TyDatabase,
    pub file: FileId,
    pub item: ItemId,
    pub scope: &'a hir::ResolutionScope,
    pub resolver: &'a Resolver,
    pub bodies: &'a BodyTree,
}

/// The verdict of a *name* element value — a simple name referring to a field
/// ([§6.5.6.1]), a qualified name whose qualifier is a type name
/// ([§6.5.6.2]) or a simple name bound by a static import ([§7.5.4]).
///
/// [§6.5.6.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.6.1
/// [§6.5.6.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.6.2
/// [§7.5.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-7.html#jls-7.5.4
#[derive(Debug, Clone)]
pub(crate) enum NameKind {
    /// The name denotes a field, with the field's [§4.12.4] verdict.
    ///
    /// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
    Field(ConstKind),
    /// The name denotes no *accessible* field: the caller reports
    /// `Cannot resolve symbol` ([§6.5.6.1]).
    Unresolved,
    /// The qualifier is not a type name — `this`, `super` or a variable — so
    /// the qualified form is not a qualified name of [§6.5.6.2] and denotes no
    /// field. Never returned for a simple name.
    NotQualified,
}

/// The verdict of one item-tree element value — the declaration path's
/// ([`super::annotation_check`]) form of §9.7.1's value checks.
pub(crate) fn value_kind(cx: &ValueCtx<'_>, value: &ItemAnnotationValue) -> ConstKind {
    let mut visited = FxHashSet::default();
    item_kind(cx, value, &mut visited)
}

/// The verdict of one *spanned* element value — the body path's form, whose
/// values are lowered the same way but keep their source ranges.
pub(crate) fn ranged_value_kind(
    cx: &ValueCtx<'_>,
    value: &hir_expand::span::AnnotationValue,
) -> ConstKind {
    let mut visited = FxHashSet::default();
    ranged_kind(cx, value, &mut visited)
}

/// The verdict of a name element value ([§6.5.6.1], [§6.5.6.2], [§7.5.4]): the
/// name is resolved to a field and [§4.12.4] decides whether the field is a
/// *constant variable*.
pub(crate) fn name_kind(cx: &ValueCtx<'_>, qualifier: Option<&Name>, member: &Name) -> NameKind {
    let mut visited = FxHashSet::default();
    name_verdict(cx, qualifier, member, &mut visited)
}

fn item_kind(
    cx: &ValueCtx<'_>,
    value: &ItemAnnotationValue,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> ConstKind {
    match value {
        ItemAnnotationValue::Literal(literal) => literal_kind(cx.db, literal),
        ItemAnnotationValue::EnumConstant { qualifier, member } => {
            match name_verdict(cx, qualifier.as_ref(), member, visited) {
                NameKind::Field(kind) => kind,
                _ => ConstKind::Unknown,
            }
        }
        // §15.29: a class literal is a reference value, not a constant
        // expression — the list has only literals of primitive type and of
        // type `String`.
        ItemAnnotationValue::ClassLit(_) => ConstKind::NotConstant {
            ty: Some(class_ty(cx.db)),
        },
        // §15.29: a nested annotation values an annotation type, not a
        // primitive or `String`.
        ItemAnnotationValue::Annotation(inner) => match type_of_name(cx, &inner.name) {
            Some(ty) => ConstKind::NotConstant { ty: Some(ty) },
            None => ConstKind::Unknown,
        },
        ItemAnnotationValue::Array(_) => ConstKind::NotConstant { ty: None },
        ItemAnnotationValue::Expr(expr) => expr_kind(cx, *expr, visited),
        // A value of a *written type* ([§9.7.4]), whose expression arena does
        // not exist.
        ItemAnnotationValue::Unresolved { .. } => ConstKind::Unknown,
    }
}

fn ranged_kind(
    cx: &ValueCtx<'_>,
    value: &hir_expand::span::AnnotationValue,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> ConstKind {
    use hir_expand::span::AnnotationValue;
    match value {
        AnnotationValue::Literal(literal) => literal_kind(cx.db, literal),
        AnnotationValue::EnumConstant { qualifier, member } => {
            match name_verdict(cx, qualifier.as_ref(), member, visited) {
                NameKind::Field(kind) => kind,
                _ => ConstKind::Unknown,
            }
        }
        AnnotationValue::ClassLit(_) => ConstKind::NotConstant {
            ty: Some(class_ty(cx.db)),
        },
        AnnotationValue::Annotation(inner) => match type_of_name(cx, &inner.name.name) {
            Some(ty) => ConstKind::NotConstant { ty: Some(ty) },
            None => ConstKind::Unknown,
        },
        AnnotationValue::Array(_) => ConstKind::NotConstant { ty: None },
        AnnotationValue::Expr(expr) => expr_kind(cx, *expr, visited),
        AnnotationValue::Unresolved { .. } => ConstKind::Unknown,
    }
}

// --- expressions ([§15]) -----------------------------------------------------

/// The verdict of one expression of the file's body tree.
///
/// [§15]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html
fn expr_kind(
    cx: &ValueCtx<'_>,
    expr: ExprId,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> ConstKind {
    match cx.bodies.expr(expr).clone() {
        // §15.29: a literal of primitive type or of type `String`.
        ExprData::Literal(literal) => literal_kind(cx.db, &literal),
        // §15.29/§3.10.8: the null literal is not a constant expression (the
        // list has only literals of primitive type and of type `String`).
        ExprData::Null => ConstKind::NotConstant {
            ty: Some(Ty::null(cx.db)),
        },
        // §15.8.5: parentheses do not change what the expression is.
        ExprData::Paren(inner) => expr_kind(cx, inner, visited),
        // §15.29: "casts to primitive types and casts to `String`" are among
        // the forms a constant expression is composed of.
        ExprData::Cast { ty, expr } => {
            let target = resolve_type_ref(cx.db, cx.scope, cx.resolver, &ty.ty);
            if !is_primitive_or_string(&target, cx.db) {
                return ConstKind::NotConstant { ty: Some(target) };
            }
            match expr_kind(cx, expr, visited) {
                ConstKind::Constant { int, .. } => {
                    let int = match (int, target.kind(cx.db)) {
                        (Some(value), TyKind::Primitive(primitive)) => {
                            narrowing_value(&target, cx.db, narrow_value(value, *primitive))
                        }
                        (value, _) if is_string(&target, cx.db) => value,
                        _ => None,
                    };
                    ConstKind::Constant {
                        ty: Some(target),
                        int,
                    }
                }
                ConstKind::NotConstant { .. } => ConstKind::NotConstant { ty: Some(target) },
                ConstKind::Unknown => ConstKind::Unknown,
            }
        }
        // §15.15.3/§15.15.4/§15.15.5: unary numeric promotion, negation and
        // bitwise complement, all of which §15.29 admits; §15.15.6: logical
        // complement.
        ExprData::Unary { op, expr } => {
            let operand = expr_kind(cx, expr, visited);
            match op {
                UnaryOp::Plus => unary_kind(cx, &operand, |value| value),
                UnaryOp::Minus => unary_kind(cx, &operand, |value| value.wrapping_neg()),
                UnaryOp::BitNot => unary_kind(cx, &operand, |value| !value),
                // §15.15.6: the logical complement of a `boolean` constant.
                UnaryOp::Not => match &operand {
                    ConstKind::Constant { ty, int }
                        if ty.as_ref().is_some_and(|ty| is_boolean(ty, cx.db)) =>
                    {
                        ConstKind::Constant {
                            ty: ty.clone(),
                            int: int.map(|value| (value == 0) as i64),
                        }
                    }
                    ConstKind::Unknown => ConstKind::Unknown,
                    // Anything else is not a `boolean` constant; the value's
                    // own verdict is not preserved (`!x` is not a constant of
                    // a type this layer tracks).
                    _ => ConstKind::NotConstant { ty: None },
                },
                // §15.29: `++` and `--` are assignments, never constant
                // expressions; the operand's type stands as the value's.
                UnaryOp::Inc | UnaryOp::Dec => demote(operand),
            }
        }
        // §15.14.2/§15.14.3: postfix `++`/`--` likewise.
        ExprData::Postfix { expr, .. } => demote(expr_kind(cx, expr, visited)),
        // §15.17–§15.24: the binary operators, of which §15.29 admits the
        // arithmetic, shift, relational, equality, bitwise and logical ones.
        ExprData::Binary { op, lhs, rhs } => {
            let left = expr_kind(cx, lhs, visited);
            let right = expr_kind(cx, rhs, visited);
            binary_kind(cx, op, &left, &right)
        }
        // §15.25/§15.29: the conditional operator is a constant expression
        // when its condition is a `boolean` constant *and both branches* are
        // constant expressions of primitive or `String` type — even though
        // only one branch contributes the value.
        ExprData::Conditional { cond, then, els } => {
            let condition = expr_kind(cx, cond, visited);
            let then_arm = expr_kind(cx, then, visited);
            let else_arm = expr_kind(cx, els, visited);
            conditional_kind(cx, &condition, &then_arm, &else_arm)
        }
        // §15.8.3/§15.8.4: `this`/`super` denote objects, not constants.
        ExprData::This { .. } | ExprData::Super { .. } => ConstKind::NotConstant { ty: None },
        // §15.8.2: a class literal values a `Class` object, not a constant.
        ExprData::ClassLit(_) => ConstKind::NotConstant {
            ty: Some(class_ty(cx.db)),
        },
        // §6.5.6.1: a simple name. This layer tracks no locals, so the only
        // reading it can answer is the field one — a local of the same name
        // would *shadow* a field, and refusing to answer would lose the
        // `@Ann(i = K)` of a constant field entirely.
        ExprData::Var(name) => match name_verdict(cx, None, &name, visited) {
            NameKind::Field(kind) => kind,
            // A name this layer cannot resolve: possibly constant.
            NameKind::Unresolved => ConstKind::Unknown,
            NameKind::NotQualified => ConstKind::NotConstant { ty: None },
        },
        ExprData::NamePath(name) => {
            let resolved = match qualified_parts(&name) {
                Some((qualifier, member)) => name_verdict(cx, Some(&qualifier), &member, visited),
                None => name_verdict(cx, None, &name, visited),
            };
            match resolved {
                NameKind::Field(kind) => kind,
                NameKind::Unresolved => ConstKind::Unknown,
                NameKind::NotQualified => ConstKind::NotConstant { ty: None },
            }
        }
        // §15.11: `expr.name` — a qualified name of [§6.5.6.2] (`Type.NAME`,
        // whose qualifier denotes a type) is the only form that can denote a
        // constant variable; anything else (`.this`, `.super`, a variable
        // qualifier, an array's `.length`) is not a constant expression.
        ExprData::FieldAccess { target, name } => {
            let resolved = match target {
                Some(target) => match qualifier_type_name(cx, target) {
                    Some(qualifier) => name_verdict(cx, Some(&qualifier), &name, visited),
                    None => return ConstKind::NotConstant { ty: None },
                },
                None => name_verdict(cx, None, &name, visited),
            };
            match resolved {
                NameKind::Field(kind) => kind,
                NameKind::Unresolved => ConstKind::Unknown,
                NameKind::NotQualified => ConstKind::NotConstant { ty: None },
            }
        }
        // §15.12: a method invocation may complete abruptly and is not a
        // constant expression ([§15.29]).
        ExprData::MethodCall { .. } => ConstKind::NotConstant { ty: None },
        // §15.9/§15.10: an instance or array creation is not a constant
        // expression; its type is the type it creates.
        ExprData::New { ty, .. } | ExprData::NewArray { ty, .. } => ConstKind::NotConstant {
            ty: Some(resolve_type_ref(cx.db, cx.scope, cx.resolver, &ty.ty)),
        },
        // Everything else — an assignment ([§15.26]), an array access
        // ([§15.13]), an array initializer ([§10.6]), `instanceof` ([§15.20]),
        // a lambda ([§15.27]), a method reference ([§15.13]), an explicit
        // constructor invocation, a switch expression ([§15.28]), a string
        // template, a missing expression — is not a constant expression.
        ExprData::Assign { .. }
        | ExprData::ArrayAccess { .. }
        | ExprData::ArrayInit(_)
        | ExprData::InstanceOf { .. }
        | ExprData::CtorCall { .. }
        | ExprData::Lambda { .. }
        | ExprData::MethodRef { .. }
        | ExprData::Switch { .. }
        | ExprData::Template { .. }
        | ExprData::Missing => ConstKind::NotConstant { ty: None },
    }
}

/// The verdict of a unary operator applied to `operand`: §15.29 admits unary
/// `+`, `-`, `~` and `!`, and [§5.6.1]'s unary numeric promotion fixes the
/// result's type (`byte`, `short` and `char` promote to `int`).
///
/// [§5.6.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1
fn unary_kind(cx: &ValueCtx<'_>, operand: &ConstKind, apply: impl Fn(i64) -> i64) -> ConstKind {
    // The promotion applies to the operand's type, whatever its verdict.
    match operand {
        ConstKind::Constant { ty, int } => {
            let promoted = ty.as_ref().map(|ty| unary_numeric_promotion(cx.db, ty));
            let int = match (int, promoted.as_ref()) {
                (Some(value), Some(ty)) => narrowing_value(ty, cx.db, apply(*value)),
                _ => None,
            };
            ConstKind::Constant { ty: promoted, int }
        }
        ConstKind::NotConstant { ty } => ConstKind::NotConstant {
            ty: ty.as_ref().map(|ty| unary_numeric_promotion(cx.db, ty)),
        },
        ConstKind::Unknown => ConstKind::Unknown,
    }
}

/// The verdict of a binary operator ([§15.17]–[§15.24]).
///
/// [§15.24]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.24
fn binary_kind(cx: &ValueCtx<'_>, op: BinaryOp, left: &ConstKind, right: &ConstKind) -> ConstKind {
    let db = cx.db;
    // §15.18.1: `+` with a `String` operand is string concatenation, and its
    // result is a `String` — a form §15.29 admits, so the concatenation of two
    // constants is a constant.
    let concatenation = op == BinaryOp::Add
        && (left.ty().is_some_and(|ty| is_string(ty, db))
            || right.ty().is_some_and(|ty| is_string(ty, db)));
    let ty = if concatenation {
        Some(string_ty(db))
    } else {
        result_ty(db, op, left.ty(), right.ty())
    };
    if !(left.is_constant() && right.is_constant()) {
        return ConstKind::NotConstant { ty };
    }
    let Some(target) = ty else {
        return ConstKind::Constant {
            ty: None,
            int: None,
        };
    };
    // §15.20–§15.24: the relational, equality and logical operators yield a
    // `boolean`; a `String` concatenation yields a `String`. Neither carries
    // the integral value the narrowing conversion needs — but a `boolean`
    // result's own value is recorded (`0`/`1`), which the conditional
    // operator's branch selection reads.
    if is_boolean(&target, db) || is_string(&target, db) {
        let int = boolean_fold(op, int_constant(left), int_constant(right));
        return ConstKind::Constant {
            ty: Some(target),
            int,
        };
    }
    // An integral result: fold at the promoted width (§5.6.2/§15.18.2/§15.19).
    // The operators left here are exactly the integral ones, so a `None` fold
    // is a division or remainder by a zero divisor — an expression that does
    // not complete normally, which §15.29 excludes.
    match (int_constant(left), int_constant(right)) {
        (Some(left), Some(right)) => match fold(db, op, &target, left, right) {
            Some(value) => ConstKind::Constant {
                int: narrowing_value(&target, db, value),
                ty: Some(target),
            },
            None => ConstKind::NotConstant { ty: Some(target) },
        },
        // An operand whose integral value this layer does not know — the
        // expression is still a constant (its type says so), but its value is
        // not recorded.
        _ => ConstKind::Constant {
            ty: Some(target),
            int: None,
        },
    }
}

/// The verdict of a conditional expression ([§15.25], [§15.29]).
///
/// [§15.25]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25
fn conditional_kind(
    cx: &ValueCtx<'_>,
    condition: &ConstKind,
    then_arm: &ConstKind,
    else_arm: &ConstKind,
) -> ConstKind {
    let db = cx.db;
    let boolean_condition =
        condition.ty().is_some_and(|ty| is_boolean(ty, db)) && condition.is_constant();
    if !boolean_condition || !(then_arm.is_constant() && else_arm.is_constant()) {
        return ConstKind::NotConstant { ty: None };
    }
    // §15.25's constant cases: a numeric pair by binary numeric promotion, a
    // `boolean` pair, a `String` pair. Any other pair has a reference type
    // whose computation this layer does not do — recorded as a constant of
    // unknown type, so the type is simply not checked.
    let ty = match (then_arm.ty(), else_arm.ty()) {
        (Some(then_ty), Some(else_ty)) => numeric_promotion(db, then_ty, else_ty).or_else(|| {
            (is_boolean(then_ty, db) && is_boolean(else_ty, db))
                .then(|| Ty::primitive(db, PrimitiveType::Boolean))
        }),
        _ => None,
    };
    // §15.25/§15.29: the value is the *taken* branch's, and the condition is a
    // `boolean` constant whose value this layer records.
    let taken = match condition {
        ConstKind::Constant { int: Some(0), .. } => else_arm,
        _ => then_arm,
    };
    let int = match (&ty, int_constant(taken)) {
        (Some(ty), Some(value)) => narrowing_value(ty, db, value),
        _ => None,
    };
    ConstKind::Constant { ty, int }
}

// --- names ([§6.5.6], [§4.12.4]) --------------------------------------------

/// The verdict of a name element value ([§6.5.6.1], [§6.5.6.2], [§7.5.4]): the
/// name is resolved to a field and [§4.12.4] decides whether the field is a
/// *constant variable*.
///
/// A simple name (`qualifier` is `None`) is looked up in every class-like
/// declaration enclosing the value's item, innermost first, and then in the
/// declaring type of every static import that could bind it ([§7.5.4]). A
/// qualified name's qualifier must denote a type ([§6.5.6.2]); when it does
/// not, the value is not a qualified name at all ([`NameKind::NotQualified`]).
/// Field lookup is access-checked ([§6.6]), so a name that denotes no
/// accessible field is [`NameKind::Unresolved`] — the caller reports `Cannot
/// resolve symbol`.
///
/// [§6.5.6]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.6
/// [§6.6]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.6
/// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
fn name_verdict(
    cx: &ValueCtx<'_>,
    qualifier: Option<&Name>,
    member: &Name,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> NameKind {
    let db = cx.db;
    let ctx = access_context(db, cx.file, cx.item);
    let Some(qualifier) = qualifier else {
        // §6.5.6.1: the enclosing class-like declarations, innermost first —
        // the item's own class, then each enclosing one — and then the
        // declaring type of a static import that could bind the name
        // ([§7.5.4]).
        let mut candidates: Vec<Name> = Vec::new();
        if let Some(own) = hir::source_class_fqn(db, cx.file, cx.item) {
            candidates.push(own);
        }
        candidates.extend(cx.resolver.enclosing().iter().cloned());
        for (owner, _) in cx.resolver.static_import_owners(member.as_str()) {
            candidates.push(owner);
        }
        for fqn in candidates {
            let receiver = Ty::reference(db, fqn, Vec::new());
            if let Some(field) = pick_field(db, cx.scope, &receiver, member.as_str(), &ctx) {
                return NameKind::Field(field_kind(cx, &field, visited));
            }
        }
        return NameKind::Unresolved;
    };
    // §6.5.6.2: `TypeName.Identifier` — the qualifier must denote a type.
    let Some(receiver) = type_of_name(cx, qualifier) else {
        return NameKind::NotQualified;
    };
    match pick_field(db, cx.scope, &receiver, member.as_str(), &ctx) {
        Some(field) => NameKind::Field(field_kind(cx, &field, visited)),
        None => NameKind::Unresolved,
    }
}

/// The [§4.12.4] verdict of a resolved field: a *constant variable* — a
/// `final` variable of primitive type or of type `String`, initialized with a
/// constant expression — is a constant; anything else is not.
///
/// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
fn field_kind(
    cx: &ValueCtx<'_>,
    field: &FieldData,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> ConstKind {
    let db = cx.db;
    let not_constant = ConstKind::NotConstant {
        ty: Some(field.ty.clone()),
    };
    if !field.is_final || !is_constant_variable_type(&field.ty, db) {
        return not_constant;
    }
    match field_constant(cx, field, visited) {
        // The field's initializer *is* a constant expression, so the field is
        // a constant variable ([§4.12.4]) of its own declared type.
        FieldValue::Constant(value) => ConstKind::Constant {
            int: value.and_then(|value| narrowing_value(&field.ty, db, value)),
            ty: Some(field.ty.clone()),
        },
        FieldValue::Unreadable => not_constant,
    }
}

/// What this layer could read of a `final` field's constant value
/// ([§4.12.4]).
///
/// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
enum FieldValue {
    /// The field's initializer *is* a constant expression, with its value when
    /// the constant is integral — a `String`, `float`, `double` or `boolean`
    /// constant carries none.
    Constant(Option<i64>),
    /// The constant value cannot be read: a blank final, a classfile without a
    /// `ConstantValue` attribute, a field-initializer cycle.
    Unreadable,
}

/// The constant value a `final` field was initialized with — a source field's
/// initializer expression or a library field's classfile `ConstantValue`
/// attribute ([JVMS §4.7.2], emitted exactly for the fields javac compiled as
/// constant variables).
///
/// [JVMS §4.7.2]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.7.2
fn field_constant(
    cx: &ValueCtx<'_>,
    field: &FieldData,
    visited: &mut FxHashSet<(FileId, ItemId)>,
) -> FieldValue {
    let db = cx.db;
    let (Some(file), Some(item)) = (field.owner_file, field.decl_item) else {
        // A library field: its declaring class's stub carries the constant,
        // matched by the classfile descriptor ([JVMS §4.5]).
        let Some(descriptor) = field.descriptor.as_deref() else {
            return FieldValue::Unreadable;
        };
        let Some(hir::Resolved::Library(resolved)) =
            hir::fqn_resolve(db, cx.scope, field.owner.as_str())
        else {
            return FieldValue::Unreadable;
        };
        let Some(record) = hir::class_record(db, &resolved) else {
            return FieldValue::Unreadable;
        };
        let hir::ClassOrModuleRecord::Class(class) = record.as_ref() else {
            return FieldValue::Unreadable;
        };
        let Some(stub) = class
            .fields
            .iter()
            .find(|stub| db.hir_state().interner.resolve(&stub.descriptor) == descriptor)
        else {
            return FieldValue::Unreadable;
        };
        let Some(value) = stub.constant_value.as_ref() else {
            return FieldValue::Unreadable;
        };
        return FieldValue::Constant(library_int(value));
    };
    // The recursion guard is a *path*, not a set of everything seen: a field
    // whose initializer this evaluation is already inside is the cycle, while
    // a sibling reference to the same field (`K + K`) is not.
    if !visited.insert((file, item)) || visited.len() > MAX_FIELD_DEPTH {
        return FieldValue::Unreadable;
    }
    let tree = hir::file_item_tree(db, file);
    let ItemData::Field(data) = tree.data(item) else {
        return FieldValue::Unreadable;
    };
    let Some(initializer) = data.initializer_expr else {
        visited.remove(&(file, item));
        return FieldValue::Unreadable;
    };
    let scope = scope_for_file(db, file);
    let type_params = type_params_map_query(db, db.file_text(file));
    let resolver = Resolver::new(&tree, type_params, item);
    let bodies = hir::file_body_tree(db, file);
    let nested = ValueCtx {
        db,
        file,
        item,
        scope: &scope,
        resolver: &resolver,
        bodies: &bodies,
    };
    let value = match expr_kind(&nested, initializer, visited) {
        ConstKind::Constant { int, .. } => FieldValue::Constant(int),
        _ => FieldValue::Unreadable,
    };
    visited.remove(&(file, item));
    value
}

/// The int value of a classfile constant ([JVMS §4.4]) — the integral
/// `ConstantValue` kinds [§5.2]'s narrowing conversion range-checks.
///
/// [JVMS §4.4]: https://docs.oracle.com/javase/specs/jvms/se26/html/jvms-4.html#jvms-4.4
/// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
fn library_int(value: &ClassfileValue<hir::Symbol>) -> Option<i64> {
    let ClassfileValue::Primitive(primitive) = value else {
        return None;
    };
    match primitive {
        PrimitiveValue::Int(v) => Some(*v as i64),
        PrimitiveValue::Byte(v) => Some(*v as i64),
        PrimitiveValue::Short(v) => Some(*v as i64),
        PrimitiveValue::Char(v) => Some(*v as i64),
        PrimitiveValue::Long(_)
        | PrimitiveValue::Float(_)
        | PrimitiveValue::Double(_)
        | PrimitiveValue::Boolean(_)
        | PrimitiveValue::Void => None,
    }
}

// --- types ------------------------------------------------------------------

/// The `java.lang.Class` type a class literal values ([§15.8.2]).
///
/// [§15.8.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.8.2
fn class_ty(db: &dyn TyDatabase) -> Ty {
    Ty::reference(db, "java.lang.Class", Vec::new())
}

/// The `java.lang.String` type ([§4.3.3]).
///
/// [§4.3.3]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3.3
fn string_ty(db: &dyn TyDatabase) -> Ty {
    Ty::reference(db, "java.lang.String", Vec::new())
}

/// The [`Ty`] of an annotation element literal ([JLS §15.29]).
///
/// [§15.29]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.29
pub(crate) fn literal_ty(db: &dyn TyDatabase, literal: &Literal) -> Ty {
    match literal {
        Literal::Int(_) => Ty::primitive(db, PrimitiveType::Int),
        Literal::Long(_) => Ty::primitive(db, PrimitiveType::Long),
        Literal::Float => Ty::primitive(db, PrimitiveType::Float),
        Literal::Double => Ty::primitive(db, PrimitiveType::Double),
        Literal::Boolean(_) => Ty::primitive(db, PrimitiveType::Boolean),
        Literal::Char(_) => Ty::primitive(db, PrimitiveType::Char),
        Literal::Str(_) => string_ty(db),
    }
}

/// The verdict of a literal ([§3.10]): always a constant expression, its
/// integral value recorded for [§5.2]'s narrowing conversion and its
/// `boolean` value as `0`/`1`, which the conditional operator needs to
/// pick its branch.
///
/// [§3.10]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.10
fn literal_kind(db: &dyn TyDatabase, literal: &Literal) -> ConstKind {
    let ty = literal_ty(db, literal);
    let int = match literal {
        Literal::Int(value) | Literal::Long(value) => narrowing_value(&ty, db, *value),
        // §3.10.4: a character literal's constant value is its code point.
        Literal::Char(value) => narrowing_value(&ty, db, *value as i64),
        // §3.10.3: a boolean literal's value, as the conditional operator's
        // selector reads it.
        Literal::Boolean(value) => Some(*value as i64),
        Literal::Float | Literal::Double | Literal::Str(_) => None,
    };
    ConstKind::Constant { ty: Some(ty), int }
}

/// Whether `ty` is a constant expression's *type*: a primitive type or
/// `String` ([§9.7.1]'s commensurability requirement).
pub(crate) fn is_primitive_or_string(ty: &Ty, db: &dyn TyDatabase) -> bool {
    matches!(ty.kind(db), TyKind::Primitive(primitive) if *primitive != PrimitiveType::Void)
        || is_string(ty, db)
}

/// Whether a `final` variable of this type may be a constant variable
/// ([§4.12.4]: "of type `String` or a primitive type").
///
/// [§4.12.4]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4
fn is_constant_variable_type(ty: &Ty, db: &dyn TyDatabase) -> bool {
    is_primitive_or_string(ty, db)
}

/// Whether `ty` is `java.lang.String` ([§4.3.3]).
///
/// [§4.3.3]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3.3
pub(crate) fn is_string(ty: &Ty, db: &dyn TyDatabase) -> bool {
    matches!(ty.kind(db), TyKind::Reference { name, .. } if name.as_str() == "java.lang.String")
}

/// Whether `ty` is `java.lang.Class` ([§4.3.2]).
///
/// [§4.3.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.3.2
pub(crate) fn is_class(ty: &Ty, db: &dyn TyDatabase) -> bool {
    matches!(ty.kind(db), TyKind::Reference { name, .. } if name.as_str() == "java.lang.Class")
}

/// Whether `ty` is the primitive type `boolean` ([§4.2.5]).
///
/// [§4.2.5]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.2.5
fn is_boolean(ty: &Ty, db: &dyn TyDatabase) -> bool {
    matches!(ty.kind(db), TyKind::Primitive(PrimitiveType::Boolean))
}

/// The value §5.2's constant narrowing works from: the *integral* constant of
/// a `byte`, `short`, `char` or `int` typed constant. `None` for every other
/// type — the narrowing conversion accepts exactly these four source types.
///
/// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
fn narrowing_value(ty: &Ty, db: &dyn TyDatabase, value: i64) -> Option<i64> {
    is_narrowing_source(ty, db).then_some(value)
}

/// Whether a constant of type `ty` may narrow by §5.2's constant narrowing
/// conversion ([§5.2]: "of type byte, short, char, or int").
///
/// [§5.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.2
fn is_narrowing_source(ty: &Ty, db: &dyn TyDatabase) -> bool {
    matches!(
        ty.kind(db),
        TyKind::Primitive(
            PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char | PrimitiveType::Int
        )
    )
}

/// The value a constant records: the value of an integral constant — of any
/// width, `byte` through `long` — and `0`/`1` for a `boolean` constant. `None`
/// for a `float`, `double` or `String` constant, whose value this layer does
/// not carry.
fn int_constant(kind: &ConstKind) -> Option<i64> {
    match kind {
        ConstKind::Constant { int, .. } => *int,
        _ => None,
    }
}

/// The value of a `boolean`-valued binary operator ([§15.20.1], [§15.21.2],
/// [§15.22.2], [§15.23], [§15.24]), as `0`/`1`. `None` when the operands'
/// values are not both known — a `String` comparison, or a comparison whose
/// operands carry no value.
///
/// [§15.20.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.20.1
/// [§15.21.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.21.2
/// [§15.22.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.22.2
/// [§15.23]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.23
/// [§15.24]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.24
fn boolean_fold(op: BinaryOp, left: Option<i64>, right: Option<i64>) -> Option<i64> {
    use BinaryOp::*;
    let (left, right) = (left?, right?);
    let value = match op {
        Lt => left < right,
        Gt => left > right,
        Le => left <= right,
        Ge => left >= right,
        Eq => left == right,
        Ne => left != right,
        And => left != 0 && right != 0,
        Or => left != 0 || right != 0,
        BitAnd => left != 0 && right != 0,
        BitXor => (left != 0) != (right != 0),
        BitOr => left != 0 || right != 0,
        // Unreachable: only the `boolean`-valued operators reach here.
        Mul | Div | Rem | Add | Sub | Shl | Shr | UShr => return None,
    };
    Some(value as i64)
}

/// Unary numeric promotion ([§5.6.1]): `byte`, `short` and `char` promote to
/// `int`; every other type is unchanged.
///
/// [§5.6.1]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1
fn unary_numeric_promotion(db: &dyn TyDatabase, ty: &Ty) -> Ty {
    match ty.kind(db) {
        TyKind::Primitive(PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char) => {
            Ty::primitive(db, PrimitiveType::Int)
        }
        _ => ty.clone(),
    }
}

/// The type two numeric operands promote to ([§5.6.2] binary numeric
/// promotion): `double` > `float` > `long` > `int`, with `byte`, `short` and
/// `char` widening to `int`. `None` when either type is not numeric.
///
/// [§5.6.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2
fn numeric_promotion(db: &dyn TyDatabase, left: &Ty, right: &Ty) -> Option<Ty> {
    let rank = |ty: &Ty| match ty.kind(db) {
        TyKind::Primitive(PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char) => {
            Some(0u8)
        }
        TyKind::Primitive(PrimitiveType::Int) => Some(1),
        TyKind::Primitive(PrimitiveType::Long) => Some(2),
        TyKind::Primitive(PrimitiveType::Float) => Some(3),
        TyKind::Primitive(PrimitiveType::Double) => Some(4),
        _ => None,
    };
    let promoted = match rank(left)?.max(rank(right)?) {
        0 | 1 => PrimitiveType::Int,
        2 => PrimitiveType::Long,
        3 => PrimitiveType::Float,
        _ => PrimitiveType::Double,
    };
    Some(Ty::primitive(db, promoted))
}

/// The type a binary operator's result takes ([§15.17]–[§15.24]) — `None`
/// when an operand's type is unknown. Every operator here is one §15.29
/// admits.
///
/// [§15.17]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.17
/// [§15.24]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.24
fn result_ty(
    db: &dyn TyDatabase,
    op: BinaryOp,
    left: Option<&Ty>,
    right: Option<&Ty>,
) -> Option<Ty> {
    use BinaryOp::*;
    let (left, right) = (left?, right?);
    Some(match op {
        // §15.19: a shift's result takes the promoted *left* operand's type.
        Shl | Shr | UShr => unary_numeric_promotion(db, left),
        // §15.20.1/§15.21.2/§15.22.2/§15.23/§15.24: the relational, equality,
        // boolean-bitwise and logical operators all yield `boolean`.
        Lt | Gt | Le | Ge | Eq | Ne | And | Or => Ty::primitive(db, PrimitiveType::Boolean),
        // §15.22.1/§15.22.2: a bitwise operator on two booleans yields
        // `boolean`; otherwise binary numeric promotion decides.
        BitAnd | BitXor | BitOr if is_boolean(left, db) && is_boolean(right, db) => {
            Ty::primitive(db, PrimitiveType::Boolean)
        }
        // §15.17/§15.18/§15.22.1: binary numeric promotion.
        Mul | Div | Rem | Add | Sub | BitAnd | BitXor | BitOr => {
            numeric_promotion(db, left, right)?
        }
    })
}

/// Folds two integral constants at the promoted width ([§5.6.2], [§15.17],
/// [§15.18.2], [§15.19]), reusing [`crate::java::const_eval`]'s wrapping
/// arithmetic. `None` when the operation does not complete normally — a
/// division or remainder by a zero divisor, which §15.29 excludes.
///
/// Only the integral operators reach here: the `boolean`-valued ones are
/// decided by their type before folding.
///
/// [§15.18.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.18.2
/// [§15.19]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.19
fn fold(db: &dyn TyDatabase, op: BinaryOp, ty: &Ty, left: i64, right: i64) -> Option<i64> {
    use BinaryOp::*;
    let long = matches!(ty.kind(db), TyKind::Primitive(PrimitiveType::Long));
    Some(match op {
        Add => wrap_arith(left, right, long, ArithOp::Add),
        Sub => wrap_arith(left, right, long, ArithOp::Sub),
        Mul => wrap_arith(left, right, long, ArithOp::Mul),
        Div => wrap_divrem(left, right, long, false)?,
        Rem => wrap_divrem(left, right, long, true)?,
        Shl => left << (right & shift_mask(long)),
        Shr => left >> (right & shift_mask(long)),
        UShr => wrap_ushr(left, right, long),
        BitAnd => {
            if long {
                left & right
            } else {
                ((left as i32) & (right as i32)) as i64
            }
        }
        BitXor => {
            if long {
                left ^ right
            } else {
                ((left as i32) ^ (right as i32)) as i64
            }
        }
        BitOr => {
            if long {
                left | right
            } else {
                ((left as i32) | (right as i32)) as i64
            }
        }
        // Unreachable: [`binary_kind`] settles the `boolean`-valued operators
        // by their type and never folds them.
        Lt | Gt | Le | Ge | Eq | Ne | And | Or => return None,
    })
}

/// The [§5.1.3] narrowing conversion of an integral constant to `primitive`,
/// applied to a cast's operand value.
///
/// [§5.1.3]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.1.3
fn narrow_value(value: i64, primitive: PrimitiveType) -> i64 {
    match primitive {
        PrimitiveType::Byte => value as i8 as i64,
        PrimitiveType::Short => value as i16 as i64,
        PrimitiveType::Char => value as u16 as i64,
        PrimitiveType::Int => value as i32 as i64,
        _ => value,
    }
}

/// The verdict of a value that is not a constant expression, keeping the
/// operand's type (an assignment, a `++`/`--`).
fn demote(kind: ConstKind) -> ConstKind {
    match kind {
        ConstKind::Constant { ty, .. } | ConstKind::NotConstant { ty } => {
            ConstKind::NotConstant { ty }
        }
        ConstKind::Unknown => ConstKind::Unknown,
    }
}

/// The type a *name* denotes, resolved like any type name
/// ([§6.5.5.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.5.1));
/// `None` when it denotes no type this layer can see.
fn type_of_name(cx: &ValueCtx<'_>, name: &Name) -> Option<Ty> {
    let fqn = candidate_fqns(cx.resolver, name)
        .into_iter()
        .find(|candidate| hir::fqn_resolve(cx.db, cx.scope, candidate.as_str()).is_some())?;
    Some(Ty::reference(cx.db, fqn.as_str(), Vec::new()))
}

/// The name of the *type* the qualifier expression of a qualified name
/// ([§6.5.6.2]) denotes — `Some` only for a plain name that resolves to a
/// type.
///
/// [§6.5.6.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.6.2
fn qualifier_type_name(cx: &ValueCtx<'_>, target: ExprId) -> Option<Name> {
    let name = match cx.bodies.expr(target) {
        ExprData::Var(name) | ExprData::NamePath(name) => name.clone(),
        _ => return None,
    };
    type_of_name(cx, &name).map(|_| name)
}

/// The `TypeName` and `Identifier` of a dotted name falling through to the
/// resolver ([§6.5.6.2]): `Some` only when the text has a qualifier.
///
/// [§6.5.6.2]: https://docs.oracle.com/javase/specs/jls/se26/html/jls-6.html#jls-6.5.6.2
pub(crate) fn qualified_parts(name: &Name) -> Option<(Name, Name)> {
    let (qualifier, member) = name.as_str().rsplit_once('.')?;
    (!qualifier.is_empty() && !member.is_empty()).then(|| (Name::new(qualifier), Name::new(member)))
}
