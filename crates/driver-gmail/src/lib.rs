//! `cfs-driver-gmail` — the **first real `Driver`** (RFD-0001 §5, E4 t20) and a flagship of the
//! thesis (§1): the legacy Go `gmail-ftp` is **subsumed**, not merged, as one mount at `/mail`
//! under the uniform VFS + pipe-SQL DSL. It maps the mailbox onto the **Append/log archetype**:
//! **labels = directories, messages = files, attachments = nested entries**, addressed by `id:`
//! / `id:thread:<id>`.
//!
//! ## Surface
//! - [`GmailDriver`] — the introspective `Driver`: `mount()` = `/mail`, per-path archetype
//!   ([`Archetype::AppendLog`]) + the [`MailMessage`](schema::MailMessage) schema, **path-keyed**
//!   capabilities (`/mail/<label>` = `Select|Update|Remove`; `/mail/drafts` =
//!   `Insert|Upsert|Select|Remove`; a message is read/trash-only), the irreversible `mail.send`
//!   procedure, the pure `SEND` prelude alias, and `Partial { where_, limit }` pushdown (the
//!   Gmail search `q=`). `version_support` is [`VersionSupport::None`] for now.
//! - [`GmailApplier`] — the synchronous apply leg `applier()` returns and the
//!   [`cfs_runtime::SharedApplier`] the bridge drives under `COMMIT`.
//! - [`gmail_apply_driver`] — wraps the applier in a [`cfs_runtime::PlanApplierBridge`] ready to
//!   `register` into a `DriverRegistry` under the driver id `mail`, so a plan over `/mail`
//!   executes end-to-end through the t10 interpreter.
//!
//! ## Purity invariant (RFD §3)
//! Every write — `INSERT INTO /mail/drafts`, `UPSERT`, `REMOVE` (trash), `CALL mail.send` —
//! **constructs a `Plan` node and performs no I/O during planning**; only [`GmailApplier`] under
//! `COMMIT` touches the Gmail API. The introspective methods are pure data (no `&mut self`, no
//! future). `PREVIEW` calls **no** client method (the mock asserts zero calls).
//!
//! ## Auth + multi-account + least privilege (RFD §10)
//! Auth (tokens, refresh, multi-account) comes from t19. This driver requests **only**
//! [`GMAIL_MODIFY_SCOPE`] + [`GMAIL_COMPOSE_SCOPE`] (no full `mail.google.com`, no permanent
//! delete). The bearer is injected by the t19 `GoogleApiClient` and lives behind a
//! [`cfs_secrets::Secret`]; it is **never** logged, never in a DTO, never in plan output or a
//! [`GmailError`]. Multi-account is the t19 base: one `GoogleApiClient` per account email; the
//! driver is account-agnostic (the resolved account is bound at client construction).
//!
//! ## No vendor leak (RFD §9)
//! Gmail JSON is translated into owned DTOs at the [`client`] boundary; the `Driver` surface and
//! the `Plan` carry zero google types. The HTTP client is behind the mockable [`GmailClient`]
//! trait so it mocks in tests (no live Gmail, no network) and `reqwest` stays in
//! `cfs-driver-http` — this crate rides the t19 `HttpExchange` seam.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod client;
mod effect;
mod error;
pub mod mime;
mod path;
pub mod query;
mod schema;

use std::sync::Arc;

use cfs_driver::{
    AliasFn, Archetype, Capabilities, Driver, NodeDesc, Param, Path, ProcSig, PushdownProfile,
    Verb, VersionSupport,
};
use cfs_plan::PlanApplier;
use cfs_runtime::PlanApplierBridge;
use cfs_types::ColumnType;

pub use applier::GmailApplier;
pub use client::{GmailClient, GoogleApiGmailClient, MessageIdPage, MockGmailClient, RecordedCall};
pub use effect::{
    GmailEffect, ADD_LABELS_COL, BODY_COL, CC_COL, DRAFT_ID_COL, REMOVE_LABELS_COL, SUBJECT_COL,
    TO_COL,
};
pub use error::GmailError;
pub use path::{MailPath, DRAFTS_SEGMENT, MOUNT};
pub use schema::{Attachment, AttachmentMeta, MailDraft, MailMessage};

/// The least-privilege **modify** scope — list/search/read, trash, and label modify. NOT the
/// full `https://mail.google.com/` scope and NOT a delete scope (RFD §10 blast radius).
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
/// The least-privilege **compose** scope — create drafts and send. The `mail.send` procedure
/// declares this in `requires_scopes` so the server `POLICY` can reason about blast radius.
pub const GMAIL_COMPOSE_SCOPE: &str = "https://www.googleapis.com/auth/gmail.compose";

