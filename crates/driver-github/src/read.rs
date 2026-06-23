//! The GitHub **read path** (RFD-0001 §5): turn a `SELECT … FROM /github/.../<ns>` into a pure,
//! self-documenting [`ReadPlan`] and decode a list response's JSON into owned DTO [`Row`]s.
//!
//! ## Pagination as a pure bounded fan-out (the genuinely-hard-part note)
//! A paginated `SELECT` is modelled as **one** [`ReadPlan`] node carrying the namespace + pushed
//! params — a single batched fetch *set*, not an imperative page loop. The bound (`MAX_PAGES`) and
//! the Link-header follow live at the edge in [`crate::client`], so the plan stays a single pure
//! node that PREVIEW can show and the planner can batch. A pushdown test asserts that an N-page
//! `SELECT` collapses into exactly one fetch node in the plan.

use cfs_types::{Row, RowBatch, Schema};

use crate::dto::{
    BranchDto, CommentDto, FileMetaDto, IssueDto, PullDto, ReleaseDto, ReviewDto, RunDto,
};
use crate::error::GitHubError;
use crate::path::Namespace;
use crate::pushdown::{build_params, PushdownResult};
use crate::schema::schema_for;

/// A pure, self-documenting read: which namespace under which `owner/repo`, the pushed query
/// params, the optional sub-collection scope, and the **residual** predicate the engine re-checks
/// locally. One node — the planner batches the page fan-out at the edge.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ReadPlan {
    /// `owner/repo` slug.
    pub slug: String,
    /// The collection namespace to list.
    pub namespace: Namespace,
    /// An optional `(id, sub-namespace)` scope (e.g. `("123", Comments)` for issue comments).
    pub sub: Option<(String, Namespace)>,
    /// The pushdown outcome: the pushed params + the truthful residual.
    pub pushdown: PushdownResult,
}

impl ReadPlan {
    /// Plan a list read for `namespace` under `slug`, lowering `predicate` into pushed params +
    /// a truthful residual (the t20 lesson — never drop a predicate that the param does not mean
    /// exactly). Pure: builds data, performs no I/O, holds no token.
    #[must_use]
    pub fn list(
        slug: impl Into<String>,
        namespace: Namespace,
        sub: Option<(String, Namespace)>,
        predicate: Option<&cfs_types::Predicate>,
    ) -> Self {
        Self {
            slug: slug.into(),
            namespace,
            sub: sub.clone(),
            // Pushdown applies to the listable issues/pulls collections; for the others it yields
            // an empty param set + the whole predicate as residual (correctness over completeness).
            pushdown: match sub.as_ref().map(|(_, n)| *n).unwrap_or(namespace) {
                Namespace::Issues | Namespace::Pulls => build_params(predicate),
                _ => PushdownResult {
                    params: Vec::new(),
                    residual: predicate.cloned(),
                },
            },
        }
    }

    /// The pushed query params (what the client sends to the GitHub list endpoint).
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        &self.pushdown.params
    }

    /// The effective namespace (sub if present) — selects the row schema.
    #[must_use]
    pub fn effective_namespace(&self) -> Namespace {
        self.sub.as_ref().map(|(_, n)| *n).unwrap_or(self.namespace)
    }

    /// The row schema this read produces.
    #[must_use]
    pub fn schema(&self) -> Schema {
        schema_for(self.effective_namespace())
    }
}

/// Decode a list JSON value into a typed [`RowBatch`] for `namespace` (the effective namespace).
/// The boundary where GitHub JSON becomes owned DTOs → rows; no vendor type escapes.
///
/// # Errors
/// [`GitHubError::Decode`] never fires (a non-object element is skipped); the `Result` is kept for
/// symmetry with a future strict mode.
pub fn decode_list(
    namespace: Namespace,
    value: &serde_json::Value,
) -> Result<RowBatch, GitHubError> {
    let rows: Vec<Row> = match namespace {
        Namespace::Issues => decode_issues(value).iter().map(Row::from).collect(),
        Namespace::Pulls => decode_pulls(value).iter().map(Row::from).collect(),
        Namespace::Comments => decode_comments(value).iter().map(Row::from).collect(),
        Namespace::Reviews => decode_reviews(value).iter().map(Row::from).collect(),
        Namespace::Runs => decode_runs(value).iter().map(Row::from).collect(),
        Namespace::Releases => decode_releases(value).iter().map(Row::from).collect(),
        Namespace::Files => decode_files(value).iter().map(Row::from).collect(),
        Namespace::Branches => decode_branches(value).iter().map(Row::from).collect(),
    };
    Ok(RowBatch::new(schema_for(namespace), rows))
}

