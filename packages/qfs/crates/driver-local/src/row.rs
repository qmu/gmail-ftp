//! [`LocalRow`] — the schema DTO for a directory listing entry (RFD-0001 §5). Owned data
//! only; no `std::fs::DirEntry` crosses the driver boundary (RFD §9 no-vendor-leak).

use qfs_types::{Column, ColumnType, Row, Schema, Value};

/// One entry in a directory/glob listing — the row a `/local/dir` scan yields.
/// Fields mirror `lstat`/`metadata`; `mode` is the Unix permission bits (0 on platforms
/// without them). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalRow {
    /// The entry's final path component (file/dir name).
    pub name: String,
    /// The entry's VFS path within the mount, e.g. `/local/sub/a.md`.
    pub path: String,
    /// The byte length (0 for directories).
    pub size: u64,
    /// The modified time as epoch milliseconds (0 if unavailable).
    pub modified: i64,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The Unix permission bits (e.g. `0o644`); 0 where the platform has none.
    pub mode: u32,
}

impl LocalRow {
    /// The canonical listing [`Schema`] — the typed columns `describe` reports and the
    /// `scan` rows conform to. Stable column order powers deterministic golden snapshots.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("path", ColumnType::Text, false),
            Column::new("size", ColumnType::Int, false),
            Column::new("modified", ColumnType::Timestamp, false),
            Column::new("is_dir", ColumnType::Bool, false),
            Column::new("mode", ColumnType::Int, false),
        ])
    }

    /// Project this row onto the canonical [`LocalRow::schema`] column order as a typed
    /// [`Row`] — the form that flows through a pipeline / `INSERT … FROM` listing.
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.name.clone()),
            Value::Text(self.path.clone()),
            Value::Int(i64::try_from(self.size).unwrap_or(i64::MAX)),
            Value::Timestamp(self.modified),
            Value::Bool(self.is_dir),
            Value::Int(i64::from(self.mode)),
        ])
    }
}
