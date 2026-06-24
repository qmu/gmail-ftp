//! The object-listing relation schema (RFD-0001 §5): the typed `qfs_types::Schema` an `ls` /
//! `DESCRIBE` over a bucket node returns. A single source shared with the [`crate::dto::ObjectMeta`]
//! row projection so the column set and order cannot drift.

use qfs_types::{Column, ColumnType, Schema};

use crate::dto::{
    ETAG_COL, KEY_COL, LAST_MODIFIED_COL, SIZE_COL, STORAGE_CLASS_COL, VERSION_ID_COL,
};

/// The typed schema of the object-listing relation
/// `(key, size, etag, last_modified, version_id?, storage_class)` — the BlobNamespace `ls` row
/// shape. `version_id` is nullable (populated only on versioned buckets).
#[must_use]
pub fn object_listing_schema() -> Schema {
    Schema::new(vec![
        Column::new(KEY_COL, ColumnType::Text, false),
        Column::new(SIZE_COL, ColumnType::Int, false),
        Column::new(ETAG_COL, ColumnType::Text, false),
        Column::new(LAST_MODIFIED_COL, ColumnType::Timestamp, false),
        Column::new(VERSION_ID_COL, ColumnType::Text, true),
        Column::new(STORAGE_CLASS_COL, ColumnType::Text, false),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_listing_columns_in_order() {
        let s = object_listing_schema();
        assert_eq!(s.columns.len(), 6);
        assert_eq!(s.columns[0].name.as_str(), "key");
        assert!(s.column("version_id").unwrap().nullable);
        assert!(!s.column("key").unwrap().nullable);
    }
}
