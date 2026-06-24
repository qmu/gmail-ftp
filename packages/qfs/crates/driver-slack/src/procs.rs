//! The `CALL slack.*` procedure declarations (RFD-0001 §3 — the irreducible state transitions
//! Slack has no universal verb for): `react`, `pin`, `unpin`, `update`, `delete`.
//!
//! Each declared [`ProcSig`] names typed params; the effect decoder ([`crate::effect`]) builds a
//! `Call` effect node from a `CALL`. `pin` and `delete` are flagged **irreversible** so `PREVIEW`
//! always surfaces them and `COMMIT` requires explicit confirmation (RFD §6/§10).
//!
//! ## react ≡ INSERT INTO reactions (equivalence, the ticket's plan-assertion)
//! `CALL slack.react(channel, ts, emoji)` and `INSERT INTO .../messages/<ts>/reactions` produce
//! **equivalent** plans (both a `reactions.add`). `react` is the explicit-CALL spelling; the path
//! INSERT is the universal-verb spelling — one effect, two surfaces.
//!
//! ## The pure `POST` prelude alias (RFD §3)
//! `POST(d) = d |> CALL slack.post` desugars a message INSERT — a pure alias function in the
//! registry, never a keyword, in scope only for plans whose receiver is a Slack append node.

use qfs_driver::{AliasFn, Param, ProcSig};
use qfs_types::ColumnType;

/// The least-privilege scope a posting/reaction proc advertises (RFD §10 blast-radius reasoning).
/// A label only — never a token.
pub const CHAT_WRITE_SCOPE: &str = "chat:write";
/// The scope a reaction add/remove needs.
pub const REACTIONS_WRITE_SCOPE: &str = "reactions:write";
/// The scope a pin add/remove needs.
pub const PINS_WRITE_SCOPE: &str = "pins:write";

/// The `react` procedure name.
pub const PROC_REACT: &str = "react";
/// The `pin` procedure name (irreversible).
pub const PROC_PIN: &str = "pin";
/// The `unpin` procedure name.
pub const PROC_UNPIN: &str = "unpin";
/// The `update` procedure name (edit a message by `ts`).
pub const PROC_UPDATE: &str = "update";
/// The `delete` procedure name (irreversible — `chat.delete`).
pub const PROC_DELETE: &str = "delete";

/// The prelude alias surface name (`POST`).
pub const ALIAS_POST: &str = "POST";

/// Build the full declared procedure set (RFD §3). The order is stable for golden snapshots.
#[must_use]
pub fn procedures() -> Vec<ProcSig> {
    vec![
        // react(channel, ts, emoji) — reversible (unreact removes it); naturally idempotent.
        ProcSig::new(PROC_REACT)
            .with_params(vec![
                Param::new("channel", ColumnType::Text),
                Param::new("ts", ColumnType::Text),
                Param::new("emoji", ColumnType::Text),
            ])
            .requires_scopes(vec![REACTIONS_WRITE_SCOPE.to_string()]),
        // pin(channel, ts) — IRREVERSIBLE in the audit sense (a deliberate, surfaced transition).
        ProcSig::new(PROC_PIN)
            .with_params(vec![
                Param::new("channel", ColumnType::Text),
                Param::new("ts", ColumnType::Text),
            ])
            .irreversible(true)
            .requires_scopes(vec![PINS_WRITE_SCOPE.to_string()]),
        // unpin(channel, ts) — reversible (re-pin).
        ProcSig::new(PROC_UNPIN)
            .with_params(vec![
                Param::new("channel", ColumnType::Text),
                Param::new("ts", ColumnType::Text),
            ])
            .requires_scopes(vec![PINS_WRITE_SCOPE.to_string()]),
        // update(channel, ts, text) — edit a message by ts (Snapshot @version; reversible-ish, a
        // later edit supersedes).
        ProcSig::new(PROC_UPDATE)
            .with_params(vec![
                Param::new("channel", ColumnType::Text),
                Param::new("ts", ColumnType::Text),
                Param::new("text", ColumnType::Text),
            ])
            .requires_scopes(vec![CHAT_WRITE_SCOPE.to_string()]),
        // delete(channel, ts) — chat.delete, IRREVERSIBLE (the message is gone).
        ProcSig::new(PROC_DELETE)
            .with_params(vec![
                Param::new("channel", ColumnType::Text),
                Param::new("ts", ColumnType::Text),
            ])
            .irreversible(true)
            .requires_scopes(vec![CHAT_WRITE_SCOPE.to_string()]),
    ]
}

/// Build the prelude alias set (RFD §3): `POST` desugars to a message INSERT (`slack.post`).
#[must_use]
pub fn prelude() -> Vec<AliasFn> {
    vec![AliasFn::new(ALIAS_POST, "slack.post")]
}
