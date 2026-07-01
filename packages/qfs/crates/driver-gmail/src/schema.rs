//! Owned message/draft/attachment DTOs and their canonical typed [`Schema`] (RFD-0001 §5/§9).
//!
//! Gmail JSON is translated into these owned DTOs at the [`crate::client`] boundary; the
//! `Driver` trait surface and the effect `Plan` carry **zero** google types (the no-vendor-leak
//! invariant, RFD §9). The message [`Schema`] is the canonical `qfs_types::Schema` so
//! `DESCRIBE /mail/<label>` and type-checking agree on the same typed columns. `attachments` is
//! a nested `Array(Struct{..})` column — the `EXPAND` target (RFD §4 "same operator for mail
//! attachments and JSON arrays").

use qfs_types::{Column, ColumnType, Row, Schema, Value};

/// One Gmail message projected into the owned, vendor-free DTO (RFD §9). `date` is epoch
/// milliseconds (the canonical `Timestamp` runtime form). Attachment **bytes are not carried
/// here** — the listing row holds attachment metadata; the bytes are fetched on demand via the
/// attachment read path so a label scan stays cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MailMessage {
    /// The Gmail message id.
    pub id: String,
    /// The Gmail thread id this message belongs to.
    pub thread_id: String,
    /// The label ids applied to this message (e.g. `INBOX`, `UNREAD`).
    pub label_ids: Vec<String>,
    /// The message date as epoch milliseconds (0 if unavailable).
    pub date: i64,
    /// The `From` header value.
    pub from: String,
    /// The `Subject` header value.
    pub subject: String,
    /// The Gmail-provided snippet (a short preview).
    pub snippet: String,
    /// Attachment metadata (filename + mime + attachment id); bytes fetched on demand.
    pub attachments: Vec<AttachmentMeta>,
}

/// Attachment metadata carried in a listing row (no bytes). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AttachmentMeta {
    /// The attachment filename.
    pub filename: String,
    /// The attachment MIME type (e.g. `application/pdf`).
    pub mime: String,
    /// The Gmail attachment id (used to fetch the bytes on demand).
    pub attachment_id: String,
    /// The attachment size in bytes (0 if unknown).
    pub size: i64,
}

/// An attachment with bytes — the form a draft carries (the MIME builder consumes it) and the
/// form an attachment read yields. Owned bytes; no vendor type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Attachment {
    /// The attachment filename.
    pub filename: String,
    /// The attachment MIME type.
    pub mime: String,
    /// The attachment bytes.
    pub bytes: Vec<u8>,
}

/// A draft to create/replace — the owned DTO an `INSERT`/`UPSERT INTO /mail/drafts` carries.
/// The MIME builder ([`crate::mime::build_mime`]) turns it into RFC 5322 bytes. Owned only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MailDraft {
    /// The draft id, when replacing an existing draft (the `UPSERT` key); `None` to create.
    pub id: Option<String>,
    /// The `To` recipients.
    pub to: Vec<String>,
    /// The `Cc` recipients.
    pub cc: Vec<String>,
    /// The `Subject` (may be non-ASCII — encoded RFC 2047 by the MIME builder).
    pub subject: String,
    /// The plain-text body.
    pub body: String,
    /// The attachments (each carries its bytes for the multipart build).
    pub attachments: Vec<Attachment>,
}

impl MailDraft {
    /// A test-only constructor for a [`MailDraft`] with the given recipients/subject/body and the
    /// rest defaulted (no `id`, no `cc`, no attachments). Lets a downstream consumer fabricate the
    /// otherwise `#[non_exhaustive]` DTO directly in their own tests. Gated behind `cfg(test)` or
    /// the `test-util` feature so it never widens the production surface.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(to: Vec<String>, subject: &str, body: &str) -> Self {
        Self {
            id: None,
            to,
            cc: Vec::new(),
            subject: subject.to_string(),
            body: body.to_string(),
            attachments: Vec::new(),
        }
    }
}

/// The label-listing [`Schema`] for `ls /mail` (the mailbox root): one `name` (Text) row per Gmail
/// label — the directory view of the archetype (labels = directories, RFD §4). The client's
/// `list_labels` yields the label names; richer label metadata (id/type/counts) is a later add.
#[must_use]
pub fn label_listing_schema() -> Schema {
    Schema::new(vec![Column::new("name", ColumnType::Text, false)])
}

