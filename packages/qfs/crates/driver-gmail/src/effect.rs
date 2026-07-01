//! [`GmailEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one
//! of these against the Gmail API under `COMMIT`.
//!
//! ## Why an explicit effect enum
//! The closed core [`EffectKind`] (`Insert`/`Upsert`/`Update`/`Remove`/`Call`) is universal. The
//! Gmail driver maps each onto a concrete Gmail op via the `(kind, path, args)` triple:
//! - `INSERT INTO /mail/drafts`  → [`GmailEffect::CreateDraft`]
//! - `UPSERT INTO /mail/drafts`  → [`GmailEffect::UpsertDraft`] (retry-safe, keyed by draft id)
//! - `UPDATE /mail/<label>`      → [`GmailEffect::ModifyLabels`]
//! - `REMOVE id:<msg>`           → [`GmailEffect::TrashMessage`]
//! - `REMOVE id:thread:<id>`     → [`GmailEffect::TrashThread`]
//! - `CALL mail.send`            → [`GmailEffect::Send`] (irreversible)
//!
//! The draft fields ride in well-known row columns ([`TO_COL`] etc.); the label add/remove ride
//! in [`ADD_LABELS_COL`]/[`REMOVE_LABELS_COL`]. No vendor type appears here.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::error::GmailError;
use crate::mime;
use crate::path::MailPath;
use crate::schema::{Attachment, MailDraft};

/// Row column carrying the draft id (the `UPSERT` key, and the `mail.send` draft-id de-dupe key).
pub const DRAFT_ID_COL: &str = "draft_id";
/// Row column carrying the `To` recipients (comma-separated).
pub const TO_COL: &str = "to";
/// Row column carrying the `Cc` recipients (comma-separated).
pub const CC_COL: &str = "cc";
/// Row column carrying the `Subject`.
pub const SUBJECT_COL: &str = "subject";
/// Row column carrying the plain-text body.
pub const BODY_COL: &str = "body";
/// Row column carrying label ids to add (comma-separated) for a label `UPDATE`.
pub const ADD_LABELS_COL: &str = "add_labels";
/// Row column carrying label ids to remove (comma-separated) for a label `UPDATE`.
pub const REMOVE_LABELS_COL: &str = "remove_labels";
/// Row column carrying a new label's name (`INSERT INTO /mail/labels`, the label-create `name`).
pub const NAME_COL: &str = "name";

/// One fully-decoded Gmail effect — what the apply leg executes against the API. Owned DTOs;
/// no google type appears here. `Send`, `TrashMessage`, `TrashThread` are irreversible (RFD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GmailEffect {
    /// Create a draft (`INSERT INTO /mail/drafts`).
    CreateDraft {
        /// The draft to create.
        draft: MailDraft,
    },
    /// Create-or-replace a draft by id (`UPSERT INTO /mail/drafts`) — retry-safe.
    UpsertDraft {
        /// The draft id to replace.
        id: String,
        /// The draft content.
        draft: MailDraft,
    },
    /// Create a new user label (`INSERT INTO /mail/labels`, gmail-ftp `mkdir`) — reversible.
    CreateLabel {
        /// The new label's name (may be nested, e.g. `Work/Receipts`).
        name: String,
    },
    /// Modify a message's labels (`UPDATE /mail/<label>`).
    ModifyLabels {
        /// The message id whose labels change.
        message: String,
        /// Label ids to add.
        add: Vec<String>,
        /// Label ids to remove.
        remove: Vec<String>,
    },
    /// Trash a single message (`REMOVE id:<msg>`) — the `gmail.modify` trash op, not delete.
    TrashMessage {
        /// The message id to trash.
        id: String,
    },
    /// Trash a whole thread (`REMOVE id:thread:<id>`).
    TrashThread {
        /// The thread id to trash.
        id: String,
    },
    /// Send a draft (`CALL mail.send`) — irreversible. Carries the draft id (the de-dupe key)
    /// when sending a previously-created draft, or a freshly-built draft to create-then-send.
    Send {
        /// An existing draft id to send (the retry-safe path), or `None` to send `draft`.
        draft_id: Option<String>,
        /// The draft to create-then-send when `draft_id` is `None`.
        draft: Option<MailDraft>,
    },
}

