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
use qfs_types::{Predicate, Row};

use crate::client::GDriveClient;
use crate::error::DriveError;
use crate::export::{default_export_target, override_export_target, ExportTarget};
use crate::path::DrivePath;
use crate::query::build_query;
use crate::schema::{FileMeta, FOLDER_MIME};

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

/// The fan-out cap for a folder listing — the engine residual applies the exact `WHERE`/`LIMIT`.
const READ_CAP: u32 = 1_000;

/// Drive's reserved alias for the My Drive root folder (the parent of top-level My Drive items).
const MY_DRIVE_ROOT: &str = "root";

/// Read a `/drive/...` folder listing into [`FileMeta`] rows: resolve the addressed folder to its
/// Drive **file id** by walking parent pointers name-by-name, then list that folder's children.
///
/// The pushed `predicate` narrows Drive's `q` search; the engine still re-applies the exact `WHERE`
/// locally (over-fetch then filter, RFD §6), so a lossy Drive term (`contains`) never returns wrong
/// rows. Trashed files are excluded from a listing unless the predicate asks for them.
///
/// # Errors
/// [`DriveError`] when the path is not a `/drive` address, a path segment names no child, a Shared
/// Drive is unknown, or the client hits an auth / transport / API failure (secret-free `code`).
pub fn read_rows(
    client: &dyn GDriveClient,
    path: &str,
    predicate: Option<&Predicate>,
) -> Result<RowBatch, DriveError> {
    let rows = match DrivePath::parse_str(path)? {
        // The virtual root and the Shared-Drives root list pseudo-directories, not real files.
        DrivePath::Root => corpora_rows(),
        DrivePath::SharedRoot => shared_drive_rows(client)?,
        // My Drive: list the root, or walk a path under it then list the resolved folder.
        DrivePath::MyRoot => list_children(client, MY_DRIVE_ROOT, None, predicate)?,
        DrivePath::My { segments, .. } => {
            let folder = resolve_folder(client, MY_DRIVE_ROOT, None, &segments, path)?;
            list_children(client, &folder, None, predicate)?
        }
        // A Shared Drive: resolve the drive name → id, walk inside it (scoped by `driveId`).
        DrivePath::Shared {
            drive, segments, ..
        } => {
            let drive_id = resolve_shared_drive(client, &drive, path)?;
            let folder = resolve_folder(client, &drive_id, Some(&drive_id), &segments, path)?;
            list_children(client, &folder, Some(&drive_id), predicate)?
        }
        // A folder addressed directly by id — list its children.
        DrivePath::ById { id, .. } => list_children(client, &id, None, predicate)?,
    };
    Ok(RowBatch::new(FileMeta::schema(), rows))
}

/// List a folder's children as rows, narrowed by the pushed predicate and excluding trashed files
/// (unless the predicate already constrains `trashed`).
fn list_children(
    client: &dyn GDriveClient,
    parent_id: &str,
    drive_id: Option<&str>,
    predicate: Option<&Predicate>,
) -> Result<Vec<Row>, DriveError> {
    let pushdown = build_query(Some(parent_id), predicate);
    let query = if pushdown.query.contains("trashed") {
        pushdown.query
    } else if pushdown.query.is_empty() {
        "trashed = false".to_string()
    } else {
        format!("{} and trashed = false", pushdown.query)
    };
    let page = client.list_files(&query, drive_id, Some(READ_CAP))?;
    Ok(page.files.iter().map(FileMeta::to_row).collect())
}

/// Walk `segments` from `start_id`, resolving each name to its child id, returning the final
/// node's id. Each step is one `name = '<seg>' and '<parent>' in parents` lookup against Drive.
fn resolve_folder(
    client: &dyn GDriveClient,
    start_id: &str,
    drive_id: Option<&str>,
    segments: &[String],
    path: &str,
) -> Result<String, DriveError> {
    let mut current = start_id.to_string();
    for segment in segments {
        let query = format!(
            "name = '{}' and '{}' in parents and trashed = false",
            q_escape(segment),
            q_escape(&current),
        );
        let page = client.list_files(&query, drive_id, Some(2))?;
        let next = page
            .files
            .into_iter()
            .next()
            .ok_or_else(|| DriveError::NotFound {
                path: path.to_string(),
                segment: segment.clone(),
                reason: "no child of this name under the parent folder",
            })?;
        current = next.id;
    }
    Ok(current)
}

