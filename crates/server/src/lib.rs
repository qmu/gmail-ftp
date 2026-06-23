//! `cfs-server` — the server face of cfs (RFD-0001 §8).
//!
//! `cfs serve <config.cfs>` boots a long-lived server whose endpoints, triggers, jobs,
//! views, policies, and webhooks are **data** managed with cfs — because the **server is a
//! driver** over `/server/...`. Booting a config file is replaying `INSERT INTO /server/...`
//! statements; the frozen `CREATE …` DDL is sugar over those writes ([`lower`]).
//!
//! ## The runtime is a pure consumer of plans (the hard part, RFD §6/§8)
//! [`Runtime::boot`] reads the file → parses → lowers each statement to a `/server` write
//! [`Plan`](cfs_core::Plan) → `COMMIT`s it through [`ServerConfigApplier`] (the same applier
//! seam a live write uses). The **only** way [`ServerState`] changes is an
//! [`EffectKind::ServerConfigWrite`](cfs_core::EffectKind::ServerConfigWrite) applied at
//! `COMMIT` — there is no privileged config loader. This is also where **real
//! COMMIT-applies-state** begins: unlike the shell/CLI's in-memory `RecordingApplier`, the
//! `/server` applier actually mutates `ServerState` under its `RwLock`.
//!
//! ## E7 seam: bindings (`ENDPOINT`/`TRIGGER`/`JOB`/`WEBHOOK` causes)
//! The cause-execution semantics (HTTP serving, cron firing, webhook ingestion) land in
//! E7 sibling tickets t31–t35 behind the [`Binding`] trait: after every committed `/server`
//! mutation the runtime calls [`Binding::reconcile`] with a read snapshot, so bindings
//! converge to the registry declaratively. `cfs-server` stays **free of HTTP/cron deps**.
//!
//! ## Confinement (boundary B5)
//! `cfs-server` is consumed by `cfs-cmd` (the `serve` arm), so it must **not** depend on
//! `cfs-runtime` (that would make a non-leaf a runtime consumer and trip the confinement
//! guard). The `/server` writes are in-memory and apply through the **pure**
//! [`cfs_core::commit`] seam — no async interpreter is needed.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod audit;
pub mod binding;
pub mod driver;
mod error;
pub mod lower;
pub mod mount;
pub mod runtime;
mod state;

use std::path::Path as FsPath;

use cfs_core::CfsError;

pub use audit::{AuditEntry, AuditSink};
pub use binding::{Binding, BindingKind, CountingBinding, NullBinding};
pub use driver::{
    apply_server_write, server_node_capabilities, server_node_schema, ConfigChange, ServerDriver,
    SERVER_MOUNT,
};
pub use error::ServerError;
pub use lower::lower_statement;
// The canonical config-row / plan-build primitives now live in closed core (t31); re-export
// them from `cfs-core` so the server's public surface is unchanged for consumers.
pub use cfs_core::{config_row_batch, server_write_plan, ConfigRow};
pub use runtime::{Runtime, ServerConfigApplier};
pub use state::{
    EndpointDef, JobDef, PolicyDef, ServerState, StatementSource, TriggerDef, ViewDef, WebhookDef,
};

/// Boot and run the server from a `.cfs` config file (RFD-0001 §8).
///
/// Builds a [`Runtime`] with a default [`NullBinding`] (the E7 cause bindings register
/// here), boots it (parse → lower → COMMIT each statement against the `/server` driver), and
/// then blocks in the supervised run loop until `ctrl_c`, draining the audit ledger on exit.
/// Boot requires **no network and no live credentials** — `/server` writes are in-memory.
///
/// # Errors
/// [`CfsError::Server`] — carrying the line-located, secret-free [`ServerError`] code +
/// message — on any read / parse / lower / unsupported-verb / commit failure during boot.
pub fn serve(config: &FsPath) -> Result<(), CfsError> {
    let mut runtime = Runtime::new().with_binding(Box::new(NullBinding));
    runtime.boot(config).map_err(to_cfs_error)?;

    // The run loop is async (`ctrl_c`). Build a current-thread tokio runtime here — at the
    // `serve` boundary, the leaf — so `cfs-server` stays off `cfs-runtime` while still
    // owning its own supervised wait. A runtime-build failure surfaces as a structured error.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CfsError::Server {
            server_code: "runtime_init".to_string(),
            message: format!("cannot start server runtime: {e}"),
        })?;
    rt.block_on(runtime.run()).map_err(to_cfs_error)
}

/// Map a [`ServerError`] into the workspace-wide [`CfsError`] at the `serve` boundary,
/// preserving the granular code + secret-free message.
fn to_cfs_error(err: ServerError) -> CfsError {
    CfsError::Server {
        server_code: err.code().to_string(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests;
