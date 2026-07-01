//! `qfs-driver-gmail` — the **first real `Driver`** (RFD-0001 §5, E4 t20) and a flagship of the
//! thesis (§1): the Gmail mailbox is exposed as one mount at `/mail`
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
//!   [`qfs_runtime::SharedApplier`] the bridge drives under `COMMIT`.
//! - [`gmail_apply_driver`] — wraps the applier in a [`qfs_runtime::PlanApplierBridge`] ready to
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
//! [`qfs_secrets::Secret`]; it is **never** logged, never in a DTO, never in plan output or a
//! [`GmailError`]. Multi-account is the t19 base: one `GoogleApiClient` per account email; the
//! driver is account-agnostic (the resolved account is bound at client construction).
//!
//! ## No vendor leak (RFD §9)
//! Gmail JSON is translated into owned DTOs at the [`client`] boundary; the `Driver` surface and
//! the `Plan` carry zero google types. The HTTP client is behind the mockable [`GmailClient`]
//! trait so it mocks in tests (no live Gmail, no network) and `reqwest` stays in
//! `qfs-driver-http` — this crate rides the t19 `HttpExchange` seam.
//!
//! ## Deferred for t20 (named parks)
//! - **Attachment bytes fetch — wired (t92).** A listing row carries attachment *metadata* only
//!   ([`AttachmentMeta`]); the on-demand bytes fetch reads the single node
//!   [`MailPath::Attachment`](path::MailPath) (`/mail/<label>/<msg>/<att>`, its `Select`
//!   capability) via [`GmailClient::get_attachment`](client::GmailClient::get_attachment) —
//!   attachments.get bytes (base64url-decoded at the client seam) paired with the message part's
//!   `filename`/`mime`/`size` into a `content`-bearing row. gmail-ftp `get id:att:<msg>:<att>` parity.
//! - **`historyId` / `@version` incremental sync — parked.** [`VersionSupport::None`]; deferred
//!   to the E7 trigger sibling.
//! - **Live create→send→trash smoke test — parked.** The suite is mock-only (no env-gated live
//!   Gmail test); the opt-in smoke test is tracked as a follow-up acceptance item.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod client;
mod effect;
mod error;
pub mod mime;
mod path;
pub mod query;
pub mod read;
mod schema;

use std::sync::Arc;

use qfs_driver::{
    AliasFn, Archetype, Capabilities, Driver, NodeDesc, Param, Path, ProcSig, PushdownProfile,
    Verb, VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;
use qfs_types::ColumnType;

pub use applier::GmailApplier;
pub use client::{GmailClient, GoogleApiGmailClient, MessageIdPage, MockGmailClient, RecordedCall};
pub use effect::{
    GmailEffect, ADD_LABELS_COL, BODY_COL, CC_COL, DRAFT_ID_COL, NAME_COL, REMOVE_LABELS_COL,
    SUBJECT_COL, TO_COL,
};
pub use error::GmailError;
pub use path::{MailPath, DRAFTS_SEGMENT, MOUNT};
pub use read::read_rows;
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

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to build
    /// the runtime bridge).
    #[must_use]
    pub fn gmail_applier(&self) -> &GmailApplier {
        &self.applier
    }

    /// **Purity invariant proof (t40, RFD §3/§6).** Desugar the `SEND` prelude alias for a draft
    /// node into the [`qfs_plan::Plan`] it represents — a single `CALL mail.send` effect node —
    /// **performing no I/O**. This is the in-code witness for `docs/language.md`'s purity section:
    /// `SEND(d) = d |> CALL mail.send` is a *pure function returning a Plan*; nothing is sent until
    /// a separate `COMMIT` applies the plan (the applier seam, never reached here). No credential
    /// is resolved, no socket is opened — building the plan only allocates owned data.
    ///
    /// ```
    /// use qfs_driver_gmail::GmailDriver;
    /// use qfs_plan::EffectKind;
    ///
    /// // SEND(d) desugars to a plan with exactly one CALL mail.send node — and runs NO I/O.
    /// let plan = GmailDriver::send_alias_plan("id:draft-1");
    /// assert_eq!(plan.nodes().len(), 1);
    /// match &plan.nodes()[0].kind {
    ///     EffectKind::Call(proc) => assert_eq!(proc.0, "mail.send"),
    ///     other => panic!("SEND must desugar to a CALL node, got {other:?}"),
    /// }
    /// // mail.send is irreversible (RFD §10) — surfaced on the node so PREVIEW can warn.
    /// assert!(plan.nodes()[0].irreversible);
    /// assert!(plan.is_irreversible());
    /// ```
    #[must_use]
    pub fn send_alias_plan(draft: &str) -> qfs_plan::Plan {
        use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, ProcId, Target, VfsPath};
        // The `SEND` alias (declared in the prelude as `SEND -> mail.send`) desugars to a single
        // `CALL mail.send` effect on the draft target. The procedure is declared irreversible, so
        // the node carries that bit (the planner's per-proc irreversibility, set explicitly).
        let target = Target::new(DriverId::new("mail"), VfsPath::new(draft));
        let node = EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("mail.send")),
            target,
        )
        .irreversible(true);
        qfs_plan::Plan::leaf(node)
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
            Ok(MailPath::Labels) => Capabilities::from_verbs(&[Verb::Insert]),
            Ok(MailPath::Attachment { .. }) => Capabilities::from_verbs(&[Verb::Select]),
            Err(_) => Capabilities::none(),
        }
    }
}

impl Driver for GmailDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Every /mail node is the Append/log archetype. The ROOT lists labels (the directory view,
        // `ls /mail`), so it reports the label-listing schema; every other node reads messages, so
        // it reports the canonical MailMessage schema. Pure: builds data, no I/O.
        let schema = match MailPath::parse(path) {
            // The root lists labels; the label-management collection takes a `name` (its INSERT
            // column) — both report the `name` label schema. Every other node reads messages.
            Ok(MailPath::Root | MailPath::Labels) => schema::label_listing_schema(),
            _ => MailMessage::schema(),
        };
        Ok(NodeDesc::new(Archetype::AppendLog, schema))
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
