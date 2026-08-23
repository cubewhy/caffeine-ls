//! Constant expressions ([JLS §15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28))
//! and constant variables ([§4.12.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4)).
//!
//! A constant expression is composed of literals of primitive type and
//! `String`, simple names referring to *constant variables*, parenthesized
//! forms, and a closed operator set — unary `+`/`-`/`~`/`!`, the arithmetic,
//! shift, bitwise, logical, relational and equality operators, and string
//! concatenation with `+`. The conditional operator `?:` is a constant
//! expression when its condition and both branches are. Evaluation is exact:
//! integer arithmetic wraps at 32/64 bits as at runtime, division by zero
//! makes the expression non-constant (the compile-time error is reported by
//! the type layer separately), and a boolean or `String` operand of `+`
//! concatenates its textual form.
//!
//! The evaluator is used by the type layer to validate `case` labels
//! ([§14.11.1]), detect duplicate labels and apply the narrowing conversion
//! of constants in assignment context ([§5.2], [§5.1.3]).

use std::sync::Arc;

use rustc_hash::FxHashMap;

use hir_expand::body::{BinaryOp, BodyTree, ExprData, ExprId, Literal, LocalId, UnaryOp};

/// The value of a constant expression ([JLS §15.28]): an `int`, `long`,
/// `char` value folds into [`Const::Int`] (the int-compatible constants are
/// what [§5.1.3] narrowing and [§14.11.1] case labels need), a `boolean`
/// constant is [`Const::Bool`] and a `String` constant is [`Const::Str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Const {
    /// An int-compatible constant: an `int`, `long` or `char` constant
    /// expression ([§4.2], [§4.2.1]), with its value.
    Int(i64),
    /// A `boolean` constant expression.
    Bool(bool),
    /// A `String` constant expression.
    Str(Arc<str>),
}

impl Const {
    /// The value as an `int`-compatible constant ([§4.2.1]), when this is one.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Const::Int(v) => Some(*v),
            _ => None,
        }
    }
}

/// The environment a constant expression is evaluated in: the body tree plus
/// the constant variables seen so far ([§4.12.4]) — a `final` local whose
/// initializer was itself constant.
#[derive(Clone)]
pub struct ConstEnv<'a> {
    tree: &'a BodyTree,
    consts: &'a FxHashMap<LocalId, Const>,
}

impl<'a> ConstEnv<'a> {
    pub fn new(tree: &'a BodyTree, consts: &'a FxHashMap<LocalId, Const>) -> Self {
        Self { tree, consts }
    }

