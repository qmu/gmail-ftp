//! Owned Slack DTOs and their canonical typed [`Schema`]s (RFD-0001 §5/§9).
//!
//! Slack JSON is translated into these owned, vendor-free DTOs at the [`crate::client`] boundary;
//! the `Driver` surface and the effect `Plan` carry **zero** Slack-SDK/vendor types (a DTO-boundary
//! test asserts no vendor type appears in any public signature). Each DTO has a stable [`Schema`]
//! (powering `DESCRIBE`) and a `From<&DtoX> for Row` projection in the schema's column order
//! (powering golden snapshots).
//!
//! No DTO carries a token — the bot token lives only behind the auth seam, never in a decoded body.

use qfs_types::{Column, ColumnType, Row, Schema, Value};

/// Render a Slack `ts` (a `"1700000000.000100"` string) as a `Text` value, or `NULL` if empty.
fn ts_text(ts: &str) -> Value {
    if ts.is_empty() {
        Value::Null
    } else {
        Value::Text(ts.to_string())
    }
}

/// One Slack message projected into the owned DTO (the Append/log row of `messages`/`replies`/
/// `dms`). The `ts` is the message's stable point coordinate (the `@version` Snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageDto {
    /// The message `ts` (Slack's per-channel ordering + identity coordinate).
    pub ts: String,
    /// The author user id, if any (a bot/system message may have none).
    pub user: String,
    /// The message text.
    pub text: String,
    /// The parent thread `ts`, if this is a threaded reply (else empty).
    pub thread_ts: String,
    /// The message subtype (e.g. `bot_message`, `channel_join`), if any.
    pub subtype: String,
}

impl MessageDto {
    /// The canonical message listing [`Schema`] — the typed columns `DESCRIBE .../messages` reports.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("ts", ColumnType::Text, false),
            Column::new("user", ColumnType::Text, true),
            Column::new("text", ColumnType::Text, true),
            Column::new("thread_ts", ColumnType::Text, true),
            Column::new("subtype", ColumnType::Text, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(ts: &str, text: &str) -> Self {
        Self {
            ts: ts.to_string(),
            user: "U0CALLER".to_string(),
            text: text.to_string(),
            thread_ts: String::new(),
            subtype: String::new(),
        }
    }
}

impl From<&MessageDto> for Row {
    fn from(d: &MessageDto) -> Self {
        Row::new(vec![
            Value::Text(d.ts.clone()),
            if d.user.is_empty() {
                Value::Null
            } else {
                Value::Text(d.user.clone())
            },
            Value::Text(d.text.clone()),
            ts_text(&d.thread_ts),
            if d.subtype.is_empty() {
                Value::Null
            } else {
                Value::Text(d.subtype.clone())
            },
        ])
    }
}

/// One Slack file projected into the owned DTO (the Blob/namespace `ls` row of `files`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileDto {
    /// The file id (`Fxxxx`).
    pub id: String,
    /// The file name.
    pub name: String,
    /// The MIME type.
    pub mimetype: String,
    /// The size in bytes.
    pub size: i64,
    /// The uploader user id.
    pub user: String,
}

impl FileDto {
    /// The canonical files listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Text, false),
            Column::new("name", ColumnType::Text, false),
            Column::new("mimetype", ColumnType::Text, true),
            Column::new("size", ColumnType::Int, false),
            Column::new("user", ColumnType::Text, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            mimetype: "text/plain".to_string(),
            size: 0,
            user: "U0CALLER".to_string(),
        }
    }
}

impl From<&FileDto> for Row {
    fn from(d: &FileDto) -> Self {
        Row::new(vec![
            Value::Text(d.id.clone()),
            Value::Text(d.name.clone()),
            if d.mimetype.is_empty() {
                Value::Null
            } else {
                Value::Text(d.mimetype.clone())
            },
            Value::Int(d.size),
            if d.user.is_empty() {
                Value::Null
            } else {
                Value::Text(d.user.clone())
            },
        ])
    }
}

/// One Slack user projected into the owned DTO (the Relational `users` directory row).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UserDto {
    /// The user id (`Uxxxx`).
    pub id: String,
    /// The handle (`name`).
    pub name: String,
    /// The real name / display name.
    pub real_name: String,
    /// Whether this is a bot user.
    pub is_bot: bool,
    /// Whether the account is deleted/deactivated.
    pub deleted: bool,
}

impl UserDto {
    /// The canonical users listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Text, false),
            Column::new("name", ColumnType::Text, false),
            Column::new("real_name", ColumnType::Text, true),
            Column::new("is_bot", ColumnType::Bool, false),
            Column::new("deleted", ColumnType::Bool, false),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            real_name: name.to_string(),
            is_bot: false,
            deleted: false,
        }
    }
}

impl From<&UserDto> for Row {
    fn from(d: &UserDto) -> Self {
        Row::new(vec![
            Value::Text(d.id.clone()),
            Value::Text(d.name.clone()),
            if d.real_name.is_empty() {
                Value::Null
            } else {
                Value::Text(d.real_name.clone())
            },
            Value::Bool(d.is_bot),
            Value::Bool(d.deleted),
        ])
    }
}

/// One reaction projected into the owned DTO (the `reactions` Append/log row). Reactions read as
/// `{name, count, users[]}` but the row model surfaces the emoji name + count (the membership is a
/// relational detail kept minimal for E3 pushdown).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReactionDto {
    /// The emoji name (without the surrounding colons).
    pub name: String,
    /// How many users added this reaction.
    pub count: i64,
}

impl ReactionDto {
    /// The canonical reactions listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("count", ColumnType::Int, false),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(name: &str, count: i64) -> Self {
        Self {
            name: name.to_string(),
            count,
        }
    }
}

impl From<&ReactionDto> for Row {
    fn from(d: &ReactionDto) -> Self {
        Row::new(vec![Value::Text(d.name.clone()), Value::Int(d.count)])
    }
}
