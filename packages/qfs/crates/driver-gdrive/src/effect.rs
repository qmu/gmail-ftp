//! [`DriveEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one
//! of these against the Drive API under `COMMIT`.
//!
//! ## Why an explicit effect enum
//! The closed core [`EffectKind`] (`Insert`/`Upsert`/`Update`/`Remove`/`Call`) is universal. The
//! Drive driver maps each onto a concrete Drive op via the `(kind, path, args)` triple:
//! - `INSERT INTO /drive/...`   → [`DriveEffect::Upload`] (a fresh file under a resolved parent)
//! - `UPSERT INTO /drive/...`   → [`DriveEffect::Update`] (retry-safe content replace by id) or
//!   [`DriveEffect::Upload`] when no `file_id` key is present (create)
//! - `UPDATE /drive/...`        → [`DriveEffect::Move`] (rename and/or re-parent)
//! - `REMOVE id:<file>`         → [`DriveEffect::Trash`] (default; irreversible) or
//!   [`DriveEffect::Delete`] when the `hard_delete` flag column is set (irreversible)
//! - `CALL drive.copy`          → [`DriveEffect::Copy`] (server-side copy; the `cp` apply)
//!
//! The well-known row columns carry the resolved ids/bytes the planner snapshotted at plan time
//! (RFD §5 snapshot-resolution). No vendor type appears here. `Trash`/`Delete` carry
//! `irreversible = true` for RFD §6 PREVIEW gating.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::error::DriveError;
use crate::path::DrivePath;

/// Row column carrying the resolved parent folder id (the upload destination).
pub const PARENT_ID_COL: &str = "parent_id";
/// Row column carrying the resolved file id (the UPSERT/UPDATE/REMOVE key).
pub const FILE_ID_COL: &str = "file_id";
/// Row column carrying the file name (upload / rename).
pub const NAME_COL: &str = "name";
/// Row column carrying the MIME type for an upload.
pub const MIME_COL: &str = "mime_type";
/// Row column carrying the file content bytes for an upload/update.
pub const BYTES_COL: &str = "bytes";
/// Row column carrying parent ids to add (comma-separated) for a move.
pub const ADD_PARENTS_COL: &str = "add_parents";
/// Row column carrying parent ids to remove (comma-separated) for a move.
pub const REMOVE_PARENTS_COL: &str = "remove_parents";
/// Row column flagging an irreversible **permanent** delete instead of the default trash.
pub const HARD_DELETE_COL: &str = "hard_delete";

/// One fully-decoded Drive effect — what the apply leg executes against the API. Owned DTOs; no
/// google type appears here. `Trash`, `Delete` are irreversible (RFD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DriveEffect {
    /// Create a new file under `parent` (`INSERT`, or `UPSERT` with no `file_id`).
    Upload {
        /// The resolved parent folder id.
        parent: String,
        /// The new file name.
        name: String,
        /// The MIME type.
        mime: String,
        /// The file content bytes.
        bytes: Vec<u8>,
    },
    /// Replace an existing file's content by id (`UPSERT` with a `file_id`) — retry-safe.
    Update {
        /// The file id to replace.
        id: String,
        /// The MIME type.
        mime: String,
        /// The new content bytes.
        bytes: Vec<u8>,
    },
    /// Rename and/or re-parent a file (`UPDATE`) — the metadata-only move.
    Move {
        /// The file id to move/rename.
        id: String,
        /// The new name, if renamed.
        new_name: Option<String>,
        /// Parent ids to add.
        add_parents: Vec<String>,
        /// Parent ids to remove.
        remove_parents: Vec<String>,
    },
    /// Server-side copy a file (`CALL drive.copy` / the `cp` apply).
    Copy {
        /// The source file id.
        id: String,
        /// The destination parent id.
        parent: String,
        /// The copy's name.
        name: String,
    },
    /// Trash a file (`REMOVE` default) — irreversible but recoverable from trash, **not** a
    /// permanent delete.
    Trash {
        /// The file id to trash.
        id: String,
    },
    /// Permanently delete a file (`REMOVE` with `hard_delete = true`) — irreversible.
    Delete {
        /// The file id to permanently delete.
        id: String,
    },
}

impl DriveEffect {
    /// Decode a runtime [`EffectNode`] into the concrete Drive operation.
    ///
    /// # Errors
    /// [`DriveError`] if the `(kind, path)` pair is not one the Drive driver services, or the
    /// row args carry no usable payload.
    pub fn from_node(node: &EffectNode) -> Result<Self, DriveError> {
        let path = DrivePath::parse_str(node.target.path.as_str())?;
        match &node.kind {
            EffectKind::Insert => Self::decode_upload(node, &path),
            EffectKind::Upsert => Self::decode_upsert(node, &path),
            EffectKind::Update => Self::decode_move(node, &path),
            EffectKind::Remove => Self::decode_remove(node, &path),
            EffectKind::Call(proc) => Self::decode_call(proc.as_str(), node),
            other => Err(DriveError::MalformedEffect {
                verb: "EFFECT",
                path: node.target.path.as_str().to_string(),
                reason: format!("{} is not serviced by the Drive driver", other.label()),
            }),
        }
    }

    fn decode_upload(node: &EffectNode, path: &DrivePath) -> Result<Self, DriveError> {
        let parent = text_col(node, PARENT_ID_COL).ok_or_else(|| DriveError::MalformedEffect {
            verb: "INSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("upload needs the resolved `{PARENT_ID_COL}`"),
        })?;
        let name = upload_name(node, path)?;
        Ok(DriveEffect::Upload {
            parent,
            name,
            mime: text_col(node, MIME_COL)
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            bytes: bytes_col(node),
        })
    }

