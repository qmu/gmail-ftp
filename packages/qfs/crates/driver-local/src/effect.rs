//! [`LocalEffect`] — the owned effect DTOs the evaluator emits for the local FS, and the
//! mapping from a runtime [`EffectNode`] onto them (RFD-0001 §6). The driver's apply leg
//! ([`crate::applier`]) interprets these against the World.
//!
//! ## Why an explicit local-effect enum
//! The closed core [`EffectKind`] (`Read`/`List`/`Insert`/`Upsert`/`Update`/`Remove`/`Call`)
//! is universal. A blob driver realises `cp`/`mv` as plan shapes, not new verbs — so the
//! source side of a copy/move is carried in the effect's row args under a well-known
//! [`SRC_COL`] column. [`LocalEffect::from_node`] decodes the `(kind, target, args)` triple
//! into the concrete local operation, keeping the universal verb set intact while still
//! driving the native copy→verify→[delete] recovery shape (RFD §6).
//!
//! ## Blob payload contract
//! A write (`Upsert`/`Insert`) carries the blob bytes in the first row's [`CONTENT_COL`]
//! value (`Value::Bytes` or `Value::Text`). A copy/move carries the source VFS path in
//! [`SRC_COL`] instead, and the destination is the node `target.path`.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

/// The well-known column whose value holds the blob bytes a write effect publishes.
pub const CONTENT_COL: &str = "content";
/// The well-known column whose value (a VFS path string) marks a copy/move **source**.
pub const SRC_COL: &str = "src";

/// One fully-decoded local filesystem effect — what the apply leg executes. Owned DTOs; no
/// `std::fs` type appears here. `Move`/`Remove` are inherently irreversible (RFD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalEffect {
    /// List a directory / glob set (`Read`/`List` over a `/local` path) — a pure dependency.
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
    /// Remove a blob (`Remove` / `rm`).
    Remove {
        /// The VFS path to delete.
        path: String,
    },
}

/// Why a node could not be decoded into a [`LocalEffect`] — a construction/contract bug
/// surfaced as a terminal effect failure (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// A machine-facing reason.
    pub reason: String,
}

impl LocalEffect {
    /// Decode a runtime [`EffectNode`] into the concrete local operation.
    ///
    /// # Errors
    /// [`DecodeError`] if a write carries no blob payload, or the kind is not one the local
    /// FS driver services (e.g. `Update`/`Call`).
    pub fn from_node(node: &EffectNode) -> Result<Self, DecodeError> {
        let path = node.target.path.as_str().to_string();
        match &node.kind {
            EffectKind::Read | EffectKind::List => Ok(LocalEffect::Scan { path }),
            EffectKind::Remove => Ok(LocalEffect::Remove { path }),
            EffectKind::Insert | EffectKind::Upsert => Self::decode_write(node, path),
            EffectKind::Update => Err(DecodeError {
                reason: "UPDATE is not supported on a blob namespace".to_string(),
            }),
            EffectKind::Call(proc) => Err(DecodeError {
                reason: format!("CALL {proc} is not supported by the local FS driver"),
            }),
            // `EffectKind` is `#[non_exhaustive]`: a future verb the local FS does not yet
            // service is a terminal decode failure, never a panic.
            other => Err(DecodeError {
                reason: format!("{} is not supported by the local FS driver", other.label()),
            }),
        }
    }

    /// Decode an `Insert`/`Upsert` node into a [`LocalEffect::Write`], or — when the first
    /// row carries a [`SRC_COL`] source — a [`LocalEffect::Copy`]/[`LocalEffect::Move`]
    /// (move iff the node is flagged irreversible).
    fn decode_write(node: &EffectNode, dst: String) -> Result<Self, DecodeError> {
        let schema = &node.args.schema;
        let first = node.args.rows.first();

        // Copy/move: a SRC_COL value names the source path.
        if let Some(idx) = schema.columns.iter().position(|c| c.name == SRC_COL) {
            if let Some(Value::Text(src)) = first.and_then(|r| r.values.get(idx)) {
                let src = src.clone();
                return Ok(if node.irreversible {
                    LocalEffect::Move { src, dst }
                } else {
                    LocalEffect::Copy { src, dst }
                });
            }
        }

        // Plain blob write: CONTENT_COL bytes/text from the first row.
        if let Some(idx) = schema.columns.iter().position(|c| c.name == CONTENT_COL) {
            match first.and_then(|r| r.values.get(idx)) {
                Some(Value::Bytes(b)) => {
                    return Ok(LocalEffect::Write {
                        dst,
                        bytes: b.clone(),
                    })
                }
                Some(Value::Text(t)) => {
                    return Ok(LocalEffect::Write {
                        dst,
                        bytes: t.clone().into_bytes(),
                    })
                }
                _ => {}
            }
        }

        // Fallback — a positional / single-column write payload (`VALUES ('hi')`, or a piped
        // relation whose one column isn't named `content`) maps onto the blob when it is
        // unambiguous: exactly one column carrying a `Text`/`Bytes` value. Same write as the
        // explicit `content` contract above, so `upsert into /local/f values ('hi')` writes `f`.
        if schema.columns.len() == 1 {
            match first.and_then(|r| r.values.first()) {
                Some(Value::Bytes(b)) => {
                    return Ok(LocalEffect::Write {
                        dst,
                        bytes: b.clone(),
                    })
                }
                Some(Value::Text(t)) => {
                    return Ok(LocalEffect::Write {
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
        matches!(self, LocalEffect::Move { .. } | LocalEffect::Remove { .. })
    }
}
