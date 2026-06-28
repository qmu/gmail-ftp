//! The Slack **read path** (RFD-0001 §5): turn a `SELECT … /slack/...` into a pure,
//! self-documenting [`ReadPlan`] and decode a list response's JSON into owned DTO [`Row`]s.
//!
//! ## Cursor pagination as a pure bounded fan-out
//! A paginated `SELECT` is modelled as **one** [`ReadPlan`] node carrying the node + pushed params
//! (`oldest`/`latest`/`limit` for a message log) — a single batched fetch *set*, not an imperative
//! page loop. The bound ([`crate::client::MAX_PAGES`]) and the `response_metadata.next_cursor`
//! follow live at the edge in [`crate::client`], so the plan stays a single pure node PREVIEW can
//! show and the planner can batch.

use qfs_types::{Row, RowBatch, Schema};

use crate::client::SlackClient;
use crate::dto::{FileDto, MessageDto, ReactionDto, UserDto};
use crate::error::SlackError;
use crate::path::{NodeKind, SlackPath};
use crate::pushdown::{build_params, PushdownResult};
use crate::schema::schema_for;

/// Execute a `FROM /slack/<ws>/<node>` **collection read** end to end — the single in-crate entry
/// point the binary's async `ReadDriver` adapter drives, mirroring [`qfs_driver_local::scan_rows`].
/// It composes the pure-then-I/O stages this module and its siblings already own, so the binary
/// adapter never re-derives the path→plan→fetch→decode logic:
///
/// 1. [`SlackPath::parse_str`] — parse the addressed node (pure, no I/O, no token).
/// 2. [`ReadPlan::list`] — lower `predicate` into pushed params + a truthful residual (pure).
/// 3. [`SlackClient::list`] — the **only** I/O: the credentialed, cursor-paginated read call (the
///    real client resolves the bot token lazily at request-build time, so a missing/locked
///    credential surfaces here as [`SlackError::Auth`] — fail closed, never empty rows, never a
///    panic).
/// 4. [`decode_list`] — the Slack JSON → owned typed [`RowBatch`] boundary (no vendor type escapes).
///
/// The pushed query may honestly over-return relative to any unpushable predicate/limit; the
/// executor re-applies the residual locally (the t20 property), exactly like the local scan.
///
/// ## The `#channel`/`@user` resolution seam (honesty)
/// The workspace-global nodes (`users`, `files`) read fully through this seam. A message-log node
/// (`<#channel>/messages`, `dms/<user>/messages`, …) additionally needs the channel/peer id passed
/// to the Slack Web API as a request param; that symbolic-`#name`→id resolution is **I/O performed
/// by the live client at request time** (the same resolution the commit applier does), so it is a
/// documented live seam rather than something this pure composition synthesizes.
///
/// # Errors
/// [`SlackError`] on a malformed path, an auth/transport/HTTP/body failure, or a decode failure.
pub fn read_rows(
    client: &dyn SlackClient,
    path: &str,
    predicate: Option<&qfs_types::Predicate>,
) -> Result<RowBatch, SlackError> {
    let parsed = SlackPath::parse_str(path)?;
    let kind = parsed.kind();
    let plan = ReadPlan::list(kind, predicate);
    let value = client.list(kind, plan.params())?;
    decode_list(kind, &value)
}

/// A pure, self-documenting read: which node, the pushed query params, and the **residual**
/// predicate the engine re-checks locally. One node — the planner batches the cursor fan-out at
/// the edge.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ReadPlan {
    /// The node kind being listed (selects the schema + decode).
    pub kind: NodeKind,
    /// The pushdown outcome: the pushed params + the truthful residual.
    pub pushdown: PushdownResult,
}

impl ReadPlan {
    /// Plan a list read for `kind`, lowering `predicate` into pushed params + a truthful residual
    /// (the t20 lesson). Pure: builds data, performs no I/O, holds no token.
    #[must_use]
    pub fn list(kind: NodeKind, predicate: Option<&qfs_types::Predicate>) -> Self {
        Self {
            kind,
            // Only the message-log nodes push a time window; the others keep the whole predicate
            // residual (correctness over completeness — the ticket scopes richer pushdown to E3).
            pushdown: match kind {
                NodeKind::Messages | NodeKind::Replies | NodeKind::Dms => build_params(predicate),
                _ => PushdownResult {
                    params: Vec::new(),
                    residual: predicate.cloned(),
                },
            },
        }
    }

    /// The pushed query params (what the client sends to the Slack endpoint).
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        &self.pushdown.params
    }

    /// The row schema this read produces.
    #[must_use]
    pub fn schema(&self) -> Schema {
        schema_for(self.kind)
    }
}

