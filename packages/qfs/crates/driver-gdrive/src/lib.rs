//! `qfs-driver-gdrive` — the **Google Drive blob/namespace `Driver`** (RFD-0001 §5, E4 t21) and,
//! with Gmail (t20), one of the two drivers that **subsume** the legacy Go `gdrive-ftp` tool: it
//! is *reimplemented*, not merged, as one mount at `/drive` under the uniform VFS + pipe-SQL DSL.
//! Drive maps onto the **Blob/namespace archetype** — **folders = directories, files = blobs** —
//! over My Drive (`/drive/my`) and Shared Drives (`/drive/shared/<driveName>`).
//!
//! ## Surface
//! - [`GDriveDriver`] — the introspective `Driver`: `mount()` = `/drive`, the
//!   [`Archetype::BlobNamespace`] per-path archetype + the [`FileMeta`](schema::FileMeta) schema,
//!   **path-keyed** capabilities (a corpus/folder = `Ls|Select|Insert|Upsert|Cp|Mv`; a file =
//!   `Select|Upsert|Update|Remove|Cp|Mv`), the `drive.copy` procedure (the `cp` apply), and
//!   `Partial { where_, limit }` pushdown (the Drive `q` search). `version_support` is
//!   [`VersionSupport::Versioned`] (Drive revisions, the `@rev` coordinate).
//! - [`DriveApplier`] — the synchronous apply leg `applier()` returns and the
//!   [`qfs_runtime::SharedApplier`] the bridge drives under `COMMIT`.
//! - [`gdrive_apply_driver`] — wraps the applier in a [`qfs_runtime::PlanApplierBridge`] ready to
//!   `register` into a `DriverRegistry` under the driver id `drive`, so a plan over `/drive`
//!   executes end-to-end through the t10 interpreter.
//!
//! ## Purity invariant (RFD §3)
//! Every write — `UPSERT INTO /drive/...`, `REMOVE`, `UPDATE` (rename/move), `CALL drive.copy` —
//! **constructs a `Plan` node and performs no I/O during planning**; only [`DriveApplier`] under
//! `COMMIT` touches the Drive API. The introspective methods are pure data (no `&mut self`, no
//! future). `PREVIEW` calls **no** client method (the mock asserts zero calls).
//!
//! ## WHERE → Drive `q` pushdown with a TRUTHFUL residual (the t20 lesson)
//! [`query::build_query`] lowers a typed `WHERE` into the Drive `q` search. A term is pushed as a
//! residual-dropping **exact** mapping **only** when the Drive operator means *exactly* the SQL
//! predicate (`name = 'x'`, `mimeType = 'x'`, `trashed = b`, `'<id>' in parents`). Every looser
//! Drive operator (`name contains`, `fullText contains`, the `modifiedTime` bound) is pushed as a
//! cheap **pre-filter** and the exact predicate is **kept as the local residual** so the engine
//! re-applies exact filtering — over-fetch then filter, never wrong rows (RFD §6).
//!
//! ## Trash, not delete (RFD §10)
//! `REMOVE` defaults to **trash** (recoverable). A permanent, irreversible delete requires an
//! explicit `hard_delete` flag column on the effect; both legs are flagged irreversible so the
//! runtime never auto-retries them and `PREVIEW` warns.
//!
//! ## Auth + multi-account + least privilege (RFD §10)
//! Auth (tokens, refresh, multi-account) comes from t19. The bearer is injected by the t19
//! `GoogleApiClient` and lives behind a [`qfs_secrets::Secret`]; it is **never** logged, never in
//! a DTO, never in plan output or a [`DriveError`]. Multi-account is the t19 base: one
//! `GoogleApiClient` per account email; the driver is account-agnostic (the resolved account is
//! bound at client construction).
//!
//! ## No vendor leak (RFD §9)
//! Drive JSON is translated into owned DTOs at the [`client`] boundary; the `Driver` surface and
//! the `Plan` carry zero google types. The HTTP client is behind the mockable [`GDriveClient`]
//! trait so it mocks in tests (no live Drive, no network) and `reqwest` stays in
//! `qfs-driver-http` — this crate rides the t19 `HttpExchange` seam.
//!
//! ## Named parks (deferred)
//! - **Live path resolution (`name → id` walk) — surface present, no live test.** The pure parse
//!   ([`DrivePath`]) and the resolved-id effect columns exist; the snapshot folder-tree walk that
//!   fills them is exercised through the mocked `list_files` seam, not a live Drive.
//! - **Streaming / resumable upload chunking — modelled as a single seam call.** [`GDriveClient`]
//!   exposes `upload`/`update_content`; the chunked resumable-session retry loop is a follow-up.
//! - **`@rev` revision history walk — column + parse present.** The `rev` column and the `@<rev>`
//!   parse exist; `revisions.list` enumeration is deferred to the read-path follow-up.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod client;
mod effect;
mod error;
pub mod export;
mod path;
pub mod query;
pub mod read;
mod schema;

use std::sync::Arc;

use qfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Param, Path, ProcSig, PushdownProfile, Verb,
    VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;
use qfs_types::ColumnType;

mod applier;