impl GmailEffect {
    /// Decode a runtime [`EffectNode`] into the concrete Gmail operation.
    ///
    /// # Errors
    /// [`GmailError`] if the `(kind, path)` pair is not one the Gmail driver services, or the
    /// row args carry no usable payload.
    pub fn from_node(node: &EffectNode) -> Result<Self, GmailError> {
        let path = MailPath::parse_str(node.target.path.as_str())?;
        match &node.kind {
            EffectKind::Insert => match &path {
                MailPath::Labels => Self::decode_create_label(node),
                _ => Self::decode_create_draft(node, &path),
            },
            EffectKind::Upsert => Self::decode_upsert_draft(node, &path),
            EffectKind::Update => Self::decode_modify_labels(node, &path),
            EffectKind::Remove => Self::decode_trash(&path, node),
            EffectKind::Call(proc) => Self::decode_call(proc.as_str(), node),
            other => Err(GmailError::MalformedEffect {
                verb: "EFFECT",
                path: node.target.path.as_str().to_string(),
                reason: format!("{} is not serviced by the Gmail driver", other.label()),
            }),
        }
    }

    fn decode_create_draft(node: &EffectNode, path: &MailPath) -> Result<Self, GmailError> {
        require_drafts(path, "INSERT", node)?;
        let draft = draft_from_row(node)?;
        Ok(GmailEffect::CreateDraft { draft })
    }

    /// Decode `INSERT INTO /mail/labels VALUES ('<name>')` — the label-create (gmail-ftp `mkdir`).
    fn decode_create_label(node: &EffectNode) -> Result<Self, GmailError> {
        let name = text_col(node, NAME_COL).ok_or_else(|| GmailError::MalformedEffect {
            verb: "INSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("INSERT INTO /mail/labels needs a `{NAME_COL}` for the new label"),
        })?;
        Ok(GmailEffect::CreateLabel { name })
    }

    fn decode_upsert_draft(node: &EffectNode, path: &MailPath) -> Result<Self, GmailError> {
        require_drafts(path, "UPSERT", node)?;
        let draft = draft_from_row(node)?;
        let id = draft
            .id
            .clone()
            .ok_or_else(|| GmailError::MalformedEffect {
                verb: "UPSERT",
                path: node.target.path.as_str().to_string(),
                reason: format!("UPSERT INTO /mail/drafts needs a `{DRAFT_ID_COL}` to key on"),
            })?;
        Ok(GmailEffect::UpsertDraft { id, draft })
    }

    fn decode_modify_labels(node: &EffectNode, path: &MailPath) -> Result<Self, GmailError> {
        // UPDATE targets a label collection; the message id rides in the args.
        let (MailPath::Label { .. } | MailPath::Message { .. }) = path else {
            return Err(GmailError::CapabilityDenied {
                path: node.target.path.as_str().to_string(),
                verb: "UPDATE",
            });
        };
        let message = match path {
            MailPath::Message { id } => id.clone(),
            _ => text_col(node, "id").ok_or_else(|| GmailError::MalformedEffect {
                verb: "UPDATE",
                path: node.target.path.as_str().to_string(),
                reason: "UPDATE needs the target message `id` in its args".to_string(),
            })?,
        };
        let add = list_col(node, ADD_LABELS_COL);
        let remove = list_col(node, REMOVE_LABELS_COL);
        if add.is_empty() && remove.is_empty() {
            return Err(GmailError::MalformedEffect {
                verb: "UPDATE",
                path: node.target.path.as_str().to_string(),
                reason: format!(
                    "UPDATE names no labels (set `{ADD_LABELS_COL}`/`{REMOVE_LABELS_COL}`)"
                ),
            });
        }
        Ok(GmailEffect::ModifyLabels {
            message,
            add,
            remove,
        })
    }

    fn decode_trash(path: &MailPath, node: &EffectNode) -> Result<Self, GmailError> {
        match path {
            MailPath::Message { id } => Ok(GmailEffect::TrashMessage { id: id.clone() }),
            MailPath::Thread { id } => Ok(GmailEffect::TrashThread { id: id.clone() }),
            _ => Err(GmailError::CapabilityDenied {
                path: node.target.path.as_str().to_string(),
                verb: "REMOVE",
            }),
        }
    }

