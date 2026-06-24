//! Owned DTOs for object/list/version metadata (RFD-0001 §9 owned-DTO discipline): the
//! `Serialize`-able rows the engine's `ls`/`DESCRIBE` project, plus the put result. No vendor type
//! (no `aws_sdk_s3::*`, no `http::*`) ever appears here.

use qfs_types::{Row, Value};
use serde::Serialize;

/// The column names the object-listing relation projects, in order. Shared by the schema
/// ([`crate::schema`]) and the [`ObjectMeta::to_row`] projection so the two cannot drift.
pub const KEY_COL: &str = "key";
/// The object size (bytes) column.
pub const SIZE_COL: &str = "size";
/// The object ETag column.
pub const ETAG_COL: &str = "etag";
/// The last-modified timestamp column (RFC3339 text as stored by S3).
pub const LAST_MODIFIED_COL: &str = "last_modified";
/// The `@versionId` column (nullable; populated only on versioned buckets).
pub const VERSION_ID_COL: &str = "version_id";
/// The storage-class column (e.g. `STANDARD`).
pub const STORAGE_CLASS_COL: &str = "storage_class";

/// One object's metadata — the owned DTO an `ls`/`list_objects_v2` yields and a `head_object`
/// returns. Owned, vendor-free. `Serialize` for the `-json` listing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct ObjectMeta {
    /// The object key.
    pub key: String,
    /// The object size in bytes.
    pub size: u64,
    /// The object ETag (an opaque content hash; the optimistic-concurrency coordinate).
    pub etag: String,
    /// The last-modified timestamp, as S3's RFC3339/ISO text (kept verbatim; not re-parsed).
    pub last_modified: String,
    /// The object version id on a versioned bucket (RFD §4 temporal coordinate); `None` otherwise.
    pub version_id: Option<String>,
    /// The storage class (e.g. `STANDARD`, `GLACIER`).
    pub storage_class: String,
}

impl ObjectMeta {
    /// Construct an object-metadata DTO with just a key + size (the common listing case).
    #[must_use]
    pub fn new(key: impl Into<String>, size: u64) -> Self {
        Self {
            key: key.into(),
            size,
            ..Self::default()
        }
    }

    /// Builder: set the ETag.
    #[must_use]
    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = etag.into();
        self
    }

    /// Builder: set the last-modified text.
    #[must_use]
    pub fn with_last_modified(mut self, lm: impl Into<String>) -> Self {
        self.last_modified = lm.into();
        self
    }

    /// Builder: set the version id.
    #[must_use]
    pub fn with_version_id(mut self, vid: impl Into<String>) -> Self {
        self.version_id = Some(vid.into());
        self
    }

    /// Builder: set the storage class.
    #[must_use]
    pub fn with_storage_class(mut self, sc: impl Into<String>) -> Self {
        self.storage_class = sc.into();
        self
    }

    /// Project this metadata onto the listing relation row
    /// `(key, size, etag, last_modified, version_id, storage_class)` — the order the
    /// [`crate::schema::object_listing_schema`] declares.
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.key.clone()),
            Value::Int(i64::try_from(self.size).unwrap_or(i64::MAX)),
            Value::Text(self.etag.clone()),
            Value::Text(self.last_modified.clone()),
            self.version_id.clone().map_or(Value::Null, Value::Text),
            Value::Text(self.storage_class.clone()),
        ])
    }
}

/// One page of a paginated listing — the owned DTO `list_objects_v2` yields. Carries the objects,
/// the common prefixes (the "directory" rollups a delimiter produces), and the continuation token
/// for the next page (`None` at the end). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct ListPage {
    /// The objects in this page, in listing order.
    pub objects: Vec<ObjectMeta>,
    /// The common prefixes (delimiter rollups) in this page.
    pub common_prefixes: Vec<String>,
    /// The continuation token for the next page, or `None` if this is the last page.
    pub next_token: Option<String>,
}

impl ListPage {
    /// Construct a listing page.
    #[must_use]
    pub fn new(objects: Vec<ObjectMeta>) -> Self {
        Self {
            objects,
            common_prefixes: Vec::new(),
            next_token: None,
        }
    }

    /// Builder: attach common prefixes (delimiter rollups).
    #[must_use]
    pub fn with_common_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.common_prefixes = prefixes;
        self
    }

    /// Builder: attach a continuation token (more pages follow).
    #[must_use]
    pub fn with_next_token(mut self, token: impl Into<String>) -> Self {
        self.next_token = Some(token.into());
        self
    }

    /// Whether more pages follow (a continuation token is present).
    #[must_use]
    pub fn has_more(&self) -> bool {
        self.next_token.is_some()
    }

    /// Project every object in the page onto listing rows.
    #[must_use]
    pub fn to_rows(&self) -> Vec<Row> {
        self.objects.iter().map(ObjectMeta::to_row).collect()
    }
}

/// The result of a `put_object` / `complete_multipart` — the new object's ETag and (on a versioned
/// bucket) its assigned `versionId`. The ETag is the optimistic-concurrency coordinate a
/// subsequent conditional write uses. Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct PutResult {
    /// The new object's ETag.
    pub etag: String,
    /// The assigned version id on a versioned bucket; `None` otherwise.
    pub version_id: Option<String>,
}

impl PutResult {
    /// Construct a put result with an ETag.
    #[must_use]
    pub fn new(etag: impl Into<String>) -> Self {
        Self {
            etag: etag.into(),
            version_id: None,
        }
    }

    /// Builder: set the assigned version id.
    #[must_use]
    pub fn with_version_id(mut self, vid: impl Into<String>) -> Self {
        self.version_id = Some(vid.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_meta_projects_to_the_declared_row_order() {
        let meta = ObjectMeta::new("a/b.json", 42)
            .with_etag("\"abc\"")
            .with_last_modified("2026-06-23T00:00:00Z")
            .with_version_id("v7")
            .with_storage_class("STANDARD");
        let row = meta.to_row();
        assert_eq!(row.values[0], Value::Text("a/b.json".to_string()));
        assert_eq!(row.values[1], Value::Int(42));
        assert_eq!(row.values[2], Value::Text("\"abc\"".to_string()));
        assert_eq!(row.values[4], Value::Text("v7".to_string()));
    }

    #[test]
    fn unversioned_object_has_null_version_column() {
        let row = ObjectMeta::new("k", 1).to_row();
        assert_eq!(row.values[4], Value::Null);
    }

    #[test]
    fn list_page_pagination_token_drives_has_more() {
        let page = ListPage::new(vec![ObjectMeta::new("k", 1)])
            .with_common_prefixes(vec!["dir/".to_string()])
            .with_next_token("tok");
        assert!(page.has_more());
        assert_eq!(page.to_rows().len(), 1);
        assert_eq!(page.common_prefixes, vec!["dir/".to_string()]);
    }
}
