//! Frontier **coalescing** (RFD-0001 §6 auto-batching): group a ready-set of effect nodes
//! by their stable `(DriverId, EffectKind)` key into one [`BatchGroup`] per key, preserving
//! per-effect identity for result fan-out. This is the mechanism that collapses N
//! independent same-kind leaf effects into a single batched driver call (N+1 → 1).
//!
//! The grouping key is derived from **owned DTOs only** — the [`DriverId`] and the
//! [`EffectKind`] label — never a vendor SDK type (RFD §9). Grouping happens across the
//! *whole* ready-set (not pairwise) so the collapse is complete.

use std::collections::BTreeMap;

use qfs_plan::{EffectKind, EffectNode, NodeId};
use qfs_types::DriverId;

use crate::driver::EffectInput;

/// The stable, owned grouping key. `EffectKind` is rendered to its stable label so a
/// `Call(proc)` groups by `CALL` *and* the specific proc — two different procedures on the
/// same driver are distinct batches (they hit different endpoints), while N rows of the same
/// `INSERT` target coalesce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupKey {
    /// The driver the batch is dispatched to.
    pub driver: DriverId,
    /// The effect-kind label (`READ`/`INSERT`/`CALL`/…). For `Call`, the proc id is folded
    /// in so distinct procedures do not merge.
    pub kind_label: String,
}

impl GroupKey {
    /// Derive the owned key for a node. Pure; touches no vendor type.
    #[must_use]
    pub fn of(node: &EffectNode) -> Self {
        let kind_label = match &node.kind {
            EffectKind::Call(proc) => format!("CALL:{}", proc.as_str()),
            other => other.label().to_string(),
        };
        Self {
            driver: node.target.driver.clone(),
            kind_label,
        }
    }
}

/// One coalesced batch: a homogeneous set of effects sharing a [`GroupKey`], ready to be
/// dispatched to the driver in a single [`ApplyDriver::apply_batch`](crate::ApplyDriver::apply_batch)
/// call. Carries the representative [`EffectKind`] (all members share it) and the owned
/// inputs in stable [`NodeId`] order, so result fan-out is by position.
#[derive(Debug, Clone)]
pub struct BatchGroup {
    /// The owned grouping key.
    pub key: GroupKey,
    /// The representative effect kind (every member shares it).
    pub kind: EffectKind,
    /// The owned per-effect inputs, in ascending [`NodeId`] order (the fan-out order).
    pub inputs: Vec<EffectInput>,
}

impl BatchGroup {
    /// The node ids in this group, in dispatch order — the keys the scheduler maps results
    /// back onto.
    #[must_use]
    pub fn ids(&self) -> Vec<NodeId> {
        self.inputs.iter().map(|i| i.id).collect()
    }
}

/// Coalesce a ready-set of nodes into batch groups keyed by `(DriverId, EffectKind)`.
///
/// Grouping is over the **entire** input slice at once (not pairwise), so all N independent
/// same-key effects land in one [`BatchGroup`] — the property the batching assertion checks.
/// Groups are returned in deterministic [`GroupKey`] order, and members within a group are
/// in ascending [`NodeId`] order, so dispatch is reproducible.
#[must_use]
pub fn coalesce(nodes: &[&EffectNode]) -> Vec<BatchGroup> {
    let mut by_key: BTreeMap<GroupKey, BatchGroup> = BTreeMap::new();
    for node in nodes {
        let key = GroupKey::of(node);
        let group = by_key.entry(key.clone()).or_insert_with(|| BatchGroup {
            key,
            kind: node.kind.clone(),
            inputs: Vec::new(),
        });
        group.inputs.push(EffectInput::from_node(node));
    }
    // Ensure within-group order is by NodeId for determinism (BTreeMap orders the keys).
    for group in by_key.values_mut() {
        group.inputs.sort_by_key(|i| i.id);
    }
    by_key.into_values().collect()
}
