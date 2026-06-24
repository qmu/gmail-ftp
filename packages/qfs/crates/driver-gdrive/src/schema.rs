//! Owned file/folder/revision DTOs and their canonical typed [`Schema`] (RFD-0001 §5/§9).
//!
//! Drive JSON is translated into these owned DTOs at the [`crate::client`] boundary; the
//! `Driver` trait surface and the effect `Plan` carry **zero** google types (the no-vendor-leak
//! invariant, RFD §9). The file [`Schema`] is the canonical `qfs_types::Schema` so
//! `DESCRIBE /drive/my/<folder>` and type-checking agree on the same typed columns.

use qfs_types::{Column, ColumnType, Row, Schema, Value};

/// The Google-native MIME prefix — a file whose `mime_type` starts with this has **no raw
/// bytes** and must be exported (Docs/Sheets/Slides/Drawings).
pub const GOOGLE_NATIVE_PREFIX: &str = "application/vnd.google-apps.";

/// The folder MIME type — a file with this `mime_type` is a directory in the namespace.
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// One Drive file/folder projected into the owned, vendor-free DTO (RFD §9). `modified_time` is
/// epoch milliseconds (the canonical `Timestamp` runtime form). File bytes are **not** carried
/// here — a listing row holds metadata; bytes are fetched on demand via the read path so a
/// folder scan stays cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileMeta {
    /// The Drive file id.
    pub id: String,
    /// The file/folder name.
    pub name: String,
    /// The Drive MIME type (e.g. `text/plain`, `application/vnd.google-apps.document`).
    pub mime_type: String,
    /// The parent folder ids (Drive is multi-parent: a file may live under several folders).
    pub parents: Vec<String>,
    /// The file size in bytes (0 for folders and Google-native docs without raw bytes).
    pub size: i64,
    /// The last-modified time as epoch milliseconds (0 if unavailable).
    pub modified_time: i64,
    /// The content MD5 checksum, if Drive provided one (empty for folders / native docs).
    pub md5: String,
    /// The head revision id, if known (the `@rev` column source).
    pub rev: String,
    /// The owning Shared Drive id, if this file lives in a Shared Drive (empty for My Drive).
    pub drive_id: String,
    /// Whether the file is in the trash.
    pub trashed: bool,
}

impl FileMeta {
    /// Whether this entry is a folder (a directory in the namespace).
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    /// Whether this entry is a Google-native doc with no raw bytes (must export to read).
    #[must_use]
    pub fn is_google_doc(&self) -> bool {
        self.mime_type.starts_with(GOOGLE_NATIVE_PREFIX) && !self.is_folder()
    }

    /// A test-only constructor for a [`FileMeta`] with the salient identity fields set and the
    /// rest defaulted. Lets a downstream consumer fabricate the otherwise `#[non_exhaustive]`
    /// DTO to seed [`MockDriveClient`](crate::MockDriveClient) in their own tests. Gated behind
    /// `cfg(test)` or the `test-util` feature so it never widens the production surface.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str, mime_type: &str, parents: Vec<String>) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: mime_type.to_string(),
            parents,
            size: 0,
            modified_time: 0,
            md5: String::new(),
            rev: String::new(),
            drive_id: String::new(),
            trashed: false,
        }
    }

    /// The canonical file listing [`Schema`] — the typed columns `DESCRIBE /drive/...` reports
    /// and a folder scan's rows conform to. Stable column order powers golden snapshots.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Text, false),
            Column::new("name", ColumnType::Text, false),
            Column::new("mime_type", ColumnType::Text, false),
            Column::new(
                "parents",
                ColumnType::Array(Box::new(ColumnType::Text)),
                false,
            ),
            Column::new("size", ColumnType::Int, false),
            Column::new("modified_time", ColumnType::Timestamp, false),
            Column::new("md5", ColumnType::Text, false),
            Column::new("is_google_doc", ColumnType::Bool, false),
            Column::new("rev", ColumnType::Text, false),
            Column::new("drive_id", ColumnType::Text, false),
            Column::new("trashed", ColumnType::Bool, false),
        ])
    }

    /// Project this file onto the canonical [`FileMeta::schema`] column order as a typed [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        let parents = self
            .parents
            .iter()
            .map(|p| Value::Text(p.clone()))
            .collect();
        Row::new(vec![
            Value::Text(self.id.clone()),
            Value::Text(self.name.clone()),
            Value::Text(self.mime_type.clone()),
            Value::Array(parents),
            Value::Int(self.size),
            Value::Timestamp(self.modified_time),
            Value::Text(self.md5.clone()),
            Value::Bool(self.is_google_doc()),
            Value::Text(self.rev.clone()),
            Value::Text(self.drive_id.clone()),
            Value::Bool(self.trashed),
        ])
    }
}

/// One Drive revision (`@rev` history). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Revision {
    /// The revision id (the `@<rev>` coordinate).
    pub id: String,
    /// The revision's modified time as epoch milliseconds.
    pub modified_time: i64,
    /// The revision MD5 checksum, if Drive provided one.
    pub md5: String,
}

/// One Shared Drive descriptor (the `/drive/shared/<name>` listing). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedDrive {
    /// The Shared Drive id (threaded as `driveId` + `corpora=drive` on every call).
    pub id: String,
    /// The Shared Drive name (the `/drive/shared/<name>` segment).
    pub name: String,
}

impl SharedDrive {
    /// A test-only constructor for a [`SharedDrive`].
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
        }
    }
}
