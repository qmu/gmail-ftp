//! [`ObjEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one of
//! these against the [`ObjectBackend`](crate::backend::ObjectBackend) under `COMMIT`.
//!
//! ## The `(kind, node)` → concrete-op mapping
//! - `Upsert INTO /s3/<bucket>/<key>` (key in path) or `/s3/<bucket>` (key in the row) → a put
//!   (single PUT below the multipart threshold; multipart above — the applier's concern).
//! - `Remove /s3/<bucket>/<key>[@<versionId>]` → a delete (with the version when present).
//!
//! No vendor type appears here.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::error::ObjError;
use crate::path::ObjNode;

/// Row column carrying the object key when an `UPSERT`/`REMOVE` addresses a bucket root.
pub const KEY_COL: &str = "key";
/// Row column carrying the object body bytes (the `UPSERT` payload).
pub const BODY_COL: &str = "body";

/// One fully-decoded object-storage effect — what the apply leg executes. Owned DTOs; no vendor
/// type appears here. A `Put` is retry-safe (UPSERT is idempotent by key); a `Delete` is
/// idempotent, and is **irreversible** when the bucket is non-versioned or a specific `@versionId`
/// is targeted (the object is permanently gone — RFD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjEffect {
    /// An object put (`UPSERT INTO /s3/<bucket>/<key>`).
    Put {
        /// The bucket.
        bucket: String,
        /// The object key.
        key: String,
        /// The object body bytes.
        body: Vec<u8>,
    },
    /// An object delete (`REMOVE /s3/<bucket>/<key>[@<versionId>]`).
    Delete {
        /// The bucket.
        bucket: String,
        /// The object key.
        key: String,
        /// The addressed version id (a specific-version delete is permanent).
        version_id: Option<String>,
    },
}

impl ObjEffect {
    /// Decode a runtime [`EffectNode`] into the concrete object-storage operation.
    ///
    /// # Errors
    /// [`ObjError`] if the `(kind, path)` pair is not one the driver services, or the row args
    /// carry no usable payload.
    pub fn from_node(node: &EffectNode) -> Result<Self, ObjError> {
        let path = ObjNode::parse_str(node.target.path.as_str())?;
        match (&node.kind, &path) {
            // UPSERT addressing a concrete key — the body rides in the row.
            (EffectKind::Upsert | EffectKind::Insert, ObjNode::Object { bucket, key, .. }) => {
                let body = bytes_col(node, BODY_COL).unwrap_or_default();
                Ok(ObjEffect::Put {
                    bucket: bucket.clone(),
                    key: key.clone(),
                    body,
                })
            }
            // UPSERT addressing a bucket root — the key + body ride in the row.
            (EffectKind::Upsert | EffectKind::Insert, ObjNode::Bucket { bucket, .. }) => {
                let key = text_col(node, KEY_COL).ok_or_else(|| ObjError::MalformedEffect {
                    verb: "UPSERT",
                    path: node.target.path.as_str().to_string(),
                    reason: format!("UPSERT INTO a bucket root needs a `{KEY_COL}` column"),
                })?;
                let body = bytes_col(node, BODY_COL).unwrap_or_default();
                Ok(ObjEffect::Put {
                    bucket: bucket.clone(),
                    key,
                    body,
                })
            }
            // REMOVE addressing a concrete key (with optional @versionId from the path).
            (
                EffectKind::Remove,
                ObjNode::Object {
                    bucket,
                    key,
                    version_id,
                    ..
                },
            ) => Ok(ObjEffect::Delete {
                bucket: bucket.clone(),
                key: key.clone(),
                version_id: version_id.clone(),
            }),
            // REMOVE addressing a bucket root — the key rides in the row.
            (EffectKind::Remove, ObjNode::Bucket { bucket, .. }) => {
                let key = text_col(node, KEY_COL).ok_or_else(|| ObjError::MalformedEffect {
                    verb: "REMOVE",
                    path: node.target.path.as_str().to_string(),
                    reason: format!("REMOVE on a bucket root needs a `{KEY_COL}` to delete"),
                })?;
                Ok(ObjEffect::Delete {
                    bucket: bucket.clone(),
                    key,
                    version_id: None,
                })
            }
            // Everything else is not an object-storage write the driver services — a denial.
            (kind, _) => Err(ObjError::CapabilityDenied {
                path: node.target.path.as_str().to_string(),
                verb: static_verb_label(kind),
            }),
        }
    }

    /// The bucket this effect addresses.
    #[must_use]
    pub fn bucket(&self) -> &str {
        match self {
            ObjEffect::Put { bucket, .. } | ObjEffect::Delete { bucket, .. } => bucket,
        }
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            ObjEffect::Put { .. } => "UPSERT",
            ObjEffect::Delete { .. } => "REMOVE",
        }
    }
}

/// The stable `&'static str` label for an effect kind.
fn static_verb_label(kind: &EffectKind) -> &'static str {
    match kind {
        EffectKind::Read => "READ",
        EffectKind::List => "LIST",
        EffectKind::Insert => "INSERT",
        EffectKind::Upsert => "UPSERT",
        EffectKind::Update => "UPDATE",
        EffectKind::Remove => "REMOVE",
        EffectKind::Call(_) => "CALL",
        _ => "WRITE",
    }
}

/// Read a non-empty `Text` value from the node's first row by column name.
fn text_col(node: &EffectNode, name: &str) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name.as_str() == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Read a value column as bytes (a `Bytes` column verbatim, or a `Text` column's UTF-8 bytes).
fn bytes_col(node: &EffectNode, name: &str) -> Option<Vec<u8>> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name.as_str() == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        Some(Value::Text(t)) => Some(t.clone().into_bytes()),
        _ => None,
    }
}