    /// Evaluates `id` as a constant expression ([JLS §15.28]); `None` when it
    /// is not one — including when it references a non-constant name or uses
    /// an operator outside the constant set.
    pub fn eval(&self, id: ExprId) -> Option<Const> {
        match self.tree.expr(id).clone() {
            ExprData::Literal(literal) => self.literal(&literal),
            ExprData::Paren(inner) => self.eval(inner),
            // A simple name is a constant expression only when it refers to a
            // constant variable ([§4.12.4]); ordinary locals, fields, enum
            // constants and static imports are not tracked here, so they make
            // the expression non-constant.
            ExprData::Var(name) => self
                .consts
                .iter()
                .find(|(local, _)| self.tree.local(**local).name == name)
                .map(|(_, value)| value.clone()),
            ExprData::Unary { op, expr } => {
                let v = self.eval(expr)?;
                self.unary(op, &v)
            }
            ExprData::Binary { op, lhs, rhs } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                self.binary(op, l, r)
            }
            // §15.25/§15.28: a conditional expression is constant when its
            // condition is a `boolean` constant and both branches are
            // constant expressions of primitive or `String` type.
            ExprData::Conditional { cond, then, els } => {
                if !matches!(self.eval(cond)?, Const::Bool(true)) {
                    return self.eval(els);
                }
                self.eval(then)
            }
            _ => None,
        }
    }

    fn literal(&self, literal: &Literal) -> Option<Const> {
        Some(match literal {
            // An int literal is an int-typed constant; a long literal is a
            // long-typed one ([§3.10.1]); both fold to [`Const::Int`].
            Literal::Int(v) | Literal::Long(v) => Const::Int(*v),
            // A char literal is an int-typed constant with its code-point
            // value ([§3.10.4]).
            Literal::Char(c) => Const::Int(*c as i64),
            Literal::Boolean(b) => Const::Bool(*b),
            Literal::Str(s) => Const::Str(Arc::from(s.as_str())),
            // Floating-point literals are not part of any constant form the
            // type layer consumes ([§15.28] excludes FP entirely).
            Literal::Float | Literal::Double => return None,
        })
    }

    fn unary(&self, op: UnaryOp, v: &Const) -> Option<Const> {
        Some(match op {
            UnaryOp::Plus => match v {
                Const::Int(i) => Const::Int(*i),
                _ => return None,
            },
            UnaryOp::Minus => match v {
                Const::Int(i) => Const::Int(i.wrapping_neg()),
                _ => return None,
            },
            UnaryOp::BitNot => match v {
                Const::Int(i) => Const::Int(!*i),
                _ => return None,
            },
            UnaryOp::Not => match v {
                Const::Bool(b) => Const::Bool(!*b),
                _ => return None,
            },
            // `++`/`--` are assignments, never constant expressions.
            UnaryOp::Inc | UnaryOp::Dec => return None,
        })
    }

    fn binary(&self, op: BinaryOp, lhs: Const, rhs: Const) -> Option<Const> {
        use BinaryOp::*;
        // String concatenation ([§15.18.1]): either operand a `String`
        // constant converts the other to its textual form.
        if matches!(op, Add)
            && let (Some(l), Some(r)) = (lhs.as_str(), rhs.as_str())
        {
            return Some(Const::Str(Arc::from(format!("{l}{r}").as_str())));
        }
        // Logical operators on booleans ([§15.23]/[§15.24]).
        if let (Const::Bool(l), Const::Bool(r)) = (&lhs, &rhs) {
            return match op {
                And => Some(Const::Bool(*l && *r)),
                Or => Some(Const::Bool(*l || *r)),
                Eq => Some(Const::Bool(l == r)),
                Ne => Some(Const::Bool(l != r)),
                _ => None,
            };
        }
        // Integer operators ([§15.17]-[§15.22]); evaluation wraps at 64 bits,
        // matching the runtime semantics for the `int` values the callers
        // consume.
        let (l, r) = (lhs.as_int()?, rhs.as_int()?);
        Some(match op {
            Add => Const::Int(l.wrapping_add(r)),
            Sub => Const::Int(l.wrapping_sub(r)),
            Mul => Const::Int(l.wrapping_mul(r)),
            Div if r != 0 => Const::Int(l.wrapping_div(r)),
            Rem if r != 0 => Const::Int(l.wrapping_rem(r)),
            // Division/remained by zero is not a compile-time constant; the
            // type layer does not report it here.
            Div | Rem => return None,
            Shl => Const::Int(l.wrapping_shl(r as u32)),
            Shr => Const::Int(l.wrapping_shr(r as u32)),
            UShr => Const::Int(((l as u64).wrapping_shr(r as u32)) as i64),
            Lt => Const::Bool(l < r),
            Gt => Const::Bool(l > r),
            Le => Const::Bool(l <= r),
            Ge => Const::Bool(l >= r),
            Eq => Const::Bool(l == r),
            Ne => Const::Bool(l != r),
            BitAnd => Const::Int(l & r),
            BitXor => Const::Int(l ^ r),
            BitOr => Const::Int(l | r),
            And | Or => return None,
        })
    }
}

impl Const {
    fn as_str(&self) -> Option<&str> {
        match self {
            Const::Str(s) => Some(s),
            _ => None,
        }
    }
}