    fn decode_upsert(node: &EffectNode, path: &DrivePath) -> Result<Self, DriveError> {
        // UPSERT keyed by a resolved file id replaces content (retry-safe); without one it creates.
        if let Some(id) = text_col(node, FILE_ID_COL) {
            return Ok(DriveEffect::Update {
                id,
                mime: text_col(node, MIME_COL)
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                bytes: bytes_col(node),
            });
        }
        Self::decode_upload(node, path)
    }

    fn decode_move(node: &EffectNode, _path: &DrivePath) -> Result<Self, DriveError> {
        let id = text_col(node, FILE_ID_COL).ok_or_else(|| DriveError::MalformedEffect {
            verb: "UPDATE",
            path: node.target.path.as_str().to_string(),
            reason: format!("UPDATE (rename/move) needs the resolved `{FILE_ID_COL}`"),
        })?;
        let new_name = text_col(node, NAME_COL);
        let add_parents = list_col(node, ADD_PARENTS_COL);
        let remove_parents = list_col(node, REMOVE_PARENTS_COL);
        if new_name.is_none() && add_parents.is_empty() && remove_parents.is_empty() {
            return Err(DriveError::MalformedEffect {
                verb: "UPDATE",
                path: node.target.path.as_str().to_string(),
                reason: format!(
                    "UPDATE changes nothing (set `{NAME_COL}`/`{ADD_PARENTS_COL}`/`{REMOVE_PARENTS_COL}`)"
                ),
            });
        }
        Ok(DriveEffect::Move {
            id,
            new_name,
            add_parents,
            remove_parents,
        })
    }

    fn decode_remove(node: &EffectNode, path: &DrivePath) -> Result<Self, DriveError> {
        let id = match path {
            DrivePath::ById { id, .. } => id.clone(),
            _ => text_col(node, FILE_ID_COL).ok_or_else(|| DriveError::CapabilityDenied {
                path: node.target.path.as_str().to_string(),
                verb: "REMOVE",
            })?,
        };
        // `hard_delete = true` selects the irreversible permanent delete; default is trash.
        if bool_col(node, HARD_DELETE_COL) {
            Ok(DriveEffect::Delete { id })
        } else {
            Ok(DriveEffect::Trash { id })
        }
    }

    fn decode_call(proc: &str, node: &EffectNode) -> Result<Self, DriveError> {
        if proc != "drive.copy" {
            return Err(DriveError::UnknownProcedure(proc.to_string()));
        }
        let id = text_col(node, FILE_ID_COL).ok_or_else(|| DriveError::MalformedEffect {
            verb: "CALL",
            path: node.target.path.as_str().to_string(),
            reason: format!("drive.copy needs the source `{FILE_ID_COL}`"),
        })?;
        let parent = text_col(node, PARENT_ID_COL).ok_or_else(|| DriveError::MalformedEffect {
            verb: "CALL",
            path: node.target.path.as_str().to_string(),
            reason: format!("drive.copy needs the destination `{PARENT_ID_COL}`"),
        })?;
        let name = text_col(node, NAME_COL).ok_or_else(|| DriveError::MalformedEffect {
            verb: "CALL",
            path: node.target.path.as_str().to_string(),
            reason: format!("drive.copy needs the copy `{NAME_COL}`"),
        })?;
        Ok(DriveEffect::Copy { id, parent, name })
    }

    /// Whether this effect is irreversible (RFD §10): both the trash and the hard delete.
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        matches!(self, DriveEffect::Trash { .. } | DriveEffect::Delete { .. })
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            DriveEffect::Upload { .. } => "INSERT",
            DriveEffect::Update { .. } => "UPSERT",
            DriveEffect::Move { .. } => "UPDATE",
            DriveEffect::Copy { .. } => "CALL",
            DriveEffect::Trash { .. } | DriveEffect::Delete { .. } => "REMOVE",
        }
    }
}

/// The upload file name: the explicit `name` column, else the path's terminal segment.
fn upload_name(node: &EffectNode, path: &DrivePath) -> Result<String, DriveError> {
    if let Some(name) = text_col(node, NAME_COL) {
        return Ok(name);
    }
    path.leaf_name()
        .map(str::to_string)
        .ok_or_else(|| DriveError::MalformedEffect {
            verb: "INSERT",
            path: node.target.path.as_str().to_string(),
            reason: format!("upload needs a `{NAME_COL}` or a named path segment"),
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

/// Read a `Bool` value from the node's first row by column name (default `false`).
fn bool_col(node: &EffectNode, name: &str) -> bool {
    let Some(idx) = node.args.schema.columns.iter().position(|c| c.name == name) else {
        return false;
    };
    matches!(
        node.args.rows.first().and_then(|r| r.values.get(idx)),
        Some(Value::Bool(true))
    )
}

/// Read the content bytes column (`Bytes`, or `Text` treated as UTF-8 bytes) from the first row.
fn bytes_col(node: &EffectNode) -> Vec<u8> {
    let Some(idx) = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == BYTES_COL)
    else {
        return Vec::new();
    };
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Bytes(b)) => b.clone(),
        Some(Value::Text(t)) => t.clone().into_bytes(),
        _ => Vec::new(),
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
