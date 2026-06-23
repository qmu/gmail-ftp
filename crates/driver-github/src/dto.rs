//! Owned GitHub DTOs and their canonical typed [`Schema`]s (RFD-0001 §5/§9).
//!
//! GitHub JSON is translated into these owned, vendor-free DTOs at the [`crate::client`]
//! boundary; the `Driver` trait surface and the effect `Plan` carry **zero** octocrab/vendor
//! types (the no-vendor-leak invariant, RFD §9 — a DTO-boundary test asserts no vendor type
//! appears in any public signature). Each DTO has a stable [`Schema`] (powering `DESCRIBE`) and a
//! `From<&DtoX> for Row` projection in the schema's column order (powering golden snapshots).
//!
//! Timestamps are epoch milliseconds (the canonical `Timestamp` runtime form). `labels` /
//! `assignees` are text arrays. No DTO carries a token — the PAT lives only behind the auth
//! seam, never in a decoded body.

use cfs_types::{Column, ColumnType, Row, Schema, Value};

/// Render epoch-ms `i64` 0 as a SQL `NULL` timestamp (an absent time), else a `Timestamp`.
fn ts(ms: i64) -> Value {
    if ms == 0 {
        Value::Null
    } else {
        Value::Timestamp(ms)
    }
}

/// Project a list of strings into a text `Array` value.
fn text_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::Text(s.clone())).collect())
}

/// One GitHub issue projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IssueDto {
    /// The issue number (the `{id}` addressing coordinate within `/issues`).
    pub number: i64,
    /// The issue title.
    pub title: String,
    /// The issue body (markdown).
    pub body: String,
    /// The state (`open` / `closed`).
    pub state: String,
    /// The login of the issue author.
    pub user: String,
    /// The assignee logins.
    pub assignees: Vec<String>,
    /// The label names.
    pub labels: Vec<String>,
    /// Created-at as epoch milliseconds (0 ⇒ unknown ⇒ NULL).
    pub created_at: i64,
    /// Updated-at as epoch milliseconds (0 ⇒ unknown ⇒ NULL).
    pub updated_at: i64,
}

impl IssueDto {
    /// The canonical issue listing [`Schema`] — the typed columns `DESCRIBE .../issues` reports.
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("number", ColumnType::Int, false),
            Column::new("title", ColumnType::Text, false),
            Column::new("body", ColumnType::Text, true),
            Column::new("state", ColumnType::Text, false),
            Column::new("user", ColumnType::Text, false),
            Column::new(
                "assignees",
                ColumnType::Array(Box::new(ColumnType::Text)),
                false,
            ),
            Column::new(
                "labels",
                ColumnType::Array(Box::new(ColumnType::Text)),
                false,
            ),
            Column::new("created_at", ColumnType::Timestamp, true),
            Column::new("updated_at", ColumnType::Timestamp, true),
        ])
    }

    /// A test-only constructor with the salient fields set and the rest defaulted.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(number: i64, title: &str, state: &str) -> Self {
        Self {
            number,
            title: title.to_string(),
            body: String::new(),
            state: state.to_string(),
            user: "octocat".to_string(),
            assignees: Vec::new(),
            labels: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl From<&IssueDto> for Row {
    fn from(d: &IssueDto) -> Self {
        Row::new(vec![
            Value::Int(d.number),
            Value::Text(d.title.clone()),
            Value::Text(d.body.clone()),
            Value::Text(d.state.clone()),
            Value::Text(d.user.clone()),
            text_array(&d.assignees),
            text_array(&d.labels),
            ts(d.created_at),
            ts(d.updated_at),
        ])
    }
}

/// One GitHub pull request projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PullDto {
    /// The PR number.
    pub number: i64,
    /// The PR title.
    pub title: String,
    /// The PR body (markdown).
    pub body: String,
    /// The state (`open` / `closed`).
    pub state: String,
    /// The author login.
    pub user: String,
    /// The head branch ref.
    pub head_ref: String,
    /// The head commit SHA (the optimistic-concurrency coordinate for merge).
    pub head_sha: String,
    /// The base branch ref.
    pub base_ref: String,
    /// Whether the PR is merged.
    pub merged: bool,
    /// Created-at as epoch milliseconds.
    pub created_at: i64,
}

