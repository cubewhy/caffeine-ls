//! Expression-level type inference over the lowered body IR
//! ([`hir_expand::body`]).
//!
//! [`body_types`] infers the type of every expression ([JLS §15]) and local
//! variable ([JLS §14.4]) of a method, constructor or initializer body, given
//! the declaration types computed by [`crate::db::item_ty_query`] and the
//! body IR of `hir-def`. Names are resolved lexically ([JLS §6.3]); field and
//! method access is resolved by [`crate::method::pick_field`] /
//! [`crate::method::pick_method`] under the access context of the call site
//! ([JLS §6.6]).
//!
//! The types are computed bottom-up ([§15.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.1)):
//! every expression's type is a function of its operands. Numeric binary
//! expressions follow binary numeric promotion
//! ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2)),
//! unary expressions unary numeric promotion
//! ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)),
//! and conditional and array-initializer expressions follow the conditional
//! type of [§15.25](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25)
//! (identity, binary numeric promotion, the null type, then the least upper
//! bound). Method calls are refined by their *target type*
//! ([JLS §18.5.2.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-18.html#jls-18.5.2.4))
//! where the context fixes it: a declaration initializer, an assignment
//! right-hand side, or a returned expression. Lambdas and method references
//! are poly expressions whose type comes from the *target* functional
//! interface ([§15.27](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.27)):
//! their standalone type is unknown, so they infer to [`Ty::error`].

use std::sync::Arc;

use hir_expand::{
    body::{
        BinaryOp, BodyId, BodyTree, ExprData, ExprId, Literal, LocalId, StmtData, StmtId, UnaryOp,
    },
    item_tree::ItemId,
    name::Name,
};
use rustc_hash::FxHashMap;
use syntax::stub::{PrimitiveType, TypeRef};
use vfs::FileId;

use crate::{
    db::{TyDatabase, enclosing_class_query, type_params_map_query},
    inference::least_upper_bound,
    method::{FieldData, InvocationContext, access_context, pick_field, pick_method},
    resolve::{Resolver, item_data, resolve_type_ref, scope_for_file},
    ty::{Ty, TyKind},
};

/// The inferred types of a method or constructor body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyTypes {
    /// The body the types were inferred for.
    pub body: Option<BodyId>,
    /// The type of every expression reachable from the body's statements,
    /// keyed by its arena id.
    pub exprs: FxHashMap<ExprId, Ty>,
    /// The type of every local of the body — parameters, declared locals,
    /// for-loop variables, catch parameters — keyed by its arena id.
    pub locals: FxHashMap<LocalId, Ty>,
}

/// Infers the types of the body of `item` in `file`, memoized per (file,
/// item) by the tracked query in [`crate::db`]. `None` when the item has no
/// body (a declaration without statements) or is not a body-carrying item.
pub fn body_types(db: &dyn TyDatabase, file: FileId, item: ItemId) -> Option<Arc<BodyTypes>> {
    crate::db::body_types_query(db, crate::db::ItemKey::new(db, file, item))
}

pub(crate) fn body_types_impl(
    db: &dyn TyDatabase,
    file: FileId,
    item: ItemId,
) -> Option<BodyTypes> {
    let tree = hir::file_item_tree(db, file);
    let body_id = match item_data(&tree, item)? {
        hir_expand::item_tree::ItemData::Method(method) => method.body,
        hir_expand::item_tree::ItemData::StaticInit(init) => init.body,
        hir_expand::item_tree::ItemData::InstanceInit(init) => init.body,
        _ => None,
    }?;
    let scope = scope_for_file(db, file);
    let type_params = type_params_map_query(db, db.file_text(file));
    let resolver = Resolver::new(&tree, type_params, item);
    let access = access_context(db, file, item);
    let enclosing_class = enclosing_class_query(db, db.file_text(file))
        .get(&item)
        .map(|name| Ty::reference(db, name.as_str(), Vec::new()));
    // The return type of the enclosing method — the target type of a return
    // statement ([JLS §14.17], [JLS §18.5.2.4]).
    let enclosing_ret = match item_data(&tree, item) {
        Some(hir_expand::item_tree::ItemData::Method(method)) => method
            .sig
            .ret
            .as_ref()
            .map(|ret| resolve_type_ref(db, &scope, &resolver, ret)),
        _ => None,
    };

    let mut ctx = InferCtx {
        db,
        scope,
        tree: tree.bodies.clone(),
        resolver,
        access,
        enclosing_class,
        enclosing_ret,
        types: FxHashMap::default(),
        locals: FxHashMap::default(),
        scopes: vec![FxHashMap::default()],
        target: None,
    };
    for &param in &tree.bodies.body(body_id).params {
        ctx.declare_local(param);
    }
    for &stmt in &tree.bodies.body(body_id).stmts {
        ctx.infer_stmt(stmt);
    }
    Some(BodyTypes {
        body: Some(body_id),
        exprs: ctx.types,
        locals: ctx.locals,
    })
}