/// The Gmail driver (RFD §5). Owns the synchronous [`GmailApplier`] the contract returns from
/// `applier()`, plus the declared procedures, prelude, and pushdown profile. Construct with
/// [`GmailDriver::new`], injecting the [`GmailClient`] (auth is injected there at construction —
/// the real client wraps a per-account `GoogleApiClient`; never on the contract surface).
pub struct GmailDriver {
    applier: GmailApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
    prelude: Vec<AliasFn>,
}

impl GmailDriver {
    /// Build a Gmail driver over `client`. In production `client` is a
    /// [`GoogleApiGmailClient`] wrapping a per-account `GoogleApiClient` (bearer + refresh-on-
    /// 401); in tests it is a [`MockGmailClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GmailClient>) -> Self {
        Self {
            applier: GmailApplier::new(client),
            // Gmail search `q=` covers many WHERE predicates and a result cap (maxResults);
            // ordering / projection / joins stay local (RFD §6). Residual predicates combine
            // locally — see `query::build_query`.
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
                // The irreducible, irreversible state transition (RFD §3/§10).
                ProcSig::new("send")
                    .with_params(vec![
                        Param::new("to", ColumnType::Text),
                        Param::new("subject", ColumnType::Text),
                        Param::new("body", ColumnType::Text),
                    ])
                    .irreversible(true)
                    .requires_scopes(vec![GMAIL_COMPOSE_SCOPE.to_string()]),
            ],
            // The pure prelude alias: `SEND(d) = d |> CALL mail.send` (RFD §3).
            prelude: vec![AliasFn::new("SEND", "mail.send")],
        }
    }

    /// Borrow the synchronous applier (e.g. to drive a `cfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn gmail_applier(&self) -> &GmailApplier {
        &self.applier
    }

    /// The path-keyed capability set (RFD §5):
    /// - `/mail/drafts` → `Insert|Upsert|Select|Remove` (create/replace/list/trash drafts).
    /// - `/mail/<label>` → `Select|Update|Remove` (search messages, modify labels, trash).
    /// - a message (`id:<msg>` / `/mail/<label>/<msg>`) → `Select|Remove` (read + trash only).
    /// - a thread (`id:thread:<id>`) → `Remove` (trash only).
    /// - the `/mail` root → `Ls|Select` (list labels).
    /// - anything else → the empty set (every verb rejected at the parse-time gate).
    fn caps_for(&self, path: &Path) -> Capabilities {
        match MailPath::parse(path) {
            Ok(MailPath::Drafts) => {
                Capabilities::from_verbs(&[Verb::Insert, Verb::Upsert, Verb::Select, Verb::Remove])
            }
            Ok(MailPath::Label { .. }) => {
                Capabilities::from_verbs(&[Verb::Select, Verb::Update, Verb::Remove])
            }
            Ok(MailPath::Message { .. }) => Capabilities::from_verbs(&[Verb::Select, Verb::Remove]),
            Ok(MailPath::Thread { .. }) => Capabilities::from_verbs(&[Verb::Remove]),
            Ok(MailPath::Root) => Capabilities::from_verbs(&[Verb::Ls, Verb::Select]),
            Ok(MailPath::Attachment { .. }) => Capabilities::from_verbs(&[Verb::Select]),
            Err(_) => Capabilities::none(),
        }
    }
}

impl Driver for GmailDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, _path: &Path) -> Result<NodeDesc, cfs_driver::CfsError> {
        // Every /mail node is the Append/log archetype; its message relation is the canonical
        // MailMessage schema. Pure: builds data, no I/O.
        Ok(NodeDesc::new(Archetype::AppendLog, MailMessage::schema()))
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

    fn prelude(&self) -> &[AliasFn] {
        &self.prelude
    }

    fn version_support(&self, _path: &Path) -> VersionSupport {
        // @version / historyId incremental sync is deferred to the E7 trigger sibling.
        VersionSupport::None
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`GmailDriver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding
/// the async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id
/// `mail`. A plan routed to `/mail` then executes end-to-end through the t10 interpreter, which
/// dispatches each effect to this bridge (and collapses the N+1 detail-fetch frontier).
#[must_use]
pub fn gmail_apply_driver(driver: &GmailDriver) -> PlanApplierBridge<GmailApplier> {
    PlanApplierBridge::new(Arc::new(driver.gmail_applier().clone()))
}

#[cfg(test)]
mod tests;