impl PullDto {
    /// The canonical pull-request listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("number", ColumnType::Int, false),
            Column::new("title", ColumnType::Text, false),
            Column::new("body", ColumnType::Text, true),
            Column::new("state", ColumnType::Text, false),
            Column::new("user", ColumnType::Text, false),
            Column::new("head_ref", ColumnType::Text, false),
            Column::new("head_sha", ColumnType::Text, false),
            Column::new("base_ref", ColumnType::Text, false),
            Column::new("merged", ColumnType::Bool, false),
            Column::new("created_at", ColumnType::Timestamp, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(number: i64, title: &str, state: &str, head_sha: &str) -> Self {
        Self {
            number,
            title: title.to_string(),
            body: String::new(),
            state: state.to_string(),
            user: "octocat".to_string(),
            head_ref: "feature".to_string(),
            head_sha: head_sha.to_string(),
            base_ref: "main".to_string(),
            merged: false,
            created_at: 0,
        }
    }
}

impl From<&PullDto> for Row {
    fn from(d: &PullDto) -> Self {
        Row::new(vec![
            Value::Int(d.number),
            Value::Text(d.title.clone()),
            Value::Text(d.body.clone()),
            Value::Text(d.state.clone()),
            Value::Text(d.user.clone()),
            Value::Text(d.head_ref.clone()),
            Value::Text(d.head_sha.clone()),
            Value::Text(d.base_ref.clone()),
            Value::Bool(d.merged),
            ts(d.created_at),
        ])
    }
}

/// One GitHub issue/PR comment projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommentDto {
    /// The comment id.
    pub id: i64,
    /// The author login.
    pub user: String,
    /// The comment body (markdown).
    pub body: String,
    /// Created-at as epoch milliseconds.
    pub created_at: i64,
}

impl CommentDto {
    /// The canonical comment listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("user", ColumnType::Text, false),
            Column::new("body", ColumnType::Text, false),
            Column::new("created_at", ColumnType::Timestamp, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: i64, body: &str) -> Self {
        Self {
            id,
            user: "octocat".to_string(),
            body: body.to_string(),
            created_at: 0,
        }
    }
}

impl From<&CommentDto> for Row {
    fn from(d: &CommentDto) -> Self {
        Row::new(vec![
            Value::Int(d.id),
            Value::Text(d.user.clone()),
            Value::Text(d.body.clone()),
            ts(d.created_at),
        ])
    }
}

/// One GitHub PR review projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReviewDto {
    /// The review id.
    pub id: i64,
    /// The reviewer login.
    pub user: String,
    /// The review state (`APPROVED` / `CHANGES_REQUESTED` / `COMMENTED`).
    pub state: String,
    /// The review body.
    pub body: String,
}

impl ReviewDto {
    /// The canonical review listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("user", ColumnType::Text, false),
            Column::new("state", ColumnType::Text, false),
            Column::new("body", ColumnType::Text, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: i64, state: &str) -> Self {
        Self {
            id,
            user: "octocat".to_string(),
            state: state.to_string(),
            body: String::new(),
        }
    }
}

impl From<&ReviewDto> for Row {
    fn from(d: &ReviewDto) -> Self {
        Row::new(vec![
            Value::Int(d.id),
            Value::Text(d.user.clone()),
            Value::Text(d.state.clone()),
            Value::Text(d.body.clone()),
        ])
    }
}

/// One GitHub Actions workflow run projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunDto {
    /// The run id.
    pub id: i64,
    /// The workflow file name (e.g. `ci.yml`).
    pub name: String,
    /// The run status (`queued` / `in_progress` / `completed`).
    pub status: String,
    /// The run conclusion (`success` / `failure` / … ; empty while running).
    pub conclusion: String,
    /// The head branch the run was triggered on.
    pub head_branch: String,
    /// Created-at as epoch milliseconds.
    pub created_at: i64,
}

impl RunDto {
    /// The canonical run listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("name", ColumnType::Text, false),
            Column::new("status", ColumnType::Text, false),
            Column::new("conclusion", ColumnType::Text, true),
            Column::new("head_branch", ColumnType::Text, false),
            Column::new("created_at", ColumnType::Timestamp, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: i64, status: &str) -> Self {
        Self {
            id,
            name: "ci.yml".to_string(),
            status: status.to_string(),
            conclusion: String::new(),
            head_branch: "main".to_string(),
            created_at: 0,
        }
    }
}

