//! Constant expressions ([JLS §15.28](https://docs.oracle.com/javase/specs/jls/se26/html/jls-15.html#jls-15.28))
//! and constant variables ([§4.12.4](https://docs.oracle.com/javase/specs/jls/se26/html/jls-4.html#jls-4.12.4)).
//!
//! A constant expression is composed of literals of primitive type and
//! `String`, simple names referring to *constant variables*, parenthesized
//! forms, and a closed operator set — unary `+`/`-`/`~`/`!`, the arithmetic,
//! shift, bitwise, logical, relational and equality operators, and string
//! concatenation with `+`. The conditional operator `?:` is a constant
//! expression when its condition *and both branches* are constants of
//! primitive or `String` type — even though only one branch contributes the
//! value. Evaluation is exact: integral arithmetic wraps at the operand's
//! own width (`int` at 32 bits, `long` at 64) as at runtime ([§5.6.2],
//! [§15.18.2], [§15.19]), division by zero makes the expression
//! non-constant (the compile-time error is reported by the type layer
//! separately), and a boolean or `String` operand of `+` concatenates its
//! textual form.
//!
//! The evaluator is used by the type layer to validate `case` labels
//! ([§14.11.1]), detect duplicate labels and apply the narrowing conversion
//! of constants in assignment context ([§5.2], [§5.1.3]).

use triomphe::Arc;

use rustc_hash::FxHashMap;

use hir_expand::body::{BinaryOp, BodyTree, ExprData, ExprId, Literal, LocalId, UnaryOp};

/// The value of a constant expression ([JLS §15.28]): an `int`, `long`,
/// `char` value folds into [`Const::Int`] (the int-compatible constants are
/// what [§5.1.3] narrowing and [§14.11.1] case labels need), a `boolean`
/// constant is [`Const::Bool`] and a `String` constant is [`Const::Str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Const {
    /// An int-compatible constant: an `int`, `long` or `char` constant
    /// expression ([§4.2], [§4.2.1]), with its value. `long` records the
    /// constant's own type ([§4.2.1] vs [§4.2.2]) — the width arithmetic
    /// wraps at and shifts mask against ([§5.6.2], [§15.19]); it does not
    /// affect the raw magnitude consumers range-check.
    Int { v: i64, long: bool },
    /// A `boolean` constant expression.
    Bool(bool),
    /// A `String` constant expression.
    Str(Arc<str>),
}

impl Const {
    /// The magnitude as an `int`-compatible constant ([§4.2.1]).
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Const::Int { v, .. } => Some(*v),
            _ => None,
        }
    }

    /// The textual form a string conversion yields ([§5.1.11]): decimal
    /// digits for an integral constant, `true`/`false` for boolean, the
    /// characters themselves for a `String`.
    fn text(&self) -> String {
        match self {
            Const::Int { v, .. } => v.to_string(),
            Const::Bool(b) => b.to_string(),
            Const::Str(s) => s.to_string(),
        }
    }
}

/// Wrapping addition, subtraction or multiplication at the promoted width
/// ([§15.18.2]): an `int` operation wraps at 32 bits even though both
/// operands travel sign-extended as `i64` here.
fn wrap_arith(l: i64, r: i64, long: bool, op: ArithOp) -> i64 {
    if long {
        match op {
            ArithOp::Add => l.wrapping_add(r),
            ArithOp::Sub => l.wrapping_sub(r),
            ArithOp::Mul => l.wrapping_mul(r),
        }
    } else {
        let (l, r) = (l as i32, r as i32);
        let v = match op {
            ArithOp::Add => l.wrapping_add(r),
            ArithOp::Sub => l.wrapping_sub(r),
            ArithOp::Mul => l.wrapping_mul(r),
        };
        v as i64
    }
}

enum ArithOp {
    Add,
    Sub,
    Mul,
}

/// Wrapping division or remainder at the promoted width; `None` for a zero
/// divisor, which makes the expression non-constant ([§15.28] — javac
/// reports "division by zero" and the type layer owns that error).
fn wrap_divrem(l: i64, r: i64, long: bool, rem: bool) -> Option<i64> {
    if r == 0 {
        return None;
    }
    Some(if long {
        if rem {
            l.wrapping_rem(r)
        } else {
            l.wrapping_div(r)
        }
    } else {
        let (l, r) = (l as i32, r as i32);
        let v = if rem {
            l.wrapping_rem(r)
        } else {
            l.wrapping_div(r)
        };
        v as i64
    })
}

/// The shift distance mask ([§15.19]): `& 0x1f` for an `int` shift,
/// `& 0x3f` for a `long` shift.
fn shift_mask(long: bool) -> i64 {
    if long { 0x3f } else { 0x1f }
}

