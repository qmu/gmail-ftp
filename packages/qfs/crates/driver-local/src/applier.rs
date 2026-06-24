//! [`LocalApplier`] — the driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::LocalFsDriver`] hands back via `applier()`, and
//! the [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`]
//! drives under `COMMIT`.
//!
//! Stateless: every call performs fresh World I/O against the [`Sandbox`] and keeps no
//! in-process mutable accumulator, so it implements `SharedApplier` (`&self` apply) — the
//! statelessness contract the bridge requires.

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};
use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

use crate::effect::{LocalEffect, CONTENT_COL, SRC_COL};
use crate::error::LocalError;
use crate::fs_core::{self, Sandbox};

/// The synchronous local-FS apply leg. Holds the sandbox + the `read_only` flag (which gates
/// every write/effect with a structured [`LocalError::CapabilityDenied`] **before** any I/O).
#[derive(Debug, Clone)]
pub struct LocalApplier {
    sandbox: Sandbox,
    read_only: bool,
}

impl LocalApplier {
    /// Build an applier over `sandbox`. `read_only` denies every mutating effect.
    #[must_use]
    pub fn new(sandbox: Sandbox, read_only: bool) -> Self {
        Self { sandbox, read_only }
    }

    /// Apply one decoded [`LocalEffect`], returning the affected count (bytes/objects
    /// touched). The single place World I/O happens.
    fn apply_effect(&self, effect: &LocalEffect) -> Result<u64, LocalError> {
        // read_only gate: reject every mutating effect before touching the filesystem.
        if self.read_only && effect_is_mutating(effect) {
            return Err(LocalError::CapabilityDenied {
                path: effect_path(effect).to_string(),
                verb: effect_verb(effect),
            });
        }
        match effect {
            LocalEffect::Scan { path } => {
                let rows = if path.contains(['*', '?']) {
                    fs_core::resolve_glob(&self.sandbox, path)?
                } else {
                    fs_core::scan_dir(&self.sandbox, path)?
                };
                Ok(rows.len() as u64)
            }
            LocalEffect::Write { dst, bytes } => {
                fs_core::write_blob_atomic(&self.sandbox, dst, bytes)
            }
            LocalEffect::Copy { src, dst } => fs_core::copy_verify(&self.sandbox, src, dst),
            LocalEffect::Move { src, dst } => {
                // copy→verify→unlink: the source is removed ONLY after the destination is
                // size-verified, so a crash mid-move leaves the source intact (RFD §6).
                let n = fs_core::copy_verify(&self.sandbox, src, dst)?;
                fs_core::remove_blob(&self.sandbox, src)?;
                Ok(n)
            }
            LocalEffect::Remove { path } => {
                fs_core::remove_blob(&self.sandbox, path)?;
                Ok(1)
            }
        }
    }
}

impl SharedApplier for LocalApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect = LocalEffect::from_node(node).map_err(|e| EffectError::terminal(e.reason))?;
        let affected = self.apply_effect(&effect)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for LocalApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous,
    /// `&mut self` apply leg. The local applier is stateless, so this delegates to the same
    /// `&self` core as [`SharedApplier::apply_shared`] — the two views never diverge. The
    /// structured [`LocalError`] is reduced to the plan crate's owned `(id, reason)` shape so
    /// no driver type leaks into `qfs-plan`.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect =
            LocalEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.reason))?;
        let affected = self
            .apply_effect(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}

/// Whether an effect mutates the World (gated by `read_only`). `Scan` is the only read.
fn effect_is_mutating(effect: &LocalEffect) -> bool {
    !matches!(effect, LocalEffect::Scan { .. })
}

/// The primary path an effect acts on (for the structured capability-denied error).
fn effect_path(effect: &LocalEffect) -> &str {
    match effect {
        LocalEffect::Scan { path } | LocalEffect::Remove { path } => path,
        LocalEffect::Write { dst, .. }
        | LocalEffect::Copy { dst, .. }
        | LocalEffect::Move { dst, .. } => dst,
    }
}

/// The stable verb label an effect maps to (for the capability-denied error).
fn effect_verb(effect: &LocalEffect) -> &'static str {
    match effect {
        LocalEffect::Scan { .. } => "LS",
        LocalEffect::Write { .. } => "UPSERT",
        LocalEffect::Copy { .. } => "CP",
        LocalEffect::Move { .. } => "MV",
        LocalEffect::Remove { .. } => "RM",
    }
}

/// Build a single-row [`RowBatch`] carrying a blob's bytes under [`CONTENT_COL`] — the
/// payload an `UPSERT INTO /local/<path>` write effect expects (the evaluator/tests build
/// this; the applier reads it back). Helper so a caller never hand-rolls the column shape.
#[must_use]
pub fn blob_write_args(bytes: Vec<u8>) -> RowBatch {
    let schema = Schema::new(vec![Column::new(CONTENT_COL, ColumnType::Bytes, false)]);
    RowBatch::new(schema, vec![Row::new(vec![Value::Bytes(bytes)])])
}

/// Build a single-row [`RowBatch`] marking a copy/move **source** under [`SRC_COL`] — the
/// payload a `cp`/`mv` write effect carries (move iff the node is flagged irreversible).
#[must_use]
pub fn copy_move_args(src_vfs: impl Into<String>) -> RowBatch {
    let schema = Schema::new(vec![Column::new(SRC_COL, ColumnType::Text, false)]);
    RowBatch::new(schema, vec![Row::new(vec![Value::Text(src_vfs.into())])])
}