impl From<&RunDto> for Row {
    fn from(d: &RunDto) -> Self {
        Row::new(vec![
            Value::Int(d.id),
            Value::Text(d.name.clone()),
            Value::Text(d.status.clone()),
            Value::Text(d.conclusion.clone()),
            Value::Text(d.head_branch.clone()),
            ts(d.created_at),
        ])
    }
}

/// One GitHub release projected into the owned DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReleaseDto {
    /// The release id.
    pub id: i64,
    /// The git tag the release points at.
    pub tag_name: String,
    /// The release name/title.
    pub name: String,
    /// The release body (markdown notes).
    pub body: String,
    /// Whether this is a draft.
    pub draft: bool,
    /// Whether this is a prerelease.
    pub prerelease: bool,
    /// Created-at as epoch milliseconds.
    pub created_at: i64,
}

impl ReleaseDto {
    /// The canonical release listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("tag_name", ColumnType::Text, false),
            Column::new("name", ColumnType::Text, true),
            Column::new("body", ColumnType::Text, true),
            Column::new("draft", ColumnType::Bool, false),
            Column::new("prerelease", ColumnType::Bool, false),
            Column::new("created_at", ColumnType::Timestamp, true),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(id: i64, tag: &str) -> Self {
        Self {
            id,
            tag_name: tag.to_string(),
            name: tag.to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            created_at: 0,
        }
    }
}

impl From<&ReleaseDto> for Row {
    fn from(d: &ReleaseDto) -> Self {
        Row::new(vec![
            Value::Int(d.id),
            Value::Text(d.tag_name.clone()),
            Value::Text(d.name.clone()),
            Value::Text(d.body.clone()),
            Value::Bool(d.draft),
            Value::Bool(d.prerelease),
            ts(d.created_at),
        ])
    }
}

/// One GitHub branch-ref metadata view projected into the owned DTO. NOT a working tree — see the
/// boundary doc in the crate root (the working tree is the t26 git driver).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BranchDto {
    /// The branch name.
    pub name: String,
    /// The commit SHA the branch ref points at.
    pub sha: String,
    /// Whether the branch is protected.
    pub protected: bool,
}

impl BranchDto {
    /// The canonical branch listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("sha", ColumnType::Text, false),
            Column::new("protected", ColumnType::Bool, false),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(name: &str, sha: &str) -> Self {
        Self {
            name: name.to_string(),
            sha: sha.to_string(),
            protected: false,
        }
    }
}

impl From<&BranchDto> for Row {
    fn from(d: &BranchDto) -> Self {
        Row::new(vec![
            Value::Text(d.name.clone()),
            Value::Text(d.sha.clone()),
            Value::Bool(d.protected),
        ])
    }
}

/// One GitHub content-metadata `files` view projected into the owned DTO. A read-only API
/// metadata view (path + sha + size + type), NOT file content and NOT a working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileMetaDto {
    /// The repo-relative path of the entry.
    pub path: String,
    /// The blob/tree sha.
    pub sha: String,
    /// The size in bytes (0 for directories).
    pub size: i64,
    /// The entry type (`file` / `dir` / `symlink` / `submodule`).
    pub kind: String,
}

impl FileMetaDto {
    /// The canonical files listing [`Schema`].
    #[must_use]
    pub fn schema() -> Schema {
        Schema::new(vec![
            Column::new("path", ColumnType::Text, false),
            Column::new("sha", ColumnType::Text, false),
            Column::new("size", ColumnType::Int, false),
            Column::new("kind", ColumnType::Text, false),
        ])
    }

    /// A test-only constructor.
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(path: &str, sha: &str) -> Self {
        Self {
            path: path.to_string(),
            sha: sha.to_string(),
            size: 0,
            kind: "file".to_string(),
        }
    }
}

impl From<&FileMetaDto> for Row {
    fn from(d: &FileMetaDto) -> Self {
        Row::new(vec![
            Value::Text(d.path.clone()),
            Value::Text(d.sha.clone()),
            Value::Int(d.size),
            Value::Text(d.kind.clone()),
        ])
    }
}