    fn decode_call(proc: &str, node: &EffectNode) -> Result<Self, GmailError> {
        if proc != "mail.send" {
            return Err(GmailError::UnknownProcedure(proc.to_string()));
        }
        // `mail.send` may carry an existing draft id (the retry-safe de-dupe path) or a draft to
        // create-then-send. Prefer the draft id when present.
        if let Some(draft_id) = text_col(node, DRAFT_ID_COL) {
            return Ok(GmailEffect::Send {
                draft_id: Some(draft_id),
                draft: None,
            });
        }
        let draft = draft_from_row(node)?;
        Ok(GmailEffect::Send {
            draft_id: None,
            draft: Some(draft),
        })
    }

    /// Whether this effect is irreversible (RFD §10): `Send` + both trash ops.
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        matches!(
            self,
            GmailEffect::Send { .. }
                | GmailEffect::TrashMessage { .. }
                | GmailEffect::TrashThread { .. }
        )
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            GmailEffect::CreateDraft { .. } | GmailEffect::CreateLabel { .. } => "INSERT",
            GmailEffect::UpsertDraft { .. } => "UPSERT",
            GmailEffect::ModifyLabels { .. } => "UPDATE",
            GmailEffect::TrashMessage { .. } | GmailEffect::TrashThread { .. } => "REMOVE",
            GmailEffect::Send { .. } => "CALL",
        }
    }
}

/// Build the base64url `raw` message for a draft (the value the Gmail `raw` field carries).
///
/// # Errors
/// [`GmailError::Mime`] if the draft has no recipients.
pub fn draft_raw(draft: &MailDraft) -> Result<String, GmailError> {
    mime::raw_base64url(draft)
}

/// Require the path be `/mail/drafts` for a draft write; otherwise a capability denial.
fn require_drafts(
    path: &MailPath,
    verb: &'static str,
    node: &EffectNode,
) -> Result<(), GmailError> {
    if matches!(path, MailPath::Drafts) {
        Ok(())
    } else {
        Err(GmailError::CapabilityDenied {
            path: node.target.path.as_str().to_string(),
            verb,
        })
    }
}

/// Build a [`MailDraft`] from the node's first row, reading the well-known draft columns.
fn draft_from_row(node: &EffectNode) -> Result<MailDraft, GmailError> {
    let to = list_col(node, TO_COL);
    if to.is_empty() {
        return Err(GmailError::MalformedEffect {
            verb: "INSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("draft has no `{TO_COL}` recipients"),
        });
    }
    Ok(MailDraft {
        id: text_col(node, DRAFT_ID_COL),
        to,
        cc: list_col(node, CC_COL),
        subject: text_col(node, SUBJECT_COL).unwrap_or_default(),
        body: text_col(node, BODY_COL).unwrap_or_default(),
        // Attachments arrive as an `Array(Struct{filename, mime, bytes})` column when present.
        attachments: attachments_col(node),
    })
}

/// Read a `Text` value from the node's first row by column name.
fn text_col(node: &EffectNode, name: &str) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Read a comma-separated `Text` column into a list of trimmed, non-empty items.
fn list_col(node: &EffectNode, name: &str) -> Vec<String> {
    text_col(node, name)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Read the `attachments` `Array(Struct{filename, mime, bytes})` column from the first row.
fn attachments_col(node: &EffectNode) -> Vec<Attachment> {
    let Some(idx) = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == "attachments")
    else {
        return Vec::new();
    };
    let Some(Value::Array(items)) = node.args.rows.first().and_then(|r| r.values.get(idx)) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let Value::Struct(fields) = v else {
                return None;
            };
            let filename = match fields.get("filename") {
                Some(Value::Text(t)) => t.clone(),
                _ => return None,
            };
            let mime = match fields.get("mime") {
                Some(Value::Text(t)) => t.clone(),
                _ => "application/octet-stream".to_string(),
            };
            let bytes = match fields.get("bytes") {
                Some(Value::Bytes(b)) => b.clone(),
                Some(Value::Text(t)) => t.clone().into_bytes(),
                _ => Vec::new(),
            };
            Some(Attachment {
                filename,
                mime,
                bytes,
            })
        })
        .collect()
}
