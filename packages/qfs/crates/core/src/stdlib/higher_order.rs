//! The **higher-order** built-ins `map` / `filter` / `reduce` (RFD-0001 §3, M6 ticket t61).
//!
//! These are the entries that make the closed core's "functions are values" promise
//! (decision H) concrete: each takes a **function-typed** argument — a lambda
//! ([`qfs_parser::Expr::Lambda`]) — alongside a collection, and transforms the collection by
//! applying that lambda. Registering them here, in the open [`StdlibRegistry`], is the whole
//! point: a powerful new capability (inline transformation that today needs an external
//! script) is added with **zero** new keywords — they are ordinary registry functions, not
//! grammar.
//!
//! Unlike a scalar built-in, a higher-order built-in cannot be a `Value → Value`
//! [`BuiltinEval::Scalar`] (a lambda is not a [`Value`]). So they register as
//! [`BuiltinEval::HigherOrder`] markers carrying a [`HigherOrderKind`]: this module supplies
//! their **signatures** (arity + return type) for the typing pass / the function registry,
//! while the actual closure-application semantics live in the pure lambda evaluator
//! ([`crate::lambda`]), which dispatches on the kind. Everything stays pure — values in,
//! values out, no I/O (RFD §3 purity), so a `map`/`filter`/`reduce` over a relation never
//! constructs an effect node and the safety floor is untouched.

use qfs_types::ColumnType;

use super::{BuiltinFn, FnSig, HigherOrderKind};

/// The higher-order built-ins, in stable (name) order.
///
/// - `map(collection, fn)` — apply `fn` to each element; same-length collection out
///   (`Array`).
/// - `filter(collection, fn)` — keep the elements whose `fn` result is truthy; an `Array`
///   out.
/// - `reduce(collection, fn[, init])` — left-fold the elements through `fn(acc, element)`;
///   the accumulator type out (late-bound `Unknown`, since it follows `init`/the lambda).
pub(super) fn higher_order_builtins() -> Vec<BuiltinFn> {
    vec![
        BuiltinFn::higher_order(
            "map",
            FnSig::fixed(2, ColumnType::Array(Box::new(ColumnType::Unknown))),
            HigherOrderKind::Map,
        ),
        BuiltinFn::higher_order(
            "filter",
            FnSig::fixed(2, ColumnType::Array(Box::new(ColumnType::Unknown))),
            HigherOrderKind::Filter,
        ),
        // `reduce` accepts an optional initial accumulator: `reduce(coll, fn)` folds from the
        // first element, `reduce(coll, fn, init)` folds from `init` (the cookbook form).
        BuiltinFn::higher_order(
            "reduce",
            FnSig::range(2, 3, ColumnType::Unknown),
            HigherOrderKind::Reduce,
        ),
    ]
}
