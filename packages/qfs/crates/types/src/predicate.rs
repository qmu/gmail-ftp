//! The typed predicate model (RFD §4/§5): a [`Predicate`] AST carrying column
//! references and literals, and [`typecheck_predicate`] which resolves each
//! [`ColRef`] against a [`Schema`], applies the **comparability matrix**, and returns
//! a [`TypedPredicate`] decorated with resolved [`ColumnType`]s — the contract that
//! drives type-checking and predicate pushdown.
//!
//! This is the type-model's own predicate IR, distinct from the parser's general
//! `Expr` (t04): the planner lowers a `WHERE` `Expr` into this typed form once column
//! types are known. Keeping it here (not in the parser) keeps `qfs-types` the single
//! home of typing rules and keeps the parser vendor-free.

use serde::{Deserialize, Serialize};

use crate::error::TypeError;
use crate::schema::{ColumnType, Name, Schema};

/// A reference to a column by dotted path (`a` or `a.b.c`). Resolved against a
/// [`Schema`] during type-checking via [`Schema::resolve_path`](crate::Schema::resolve_path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColRef {
    /// The path segments (length 1 for a bare column).
    pub path: Vec<Name>,
}

impl ColRef {
    /// A bare single-segment column reference.
    #[must_use]
    pub fn col(name: impl Into<Name>) -> Self {
        Self {
            path: vec![name.into()],
        }
    }

    /// A dotted-path column reference.
    #[must_use]
    pub fn path(path: Vec<Name>) -> Self {
        Self { path }
    }
}

/// A comparison operator usable in a [`Predicate`] (RFD §3 operator set, the
/// comparison subset; logical `AND`/`OR`/`NOT` are structural in [`Predicate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `~` (regex match) — requires `Text`.
    Match,
}

impl CmpOp {
    /// Whether this operator is an ordering comparison (`< > <= >=`). Equality
    /// (`= <>`) is allowed on a wider set of types than ordering.
    #[must_use]
    pub fn is_ordering(self) -> bool {
        matches!(self, Self::Lt | Self::Gt | Self::Le | Self::Ge)
    }
}

/// A `LIKE` pattern (RFD §3). An owned wrapper so the predicate IR carries no parser
/// type; requires a `Text` operand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern(pub String);

/// A literal operand in a predicate (RFD §4). Mirrors the scalar [`ColumnType`]s; the
/// type-model's own literal, independent of the parser literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    /// A null literal (matches any nullable comparison context).
    Null,
    /// A boolean literal.
    Bool(bool),
    /// An integer literal.
    Int(i64),
    /// A float literal.
    Float(f64),
    /// A text literal.
    Text(String),
}

impl Literal {
    /// The [`ColumnType`] of this literal (`Null` ⇒ `Unknown`, RFD §4).
    #[must_use]
    pub fn type_of(&self) -> ColumnType {
        match self {
            Literal::Null => ColumnType::Unknown,
            Literal::Bool(_) => ColumnType::Bool,
            Literal::Int(_) => ColumnType::Int,
            Literal::Float(_) => ColumnType::Float,
            Literal::Text(_) => ColumnType::Text,
        }
    }
}

/// A predicate AST (RFD §4): comparisons against columns, combined with boolean
/// structure. Type-checked into a [`TypedPredicate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    /// `<col> <op> <literal>`
    Cmp(ColRef, CmpOp, Literal),
    /// `<lhs> AND <rhs>`
    And(Box<Predicate>, Box<Predicate>),
    /// `<lhs> OR <rhs>`
    Or(Box<Predicate>, Box<Predicate>),
    /// `NOT <inner>`
    Not(Box<Predicate>),
    /// `<col> IN (<literals>)`
    In(ColRef, Vec<Literal>),
    /// `<col> BETWEEN <low> AND <high>`
    Between(ColRef, Literal, Literal),
    /// `<col> LIKE <pattern>` — requires a `Text` column.
    Like(ColRef, Pattern),
}

/// A predicate decorated with the resolved column type of each [`ColRef`] (RFD §5).
/// Mirrors [`Predicate`] structurally; the planner uses the carried types for pushdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TypedPredicate {
    /// A type-checked comparison: the resolved column type is carried.
    Cmp {
        /// The column reference.
        col: ColRef,
        /// The resolved column type.
        col_ty: ColumnType,
        /// The operator.
        op: CmpOp,
        /// The literal operand.
        lit: Literal,
    },
    /// `AND`
    And(Box<TypedPredicate>, Box<TypedPredicate>),
    /// `OR`
    Or(Box<TypedPredicate>, Box<TypedPredicate>),
    /// `NOT`
    Not(Box<TypedPredicate>),
    /// `IN` with the resolved column type.
    In {
        /// The column reference.
        col: ColRef,
        /// The resolved column type.
        col_ty: ColumnType,
        /// The candidate literal set.
        set: Vec<Literal>,
    },
    /// `BETWEEN` with the resolved column type.
    Between {
        /// The column reference.
        col: ColRef,
        /// The resolved column type.
        col_ty: ColumnType,
        /// The lower bound.
        low: Literal,
        /// The upper bound.
        high: Literal,
    },
    /// `LIKE` (always a `Text` column once type-checked).
    Like {
        /// The column reference.
        col: ColRef,
        /// The pattern.
        pattern: Pattern,
    },
}

