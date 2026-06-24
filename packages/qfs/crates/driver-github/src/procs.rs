//! The `CALL github.*` procedure declarations (RFD-0001 §3 — the irreducible state transitions
//! GitHub has no universal verb for): `merge`, `dispatch`, `review`.
//!
//! Each declared [`ProcSig`] names typed params; `plan_call` (in [`crate::effect`]) builds an
//! `Effect::Call` HTTP-call node from a `CALL`. `merge` and `dispatch` are marked
//! **irreversible** so `PREVIEW` always surfaces them and `COMMIT` requires explicit
//! confirmation; `review` (submitting an approval/comment) is reversible-ish (a later review can
//! supersede it) and is left reversible.
//!
//! ## Optimistic concurrency on merge (RFD §6)
//! `merge` takes an optional `sha` param — the PR head SHA the caller observed. When set it is
//! sent as the merge's `sha` precondition so GitHub refuses to merge a *stale* ref (a 409), never
//! merging something the caller did not see.
//!
//! ## `dispatch` returns 204 with no run id (the genuinely-hard-part note)
//! `workflow_dispatch` returns 204 No Content — there is no run id to return synchronously. The
//! effect node resolves on COMMIT by reporting a *queued* status (the honest answer), not a
//! fabricated id; a follow-up `SELECT … FROM runs` polls the freshly-triggered run.

use qfs_driver::{Param, ProcSig};
use qfs_types::ColumnType;

/// The least-privilege scope hint a `merge`/`dispatch`/`review` advertises (RFD §10 blast-radius
/// reasoning for the server `POLICY`). A label only — never a token.
pub const REPO_SCOPE: &str = "repo";
/// The scope a workflow `dispatch` needs.
pub const WORKFLOW_SCOPE: &str = "workflow";

/// The unqualified procedure names this driver declares (the `CALL github.<name>` targets).
pub const PROC_MERGE: &str = "merge";
/// The `dispatch` procedure name.
pub const PROC_DISPATCH: &str = "dispatch";
/// The `review` procedure name.
pub const PROC_REVIEW: &str = "review";

/// Build the full declared procedure set (RFD §3). The order is stable for golden snapshots.
#[must_use]
pub fn procedures() -> Vec<ProcSig> {
    vec![
        // merge(method=>'squash'|'merge'|'rebase', sha=>'<head-sha>'?) — irreversible.
        ProcSig::new(PROC_MERGE)
            .with_params(vec![
                Param::new("method", ColumnType::Text),
                // The optimistic-concurrency precondition: the PR head SHA the caller observed.
                Param::new("sha", ColumnType::Text),
            ])
            .irreversible(true)
            .requires_scopes(vec![REPO_SCOPE.to_string()]),
        // dispatch(workflow=>'ci.yml', ref=>'main', inputs=>{...}) — irreversible (it triggers a
        // run; you cannot un-trigger it).
        ProcSig::new(PROC_DISPATCH)
            .with_params(vec![
                Param::new("workflow", ColumnType::Text),
                Param::new("ref", ColumnType::Text),
                Param::new("inputs", ColumnType::Json),
            ])
            .irreversible(true)
            .requires_scopes(vec![WORKFLOW_SCOPE.to_string()]),
        // review(event=>'APPROVE'|'REQUEST_CHANGES'|'COMMENT', body=>'...') — a submitted review.
        // Reversible-ish (a later review supersedes), so not flagged irreversible.
        ProcSig::new(PROC_REVIEW)
            .with_params(vec![
                Param::new("event", ColumnType::Text),
                Param::new("body", ColumnType::Text),
            ])
            .requires_scopes(vec![REPO_SCOPE.to_string()]),
    ]
}
