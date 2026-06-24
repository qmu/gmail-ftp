//! The Drive **read path** (RFD-0001 §5): turn a file's bytes into rows, choosing between a raw
//! download and a Google-native **export**, and decoding the resulting bytes through a
//! [`qfs_codec::Codec`].
//!
//! Drive is special: a Google-native doc (Docs/Sheets/Slides) has **no raw bytes**, so a read
//! must export to a concrete office/text MIME first ([`crate::export`]). This module models that
//! choice as a pure [`ReadPlan`] (what to fetch + which export, if any) so the plan is
//! deterministic and self-documenting, and a pure [`decode_body`] that runs a codec over the
//! fetched bytes. The actual fetch is the impure client call; everything here is pure.

use qfs_codec::{Codec, RowBatch};

use crate::error::DriveError;
use crate::export::{default_export_target, override_export_target, ExportTarget};
use crate::schema::FileMeta;

/// How a file's content is read: a raw byte download, or an export of a Google-native doc to a
/// concrete MIME. Owned, vendor-free — the deterministic, self-documenting read decision.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadPlan {
    /// Download the file's raw bytes (`files.get?alt=media`).
    Download {
        /// The file id to download.
        id: String,
        /// The pinned revision id, if the address carried one.
        revision: Option<String>,
    },
    /// Export a Google-native doc to a concrete MIME (`files.export`).
    Export {
        /// The file id to export.
        id: String,
        /// The chosen export target (MIME + suffix).
        target: ExportTarget,
    },
}

/// Plan the read for `file`, honouring an optional explicit export override token (from a path
/// `!<token>` suffix or `?export=<token>`). A Google-native doc with no override exports to its
/// default target; a binary file downloads raw (an override on a binary file is ignored — there
/// is nothing to convert).
///
/// # Errors
/// [`DriveError::NoExportTarget`] never fires here (a default always exists for native docs); the
/// `Result` is kept for symmetry with future per-type refusal.
pub fn plan_read(
    file: &FileMeta,
    revision: Option<&str>,
    export_override: Option<&str>,
) -> Result<ReadPlan, DriveError> {
    if file.is_google_doc() {
        let target = match export_override {
            Some(token) => override_export_target(token),
            None => default_export_target(&file.mime_type).ok_or_else(|| {
                DriveError::NoExportTarget {
                    mime: file.mime_type.clone(),
                }
            })?,
        };
        return Ok(ReadPlan::Export {
            id: file.id.clone(),
            target,
        });
    }
    Ok(ReadPlan::Download {
        id: file.id.clone(),
        revision: revision.map(str::to_string),
    })
}

/// Decode a fetched file body into rows through `codec` (the pure `bytes → rows` boundary). The
/// caller selects the codec from the (export or source) MIME; this function never touches the
/// network and never holds a token.
///
/// # Errors
/// [`DriveError::CodecDecode`] if the codec rejects the bytes (carrying its secret-free reason,
/// never the body).
pub fn decode_body(codec: &dyn Codec, bytes: &[u8]) -> Result<RowBatch, DriveError> {
    codec.decode(bytes).map_err(|e| DriveError::CodecDecode {
        reason: e.to_string(),
    })
}