struct InferCtx<'a> {
    db: &'a dyn TyDatabase,
    scope: hir::ResolutionScope,
    tree: Arc<BodyTree>,
    resolver: Resolver,
    access: InvocationContext,
    enclosing_class: Option<Ty>,
    /// The return type of the enclosing method or constructor: the target
    /// type ([JLS §18.5.2.4]) of the expressions it returns.
    enclosing_ret: Option<Ty>,
    types: FxHashMap<ExprId, Ty>,
    locals: FxHashMap<LocalId, Ty>,
    /// The lexical scope stack ([JLS §6.3]): innermost first.
    scopes: Vec<FxHashMap<Name, LocalId>>,
    /// The expected type of the expression currently being inferred — set
    /// where the context fixes the type: a declaration initializer, an
    /// assignment right-hand side, or a return statement.
    target: Option<Ty>,
}

impl<'a> InferCtx<'a> {
    fn error(&self) -> Ty {
        Ty::error(self.db)
    }

    /// Infers `expr` under the expected type `target`, restoring the previous
    /// target afterwards. The target participates in method invocation type
    /// inference ([JLS §18.5.2.4]).
    fn with_target<T>(&mut self, target: Option<Ty>, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.target;
        self.target = target;
        let result = f(self);
        self.target = saved;
        result
    }

    fn primitive(&self, p: PrimitiveType) -> Ty {
        Ty::primitive(self.db, p)
    }

    fn string(&self) -> Ty {
        Ty::reference(self.db, "java.lang.String", Vec::new())
    }

    fn is_string(&self, ty: Ty) -> bool {
        matches!(ty.kind(self.db), TyKind::Reference { name, .. } if name.as_str() == "java.lang.String")
    }