/// Unsigned right shift at the left operand's width ([§15.19]): `>>>` on an
/// `int` shifts the 32-bit pattern and zero-fills within those 32 bits.
fn wrap_ushr(l: i64, dist: i64, long: bool) -> i64 {
    if long {
        ((l as u64) >> (dist & shift_mask(true))) as i64
    } else {
        (((l as u32) >> (dist & shift_mask(false))) as i32) as i64
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
            // condition is a `boolean` constant *and both branches* are
            // constant expressions of primitive or `String` type — even
            // though only the taken branch contributes the value. Folding
            // just the taken branch would wrongly accept `false ? x : 1`
            // for a non-constant `x`.
            ExprData::Conditional { cond, then, els } => {
                let cond = match self.eval(cond)? {
                    Const::Bool(b) => b,
                    _ => return None,
                };
                let then_value = self.eval(then)?;
                let else_value = self.eval(els)?;
                Some(if cond { then_value } else { else_value })
            }
            _ => None,
        }
    }

    fn literal(&self, literal: &Literal) -> Option<Const> {
        Some(match literal {
            // §3.10.1: an integer literal is int-typed; a long literal (its
            // `L` suffix) is long-typed — the width later operations wrap
            // and shift at.
            Literal::Int(v) => Const::Int { v: *v, long: false },
            Literal::Long(v) => Const::Int { v: *v, long: true },
            // A char literal is an int-typed constant with its code-point
            // value ([§3.10.4]).
            Literal::Char(c) => Const::Int {
                v: *c as i64,
                long: false,
            },
            Literal::Boolean(b) => Const::Bool(*b),
            Literal::Str(s) => Const::Str(Arc::from(s.as_str())),
            // Floating-point literals are not part of any constant form the
            // type layer consumes ([§15.28] excludes FP entirely).
            Literal::Float | Literal::Double => return None,
        })
    }

    fn unary(&self, op: UnaryOp, v: &Const) -> Option<Const> {
        Some(match op {
            UnaryOp::Plus => v.clone(),
            // §15.15.4: `-x` equals `0 - x`, wrapping at the operand's width.
            UnaryOp::Minus => match *v {
                Const::Int { v, long } => Const::Int {
                    v: if long {
                        v.wrapping_neg()
                    } else {
                        (v as i32).wrapping_neg() as i64
                    },
                    long,
                },
                _ => return None,
            },
            UnaryOp::BitNot => match *v {
                Const::Int { v, long } => Const::Int {
                    v: if long { !v } else { (!v as i32) as i64 },
                    long,
                },
                _ => return None,
            },
            UnaryOp::Not => match *v {
                Const::Bool(b) => Const::Bool(!b),
                _ => return None,
            },
            // `++`/`--` are assignments, never constant expressions.
            UnaryOp::Inc | UnaryOp::Dec => return None,
        })
    }

    fn binary(&self, op: BinaryOp, lhs: Const, rhs: Const) -> Option<Const> {
        use BinaryOp::*;
        // String concatenation ([§15.18.1]): *either* operand being a
        // `String` constant converts the other to its textual form
        // ([§5.1.11]), so `"a" + 1` is itself a constant ([§15.28]).
        if matches!(op, Add) && (matches!(lhs, Const::Str(_)) || matches!(rhs, Const::Str(_))) {
            let mut out = String::with_capacity(lhs.text().len() + rhs.text().len());
            out.push_str(&lhs.text());
            out.push_str(&rhs.text());
            return Some(Const::Str(Arc::from(out.as_str())));
        }
        // Logical operators on booleans ([§15.23]/[§15.24]); the bitwise
        // forms also apply to booleans, without short-circuiting
        // ([§15.22.2]) — all are constant operators ([§15.28]).
        if let (Const::Bool(l), Const::Bool(r)) = (&lhs, &rhs) {
            return match op {
                And => Some(Const::Bool(*l && *r)),
                Or => Some(Const::Bool(*l || *r)),
                Eq => Some(Const::Bool(l == r)),
                Ne => Some(Const::Bool(l != r)),
                BitAnd => Some(Const::Bool(*l & *r)),
                BitXor => Some(Const::Bool(*l ^ *r)),
                BitOr => Some(Const::Bool(*l | *r)),
                _ => None,
            };
        }
        // Integral operators. Binary numeric promotion ([§5.6.2]): either
        // operand `long` runs the operation at 64 bits; otherwise both widen
        // to `int` and it wraps at 32 ([§15.17]-[§15.18.2]). Shifts are the
        // exception — each operand promotes independently and the result
        // takes the *left* operand's type, with the distance masked to that
        // width ([§15.19]).
        let (Const::Int { v: l, long: ll }, Const::Int { v: r, long: rl }) = (&lhs, &rhs) else {
            return None;
        };
        let (l, r) = (*l, *r);
        let long = *ll || *rl;
        Some(match op {
            Add => Const::Int {
                v: wrap_arith(l, r, long, ArithOp::Add),
                long,
            },
            Sub => Const::Int {
                v: wrap_arith(l, r, long, ArithOp::Sub),
                long,
            },
            Mul => Const::Int {
                v: wrap_arith(l, r, long, ArithOp::Mul),
                long,
            },
            Div => Const::Int {
                v: wrap_divrem(l, r, long, false)?,
                long,
            },
            Rem => Const::Int {
                v: wrap_divrem(l, r, long, true)?,
                long,
            },
            Shl => Const::Int {
                v: l << (r & shift_mask(*ll)),
                long: *ll,
            },
            Shr => Const::Int {
                v: l >> (r & shift_mask(*ll)),
                long: *ll,
            },
            UShr => Const::Int {
                v: wrap_ushr(l, r, *ll),
                long: *ll,
            },
            Lt => Const::Bool(l < r),
            Gt => Const::Bool(l > r),
            Le => Const::Bool(l <= r),
            Ge => Const::Bool(l >= r),
            Eq => Const::Bool(l == r),
            Ne => Const::Bool(l != r),
            BitAnd => Const::Int {
                v: if long {
                    l & r
                } else {
                    ((l as i32) & (r as i32)) as i64
                },
                long,
            },
            BitXor => Const::Int {
                v: if long {
                    l ^ r
                } else {
                    ((l as i32) ^ (r as i32)) as i64
                },
                long,
            },
            BitOr => Const::Int {
                v: if long {
                    l | r
                } else {
                    ((l as i32) | (r as i32)) as i64
                },
                long,
            },
            // `&&`/`||` operate on booleans only; they were handled above.
            And | Or => return None,
        })
    }
}
