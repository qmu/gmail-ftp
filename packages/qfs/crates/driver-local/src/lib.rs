//! `qfs-driver-local` — the **first concrete driver** (RFD-0001 §5): a blob/namespace
//! driver over the host filesystem, mounted at `/local`. It is the reference implementation
//! of the t13 [`qfs_driver::Driver`] contract and the simplest member of the Blob/namespace
//! archetype (native verbs `ls cp mv rm`, plus `upsert`/`remove` as universal writes).
//!
//! Because it needs no network, no credentials, and no vendor SDK, it proves the whole
//! driver contract **and** the runtime's effect-plan path end-to-end. It is also the anchor
//! that makes cross-mount `cp` work (RFD §1/§7): every cloud driver's upload/download is a
//! cross-source plan with `/local/...` on one side, so the streaming copy→verify→[delete]
//! recovery shape lands here first.
//!
//! ## Surface
//! - [`LocalFsDriver`] — the introspective `Driver`: `mount()` = `/local`, archetype
//!   [`Archetype::BlobNamespace`], the [`LocalRow`] listing schema, capabilities
//!   `{ls,cp,mv,rm,upsert,remove}` (narrowed to `{ls}` on a `read_only` mount), pushdown
//!   `Partial{project}` (it can project the listing name set; filtering/glob is its own
//!   scan), no procedures, no prelude.
//! - [`LocalApplier`] — the synchronous apply leg the contract hands back via `applier()`
//!   *and* the [`qfs_runtime::SharedApplier`] the runtime bridge drives.
//! - [`local_apply_driver`] — the convenience that wraps a [`LocalFsDriver`]'s applier in a
//!   [`qfs_runtime::PlanApplierBridge`], ready to `register` into a `DriverRegistry` so a
//!   plan over `/local` executes end-to-end through the t10 interpreter.
//!
//! ## Codecs (RFD §4)
//! This crate holds **no** format-specific code. A local `.md`/`.json`/`.csv` blob becomes a
//! queryable relation by reading its bytes ([`fs_core::read_blob`]) and decoding them with a
//! registered [`qfs_codec::Codec`] — pure `bytes ↔ rows`, independent of driver identity.
//!
//! ## Sandbox (RFD §10)
//! Every path crosses the [`fs_core::Sandbox`] resolve, which rejects `..`/symlink escapes
//! with [`LocalError::OutsideSandbox`] and performs no I/O on a rejected path. `root` is the
//! blast-radius boundary.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
mod effect;
mod error;
pub mod fs_core;
pub mod read;
mod row;

use std::path::PathBuf;
use std::sync::Arc;

use qfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::{blob_write_args, copy_move_args, LocalApplier};
pub use effect::{LocalEffect, CONTENT_COL, SRC_COL};
pub use error::LocalError;
pub use fs_core::Sandbox;
pub use read::scan_rows;
pub use row::LocalRow;

/// The local-filesystem driver (RFD §5). Owns the sandbox `root` (the least-privilege
/// boundary) and the `read_only` flag, plus the synchronous [`LocalApplier`] the contract
/// returns from `applier()`. Construct with [`LocalFsDriver::new`] (writable) or
/// [`LocalFsDriver::read_only`].
pub struct LocalFsDriver {
    read_only: bool,
    applier: LocalApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl LocalFsDriver {
    /// Build a **writable** driver confined to `root` (the sandbox boundary).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::build(Sandbox::new(root.into()), false)
    }

    /// Build a **read-only** driver confined to `root`: every write/effect is denied with a
    /// structured capability error and touches no files.
    #[must_use]
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        Self::build(Sandbox::new(root.into()), true)
    }

    fn build(sandbox: Sandbox, read_only: bool) -> Self {
        Self {
            read_only,
            applier: LocalApplier::new(sandbox, read_only),
            // A blob namespace pushes projection (the listing name subset) down to its own
            // scan; WHERE/glob filtering is the scan's own work, not a native predicate API,
            // so the rest stays local.
            pushdown: PushdownProfile::Partial {
                where_: false,
                project: true,
                limit: false,
                order: false,
                join: false,
                aggregate: false,
                distinct: false,
                group_by: false,
            },
            // A pure blob/namespace driver declares no `CALL` procedures.
            procs: Vec::new(),
        }
    }

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to
    /// build the runtime bridge).
    #[must_use]
    pub fn local_applier(&self) -> &LocalApplier {
        &self.applier
    }

    /// The capability set for this mount: a writable mount supports `{ls,cp,mv,rm,upsert,
    /// remove}`; a `read_only` mount narrows to `{ls}` so every mutating verb is rejected at
    /// the parse-time gate (`qfs_driver::check_capability`).
    #[must_use]
    fn caps(&self) -> Capabilities {
        if self.read_only {
            Capabilities::from_verbs(&[Verb::Ls])
        } else {
            Capabilities::from_verbs(&[
                Verb::Ls,
                Verb::Cp,
                Verb::Mv,
                Verb::Rm,
                Verb::Upsert,
                Verb::Remove,
            ])
        }
    }
}

impl Driver for LocalFsDriver {
    fn mount(&self) -> &str {
        fs_core::MOUNT
    }

    fn describe(&self, _path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // A `/local` node is the blob namespace; its listing relation is the LocalRow schema.
        // Pure: builds data, no I/O.
        Ok(NodeDesc::new(Archetype::BlobNamespace, LocalRow::schema()))
    }

    fn capabilities(&self, _path: &Path) -> Capabilities {
        self.caps()
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`LocalFsDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`],
/// yielding the async `ApplyDriver` ready to `register` into a `DriverRegistry` under the
/// driver's id. This is the end-to-end seam: a plan routed to `/local` (driver id `local`)
/// executes through the t10 interpreter, which dispatches each effect to this bridge.
#[must_use]
pub fn local_apply_driver(driver: &LocalFsDriver) -> PlanApplierBridge<LocalApplier> {
    PlanApplierBridge::new(Arc::new(driver.local_applier().clone()))
}

#[cfg(test)]
mod tests;