    fn infer_expr(&mut self, id: ExprId) -> Ty {
        let expr = self.tree.expr(id).clone();
        let ty = match expr {
            ExprData::Literal(Literal::Int) => self.primitive(PrimitiveType::Int),
            ExprData::Literal(Literal::Long) => self.primitive(PrimitiveType::Long),
            ExprData::Literal(Literal::Char) => self.primitive(PrimitiveType::Char),
            ExprData::Literal(Literal::Float) => self.primitive(PrimitiveType::Float),
            ExprData::Literal(Literal::Double) => self.primitive(PrimitiveType::Double),
            ExprData::Literal(Literal::Boolean) => self.primitive(PrimitiveType::Boolean),
            ExprData::Literal(Literal::Str) => self.string(),
            // §3.10.8: the null literal has the null type.
            ExprData::Null => Ty::null(self.db),
            // §15.8.3: `this` is the type of the enclosing class.
            ExprData::This { .. } => self.enclosing_class.unwrap_or_else(|| self.error()),
            ExprData::Super => self.error(),
            // §15.8.2: `T.class` has type `Class<T>`.
            ExprData::ClassLit(tyref) => {
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
                Ty::reference(self.db, "java.lang.Class", vec![inner])
            }
            ExprData::Var(name) => self.var(name),
            ExprData::NamePath(name) => self.name_path(name),
            ExprData::FieldAccess { target, name } => self.field_access(target, name),
            // §15.13: the type of `array[index]` is the array's element type.
            ExprData::ArrayAccess { array, index } => {
                let _ = self.infer_expr(index);
                let array_ty = self.infer_expr(array);
                if array_ty.is_array(self.db) {
                    array_ty
                        .element(self.db)
                        .copied()
                        .unwrap_or_else(|| self.error())
                } else {
                    self.error()
                }
            }
            ExprData::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.method_call(receiver, name, &args, self.target),
            // §15.9: a class instance creation has the type of the created
            // class.
            ExprData::New { ty, args } => self.new_expr(ty, &args),
            // §15.10: `new T[n][m]` has type `T[n][m]` (an array nested as
            // deep as there are dimensions).
            ExprData::NewArray { ty, dims } => {
                for &dim in &dims {
                    let _ = self.infer_expr(dim);
                }
                let inner = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
                let mut result = inner;
                for _ in 0..dims.len() {
                    result = Ty::array(self.db, result);
                }
                result
            }
            // §10.6: an array initializer has an array type whose element type
            // is derived from the elements.
            ExprData::ArrayInit(elements) => {
                let element = if elements.is_empty() {
                    self.error()
                } else {
                    let mut element = self.infer_expr(elements[0]);
                    for &element_expr in &elements[1..] {
                        let next = self.infer_expr(element_expr);
                        element = self.conditional_type(element, next);
                    }
                    element
                };
                Ty::array(self.db, element)
            }
            ExprData::Unary { op, expr } => self.unary(op, expr),
            // §15.14: a postfix increment/decrement has the type of its
            // operand.
            ExprData::Postfix { expr, .. } => self.infer_expr(expr),
            ExprData::Binary { op, lhs, rhs } => self.binary(op, lhs, rhs),
            // §15.26: an assignment expression has the type of its left-hand
            // side; the right-hand side is a poly expression with the left
            // side's type as target ([JLS §18.5.2.4]).
            ExprData::Assign { lhs, rhs, .. } => {
                let lhs_ty = self.infer_expr(lhs);
                let _ = self.with_target(Some(lhs_ty), |this| this.infer_expr(rhs));
                lhs_ty
            }
            // §15.16: a cast has the type named in the cast.
            ExprData::Cast { ty, expr } => {
                let _ = self.infer_expr(expr);
                resolve_type_ref(self.db, &self.scope, &self.resolver, &ty)
            }
            // §15.20.2: `instanceof` always has type `boolean`.
            ExprData::InstanceOf { expr, .. } => {
                let _ = self.infer_expr(expr);
                self.primitive(PrimitiveType::Boolean)
            }
            // §15.25: a conditional expression's type follows the rules of
            // §15.25.2/§15.25.3 (identity, numeric promotion, then lub).
            ExprData::Conditional { cond, then, els } => {
                let _ = self.infer_expr(cond);
                let then_ty = self.infer_expr(then);
                let els_ty = self.infer_expr(els);
                self.conditional_type(then_ty, els_ty)
            }
            // §15.27/§15.13: lambdas and method references are poly
            // expressions; their type comes from the target functional
            // interface, which is not available in isolation.
            ExprData::Lambda { .. } | ExprData::MethodRef { .. } => self.error(),
            // §15.28: a switch expression's type is derived from its arm result
            // types.
            ExprData::Switch { scrutinee, arms } => {
                let _ = self.infer_expr(scrutinee);
                let mut result_tys: Vec<Ty> = Vec::new();
                for arm in &arms {
                    for &label in &arm.labels {
                        let _ = self.infer_expr(label);
                    }
                    for &stmt in &arm.body {
                        let data = self.tree.stmt(stmt).clone();
                        match data {
                            StmtData::Expr(expr) => result_tys.push(self.infer_expr(expr)),
                            _ => self.infer_stmt_data(&data),
                        }
                    }
                }
                if result_tys.is_empty() {
                    self.error()
                } else {
                    let mut ty = result_tys[0];
                    for &result in &result_tys[1..] {
                        ty = self.conditional_type(ty, result);
                    }
                    ty
                }
            }
            ExprData::Paren(inner) => self.infer_expr(inner),
            ExprData::Missing => self.error(),
        };
        self.types.insert(id, ty);
        ty
    }

