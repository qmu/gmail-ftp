//! `qfs-driver-fs` — the **first-class filesystem driver** (t68, RFD-0001 §5): the real host
//! filesystem as a [`Archetype::BlobNamespace`] mounted at `/fs`, addressed under
//! operator-configured **named roots** rather than the fixed `/local` sandbox.
//!
//! ## Why `fs` exists beyond `/local`
//! `/local` (t28) is a sandboxed convenience over a single fixed root. `fs` addresses files on the
//! machine as ordinary paths under an operator-chosen **allowlist of roots**: a VFS path is
//! `/fs/<root>/<rel…>`, where `<root>` selects one configured base and `<rel…>` resolves under it.
//! That widens blast radius beyond a sandbox, so the security floor is the headline (t68): explicit
//! root scoping, hard rejection of `..`/absolute/symlink escapes **validated at BOTH scan and apply
//! time**, `POLICY` scopability per root (e.g. `ALLOW SELECT ON 'fs/projects/*'`), and `PREVIEW`
//! before commit. `REMOVE` deletes a real file and is **irreversible** (needs the extra ack); it is
//! never reclassified as reversible. The **default is deny-all** — with no root configured nothing
//! resolves (no implicit whole-disk access).
//!
//! ## Surface (templated structure-for-structure on `qfs-driver-local`)
//! - [`FsDriver`] — the introspective `Driver`: `mount()` = `/fs`, archetype
//!   [`Archetype::BlobNamespace`], the [`FsRow`] listing schema, capabilities
//!   `{ls,cp,mv,rm,upsert,remove}` (narrowed to `{ls}` on a `read_only` mount), pushdown
//!   `Partial{project}`, no procedures, no prelude. **Pure + cred-free + path-free**: the roots are
//!   injected from the binary, so the introspective half names no absolute host path and does no
//!   I/O (keeping it wasm-buildable and the purity proof green).
//! - [`FsApplier`] — the synchronous apply leg the contract hands back via `applier()` *and* the
//!   [`qfs_runtime::SharedApplier`] the runtime bridge drives.
//! - [`fs_apply_driver`] — wraps a [`FsDriver`]'s applier in a [`qfs_runtime::PlanApplierBridge`],
//!   ready to `register` into a `DriverRegistry` so a plan over `/fs` executes end-to-end.
//!
//! ## Confinement (RFD §10)
//! Every path crosses [`fs_core::FsRoots::resolve`], which rejects unknown-root / `..` / symlink
//! escapes with [`FsError::UnknownRoot`]/[`FsError::OutsideRoot`] and performs no I/O on a rejected
//! path. The apply leg re-validates through the same resolve — defence in depth.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
mod effect;
mod error;
pub mod fs_core;
pub mod read;
mod row;

use std::sync::Arc;

use qfs_driver::{Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::{blob_write_args, copy_move_args, FsApplier};
pub use effect::{FsEffect, CONTENT_COL, SRC_COL};
pub use error::FsError;
pub use fs_core::FsRoots;
pub use read::scan_rows;
pub use row::FsRow;

/// The first-class filesystem driver (RFD §5). Owns the operator-configured [`FsRoots`] allowlist
/// (the least-privilege boundary) and the `read_only` flag, plus the synchronous [`FsApplier`] the
/// contract returns from `applier()`. Construct with [`FsDriver::new`] (writable) or
/// [`FsDriver::read_only`].
pub struct FsDriver {
    read_only: bool,
    applier: FsApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl FsDriver {
    /// Build a **writable** driver confined to the configured `roots` (the allowlist boundary). An
    /// empty `roots` is deny-all (no implicit whole-disk access).
    #[must_use]
    pub fn new(roots: FsRoots) -> Self {
        Self::build(roots, false)
    }

    /// Build a **read-only** driver confined to `roots`: every write/effect is denied with a
    /// structured capability error and touches no files.
    #[must_use]
    pub fn read_only(roots: FsRoots) -> Self {
        Self::build(roots, true)
    }

    fn build(roots: FsRoots, read_only: bool) -> Self {
        Self {
            read_only,
            applier: FsApplier::new(roots, read_only),
            // A blob namespace pushes projection (the listing name subset) down to its own scan;
            // WHERE/glob filtering is the scan's own work, not a native predicate API, so the rest
            // stays local.
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

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn fs_applier(&self) -> &FsApplier {
        &self.applier
    }

    /// The capability set for this mount: a writable mount supports `{ls,cp,mv,rm,upsert,remove}`;
    /// a `read_only` mount narrows to `{ls}` so every mutating verb is rejected at the parse-time
    /// gate (`qfs_driver::check_capability`).
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

impl Driver for FsDriver {
    fn mount(&self) -> &str {
        fs_core::MOUNT
    }

    fn describe(&self, _path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // A `/fs` node is the blob namespace; its listing relation is the FsRow schema. Pure:
        // builds data, no I/O — names no host path (the roots are injected, never described).
        Ok(NodeDesc::new(Archetype::BlobNamespace, FsRow::schema()))
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

/// Wrap a [`FsDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding the
/// async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver's id (`fs`).
/// This is the end-to-end seam: a plan routed to `/fs` executes through the t10 interpreter, which
/// dispatches each effect to this bridge.
#[must_use]
pub fn fs_apply_driver(driver: &FsDriver) -> PlanApplierBridge<FsApplier> {
    PlanApplierBridge::new(Arc::new(driver.fs_applier().clone()))
}

#[cfg(test)]
mod tests;
