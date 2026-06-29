//! [`FsEffect`] — the owned effect DTOs the evaluator emits for `/fs`, and the mapping from a
//! runtime [`EffectNode`] onto them (RFD-0001 §6). The driver's apply leg ([`crate::applier`])
//! interprets these against the World. Templated on `qfs-driver-local`'s `LocalEffect`.
//!
//! ## Blob payload contract
//! A write (`Upsert`/`Insert`) carries the blob bytes in the first row's [`CONTENT_COL`] value
//! (`Value::Bytes` or `Value::Text`). A copy/move carries the source VFS path in [`SRC_COL`]
//! instead, and the destination is the node `target.path`. `Move`/`Remove` are inherently
//! irreversible (RFD §10) — deleting/relocating a real file cannot be undone.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

/// The well-known column whose value holds the blob bytes a write effect publishes.
pub const CONTENT_COL: &str = "content";
/// The well-known column whose value (a VFS path string) marks a copy/move **source**.
pub const SRC_COL: &str = "src";

/// One fully-decoded `/fs` effect — what the apply leg executes. Owned DTOs; no `std::fs` type
/// appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsEffect {
    /// List a directory / glob set (`Read`/`List` over a `/fs` path) — a pure dependency.
    Scan {
        /// The VFS directory or glob pattern to list.
        path: String,
    },
    /// Write/overwrite a blob atomically (`Upsert`/`Insert` with [`CONTENT_COL`] bytes).
    Write {
        /// The destination VFS path.
        dst: String,
        /// The blob bytes to publish.
        bytes: Vec<u8>,
    },
    /// Copy a blob (`Upsert`/`Insert` carrying a [`SRC_COL`] source) — copy→verify, no delete.
    Copy {
        /// The source VFS path.
        src: String,
        /// The destination VFS path.
        dst: String,
    },
    /// Move a blob (a copy carrying [`SRC_COL`] with the node flagged irreversible) —
    /// copy→verify→unlink-source (the source is removed **only after** verification).
    Move {
        /// The source VFS path.
        src: String,
        /// The destination VFS path.
        dst: String,
    },
    /// Remove a blob (`Remove` / `rm`) — irreversible.
    Remove {
        /// The VFS path to delete.
        path: String,
    },
}

/// Why a node could not be decoded into an [`FsEffect`] — a construction/contract bug surfaced as
/// a terminal effect failure (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// A machine-facing reason.
    pub reason: String,
}

impl FsEffect {
    /// Decode a runtime [`EffectNode`] into the concrete `/fs` operation.
    ///
    /// # Errors
    /// [`DecodeError`] if a write carries no blob payload, or the kind is not one the `/fs`
    /// driver services (e.g. `Update`/`Call`).
    pub fn from_node(node: &EffectNode) -> Result<Self, DecodeError> {
        let path = node.target.path.as_str().to_string();
        match &node.kind {
            EffectKind::Read | EffectKind::List => Ok(FsEffect::Scan { path }),
            EffectKind::Remove => Ok(FsEffect::Remove { path }),
            EffectKind::Insert | EffectKind::Upsert => Self::decode_write(node, path),
            EffectKind::Update => Err(DecodeError {
                reason: "UPDATE is not supported on a blob namespace".to_string(),
            }),
            EffectKind::Call(proc) => Err(DecodeError {
                reason: format!("CALL {proc} is not supported by the fs driver"),
            }),
            // `EffectKind` is `#[non_exhaustive]`: a future verb the fs driver does not yet
            // service is a terminal decode failure, never a panic.
            other => Err(DecodeError {
                reason: format!("{} is not supported by the fs driver", other.label()),
            }),
        }
    }

    /// Decode an `Insert`/`Upsert` node into an [`FsEffect::Write`], or — when the first row
    /// carries a [`SRC_COL`] source — an [`FsEffect::Copy`]/[`FsEffect::Move`] (move iff the node
    /// is flagged irreversible).
    fn decode_write(node: &EffectNode, dst: String) -> Result<Self, DecodeError> {
        let schema = &node.args.schema;
        let first = node.args.rows.first();

        // Copy/move: a SRC_COL value names the source path.
        if let Some(idx) = schema.columns.iter().position(|c| c.name == SRC_COL) {
            if let Some(Value::Text(src)) = first.and_then(|r| r.values.get(idx)) {
                let src = src.clone();
                return Ok(if node.irreversible {
                    FsEffect::Move { src, dst }
                } else {
                    FsEffect::Copy { src, dst }
                });
            }
        }

        // Plain blob write: CONTENT_COL bytes/text from the first row.
        if let Some(idx) = schema.columns.iter().position(|c| c.name == CONTENT_COL) {
            match first.and_then(|r| r.values.get(idx)) {
                Some(Value::Bytes(b)) => {
                    return Ok(FsEffect::Write {
                        dst,
                        bytes: b.clone(),
                    })
                }
                Some(Value::Text(t)) => {
                    return Ok(FsEffect::Write {
                        dst,
                        bytes: t.clone().into_bytes(),
                    })
                }
                _ => {}
            }
        }

        Err(DecodeError {
            reason: format!(
                "write to {dst:?} carries no `{CONTENT_COL}` blob payload and no `{SRC_COL}` source"
            ),
        })
    }

    /// Whether applying this effect cannot be undone (`Move`/`Remove`, RFD §10).
    #[must_use]
    pub fn is_irreversible(&self) -> bool {
        matches!(self, FsEffect::Move { .. } | FsEffect::Remove { .. })
    }
}