    /// A bare name: a local variable or parameter, or — when no local — a
    /// field of the implicit receiver ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1)).
    fn var(&mut self, name: Name) -> Ty {
        if let Some(local) = self.lookup_local(&name) {
            return self
                .locals
                .get(&local)
                .copied()
                .unwrap_or_else(|| self.error());
        }
        if let Some(field) = self.pick_field_of(self.enclosing_class, name.as_str()) {
            return field.ty;
        }
        self.error()
    }

    /// A qualified name in expression position: `Type.field` (a static field
    /// access, [§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1))
    /// when the prefix resolves to a type; a simple non-local name falls back
    /// to a field of the implicit receiver.
    fn name_path(&mut self, name: Name) -> Ty {
        let text = name.as_str();
        let (prefix, last) = match text.rsplit_once('.') {
            Some((prefix, last)) => (prefix, last),
            None => ("", text),
        };
        if prefix.is_empty() {
            if let Some(field) = self.pick_field_of(self.enclosing_class, last) {
                return field.ty;
            }
            return self.error();
        }
        let prefix_ty = {
            let tyref = TypeRef::Reference {
                name: Name::new(prefix),
                generic_args: Vec::new(),
            };
            resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref)
        };
        if let Some(field) = pick_field(self.db, &self.scope, &prefix_ty, last, &self.access) {
            return field.ty;
        }
        self.error()
    }

    /// A bare type name in receiver position: `Type.name` — a static member
    /// access ([§15.11.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.11.1))
    /// or `Type.method(...)` call
    /// ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1))
    /// whose receiver is a type, not a value. `None` when `name` is a local
    /// variable or does not resolve to a type.
    fn type_name_ty(&self, name: &Name) -> Option<Ty> {
        if self.lookup_local(name).is_some() {
            return None;
        }
        let tyref = TypeRef::Reference {
            name: name.clone(),
            generic_args: Vec::new(),
        };
        let ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &tyref);
        // The name is a type only when its canonical FQN resolves on the
        // classpath; a name that no candidate resolved to (a field or method
        // of the implicit receiver) is not.
        let TyKind::Reference { name: resolved, .. } = ty.kind(self.db) else {
            return None;
        };
        (hir::fqn_resolve(self.db, &self.scope, resolved.as_str()).is_some()).then_some(ty)
    }

    fn field_access(&mut self, target: Option<ExprId>, name: Name) -> Ty {
        let Some(target) = target else {
            return self.var(name);
        };
        // `Type.name` — the receiver expression is a bare name that resolves
        // to a type, not a value ([§15.11.1]).
        let (receiver, is_static) = if let ExprData::Var(type_name) = self.tree.expr(target).clone()
            && let Some(ty) = self.type_name_ty(&type_name)
        {
            (ty, true)
        } else {
            (self.infer_expr(target), false)
        };
        // §10.7: every array type has a public final `length` field.
        if receiver.is_array(self.db) && name.as_str() == "length" {
            return self.primitive(PrimitiveType::Int);
        }
        match pick_field(self.db, &self.scope, &receiver, name.as_str(), &self.access) {
            Some(field) => field.ty,
            // `Type.name` read without a call — or used as the receiver of a
            // `Type.method(...)` call — is the type itself.
            None if is_static => receiver,
            None => self.error(),
        }
    }

    fn pick_field_of(&mut self, receiver: Option<Ty>, name: &str) -> Option<FieldData> {
        let receiver = receiver?;
        pick_field(self.db, &self.scope, &receiver, name, &self.access)
    }

    fn method_call(
        &mut self,
        receiver: Option<ExprId>,
        name: Name,
        args: &[ExprId],
        target: Option<Ty>,
    ) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|arg| self.infer_expr(*arg)).collect();
        let receiver_ty = match receiver {
            Some(receiver) => {
                // `Type.method(...)` — a static invocation whose receiver
                // expression is a bare type name ([§15.12.1]).
                if let ExprData::Var(type_name) = self.tree.expr(receiver).clone()
                    && let Some(ty) = self.type_name_ty(&type_name)
                {
                    ty
                } else {
                    self.infer_expr(receiver)
                }
            }
            // An unqualified call is an implicit `this` invocation
            // ([§15.12.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.12.1)).
            None => self.enclosing_class.unwrap_or_else(|| self.error()),
        };
        match pick_method(
            self.db,
            &self.scope,
            &receiver_ty,
            name.as_str(),
            &arg_tys,
            &self.access,
            target,
        ) {
            Some(method) => method.ret,
            None => self.error(),
        }
    }

    /// A class instance creation ([§15.9](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.9)):
    /// the created class's type. Constructors are resolved so the arguments
    /// are checked; source constructors are named after the class, library
    /// constructors are `<init>`.
    fn new_expr(&mut self, ty: TypeRef<Name>, args: &[ExprId]) -> Ty {
        let arg_tys: Vec<Ty> = args.iter().map(|arg| self.infer_expr(*arg)).collect();
        let class_ty = resolve_type_ref(self.db, &self.scope, &self.resolver, &ty);
        let TyKind::Reference { name, .. } = class_ty.kind(self.db) else {
            return class_ty;
        };
        let constructor_name = match hir::fqn_resolve(self.db, &self.scope, name.as_str()) {
            Some(hir::Resolved::Library(_)) => "<init>".to_owned(),
            _ => simple_name(name.as_str()),
        };
        let _ = pick_method(
            self.db,
            &self.scope,
            &class_ty,
            &constructor_name,
            &arg_tys,
            &self.access,
            None,
        );
        class_ty
    }

    fn unary(&mut self, op: UnaryOp, expr: ExprId) -> Ty {
        let inner = self.infer_expr(expr);
        match op {
            // §15.15.6: `!` has type `boolean`.
            UnaryOp::Not => self.primitive(PrimitiveType::Boolean),
            // §15.15.1-3: unary numeric promotion (§5.6.1).
            UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => self.unary_promotion(inner),
            // §15.15.1/§15.15.2: `++`/`--` have the operand's type.
            UnaryOp::Inc | UnaryOp::Dec => inner,
        }
    }

    /// The type of a conditional expression over two operands, following the
    /// rules of [§15.25.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25.2)
    /// and [§15.25.3](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.25.3):
    /// identical types and primitive types use identity / binary numeric
    /// promotion, the null type yields the reference operand, and reference
    /// types fall back to the least upper bound
    /// ([§4.10.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.10.4)).
    fn conditional_type(&self, then_ty: Ty, els_ty: Ty) -> Ty {
        if then_ty == els_ty {
            return then_ty;
        }
        if then_ty.is_primitive(self.db) && els_ty.is_primitive(self.db) {
            return self.binary_numeric_promotion(then_ty, els_ty);
        }
        if then_ty.is_null(self.db) && els_ty.is_reference(self.db) {
            return els_ty;
        }
        if els_ty.is_null(self.db) && then_ty.is_reference(self.db) {
            return then_ty;
        }
        least_upper_bound(self.db, &self.scope, &[then_ty, els_ty])
    }

    /// Unary numeric promotion ([§5.6.1](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.1)):
    /// `byte`, `short` and `char` promote to `int`; everything else keeps its
    /// type.
    fn unary_promotion(&self, ty: Ty) -> Ty {
        match ty.kind(self.db) {
            TyKind::Primitive(PrimitiveType::Byte | PrimitiveType::Short | PrimitiveType::Char) => {
                self.primitive(PrimitiveType::Int)
            }
            TyKind::Primitive(_) => ty,
            _ => self.error(),
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: ExprId, rhs: ExprId) -> Ty {
        let lhs_ty = self.infer_expr(lhs);
        let rhs_ty = self.infer_expr(rhs);
        match op {
            BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::BitAnd
            | BinaryOp::BitXor
            | BinaryOp::BitOr => {
                // §15.18.1: `+` with a `String` operand is string
                // concatenation and has type `String`.
                if matches!(op, BinaryOp::Add) && (self.is_string(lhs_ty) || self.is_string(rhs_ty))
                {
                    self.string()
                } else {
                    self.binary_numeric_promotion(lhs_ty, rhs_ty)
                }
            }
            // §15.19: a shift has the unary-promoted type of the left operand.
            BinaryOp::Shl | BinaryOp::Shr | BinaryOp::UShr => {
                let promoted = self.unary_promotion(lhs_ty);
                if promoted.is_error(self.db) {
                    self.error()
                } else {
                    promoted
                }
            }
            // §15.20-15.24: relational, equality and boolean-logical
            // expressions have type `boolean`.
            BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::And
            | BinaryOp::Or => self.primitive(PrimitiveType::Boolean),
        }
    }

    /// Binary numeric promotion ([§5.6.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-5.html#jls-5.6.2)):
    /// the promoted type is the "widest" of the two operand types; `byte`,
    /// `short` and `char` promote to `int`. Boxing of reference operands is
    /// not modelled.
    fn binary_numeric_promotion(&self, lhs: Ty, rhs: Ty) -> Ty {
        let promote = |ty: Ty| match ty.kind(self.db) {
            TyKind::Primitive(
                PrimitiveType::Byte
                | PrimitiveType::Short
                | PrimitiveType::Char
                | PrimitiveType::Int,
            ) => Some(PrimitiveType::Int),
            TyKind::Primitive(PrimitiveType::Long) => Some(PrimitiveType::Long),
            TyKind::Primitive(PrimitiveType::Float) => Some(PrimitiveType::Float),
            TyKind::Primitive(PrimitiveType::Double) => Some(PrimitiveType::Double),
            _ => None,
        };
        match (promote(lhs), promote(rhs)) {
            (Some(PrimitiveType::Double), _) | (_, Some(PrimitiveType::Double)) => {
                self.primitive(PrimitiveType::Double)
            }
            (Some(PrimitiveType::Float), _) | (_, Some(PrimitiveType::Float)) => {
                self.primitive(PrimitiveType::Float)
            }
            (Some(PrimitiveType::Long), _) | (_, Some(PrimitiveType::Long)) => {
                self.primitive(PrimitiveType::Long)
            }
            (Some(PrimitiveType::Int), _) | (_, Some(PrimitiveType::Int)) => {
                self.primitive(PrimitiveType::Int)
            }
            _ => self.error(),
        }
    }

    /// The element type of a for-each iterable
    /// ([§14.14.2](https://docs.oracle.com/javase/specs/jls/se26/html/jls-14.html#jls-14.14.2)):
    /// the element type for arrays; an `Iterable<T>` element for references.
    /// Only arrays are modelled for now.
    fn element_type(&self, iterable: Ty) -> Ty {
        if iterable.is_array(self.db) {
            return iterable
                .element(self.db)
                .copied()
                .unwrap_or_else(|| self.error());
        }
        self.error()
    }

    fn declare_local(&mut self, id: LocalId) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => self.error(),
        };
        self.bind_local(id, local.name, ty);
    }

    fn declare_local_ty(&mut self, id: LocalId, fallback: Ty) {
        let local = self.tree.local(id).clone();
        let ty = match &local.ty {
            Some(tyref) => resolve_type_ref(self.db, &self.scope, &self.resolver, tyref),
            None => fallback,
        };
        self.bind_local(id, local.name, ty);
    }

    fn bind_local(&mut self, id: LocalId, name: Name, ty: Ty) {
        self.locals.insert(id, ty);
        self.scopes
            .last_mut()
            .expect("scope stack non-empty")
            .insert(name, id);
    }

    fn lookup_local(&self, name: &Name) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&local) = scope.get(name) {
                return Some(local);
            }
        }
        None
    }

    fn infer_stmt(&mut self, id: StmtId) {
        let stmt = self.tree.stmt(id).clone();
        self.infer_stmt_data(&stmt);
    }

    fn infer_stmt_data(&mut self, stmt: &StmtData) {
        match stmt {
            StmtData::Empty => {}
            StmtData::Block(stmts) => {
                self.scopes.push(FxHashMap::default());
                for &stmt in stmts {
                    self.infer_stmt(stmt);
                }
                self.scopes.pop();
            }
            StmtData::Decl { local, initializer } => {
                self.declare_local(*local);
                if let Some(initializer) = initializer {
                    // The initializer is a poly expression whose target is the
                    // declared type of the local ([JLS §14.4]).
                    let target = self.locals.get(local).copied();
                    let _ = self.with_target(target, |this| this.infer_expr(*initializer));
                }
            }
            StmtData::Expr(expr) => {
                let _ = self.infer_expr(*expr);
            }
            StmtData::Labeled { stmt, .. } => self.infer_stmt(*stmt),
            StmtData::If { cond, then, els } => {
                let _ = self.infer_expr(*cond);
                self.infer_stmt(*then);
                if let Some(els) = els {
                    self.infer_stmt(*els);
                }
            }
            StmtData::While { cond, body } => {
                let _ = self.infer_expr(*cond);
                self.infer_stmt(*body);
            }
            StmtData::DoWhile { body, cond } => {
                self.infer_stmt(*body);
                let _ = self.infer_expr(*cond);
            }
            StmtData::For {
                init,
                cond,
                step,
                body,
            } => {
                self.scopes.push(FxHashMap::default());
                for &init in init {
                    self.infer_stmt(init);
                }
                if let Some(cond) = cond {
                    let _ = self.infer_expr(*cond);
                }
                for &step in step {
                    let _ = self.infer_expr(step);
                }
                self.infer_stmt(*body);
                self.scopes.pop();
            }
            StmtData::ForEach {
                var,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr(*iterable);
                let element = self.element_type(iterable_ty);
                self.scopes.push(FxHashMap::default());
                self.declare_local_ty(*var, element);
                self.infer_stmt(*body);
                self.scopes.pop();
            }
            StmtData::Switch { scrutinee, arms } => {
                let _ = self.infer_expr(*scrutinee);
                self.scopes.push(FxHashMap::default());
                for arm in arms {
                    for &label in &arm.labels {
                        let _ = self.infer_expr(label);
                    }
                    for &stmt in &arm.body {
                        self.infer_stmt(stmt);
                    }
                }
                self.scopes.pop();
            }
            StmtData::Return(Some(expr)) | StmtData::Throw(expr) | StmtData::Yield(expr) => {
                // A returned expression is a poly expression whose target is
                // the method's return type ([JLS §14.17]).
                let _ = self.with_target(self.enclosing_ret, |this| this.infer_expr(*expr));
            }
            StmtData::Return(None) | StmtData::Break(_) | StmtData::Continue(_) => {}
            StmtData::Synchronized { expr, body } => {
                let _ = self.infer_expr(*expr);
                self.infer_stmt(*body);
            }
            StmtData::Try {
                resources,
                body,
                catches,
                finally,
            } => {
                self.scopes.push(FxHashMap::default());
                for &resource in resources {
                    self.declare_local(resource);
                }
                self.infer_stmt(*body);
                for clause in catches {
                    self.scopes.push(FxHashMap::default());
                    self.declare_local(clause.param);
                    self.infer_stmt(clause.body);
                    self.scopes.pop();
                }
                self.scopes.pop();
                if let Some(finally) = finally {
                    self.infer_stmt(*finally);
                }
            }
            StmtData::Assert { cond, msg } => {
                let _ = self.infer_expr(*cond);
                if let Some(msg) = msg {
                    let _ = self.infer_expr(*msg);
                }
            }
            StmtData::Missing => {}
        }
    }
}

/// The simple name of a possibly-qualified FQN: everything after the last
/// `$` (nested classes are named `Outer$Inner`).
fn simple_name(fqn: &str) -> String {
    match fqn.rfind(['$', '.']) {
        Some(i) => fqn[i + 1..].to_owned(),
        None => fqn.to_owned(),
    }
}
