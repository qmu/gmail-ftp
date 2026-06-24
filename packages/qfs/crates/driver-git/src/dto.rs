//! Owned column DTOs (`CommitRow`, `ChangeRow`, `BlameRow`, `RefRow`, `ReflogRow`) for the
//! relational/log nodes + the typed [`Schema`] each declares for `DESCRIBE` (RFD §5/§9). No
//! `gix`/vendor type appears — the rows are derived from the in-house object DTOs and lowered
//! to the canonical `qfs_types::Row`/`RowBatch`. The `INSERT INTO /commits` staged row is also
//! decoded here.

use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

use crate::objectdb::Commit;
use crate::repo::ReflogEntry;

/// The `/git/<repo>/commits` row (RFD §5): `sha, tree, parents, author, committer, time,
/// message`. `parents` is rendered as a comma-joined text (the canonical row model carries a
/// real `Array`, but a comma-joined text keeps the JOIN-to-changes story simple at E0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    /// The commit oid (40-hex).
    pub sha: String,
    /// The root tree oid.
    pub tree: String,
    /// The parent oids, comma-joined.
    pub parents: String,
    /// The author `Name <email>`.
    pub author: String,
    /// The committer `Name <email>`.
    pub committer: String,
    /// The committer epoch seconds (the ORDER BY time key).
    pub time: i64,
    /// The commit message.
    pub message: String,
}

impl CommitRow {
    /// Derive a row from an oid + parsed commit.
    #[must_use]
    pub fn from_commit(sha: &str, c: &Commit) -> Self {
        Self {
            sha: sha.to_string(),
            tree: c.tree.as_str().to_string(),
            parents: c
                .parents
                .iter()
                .map(|p| p.as_str().to_string())
                .collect::<Vec<_>>()
                .join(","),
            author: c.author.clone(),
            committer: c.committer.clone(),
            time: c.committer_time,
            message: c.message.clone(),
        }
    }

    /// The typed schema of the commits node.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("sha", ColumnType::Text, false),
            Column::new("tree", ColumnType::Text, false),
            Column::new("parents", ColumnType::Text, false),
            Column::new("author", ColumnType::Text, false),
            Column::new("committer", ColumnType::Text, false),
            Column::new("time", ColumnType::Timestamp, false),
            Column::new("message", ColumnType::Text, false),
        ])
    }

    /// Lower to a canonical [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.sha.clone()),
            Value::Text(self.tree.clone()),
            Value::Text(self.parents.clone()),
            Value::Text(self.author.clone()),
            Value::Text(self.committer.clone()),
            Value::Timestamp(self.time),
            Value::Text(self.message.clone()),
        ])
    }
}

/// The `/git/<repo>/changes` exploded per-file diff row (`sha, path, status, added, removed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    /// The commit this change belongs to (the JOIN key to commits.sha).
    pub sha: String,
    /// The file path changed.
    pub path: String,
    /// The status: `A`(dded)/`M`(odified)/`D`(eleted).
    pub status: String,
    /// Lines added.
    pub added: i64,
    /// Lines removed.
    pub removed: i64,
}

impl ChangeRow {
    /// The typed schema of the changes node.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("sha", ColumnType::Text, false),
            Column::new("path", ColumnType::Text, false),
            Column::new("status", ColumnType::Text, false),
            Column::new("added", ColumnType::Int, false),
            Column::new("removed", ColumnType::Int, false),
        ])
    }

    /// Lower to a canonical [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.sha.clone()),
            Value::Text(self.path.clone()),
            Value::Text(self.status.clone()),
            Value::Int(self.added),
            Value::Int(self.removed),
        ])
    }
}

/// The `/git/<repo>/blame` line-attribution row (`path, line, sha, author, time`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameRow {
    /// The file path.
    pub path: String,
    /// The 1-based line number.
    pub line: i64,
    /// The commit that last touched the line.
    pub sha: String,
    /// The author of that commit.
    pub author: String,
    /// That commit's time.
    pub time: i64,
}