/// The single-node [`Schema`] for reading one attachment's bytes (`/mail/<label>/<msg>/<att>`,
/// gmail-ftp `get id:att:<msg>:<att>`): the `filename`/`mime`/`size` metadata (from the owning
/// message's part) plus the downloaded `content` (`Bytes`). The `content` column name mirrors the
/// Drive content read so a cross-service attach pipeline lines the two up.
#[must_use]
pub fn attachment_read_schema() -> Schema {
    Schema::new(vec![
        Column::new("filename", ColumnType::Text, false),
        Column::new("mime", ColumnType::Text, false),
        Column::new("size", ColumnType::Int, false),
        Column::new("content", ColumnType::Bytes, true),
    ])
}

impl MailMessage {
    /// The canonical message listing [`Schema`] — the typed columns `DESCRIBE /mail/<label>`
    /// reports and a label scan's rows conform to. Stable column order powers golden snapshots.
    /// `label_ids` is `Array(Text)` and `attachments` is `Array(Struct{filename, mime, size})`
    /// — both `EXPAND` targets (RFD §4).
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Text, false),
            Column::new("thread_id", ColumnType::Text, false),
            Column::new("date", ColumnType::Timestamp, false),
            Column::new("from", ColumnType::Text, false),
            Column::new("subject", ColumnType::Text, false),
            Column::new("snippet", ColumnType::Text, false),
            Column::new(
                "label_ids",
                ColumnType::Array(Box::new(ColumnType::Text)),
                false,
            ),
            Column::new(
                "attachments",
                ColumnType::Array(Box::new(ColumnType::Struct(Self::attachment_schema()))),
                false,
            ),
        ])
    }

    /// The nested attachment struct schema (`{filename, mime, size}`) used inside the
    /// `attachments` `Array(Struct{..})` column.
    #[must_use]
    pub fn attachment_schema() -> Schema {
        Schema::new(vec![
            Column::new("filename", ColumnType::Text, false),
            Column::new("mime", ColumnType::Text, false),
            Column::new("size", ColumnType::Int, false),
        ])
    }

    /// A test-only constructor for a [`MailMessage`] with the salient identity/header fields
    /// set and the rest defaulted (no labels, no attachments, `date` 0, empty `snippet`). Lets a
    /// downstream consumer fabricate the otherwise `#[non_exhaustive]` DTO to seed
    /// [`MockGmailClient::with_message`](crate::MockGmailClient::with_message) in their own tests.
    /// Gated behind `cfg(test)` (the crate's own tests) or the `test-util` feature so it never
    /// widens the production surface.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: &str, thread_id: &str, from: &str, subject: &str) -> Self {
        Self {
            id: id.to_string(),
            thread_id: thread_id.to_string(),
            label_ids: Vec::new(),
            date: 0,
            from: from.to_string(),
            subject: subject.to_string(),
            snippet: String::new(),
            attachments: Vec::new(),
        }
    }

    /// Project this message onto the canonical [`MailMessage::schema`] column order as a typed
    /// [`Row`] — the form a label scan yields into a pipeline.
    #[must_use]
    pub fn to_row(&self) -> Row {
        let labels = self
            .label_ids
            .iter()
            .map(|l| Value::Text(l.clone()))
            .collect();
        let attachments = self
            .attachments
            .iter()
            .map(|a| {
                Value::Struct(qfs_types::Fields::new(vec![
                    ("filename".to_string(), Value::Text(a.filename.clone())),
                    ("mime".to_string(), Value::Text(a.mime.clone())),
                    ("size".to_string(), Value::Int(a.size)),
                ]))
            })
            .collect();
        Row::new(vec![
            Value::Text(self.id.clone()),
            Value::Text(self.thread_id.clone()),
            Value::Timestamp(self.date),
            Value::Text(self.from.clone()),
            Value::Text(self.subject.clone()),
            Value::Text(self.snippet.clone()),
            Value::Array(labels),
            Value::Array(attachments),
        ])
    }
}
