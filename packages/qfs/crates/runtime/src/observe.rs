//! Observability surface for the transactional commit (RFD-0001 §6 observability, §10 audit).
//!
//! A single [`TraceId`] is minted per `commit_txn` execution and threaded through the
//! per-plan root span and every per-leg child span, so every emitted log line for an applied
//! effect carries `trace_id`, `plan_id`, and `effect.id`. The id is an **owned, vendor-free**
//! token (no SDK trace handle leaks) and is **deterministic per (plan_id, sequence)** so the
//! observability output is reproducible in tests — there is no wall-clock or RNG dependence
//! here (the durable file ledger + real ULID minting are the E8 deployment ticket; this is the
//! in-tree `tracing` surface the interpreter emits through today).
//!
//! Secret-free invariant (RFD §10): spans/events carry **identity and shape only** — the
//! plan id, the trace id, the node id, the driver, the effect kind label, and counts — never a
//! payload, credential, version literal beyond an opaque token, or row body.

use std::sync::atomic::{AtomicU64, Ordering};

/// A per-execution trace id (RFD §6) threaded through the plan → effect → external-call span
/// hierarchy so external traces correlate. Owned text; never a vendor trace handle.
///
/// The id is derived from the `plan_id` plus a process-monotonic sequence, so two executions
/// of the same plan in one process get distinct, ordered ids while a single execution's id is
/// stable across its spans. It carries **no** wall-clock component, keeping the observability
/// output deterministic for golden tests (real ULID minting is deferred to the E8 deployment
/// ledger).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(String);

/// Process-monotonic counter seeding the per-execution trace sequence (deterministic, no RNG).
static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

impl TraceId {
    /// Mint a new trace id for one execution of `plan_id`. Monotonic within the process.
    #[must_use]
    pub fn mint(plan_id: &str) -> Self {
        let seq = TRACE_SEQ.fetch_add(1, Ordering::Relaxed);
        Self(format!("t:{plan_id}:{seq:08x}"))
    }

    /// Construct a trace id from an explicit owned token (e.g. a server-supplied request id
    /// propagated as the trace root). No parsing — the token is opaque.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The id as a string slice (the span/log field value).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_is_monotonic_and_carries_plan_id() {
        let a = TraceId::mint("plan-x");
        let b = TraceId::mint("plan-x");
        assert!(a.as_str().starts_with("t:plan-x:"));
        assert_ne!(a, b, "two mints differ");
        // No payload/secret material in the rendered id — only the plan id + a hex sequence.
        assert!(!a.as_str().contains("secret"));
    }

    #[test]
    fn trace_id_from_token_is_opaque() {
        let t = TraceId::from_token("req-abc-123");
        assert_eq!(t.as_str(), "req-abc-123");
    }
}