/// Decode a Slack list JSON value into a typed [`RowBatch`] for `kind`. The boundary where Slack
/// JSON becomes owned DTOs → rows; no vendor type escapes.
///
/// # Errors
/// [`SlackError::Decode`] never fires today (a non-object element is skipped); the `Result` is kept
/// for symmetry with a future strict mode.
pub fn decode_list(kind: NodeKind, value: &serde_json::Value) -> Result<RowBatch, SlackError> {
    let rows: Vec<Row> = match kind {
        NodeKind::Messages | NodeKind::Replies | NodeKind::Dms => {
            decode_messages(value).iter().map(Row::from).collect()
        }
        NodeKind::Reactions => decode_reactions(value).iter().map(Row::from).collect(),
        NodeKind::Files => decode_files(value).iter().map(Row::from).collect(),
        NodeKind::Users => decode_users(value).iter().map(Row::from).collect(),
    };
    Ok(RowBatch::new(schema_for(kind), rows))
}

/// The array elements of a JSON list value (an empty slice for a non-array).
fn arr(value: &serde_json::Value) -> &[serde_json::Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn s(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn i(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

fn bln(v: &serde_json::Value, key: &str) -> bool {
    v.get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Decode a `conversations.history`/`.replies` `messages` array into owned [`MessageDto`]s. Accepts
/// either the bare array or the `{messages:[...]}` envelope.
#[must_use]
pub fn decode_messages(value: &serde_json::Value) -> Vec<MessageDto> {
    let items = value.get("messages").map(arr).unwrap_or_else(|| arr(value));
    items
        .iter()
        .map(|v| MessageDto {
            ts: s(v, "ts"),
            user: s(v, "user"),
            text: s(v, "text"),
            thread_ts: s(v, "thread_ts"),
            subtype: s(v, "subtype"),
        })
        .collect()
}

/// Decode a message's `reactions` array into owned [`ReactionDto`]s.
#[must_use]
pub fn decode_reactions(value: &serde_json::Value) -> Vec<ReactionDto> {
    let items = value
        .get("reactions")
        .map(arr)
        .unwrap_or_else(|| arr(value));
    items
        .iter()
        .map(|v| ReactionDto {
            name: s(v, "name"),
            count: i(v, "count"),
        })
        .collect()
}

/// Decode a `files.list` `files` array into owned [`FileDto`]s.
#[must_use]
pub fn decode_files(value: &serde_json::Value) -> Vec<FileDto> {
    let items = value.get("files").map(arr).unwrap_or_else(|| arr(value));
    items
        .iter()
        .map(|v| FileDto {
            id: s(v, "id"),
            name: s(v, "name"),
            mimetype: s(v, "mimetype"),
            size: i(v, "size"),
            user: s(v, "user"),
        })
        .collect()
}

/// Decode a `users.list` `members` array into owned [`UserDto`]s.
#[must_use]
pub fn decode_users(value: &serde_json::Value) -> Vec<UserDto> {
    let items = value.get("members").map(arr).unwrap_or_else(|| arr(value));
    items
        .iter()
        .map(|v| UserDto {
            id: s(v, "id"),
            name: s(v, "name"),
            real_name: s(v, "real_name"),
            is_bot: bln(v, "is_bot"),
            deleted: bln(v, "deleted"),
        })
        .collect()
}

#[cfg(test)]
mod read_rows_tests {
    //! `read_rows` against the in-memory [`MockSlackClient`] — offline, no socket, no credential.
    //! Proves the path→plan→fetch→decode composition the binary adapter drives returns the right
    //! typed rows for a representative workspace-global `FROM /slack/<ws>/users` path, and records
    //! the exact list call.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::client::{MockSlackClient, RecordedCall};
    use qfs_types::Value;

    #[test]
    fn reads_the_users_directory_into_typed_rows() {
        // FROM /slack/acme/users — the representative workspace-global read (no channel needed).
        let client = MockSlackClient::new().with_list(serde_json::json!({
            "members": [
                { "id": "U1", "name": "alice", "real_name": "Alice", "is_bot": false,
                  "deleted": false },
                { "id": "U2", "name": "bot", "real_name": "Bot", "is_bot": true, "deleted": false },
            ]
        }));
        let batch = read_rows(&client, "/slack/acme/users", None).unwrap();
        assert_eq!(batch.rows.len(), 2, "two user rows decoded");
        // The first column of the users schema is `id`.
        assert_eq!(batch.rows[0].values[0], Value::Text("U1".to_string()));
        match client.recorded().as_slice() {
            [RecordedCall::List { kind, .. }] => assert_eq!(*kind, NodeKind::Users),
            other => panic!("expected one recorded List call, got {other:?}"),
        }
    }

    #[test]
    fn unknown_workspace_node_is_rejected() {
        // A path that names no recognized /slack node fails with a structured path error, never an
        // empty batch — and performs no I/O.
        let client = MockSlackClient::new();
        let err = read_rows(&client, "/slack/acme/not-a-node", None).unwrap_err();
        assert_eq!(err.code(), "slack_invalid_path");
        assert!(
            client.recorded().is_empty(),
            "a rejected path performs no I/O"
        );
    }
}
