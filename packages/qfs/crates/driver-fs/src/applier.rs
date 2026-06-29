//! [`FsApplier`] — the driver's synchronous apply leg (RFD-0001 §6). It is the lone impure seam
//! the introspective [`crate::FsDriver`] hands back via `applier()`, and the
//! [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`.
//!
//! Stateless: every call performs fresh World I/O against the [`FsRoots`] allowlist and keeps no
//! in-process mutable accumulator, so it implements `SharedApplier` (`&self` apply). Because `fs`
//! widens blast radius beyond the `/local` sandbox, every write re-validates the resolved path is
//! inside a configured root **at apply time** (the resolve in each `fs_core` op), not only at scan
//! time — defence in depth.

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};
use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

use crate::effect::{FsEffect, CONTENT_COL, SRC_COL};
use crate::error::FsError;
use crate::fs_core::{self, FsRoots};

/// The synchronous `/fs` apply leg. Holds the root allowlist + the `read_only` flag (which gates
/// every write/effect with a structured [`FsError::CapabilityDenied`] **before** any I/O).
#[derive(Debug, Clone)]
pub struct FsApplier {
    roots: FsRoots,
    read_only: bool,
}

impl FsApplier {
    /// Build an applier over `roots`. `read_only` denies every mutating effect.
    #[must_use]
    pub fn new(roots: FsRoots, read_only: bool) -> Self {
        Self { roots, read_only }
    }

    /// Apply one decoded [`FsEffect`], returning the affected count (bytes/objects touched). The
    /// single place World I/O happens; each `fs_core` op re-resolves the path through the root
    /// allowlist, so an escape is refused at apply time too.
    fn apply_effect(&self, effect: &FsEffect) -> Result<u64, FsError> {
        // read_only gate: reject every mutating effect before touching the filesystem.
        if self.read_only && effect_is_mutating(effect) {
            return Err(FsError::CapabilityDenied {
                path: effect_path(effect).to_string(),
                verb: effect_verb(effect),
            });
        }
        match effect {
            FsEffect::Scan { path } => {
                let rows = if path.contains(['*', '?']) {
                    fs_core::resolve_glob(&self.roots, path)?
                } else {
                    fs_core::scan_dir(&self.roots, path)?
                };
                Ok(rows.len() as u64)
            }
            FsEffect::Write { dst, bytes } => fs_core::write_blob_atomic(&self.roots, dst, bytes),
            FsEffect::Copy { src, dst } => fs_core::copy_verify(&self.roots, src, dst),
            FsEffect::Move { src, dst } => {
                // copy→verify→unlink: the source is removed ONLY after the destination is
                // size+hash-verified, so a crash mid-move leaves the source intact (RFD §6).
                let n = fs_core::copy_verify(&self.roots, src, dst)?;
                fs_core::remove_blob(&self.roots, src)?;
                Ok(n)
            }
            FsEffect::Remove { path } => {
                fs_core::remove_blob(&self.roots, path)?;
                Ok(1)
            }
        }
    }
}

impl SharedApplier for FsApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect = FsEffect::from_node(node).map_err(|e| EffectError::terminal(e.reason))?;
        let affected = self.apply_effect(&effect)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for FsApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The fs applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`FsError`] is reduced to the plan crate's
    /// owned `(id, reason)` shape so no driver type leaks into `qfs-plan`.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect = FsEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.reason))?;
        let affected = self
            .apply_effect(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}

/// Whether an effect mutates the World (gated by `read_only`). `Scan` is the only read.
fn effect_is_mutating(effect: &FsEffect) -> bool {
    !matches!(effect, FsEffect::Scan { .. })
}

/// The primary path an effect acts on (for the structured capability-denied error).
fn effect_path(effect: &FsEffect) -> &str {
    match effect {
        FsEffect::Scan { path } | FsEffect::Remove { path } => path,
        FsEffect::Write { dst, .. } | FsEffect::Copy { dst, .. } | FsEffect::Move { dst, .. } => {
            dst
        }
    }
}

/// The stable verb label an effect maps to (for the capability-denied error).
fn effect_verb(effect: &FsEffect) -> &'static str {
    match effect {
        FsEffect::Scan { .. } => "LS",
        FsEffect::Write { .. } => "UPSERT",
        FsEffect::Copy { .. } => "CP",
        FsEffect::Move { .. } => "MV",
        FsEffect::Remove { .. } => "RM",
    }
}

/// Build a single-row [`RowBatch`] carrying a blob's bytes under [`CONTENT_COL`] — the payload an
/// `UPSERT INTO /fs/<root>/<path>` write effect expects.
#[must_use]
pub fn blob_write_args(bytes: Vec<u8>) -> RowBatch {
    let schema = Schema::new(vec![Column::new(CONTENT_COL, ColumnType::Bytes, false)]);
    RowBatch::new(schema, vec![Row::new(vec![Value::Bytes(bytes)])])
}

/// Build a single-row [`RowBatch`] marking a copy/move **source** under [`SRC_COL`] — the payload
/// a `cp`/`mv` write effect carries (move iff the node is flagged irreversible).
#[must_use]
pub fn copy_move_args(src_vfs: impl Into<String>) -> RowBatch {
    let schema = Schema::new(vec![Column::new(SRC_COL, ColumnType::Text, false)]);
    RowBatch::new(schema, vec![Row::new(vec![Value::Text(src_vfs.into())])])
}
