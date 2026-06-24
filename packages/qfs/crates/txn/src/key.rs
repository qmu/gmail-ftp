//! Deterministic idempotency keys (RFD-0001 §6 retry-safety).
//!
//! An [`EffectKey`] is a stable, content-addressed fingerprint of a single write effect:
//! `(plan_id, effect_id, canonical(target, kind, args))`. It is **stable across retries and
//! across batch reordering** — the same logical effect always derives the same key — so the
//! audit ledger can answer "have I already applied this?" and make a re-delivered webhook /
//! resumed saga a no-op ([`LegOutcome::AlreadyApplied`](crate::LegOutcome::AlreadyApplied)).
//!
//! Determinism comes from canonical serialization: the plan node's owned DTOs all derive
//! `Serialize` with declaration-ordered fields, so `serde_json` emits a byte-identical
//! canonical form for equal effects regardless of when or in what batch they run. The hash
//! is FNV-1a (64-bit) over those canonical bytes — a pure, dependency-free, stable function
//! (no `RandomState`, no per-process seed), so the golden hash is reproducible across runs.

use qfs_plan::{EffectKind, EffectNode};
use serde::Serialize;

/// A deterministic idempotency key for one write effect (RFD §6).
///
/// Equal effects (same plan, same node, same target+kind+args) derive an equal key; the key
/// is the ledger's dedup handle. Rendered as `k:<plan>:<effect>:<hash16>` so it is readable
/// in a `-json` recovery report yet collision-resistant on the content hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct EffectKey(pub String);

impl EffectKey {
    /// Derive the key for `node` within plan `plan_id`. Pure and deterministic: the same
    /// inputs always yield the same key, independent of wall-clock time, process, or the
    /// batch/parallel order the runtime chose.
    #[must_use]
    pub fn derive(plan_id: &str, node: &EffectNode) -> Self {
        let canon = Canonical {
            plan_id,
            effect_id: node.id.index(),
            kind: node.kind.label(),
            // Folds the proc id in for CALL so two distinct procedures never collide.
            proc: match &node.kind {
                EffectKind::Call(p) => Some(p.as_str()),
                _ => None,
            },
            driver: node.target.driver.as_str(),
            path: node.target.path.as_str(),
            // The args are the canonical payload fingerprint; serde emits them
            // declaration-ordered so equal batches hash identically.
            args: &node.args,
        };
        // Canonical bytes: serde_json over owned DTOs is declaration-ordered and stable.
        // A serialization failure (impossible for these owned, non-Map DTOs) degrades to an
        // empty fingerprint rather than panicking — the lib stays panic-free.
        let bytes = serde_json::to_vec(&canon).unwrap_or_default();
        let hash = fnv1a64(&bytes);
        Self(format!("k:{plan_id}:{}:{hash:016x}", node.id.index()))
    }

    /// The key as a string slice (the ledger/report handle).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EffectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The canonical, declaration-ordered projection an [`EffectKey`] hashes over. Private — the
/// only consumer is the hash; its field order IS the canonical order.
#[derive(Serialize)]
struct Canonical<'a> {
    plan_id: &'a str,
    effect_id: u32,
    kind: &'a str,
    proc: Option<&'a str>,
    driver: &'a str,
    path: &'a str,
    args: &'a qfs_types::RowBatch,
}

/// FNV-1a 64-bit over a byte slice — a stable, seed-free, dependency-free hash so the
/// derived key is reproducible across runs and processes (unlike `DefaultHasher`, which is
/// `RandomState`-seeded). Deterministic golden-hash friendly.
#[must_use]
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