pub use applier::DriveApplier;
pub use client::{FilePage, GDriveClient, GoogleApiDriveClient, MockDriveClient, RecordedCall};
pub use effect::{
    DriveEffect, ADD_PARENTS_COL, BYTES_COL, FILE_ID_COL, HARD_DELETE_COL, MIME_COL, NAME_COL,
    PARENT_ID_COL, REMOVE_PARENTS_COL,
};
pub use error::DriveError;
pub use export::{default_export_target, ExportTarget};
pub use path::{DrivePath, MOUNT, MY_SEGMENT, SHARED_SEGMENT};
pub use read::{decode_body, plan_read, read_rows, ReadPlan};
pub use schema::{FileMeta, Revision, SharedDrive, FOLDER_MIME, GOOGLE_NATIVE_PREFIX};

/// The least-privilege Drive scope — read/write file content + metadata the driver creates and
/// opens. NOT a full-account or admin scope (RFD §10 blast radius). Declared on the `drive.copy`
/// procedure's `requires_scopes` so the server `POLICY` can reason about blast radius.
pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// The Google Drive driver (RFD §5). Owns the synchronous [`DriveApplier`] the contract returns
/// from `applier()`, plus the declared procedures and pushdown profile. Construct with
/// [`GDriveDriver::new`], injecting the [`GDriveClient`] (auth is injected there at construction —
/// the real client wraps a per-account `GoogleApiClient`; never on the contract surface).
pub struct GDriveDriver {
    applier: DriveApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl GDriveDriver {
    /// Build a Drive driver over `client`. In production `client` is a [`GoogleApiDriveClient`]
    /// wrapping a per-account `GoogleApiClient` (bearer + refresh-on-401); in tests it is a
    /// [`MockDriveClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GDriveClient>) -> Self {
        Self {
            applier: DriveApplier::new(client),
            // Drive `q` covers many WHERE predicates and a result cap (pageSize); ordering /
            // projection / joins stay local (RFD §6). Residual predicates combine locally — see
            // `query::build_query`.
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: false,
                limit: true,
                order: false,
                join: false,
                aggregate: false,
                distinct: false,
                group_by: false,
            },
            procs: vec![
                // The server-side copy (the `cp` apply). Reversible (a copy creates, never
                // destroys), so not flagged irreversible.
                ProcSig::new("copy")
                    .with_params(vec![
                        Param::new("file_id", ColumnType::Text),
                        Param::new("parent_id", ColumnType::Text),
                        Param::new("name", ColumnType::Text),
                    ])
                    .requires_scopes(vec![DRIVE_SCOPE.to_string()]),
            ],
        }
    }

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn drive_applier(&self) -> &DriveApplier {
        &self.applier
    }

    /// The path-keyed capability set (RFD §5):
    /// - the `/drive` root / a corpus root / a Shared Drive root → `Ls|Select` (list children).
    /// - a folder path (My / Shared) → `Ls|Select|Insert|Upsert|Cp|Mv` (list + create files).
    /// - a file (`id:<file>`) → `Select|Upsert|Update|Remove|Cp|Mv` (read/replace/rename/trash).
    /// - anything else → the empty set (every verb rejected at the parse-time gate).
    ///
    /// **`INSERT` of arbitrary relational columns is denied** at a file leaf — Drive is a blob,
    /// not a relational table — so a columnar `INSERT` against a file is rejected structurally.
    fn caps_for(&self, path: &Path) -> Capabilities {
        match DrivePath::parse(path) {
            Ok(p) if p.is_corpus_root() => Capabilities::from_verbs(&[Verb::Ls, Verb::Select]),
            // A path under a corpus is a folder-or-file; the parse cannot tell which without a
            // live lookup, so the node admits both the collection verbs and the blob verbs. The
            // applier/effect decode enforces the concrete shape. `INSERT` here means "create a
            // file in this folder" (blob create), never a relational column insert.
            Ok(DrivePath::My { .. } | DrivePath::Shared { .. }) => Capabilities::from_verbs(&[
                Verb::Ls,
                Verb::Select,
                Verb::Insert,
                Verb::Upsert,
                Verb::Update,
                Verb::Remove,
                Verb::Cp,
                Verb::Mv,
            ]),
            Ok(DrivePath::ById { .. }) => Capabilities::from_verbs(&[
                Verb::Select,
                Verb::Upsert,
                Verb::Update,
                Verb::Remove,
                Verb::Cp,
                Verb::Mv,
            ]),
            Ok(_) | Err(_) => Capabilities::none(),
        }
    }
}

impl Driver for GDriveDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, _path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Every /drive node is the Blob/namespace archetype; its file relation is the canonical
        // FileMeta schema. Pure: builds data, no I/O.
        Ok(NodeDesc::new(Archetype::BlobNamespace, FileMeta::schema()))
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        self.caps_for(path)
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn version_support(&self, _path: &Path) -> VersionSupport {
        // Drive files carry revisions addressable by `@<rev>` (RFD §4).
        VersionSupport::Versioned
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`GDriveDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding
/// the async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id
/// `drive`. A plan routed to `/drive` then executes end-to-end through the t10 interpreter, which
/// dispatches each effect to this bridge.
#[must_use]
pub fn gdrive_apply_driver(driver: &GDriveDriver) -> PlanApplierBridge<DriveApplier> {
    PlanApplierBridge::new(Arc::new(driver.drive_applier().clone()))
}

#[cfg(test)]
mod tests;