impl BlameRow {
    /// The typed schema of the blame node.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("path", ColumnType::Text, false),
            Column::new("line", ColumnType::Int, false),
            Column::new("sha", ColumnType::Text, false),
            Column::new("author", ColumnType::Text, false),
            Column::new("time", ColumnType::Timestamp, false),
        ])
    }

    /// Lower to a canonical [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.path.clone()),
            Value::Int(self.line),
            Value::Text(self.sha.clone()),
            Value::Text(self.author.clone()),
            Value::Timestamp(self.time),
        ])
    }
}

/// The `/git/<repo>/refs` (and `/tags`) pointer row (`name, oid`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRow {
    /// The ref name (e.g. `refs/heads/main`).
    pub name: String,
    /// The oid it points at.
    pub oid: String,
}

impl RefRow {
    /// The typed schema of the refs/tags node.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("oid", ColumnType::Text, false),
        ])
    }

    /// Lower to a canonical [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.name.clone()),
            Value::Text(self.oid.clone()),
        ])
    }
}

/// The `/git/<repo>/reflog` append-log row (`ref, old, new, who, message, time`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogRow {
    /// The ref the entry belongs to.
    pub ref_name: String,
    /// The prior oid (the recovery target).
    pub old: String,
    /// The new oid.
    pub new: String,
    /// The actor identity.
    pub who: String,
    /// The reflog message.
    pub message: String,
    /// The entry time.
    pub time: i64,
}

impl ReflogRow {
    /// Derive a row from a [`ReflogEntry`].
    #[must_use]
    pub fn from_entry(e: &ReflogEntry) -> Self {
        Self {
            ref_name: e.ref_name.clone(),
            old: e.old.as_str().to_string(),
            new: e.new.as_str().to_string(),
            who: e.who.clone(),
            message: e.message.clone(),
            time: e.time,
        }
    }

    /// The typed schema of the reflog node.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("ref", ColumnType::Text, false),
            Column::new("old", ColumnType::Text, false),
            Column::new("new", ColumnType::Text, false),
            Column::new("who", ColumnType::Text, false),
            Column::new("message", ColumnType::Text, false),
            Column::new("time", ColumnType::Timestamp, false),
        ])
    }

    /// Lower to a canonical [`Row`].
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row::new(vec![
            Value::Text(self.ref_name.clone()),
            Value::Text(self.old.clone()),
            Value::Text(self.new.clone()),
            Value::Text(self.who.clone()),
            Value::Text(self.message.clone()),
            Value::Timestamp(self.time),
        ])
    }
}

/// The BlobFs `ls` listing schema (`name, mode, oid, kind`) — a tree directory listing.
#[must_use]
pub fn blob_listing_schema() -> Schema {
    Schema::new(vec![
        Column::new("name", ColumnType::Text, false),
        Column::new("mode", ColumnType::Text, false),
        Column::new("oid", ColumnType::Text, false),
        Column::new("kind", ColumnType::Text, false),
    ])
}

/// Build a [`RowBatch`] from commit rows.
#[must_use]
pub fn commit_batch(rows: &[CommitRow]) -> RowBatch {
    RowBatch::new(
        CommitRow::schema(),
        rows.iter().map(CommitRow::to_row).collect(),
    )
}

/// Build a [`RowBatch`] from change rows.
#[must_use]
pub fn change_batch(rows: &[ChangeRow]) -> RowBatch {
    RowBatch::new(
        ChangeRow::schema(),
        rows.iter().map(ChangeRow::to_row).collect(),
    )
}

/// Build a [`RowBatch`] from ref rows.
#[must_use]
pub fn ref_batch(rows: &[RefRow]) -> RowBatch {
    RowBatch::new(RefRow::schema(), rows.iter().map(RefRow::to_row).collect())
}

/// Build a [`RowBatch`] from reflog rows.
#[must_use]
pub fn reflog_batch(rows: &[ReflogRow]) -> RowBatch {
    RowBatch::new(
        ReflogRow::schema(),
        rows.iter().map(ReflogRow::to_row).collect(),
    )
}

/// Build a [`RowBatch`] from blame rows.
#[must_use]
pub fn blame_batch(rows: &[BlameRow]) -> RowBatch {
    RowBatch::new(
        BlameRow::schema(),
        rows.iter().map(BlameRow::to_row).collect(),
    )
}