/// Whether `lhs` and `rhs` are comparable under `op` (the comparability matrix).
///
/// Rules (RFD §4/§5):
/// - `Unknown`/`Json` are comparable to anything (late-bound; resolved at runtime).
/// - Equality (`= <>`) holds between identical scalar types and `Int`/`Float`
///   (numeric widening).
/// - Ordering (`< > <= >=`) holds between identical orderable scalars and `Int`/`Float`.
/// - `~` (`Match`) requires both sides be `Text`.
/// - Nested `Struct`/`Array` are not comparable by these operators.
fn comparable(op: CmpOp, lhs: &ColumnType, rhs: &ColumnType) -> bool {
    use ColumnType::{Date, Decimal, Float, Int, Text, Timestamp};

    // Late-bound types defer the check to runtime.
    if matches!(lhs, ColumnType::Unknown | ColumnType::Json)
        || matches!(rhs, ColumnType::Unknown | ColumnType::Json)
    {
        return true;
    }

    // `~` regex match is Text-only on both sides.
    if op == CmpOp::Match {
        return matches!(lhs, Text) && matches!(rhs, Text);
    }

    let numeric = matches!(lhs, Int | Float | Decimal) && matches!(rhs, Int | Float | Decimal);
    let temporal = matches!(lhs, Timestamp | Date) && matches!(rhs, Timestamp | Date);

    if op.is_ordering() {
        // Ordering: numeric, temporal, or identical orderable scalars.
        numeric
            || temporal
            || (lhs == rhs && matches!(lhs, Text | Timestamp | Date | Decimal | Int | Float))
    } else {
        // Equality: numeric, temporal, or identical scalar types.
        numeric || temporal || (lhs == rhs && lhs.is_scalar())
    }
}

/// Resolve a [`ColRef`] to its column type, mapping resolution failures to a
/// [`TypeError`].
fn resolve_col(col: &ColRef, schema: &Schema) -> Result<ColumnType, TypeError> {
    schema.resolve_path(&col.path)
}

/// Type-check a [`Predicate`] against a [`Schema`], producing a [`TypedPredicate`]
/// decorated with resolved column types (RFD §4/§5). Rejects incomparable
/// comparisons (`Int < Text`), `~` on non-`Text`, and `LIKE` on non-`Text` with the
/// exact [`TypeError`] variant.
///
/// # Errors
/// - [`TypeError::UnknownColumn`] / [`TypeError::NotAStruct`] from column resolution.
/// - [`TypeError::IncomparableTypes`] when an operator does not type-check.
pub fn typecheck_predicate(p: &Predicate, schema: &Schema) -> Result<TypedPredicate, TypeError> {
    match p {
        Predicate::Cmp(col, op, lit) => {
            let col_ty = resolve_col(col, schema)?;
            let lit_ty = lit.type_of();
            if !comparable(*op, &col_ty, &lit_ty) {
                return Err(TypeError::IncomparableTypes {
                    op: *op,
                    lhs: col_ty,
                    rhs: lit_ty,
                });
            }
            Ok(TypedPredicate::Cmp {
                col: col.clone(),
                col_ty,
                op: *op,
                lit: lit.clone(),
            })
        }
        Predicate::And(a, b) => Ok(TypedPredicate::And(
            Box::new(typecheck_predicate(a, schema)?),
            Box::new(typecheck_predicate(b, schema)?),
        )),
        Predicate::Or(a, b) => Ok(TypedPredicate::Or(
            Box::new(typecheck_predicate(a, schema)?),
            Box::new(typecheck_predicate(b, schema)?),
        )),
        Predicate::Not(inner) => Ok(TypedPredicate::Not(Box::new(typecheck_predicate(
            inner, schema,
        )?))),
        Predicate::In(col, set) => {
            let col_ty = resolve_col(col, schema)?;
            for lit in set {
                let lit_ty = lit.type_of();
                if !comparable(CmpOp::Eq, &col_ty, &lit_ty) {
                    return Err(TypeError::IncomparableTypes {
                        op: CmpOp::Eq,
                        lhs: col_ty,
                        rhs: lit_ty,
                    });
                }
            }
            Ok(TypedPredicate::In {
                col: col.clone(),
                col_ty,
                set: set.clone(),
            })
        }
        Predicate::Between(col, low, high) => {
            let col_ty = resolve_col(col, schema)?;
            for (lit, bound_op) in [(low, CmpOp::Ge), (high, CmpOp::Le)] {
                let lit_ty = lit.type_of();
                if !comparable(bound_op, &col_ty, &lit_ty) {
                    return Err(TypeError::IncomparableTypes {
                        op: bound_op,
                        lhs: col_ty,
                        rhs: lit_ty,
                    });
                }
            }
            Ok(TypedPredicate::Between {
                col: col.clone(),
                col_ty,
                low: low.clone(),
                high: high.clone(),
            })
        }
        Predicate::Like(col, pattern) => {
            let col_ty = resolve_col(col, schema)?;
            // LIKE requires Text (or a late-bound Unknown/Json column).
            if !matches!(
                col_ty,
                ColumnType::Text | ColumnType::Unknown | ColumnType::Json
            ) {
                return Err(TypeError::IncomparableTypes {
                    op: CmpOp::Match,
                    lhs: col_ty,
                    rhs: ColumnType::Text,
                });
            }
            Ok(TypedPredicate::Like {
                col: col.clone(),
                pattern: pattern.clone(),
            })
        }
    }
}