/// The array elements of a JSON list value (an empty slice for a non-array).
fn arr(value: &serde_json::Value) -> &[serde_json::Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn s(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn i(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

fn b(v: &serde_json::Value, key: &str) -> bool {
    v.get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The `login` of a nested user object (e.g. `user.login`), or empty.
fn login(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|u| u.get("login"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `login`s of a nested array-of-users (e.g. `assignees[].login`).
fn logins(v: &serde_json::Value, key: &str) -> Vec<String> {
    arr(v.get(key).unwrap_or(&serde_json::Value::Null))
        .iter()
        .filter_map(|u| u.get("login").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The `name`s of a nested array-of-labels (e.g. `labels[].name`).
fn label_names(v: &serde_json::Value) -> Vec<String> {
    arr(v.get("labels").unwrap_or(&serde_json::Value::Null))
        .iter()
        .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Parse an RFC-3339 UTC timestamp into epoch milliseconds (0 on any unrecognized shape).
fn ms(v: &serde_json::Value, key: &str) -> i64 {
    let raw = v.get(key).and_then(serde_json::Value::as_str).unwrap_or("");
    parse_rfc3339_to_ms(raw)
}

/// Decode the `issues` list JSON into owned [`IssueDto`]s.
#[must_use]
pub fn decode_issues(value: &serde_json::Value) -> Vec<IssueDto> {
    arr(value)
        .iter()
        // GitHub's `/issues` returns PRs too (a PR is an issue); a real `pull_request` key marks
        // those — keep them out of the issues view (they belong to `pulls`).
        .filter(|v| v.get("pull_request").is_none())
        .map(|v| IssueDto {
            number: i(v, "number"),
            title: s(v, "title"),
            body: s(v, "body"),
            state: s(v, "state"),
            user: login(v, "user"),
            assignees: logins(v, "assignees"),
            labels: label_names(v),
            created_at: ms(v, "created_at"),
            updated_at: ms(v, "updated_at"),
        })
        .collect()
}

/// Decode the `pulls` list JSON into owned [`PullDto`]s.
#[must_use]
pub fn decode_pulls(value: &serde_json::Value) -> Vec<PullDto> {
    arr(value)
        .iter()
        .map(|v| PullDto {
            number: i(v, "number"),
            title: s(v, "title"),
            body: s(v, "body"),
            state: s(v, "state"),
            user: login(v, "user"),
            head_ref: v.get("head").map(|h| s(h, "ref")).unwrap_or_default(),
            head_sha: v.get("head").map(|h| s(h, "sha")).unwrap_or_default(),
            base_ref: v.get("base").map(|h| s(h, "ref")).unwrap_or_default(),
            merged: b(v, "merged"),
            created_at: ms(v, "created_at"),
        })
        .collect()
}

/// Decode the `comments` list JSON into owned [`CommentDto`]s.
#[must_use]
pub fn decode_comments(value: &serde_json::Value) -> Vec<CommentDto> {
    arr(value)
        .iter()
        .map(|v| CommentDto {
            id: i(v, "id"),
            user: login(v, "user"),
            body: s(v, "body"),
            created_at: ms(v, "created_at"),
        })
        .collect()
}

/// Decode the `reviews` list JSON into owned [`ReviewDto`]s.
#[must_use]
pub fn decode_reviews(value: &serde_json::Value) -> Vec<ReviewDto> {
    arr(value)
        .iter()
        .map(|v| ReviewDto {
            id: i(v, "id"),
            user: login(v, "user"),
            state: s(v, "state"),
            body: s(v, "body"),
        })
        .collect()
}

/// Decode the `runs` list JSON into owned [`RunDto`]s. GitHub nests runs under `workflow_runs`;
/// accept either a bare array or that envelope.
#[must_use]
pub fn decode_runs(value: &serde_json::Value) -> Vec<RunDto> {
    let items = value
        .get("workflow_runs")
        .map(arr)
        .unwrap_or_else(|| arr(value));
    items
        .iter()
        .map(|v| RunDto {
            id: i(v, "id"),
            name: s(v, "name"),
            status: s(v, "status"),
            conclusion: s(v, "conclusion"),
            head_branch: s(v, "head_branch"),
            created_at: ms(v, "created_at"),
        })
        .collect()
}

/// Decode the `releases` list JSON into owned [`ReleaseDto`]s.
#[must_use]
pub fn decode_releases(value: &serde_json::Value) -> Vec<ReleaseDto> {
    arr(value)
        .iter()
        .map(|v| ReleaseDto {
            id: i(v, "id"),
            tag_name: s(v, "tag_name"),
            name: s(v, "name"),
            body: s(v, "body"),
            draft: b(v, "draft"),
            prerelease: b(v, "prerelease"),
            created_at: ms(v, "created_at"),
        })
        .collect()
}

/// Decode the `files` content-metadata JSON into owned [`FileMetaDto`]s.
#[must_use]
pub fn decode_files(value: &serde_json::Value) -> Vec<FileMetaDto> {
    arr(value)
        .iter()
        .map(|v| FileMetaDto {
            path: s(v, "path"),
            sha: s(v, "sha"),
            size: i(v, "size"),
            kind: s(v, "type"),
        })
        .collect()
}

/// Decode the `branches` list JSON into owned [`BranchDto`]s.
#[must_use]
pub fn decode_branches(value: &serde_json::Value) -> Vec<BranchDto> {
    arr(value)
        .iter()
        .map(|v| BranchDto {
            name: s(v, "name"),
            sha: v.get("commit").map(|c| s(c, "sha")).unwrap_or_default(),
            protected: b(v, "protected"),
        })
        .collect()
}

/// Parse an RFC-3339 UTC timestamp (`YYYY-MM-DDThh:mm:ssZ`) into epoch milliseconds. Tolerant:
/// returns 0 on any shape it does not recognize (a metadata convenience, never load-bearing).
fn parse_rfc3339_to_ms(s: &str) -> i64 {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return 0;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b).and_then(|p| p.parse().ok()) };
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (
        num(0, 4),
        num(5, 7),
        num(8, 10),
        num(11, 13),
        num(14, 16),
        num(17, 19),
    ) else {
        return 0;
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    (days * 86_400 + h * 3600 + mi * 60 + se) * 1000
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm). Pure integer math.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
