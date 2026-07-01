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
use qfs_types::{Column, ColumnType, Predicate, Row, Schema, Value};

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
    let parsed = DrivePath::parse_str(path)?;
    let revision = parsed.revision().map(str::to_string);
    match parsed {
        // The virtual root and the Shared-Drives root list pseudo-directories, not real files.
        DrivePath::Root => Ok(folder_batch(corpora_rows())),
        DrivePath::SharedRoot => Ok(folder_batch(shared_drive_rows(client)?)),
        DrivePath::MyRoot => Ok(folder_batch(list_children(
            client,
            MY_DRIVE_ROOT,
            None,
            predicate,
        )?)),
        // My Drive: walk a path under it; the resolved node is listed if it is a folder, or its
        // CONTENT is downloaded/exported if it is a file (gdrive-ftp `get`).
        DrivePath::My { segments, .. } => {
            let meta = resolve_node(client, MY_DRIVE_ROOT, None, &segments, path)?;
            node_batch(client, meta, None, predicate, revision.as_deref())
        }
        // A Shared Drive: resolve the drive name → id, walk inside it (scoped by `driveId`). An empty
        // segment list addresses the drive ROOT (always a folder → list it).
        DrivePath::Shared {
            drive, segments, ..
        } => {
            let drive_id = resolve_shared_drive(client, &drive, path)?;
            if segments.is_empty() {
                return Ok(folder_batch(list_children(
                    client,
                    &drive_id,
                    Some(&drive_id),
                    predicate,
                )?));
            }
            let meta = resolve_node(client, &drive_id, Some(&drive_id), &segments, path)?;
            node_batch(
                client,
                meta,
                Some(&drive_id),
                predicate,
                revision.as_deref(),
            )
        }
        // Addressed directly by id — list children if it is a folder, else download its content.
        // Only the id is known, so fetch the node's metadata to decide which.
        DrivePath::ById { id, .. } => {
            let meta = client.get_file(&id)?;
            node_batch(client, meta, None, predicate, revision.as_deref())
        }
    }
}

/// Wrap folder-listing rows in the canonical [`FileMeta`] schema.
fn folder_batch(rows: Vec<Row>) -> RowBatch {
    RowBatch::new(FileMeta::schema(), rows)
}

/// Dispatch a resolved Drive node by id: a FOLDER lists its children; a FILE downloads (or exports a
/// Google-native doc to) its bytes into a one-row `content` batch — gdrive-ftp `get` (RFD §4
/// blob↔rows, mirroring `/local/<file>` and `/git/<repo>/<file>` so `… |> decode <fmt>` has bytes).
fn node_batch(
    client: &dyn GDriveClient,
    meta: FileMeta,
    drive_id: Option<&str>,
    predicate: Option<&Predicate>,
    revision: Option<&str>,
) -> Result<RowBatch, DriveError> {
    if meta.is_folder() {
        return Ok(folder_batch(list_children(
            client, &meta.id, drive_id, predicate,
        )?));
    }
    // A file: plan the read (raw download, or export for a Google-native doc) and fetch the bytes.
    let (bytes, mime) = match plan_read(&meta, revision, None)? {
        ReadPlan::Download { id, revision } => (
            client.download(&id, revision.as_deref())?,
            meta.mime_type.clone(),
        ),
        ReadPlan::Export { id, target } => (client.export(&id, &target.mime)?, target.mime),
    };
    Ok(content_batch(&meta.name, &mime, bytes))
}

/// The single-row file-content batch: `name` + `mime_type` + the raw bytes under the well-known
/// `content` column (the name the engine's `DECODE` reads, matching the local/git content reads).
fn content_batch(name: &str, mime: &str, bytes: Vec<u8>) -> RowBatch {
    let schema = Schema::new(vec![
        Column::new("name", ColumnType::Text, false),
        Column::new("mime_type", ColumnType::Text, false),
        Column::new("content", ColumnType::Bytes, true),
    ]);
    let row = Row::new(vec![
        Value::Text(name.to_string()),
        Value::Text(mime.to_string()),
        Value::Bytes(bytes),
    ]);
    RowBatch::new(schema, vec![row])
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

/// Walk `segments` from `start_id`, resolving each name to its child node, returning the FINAL
/// node's [`FileMeta`] (folder OR file — the caller lists a folder's children or downloads a file's
/// content). Each step is one `name = '<seg>' and '<parent>' in parents` lookup against Drive;
/// intermediate segments must be folders (a file has no children, so the next lookup finds nothing
/// and fails closed). `segments` is non-empty (the empty-path roots are handled by the caller).
fn resolve_node(
    client: &dyn GDriveClient,
    start_id: &str,
    drive_id: Option<&str>,
    segments: &[String],
    path: &str,
) -> Result<FileMeta, DriveError> {
    let mut current = start_id.to_string();
    let mut node: Option<FileMeta> = None;
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
        current = next.id.clone();
        node = Some(next);
    }
    node.ok_or_else(|| DriveError::NotFound {
        path: path.to_string(),
        segment: String::new(),
        reason: "empty path under the corpus root",
    })
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
        // The id's metadata is fetched once to decide folder-vs-file; a folder then lists its
        // children directly (no name walk).
        let folder = FileMeta::for_test("fold9", "Folder9", FOLDER_MIME, vec![]);
        let child = FileMeta::for_test("f9", "a.txt", "text/plain", vec!["fold9".to_string()]);
        let client = MockDriveClient::new()
            .with_file(folder)
            .with_list_page(page(vec![child]));
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

    #[test]
    fn by_id_to_a_file_downloads_its_content() {
        // gdrive-ftp `get`: `id:<fileId>` to a binary file returns a single `content` row (raw
        // bytes), not an empty folder listing.
        let file = FileMeta::for_test("doc1", "report.txt", "text/plain", vec!["root".to_string()]);
        let client = MockDriveClient::new()
            .with_file(file)
            .with_download("doc1", b"hello drive\n".to_vec());
        let batch = read_rows(&client, "id:doc1", None).unwrap();
        assert_eq!(batch.rows.len(), 1, "a file is exactly one content row");
        let idx = |name: &str| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == name)
                .unwrap_or_else(|| panic!("column {name}"))
        };
        assert!(matches!(&batch.rows[0].values[idx("name")], Value::Text(s) if s == "report.txt"));
        assert_eq!(
            batch.rows[0].values[idx("content")],
            Value::Bytes(b"hello drive\n".to_vec()),
            "the content column holds the file's raw bytes"
        );
    }

    #[test]
    fn my_drive_path_to_a_file_downloads_its_content() {
        // Walk `/drive/my/Reports/q3.csv` to a FILE leaf → content download (the resolved node's
        // metadata comes from the walk, so no extra get_file is needed).
        let reports = FileMeta::for_test("rep1", "Reports", FOLDER_MIME, vec!["root".to_string()]);
        let file = FileMeta::for_test("csv1", "q3.csv", "text/csv", vec!["rep1".to_string()]);
        let client = MockDriveClient::new()
            .with_list_page(page(vec![reports]))
            .with_list_page(page(vec![file]))
            .with_download("csv1", b"a,b\n1,2\n".to_vec());
        let batch = read_rows(&client, "/drive/my/Reports/q3.csv", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        let content_idx = batch
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == "content")
            .expect("content column");
        assert_eq!(
            batch.rows[0].values[content_idx],
            Value::Bytes(b"a,b\n1,2\n".to_vec())
        );
    }
}
