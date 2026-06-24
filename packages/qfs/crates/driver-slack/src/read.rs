//! The Slack **read path** (RFD-0001 §5): turn a `SELECT … FROM /slack/...` into a pure,
//! self-documenting [`ReadPlan`] and decode a list response's JSON into owned DTO [`Row`]s.
//!
//! ## Cursor pagination as a pure bounded fan-out
//! A paginated `SELECT` is modelled as **one** [`ReadPlan`] node carrying the node + pushed params
//! (`oldest`/`latest`/`limit` for a message log) — a single batched fetch *set*, not an imperative
//! page loop. The bound ([`crate::client::MAX_PAGES`]) and the `response_metadata.next_cursor`
//! follow live at the edge in [`crate::client`], so the plan stays a single pure node PREVIEW can
//! show and the planner can batch.

use qfs_types::{Row, RowBatch, Schema};

use crate::dto::{FileDto, MessageDto, ReactionDto, UserDto};
use crate::error::SlackError;
use crate::path::NodeKind;
use crate::pushdown::{build_params, PushdownResult};
use crate::schema::schema_for;

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