/// Resolve a Shared Drive name to its drive id.
fn resolve_shared_drive(
    client: &dyn GDriveClient,
    name: &str,
    path: &str,
) -> Result<String, DriveError> {
    client
        .list_drives()?
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| d.id)
        .ok_or_else(|| DriveError::NotFound {
            path: path.to_string(),
            segment: name.to_string(),
            reason: "no Shared Drive of this name",
        })
}

/// `/drive` lists the two corpora (`my`, `shared`) as folder rows.
fn corpora_rows() -> Vec<Row> {
    [crate::path::MY_SEGMENT, crate::path::SHARED_SEGMENT]
        .into_iter()
        .map(|name| folder_row(name, name, String::new()))
        .collect()
}

/// `/drive/shared` lists the named Shared Drives as folder rows.
fn shared_drive_rows(client: &dyn GDriveClient) -> Result<Vec<Row>, DriveError> {
    Ok(client
        .list_drives()?
        .into_iter()
        .map(|d| folder_row(&d.id, &d.name, d.id.clone()))
        .collect())
}

/// A synthetic folder [`FileMeta`] row — for the corpus / Shared-Drive roots, which are listable
/// directories but not real Drive files.
fn folder_row(id: &str, name: &str, drive_id: String) -> Row {
    FileMeta {
        id: id.to_string(),
        name: name.to_string(),
        mime_type: FOLDER_MIME.to_string(),
        parents: Vec::new(),
        size: 0,
        modified_time: 0,
        md5: String::new(),
        rev: String::new(),
        drive_id,
        trashed: false,
    }
    .to_row()
}

/// Escape a value for a single-quoted Drive `q` term (backslash-escape `\` then `'`).
fn q_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod read_rows_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::client::{FilePage, MockDriveClient, RecordedCall};
    use crate::schema::FOLDER_MIME;

    fn page(files: Vec<FileMeta>) -> FilePage {
        FilePage {
            files,
            next_page_token: None,
        }
    }

    #[test]
    fn my_drive_path_walks_names_to_ids_then_lists_children() {
        // /drive/my/Reports: step 1 resolves "Reports" under "root" → folder id "rep1"; step 2
        // lists "rep1"'s children.
        let reports = FileMeta::for_test("rep1", "Reports", FOLDER_MIME, vec!["root".to_string()]);
        let child = FileMeta::for_test("f1", "q3.csv", "text/csv", vec!["rep1".to_string()]);
        let client = MockDriveClient::new()
            .with_list_page(page(vec![reports]))
            .with_list_page(page(vec![child]));

        let batch = read_rows(&client, "/drive/my/Reports", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        // The name column (index 1 of FileMeta::schema) is the child file.
        assert!(matches!(&batch.rows[0].values[1], qfs_types::Value::Text(s) if s == "q3.csv"));

        // The recorded queries prove the name→id walk + the parent-scoped listing.
        let calls = client.recorded();
        let queries: Vec<String> = calls
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ListFiles { query, .. } => Some(query.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(queries.len(), 2);
        assert!(
            queries[0].contains("name = 'Reports'") && queries[0].contains("'root' in parents")
        );
        assert!(queries[1].contains("'rep1' in parents"));
        assert!(queries[1].contains("trashed = false"));
    }

    #[test]
    fn a_missing_path_segment_is_a_structured_not_found() {
        // No page seeded → the first walk lookup finds nothing.
        let client = MockDriveClient::new();
        let err = read_rows(&client, "/drive/my/Nope", None).unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn by_id_lists_a_folder_directly_without_a_walk() {
        let child = FileMeta::for_test("f9", "a.txt", "text/plain", vec!["fold9".to_string()]);
        let client = MockDriveClient::new().with_list_page(page(vec![child]));
        let batch = read_rows(&client, "id:fold9", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        let queries: Vec<String> = client
            .recorded()
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ListFiles { query, .. } => Some(query.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(queries.len(), 1, "no walk for an id: address");
        assert!(queries[0].contains("'fold9' in parents"));
    }
}
