//! **Context** built-ins (RFD-0001 §3/§8/§10, ticket t08): `NOW()`, `CURRENT_DATE()`,
//! `LAST_RUN()`, and `env(name)`. These read the **frozen, read-only** [`EvalCtx`] — never
//! the wall clock or a live process environment — so PREVIEW is reproducible and golden
//! tests are stable (determinism). `env()` is **capability-gated** (RFD §10
//! least-privilege): with the gate off it is denied, and a denied lookup never leaks the
//! value it would have returned.

use cfs_types::{ColumnType, Value};

use super::{BuiltinFn, EvalCtx, FnError, FnSig};

/// A read-only environment source `env(name)` resolves through (RFD §10). The server can
/// restrict it per-handler; the default ([`NoEnv`]) denies everything (default-deny in
/// unattended contexts). Implementors return **only** owned data — never log a value.
pub trait EnvSource {
    /// The value bound to `name`, or `None` if unset. Returning `None` is "unset"; the
    /// **capability gate** (not this method) decides whether the lookup is *permitted*.
    fn get(&self, name: &str) -> Option<String>;
}

/// The default deny-all environment source: every `env(...)` is unset. Used in pure-eval
/// and unattended contexts so no real process environment is ever read.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEnv;

impl EnvSource for NoEnv {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

/// A stub, in-memory [`EnvSource`] (for tests / a server that injects a fixed, vetted
/// allow-list). Never reads the real process environment — owned data only.
#[derive(Debug, Clone, Default)]
pub struct MapEnv {
    entries: Vec<(String, String)>,
}

impl MapEnv {
    /// An empty stub environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `value` (builder form).
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.entries.push((name.into(), value.into()));
        self
    }
}

impl EnvSource for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

/// The set of context built-ins, in stable (name) order.
pub(super) fn context_builtins() -> Vec<BuiltinFn> {
    vec![
        BuiltinFn::scalar("NOW", FnSig::fixed(0, ColumnType::Timestamp), now),
        BuiltinFn::scalar(
            "CURRENT_DATE",
            FnSig::fixed(0, ColumnType::Date),
            current_date,
        ),
        BuiltinFn::scalar("LAST_RUN", FnSig::fixed(0, ColumnType::Timestamp), last_run),
        BuiltinFn::scalar("env", FnSig::fixed(1, ColumnType::Text), env),
    ]
}

/// `NOW()` — the frozen per-statement timestamp (epoch seconds). Two calls in one
/// statement are **equal** (determinism); never the live wall clock.
fn now(_args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    Ok(Value::Timestamp(ctx.now))
}

/// `CURRENT_DATE()` — the frozen per-statement date (epoch days). Deterministic, like
/// `NOW()`.
fn current_date(_args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    Ok(Value::Int(ctx.current_date))
}

/// `LAST_RUN()` — the injected last-successful-run timestamp (RFD §8 job state), or `Null`
/// when unset. Injected state, never ambient.
fn last_run(_args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    Ok(ctx.last_run.map_or(Value::Null, Value::Timestamp))
}

/// `env(name)` — a capability-gated environment lookup (RFD §10). With the gate **off**
/// (the pure-eval default) it is denied with [`FnError::CapabilityDenied`] carrying only
/// the *name* (never the value). With the gate on, it resolves through the [`EnvSource`],
/// returning `Null` for an unset name.
fn env(args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    env_value(args, ctx)
}

/// The shared `env(name)` resolution (exposed so the evaluator can call it directly).
/// Capability-gated; secret-free errors.
pub fn env_value(args: &[Value], ctx: &EvalCtx) -> Result<Value, FnError> {
    let name = match args.first() {
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Null) | None => {
            return Err(FnError::Domain {
                name: "env".to_string(),
                reason: "name_must_be_text",
            })
        }
        Some(other) => {
            return Err(FnError::Type {
                name: "env".to_string(),
                expected: "Text",
                found: super::value_type_label(other),
            })
        }
    };
    if !ctx.capabilities_enabled {
        // Least-privilege: deny outside an authorised handler. Carry only the requested
        // NAME, never the value `env` would return (RFD §10 — no secret in the error).
        return Err(FnError::CapabilityDenied {
            builtin: "env",
            requested: name,
        });
    }
    Ok(ctx.env.get(&name).map_or(Value::Null, Value::Text))
}
