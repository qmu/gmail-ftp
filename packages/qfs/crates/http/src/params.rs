//! Typed parameter binding (t32): the request's path / query-string / (read) body params
//! become owned, **typed** [`qfs_core::Value`]s collected in [`QueryArgs`]. These are the
//! values the injection-safe AST rewrite ([`crate::rewrite`]) substitutes into the pre-parsed
//! endpoint query.
//!
//! ## Injection safety starts here
//! A param value is converted to a typed [`qfs_core::Value`] by *literal inference* (an
//! integer-looking token → [`Value::Int`], `true`/`false` → [`Value::Bool`], else
//! [`Value::Text`]). It is NEVER concatenated into DSL source text. A malicious token like
//! `'; REMOVE /mail/inbox` simply becomes `Value::Text("'; REMOVE /mail/inbox")` — one typed
//! scalar — which the rewrite drops in as a single `Literal::Str` AST node (see
//! [`crate::rewrite`]). There is no parse step over the request value, so no injection surface.

use std::collections::BTreeMap;

use qfs_core::Value;

/// The owned, typed argument set bound from one request. Keyed by declared param name; the
/// value is the typed scalar the AST rewrite substitutes. Deterministic ([`BTreeMap`]) so the
/// rewrite and any golden are stable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryArgs {
    args: BTreeMap<String, Value>,
}

impl QueryArgs {
    /// An empty argument set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a typed value for `name` (builder form).
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: Value) -> Self {
        self.args.insert(name.into(), value);
        self
    }

    /// Insert a typed value for `name`.
    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        self.args.insert(name.into(), value);
    }

    /// The typed value bound for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.args.get(name)
    }

    /// Whether `name` has a bound value.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.args.contains_key(name)
    }

    /// The bound param names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.args.keys().map(String::as_str).collect()
    }

    /// Number of bound params.
    #[must_use]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.args.is_empty()
    }

    /// Bind the declared params from a request's already-extracted sources, validating against
    /// the endpoint's `declared` param names. Path params win over query-string over body
    /// (the most specific source wins) — but a name supplied by MORE THAN the declared set is
    /// an `Extra` error, and a declared param with NO source is a `Missing` error.
    ///
    /// `path_params` are the named segments matched from the route; `query` is the parsed
    /// query string; `body` is the optional decoded body param map (read endpoints).
    ///
    /// # Errors
    /// [`BindError`] (missing / extra / type-mismatch) naming the offending param.
    pub fn bind(
        declared: &[String],
        path_params: &BTreeMap<String, String>,
        query: &BTreeMap<String, String>,
        body: &BTreeMap<String, String>,
    ) -> Result<Self, BindError> {
        let declared_set: std::collections::BTreeSet<&str> =
            declared.iter().map(String::as_str).collect();

        // 1. Reject an EXTRA param: a supplied param (from any source) the query did not
        //    declare. Path params are structural (they came from the matched route, so they
        //    are declared by construction); query/body params are caller-supplied, so an
        //    undeclared one is rejected (closed contract — RFD §5/§6 honest typing).
        for src in [query, body] {
            for key in src.keys() {
                if !declared_set.contains(key.as_str()) {
                    return Err(BindError::extra(key));
                }
            }
        }

        // 2. Bind each declared param from the most-specific available source.
        let mut args = QueryArgs::new();
        for name in declared {
            let raw = path_params
                .get(name)
                .or_else(|| query.get(name))
                .or_else(|| body.get(name));
            match raw {
                Some(token) => args.set(name, infer_value(token)),
                None => return Err(BindError::missing(name)),
            }
        }
        Ok(args)
    }
}

/// Infer a typed [`qfs_core::Value`] from a raw param token. The inference is intentionally
/// conservative and LOSSLESS for text: anything that is not unambiguously an int/bool stays
/// [`Value::Text`]. This is the typed boundary — the token is never re-parsed as DSL, so the
/// inference cannot widen the injection surface (a quote/keyword-bearing token is just text).
#[must_use]
pub fn infer_value(token: &str) -> Value {
    if let Ok(i) = token.parse::<i64>() {
        return Value::Int(i);
    }
    match token {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    Value::Text(token.to_string())
}

/// A structured parameter-bind failure — always names the offending param so the caller (or an
/// AI agent) can correct the request. Maps to HTTP 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// A declared param had no value in path / query / body.
    Missing {
        /// The missing param name.
        param: String,
    },
    /// A supplied param the endpoint query did not declare.
    Extra {
        /// The unexpected param name.
        param: String,
    },
    /// A param value could not be coerced to the type the query expects.
    TypeMismatch {
        /// The param name.
        param: String,
        /// A secret-free description of the expected vs supplied type.
        detail: String,
    },
}

impl BindError {
    /// A missing-param error.
    #[must_use]
    pub fn missing(param: impl Into<String>) -> Self {
        BindError::Missing {
            param: param.into(),
        }
    }

    /// An extra-param error.
    #[must_use]
    pub fn extra(param: impl Into<String>) -> Self {
        BindError::Extra {
            param: param.into(),
        }
    }

    /// A type-mismatch error.
    #[must_use]
    pub fn type_mismatch(param: impl Into<String>, detail: impl Into<String>) -> Self {
        BindError::TypeMismatch {
            param: param.into(),
            detail: detail.into(),
        }
    }

    /// The offending param name (always present — the 400 body names it).
    #[must_use]
    pub fn param(&self) -> &str {
        match self {
            BindError::Missing { param }
            | BindError::Extra { param }
            | BindError::TypeMismatch { param, .. } => param,
        }
    }

    /// A secret-free detail message for the problem body.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            BindError::Missing { param } => format!("missing required parameter `{param}`"),
            BindError::Extra { param } => format!("unexpected parameter `{param}`"),
            BindError::TypeMismatch { param, detail } => {
                format!("parameter `{param}` type mismatch: {detail}")
            }
        }
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl std::error::Error for BindError {}
