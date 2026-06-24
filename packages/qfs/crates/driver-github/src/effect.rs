//! [`GitHubEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one
//! of these against the GitHub REST API under `COMMIT`.
//!
//! ## Why an explicit effect enum
//! The closed core [`EffectKind`] (`Insert`/`Update`/`Remove`/`Call`/`Read`) is universal. The
//! GitHub driver maps each onto a concrete REST op via the `(kind, path, args)` triple:
//! - `INSERT INTO .../issues`           → [`GitHubEffect::OpenIssue`] (POST)
//! - `INSERT INTO .../pulls`            → [`GitHubEffect::OpenPull`] (POST)
//! - `INSERT INTO .../issues/123/comments` → [`GitHubEffect::PostComment`] (POST; at-least-once)
//! - `INSERT INTO .../releases`         → [`GitHubEffect::CreateRelease`] (POST)
//! - `INSERT INTO .../branches`         → [`GitHubEffect::CreateBranch`] (POST a ref)
//! - `UPDATE .../issues/123 SET state='closed'` / title/body/labels → [`GitHubEffect::PatchIssue`] (PATCH)
//! - `UPDATE .../pulls/7 SET ...`       → [`GitHubEffect::PatchPull`] (PATCH)
//! - `REMOVE .../comments/<id>`         → [`GitHubEffect::DeleteComment`] (DELETE)
//! - `REMOVE .../releases/<id>`         → [`GitHubEffect::DeleteRelease`] (DELETE)
//! - `REMOVE .../branches/<name>`       → [`GitHubEffect::DeleteBranch`] (DELETE a ref)
//! - `CALL github.merge/dispatch/review` → the three [`GitHubEffect`] CALL variants
//!
//! The well-known row columns carry the resolved fields the planner snapshotted at plan time.
//! No vendor type appears here. `merge`/`dispatch`/`Delete*` carry irreversibility per RFD §10/§6.

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::error::GitHubError;
use crate::path::{GitHubPath, Namespace};

/// Row column carrying an issue/PR title (INSERT / PATCH).
pub const TITLE_COL: &str = "title";
/// Row column carrying an issue/PR/comment/release body.
pub const BODY_COL: &str = "body";
/// Row column carrying the `state` (`closed`) for a close UPDATE.
pub const STATE_COL: &str = "state";
/// Row column carrying a comma-separated label set for an edit UPDATE / open INSERT.
pub const LABELS_COL: &str = "labels";
/// Row column carrying the head branch ref for an open-PR INSERT.
pub const HEAD_COL: &str = "head";
/// Row column carrying the base branch ref for an open-PR INSERT.
pub const BASE_COL: &str = "base";
/// Row column carrying a release tag for a create-release INSERT.
pub const TAG_COL: &str = "tag_name";
/// Row column carrying a branch/ref name for a create/delete-branch op.
pub const REF_COL: &str = "ref";
/// Row column carrying the commit SHA a new branch ref points at.
pub const SHA_COL: &str = "sha";
/// Row column carrying the merge method (`squash`/`merge`/`rebase`) for `CALL github.merge`.
pub const METHOD_COL: &str = "method";
/// Row column carrying the workflow file id for `CALL github.dispatch`.
pub const WORKFLOW_COL: &str = "workflow";
/// Row column carrying the JSON `inputs` payload for `CALL github.dispatch`.
pub const INPUTS_COL: &str = "inputs";
/// Row column carrying the review `event` for `CALL github.review`.
pub const EVENT_COL: &str = "event";

/// One fully-decoded GitHub effect — what the apply leg executes against the REST API. Owned
/// DTOs; no octocrab/vendor type appears here. `Merge`/`Dispatch` and the `Delete*` variants are
/// irreversible (RFD §10/§6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitHubEffect {
    /// Open a new issue (`INSERT INTO .../issues`) — POST.
    OpenIssue {
        /// `owner/repo` slug.
        slug: String,
        /// The issue title.
        title: String,
        /// The issue body.
        body: String,
        /// The label set (may be empty).
        labels: Vec<String>,
    },
    /// Open a new pull request (`INSERT INTO .../pulls`) — POST.
    OpenPull {
        /// `owner/repo` slug.
        slug: String,
        /// The PR title.
        title: String,
        /// The PR body.
        body: String,
        /// The head branch.
        head: String,
        /// The base branch.
        base: String,
    },
    /// Post a comment on an issue/PR (`INSERT INTO .../issues/<n>/comments`) — POST.
    /// **Not idempotent**: a timed-out POST may have landed, so the driver documents
    /// at-least-once and never silently retries (RFD §6).
    PostComment {
        /// `owner/repo` slug.
        slug: String,
        /// The issue/PR number the comment attaches to.
        number: String,
        /// The comment body.
        body: String,
    },
    /// Create a release (`INSERT INTO .../releases`) — POST.
    CreateRelease {
        /// `owner/repo` slug.
        slug: String,
        /// The git tag.
        tag_name: String,
        /// The release name.
        name: String,
        /// The release notes body.
        body: String,
    },
    /// Create a branch ref (`INSERT INTO .../branches`) — POST a ref pointing at a SHA.
    CreateBranch {
        /// `owner/repo` slug.
        slug: String,
        /// The new branch name.
        ref_name: String,
        /// The commit SHA the ref points at.
        sha: String,
    },
    /// Edit/close an issue (`UPDATE .../issues/<n> SET ...`) — PATCH (partial field update).
    PatchIssue {
        /// `owner/repo` slug.
        slug: String,
        /// The issue number.
        number: String,
        /// The new state (`closed`/`open`), if set.
        state: Option<String>,
        /// The new title, if set.
        title: Option<String>,
        /// The new body, if set.
        body: Option<String>,
        /// The new label set, if set (replaces the existing set).
        labels: Option<Vec<String>>,
    },
    /// Edit/close a pull request (`UPDATE .../pulls/<n> SET ...`) — PATCH.
    PatchPull {
        /// `owner/repo` slug.
        slug: String,
        /// The PR number.
        number: String,
        /// The new state, if set.
        state: Option<String>,
        /// The new title, if set.
        title: Option<String>,
        /// The new body, if set.
        body: Option<String>,
    },
    /// Delete a comment (`REMOVE .../comments/<id>`) — DELETE (irreversible).
    DeleteComment {
        /// `owner/repo` slug.
        slug: String,
        /// The comment id.
        id: String,
    },
    /// Delete a release (`REMOVE .../releases/<id>`) — DELETE (irreversible).
    DeleteRelease {
        /// `owner/repo` slug.
        slug: String,
        /// The release id.
        id: String,
    },
    /// Delete a branch ref (`REMOVE .../branches/<name>`) — DELETE (irreversible).
    DeleteBranch {
        /// `owner/repo` slug.
        slug: String,
        /// The branch name.
        ref_name: String,
    },
    /// `CALL github.merge(method=>…, sha=>…)` — POST `.../pulls/<n>/merge` (irreversible).
    Merge {
        /// `owner/repo` slug.
        slug: String,
        /// The PR number.
        number: String,
        /// The merge method (`squash`/`merge`/`rebase`).
        method: String,
        /// The optimistic-concurrency head SHA precondition, if supplied.
        sha: Option<String>,
    },
    /// `CALL github.dispatch(workflow=>…, ref=>…, inputs=>…)` — POST
    /// `.../actions/workflows/<wf>/dispatches` (irreversible; returns 204, resolves queued).
    Dispatch {
        /// `owner/repo` slug.
        slug: String,
        /// The workflow file id.
        workflow: String,
        /// The git ref to run on.
        ref_name: String,
        /// The raw JSON `inputs` payload (already serialized).
        inputs: String,
    },
    /// `CALL github.review(event=>…, body=>…)` — POST `.../pulls/<n>/reviews`.
    Review {
        /// `owner/repo` slug.
        slug: String,
        /// The PR number.
        number: String,
        /// The review event (`APPROVE`/`REQUEST_CHANGES`/`COMMENT`).
        event: String,
        /// The review body.
        body: String,
    },
}

impl GitHubEffect {
    /// Decode a runtime [`EffectNode`] into the concrete GitHub operation.
    ///
    /// # Errors
    /// [`GitHubError`] if the `(kind, path)` pair is not one the driver services, or the row
    /// args carry no usable payload.
    pub fn from_node(node: &EffectNode) -> Result<Self, GitHubError> {
        let path = GitHubPath::parse_str(node.target.path.as_str())?;
        match &node.kind {
            EffectKind::Insert => Self::decode_insert(node, &path),
            EffectKind::Update => Self::decode_update(node, &path),
            EffectKind::Remove => Self::decode_remove(node, &path),
            EffectKind::Call(proc) => Self::decode_call(proc.as_str(), node, &path),
            other => Err(GitHubError::MalformedEffect {
                verb: "EFFECT",
                path: node.target.path.as_str().to_string(),
                reason: format!("{} is not serviced by the GitHub driver", other.label()),
            }),
        }
    }

    fn decode_insert(node: &EffectNode, path: &GitHubPath) -> Result<Self, GitHubError> {
        let slug = path.slug();
        match path.effective_namespace() {
            Some(Namespace::Comments) => {
                let number = path.id.clone().ok_or_else(|| {
                    Self::malformed(
                        "INSERT",
                        node,
                        "posting a comment needs the parent issue/PR number in the path",
                    )
                })?;
                Ok(GitHubEffect::PostComment {
                    slug,
                    number,
                    body: req_text(node, BODY_COL, "INSERT", "a comment needs a `body`")?,
                })
            }
            Some(Namespace::Issues) => Ok(GitHubEffect::OpenIssue {
                slug,
                title: req_text(
                    node,
                    TITLE_COL,
                    "INSERT",
                    "opening an issue needs a `title`",
                )?,
                body: opt_text(node, BODY_COL).unwrap_or_default(),
                labels: list_col(node, LABELS_COL),
            }),
            Some(Namespace::Pulls) => Ok(GitHubEffect::OpenPull {
                slug,
                title: req_text(node, TITLE_COL, "INSERT", "opening a PR needs a `title`")?,
                body: opt_text(node, BODY_COL).unwrap_or_default(),
                head: req_text(
                    node,
                    HEAD_COL,
                    "INSERT",
                    "opening a PR needs a `head` branch",
                )?,
                base: req_text(
                    node,
                    BASE_COL,
                    "INSERT",
                    "opening a PR needs a `base` branch",
                )?,
            }),
            Some(Namespace::Releases) => Ok(GitHubEffect::CreateRelease {
                slug,
                tag_name: req_text(node, TAG_COL, "INSERT", "a release needs a `tag_name`")?,
                name: opt_text(node, TITLE_COL).unwrap_or_default(),
                body: opt_text(node, BODY_COL).unwrap_or_default(),
            }),
            Some(Namespace::Branches) => Ok(GitHubEffect::CreateBranch {
                slug,
                ref_name: req_text(node, REF_COL, "INSERT", "a branch needs a `ref` name")?,
                sha: req_text(node, SHA_COL, "INSERT", "a branch needs a target `sha`")?,
            }),
            other => Err(Self::cap_denied("INSERT", node, other)),
        }
    }

    fn decode_update(node: &EffectNode, path: &GitHubPath) -> Result<Self, GitHubError> {
        let slug = path.slug();
        let number = path.id.clone().ok_or_else(|| {
            Self::malformed("UPDATE", node, "UPDATE needs an object number in the path")
        })?;
        match path.effective_namespace() {
            Some(Namespace::Issues) => {
                let labels = if has_col(node, LABELS_COL) {
                    Some(list_col(node, LABELS_COL))
                } else {
                    None
                };
                let eff = GitHubEffect::PatchIssue {
                    slug,
                    number,
                    state: opt_text(node, STATE_COL),
                    title: opt_text(node, TITLE_COL),
                    body: opt_text(node, BODY_COL),
                    labels,
                };
                eff.ensure_patch_changes(node)
            }
            Some(Namespace::Pulls) => {
                let eff = GitHubEffect::PatchPull {
                    slug,
                    number,
                    state: opt_text(node, STATE_COL),
                    title: opt_text(node, TITLE_COL),
                    body: opt_text(node, BODY_COL),
                };
                eff.ensure_patch_changes(node)
            }
            other => Err(Self::cap_denied("UPDATE", node, other)),
        }
    }

    fn decode_remove(node: &EffectNode, path: &GitHubPath) -> Result<Self, GitHubError> {
        let slug = path.slug();
        let id = path.object_id().map(str::to_string).ok_or_else(|| {
            Self::malformed("REMOVE", node, "REMOVE needs an object id/name in the path")
        })?;
        match path.effective_namespace() {
            Some(Namespace::Comments) => Ok(GitHubEffect::DeleteComment { slug, id }),
            Some(Namespace::Releases) => Ok(GitHubEffect::DeleteRelease { slug, id }),
            Some(Namespace::Branches) => Ok(GitHubEffect::DeleteBranch { slug, ref_name: id }),
            other => Err(Self::cap_denied("REMOVE", node, other)),
        }
    }

    fn decode_call(proc: &str, node: &EffectNode, path: &GitHubPath) -> Result<Self, GitHubError> {
        let slug = path.slug();
        // The proc may be qualified (`github.merge`) or bare (`merge`); accept the suffix.
        let name = proc.rsplit('.').next().unwrap_or(proc);
        match name {
            crate::procs::PROC_MERGE => Ok(GitHubEffect::Merge {
                slug,
                number: path.id.clone().ok_or_else(|| {
                    Self::malformed("CALL", node, "merge needs the PR number in the path")
                })?,
                method: opt_text(node, METHOD_COL).unwrap_or_else(|| "merge".to_string()),
                sha: opt_text(node, SHA_COL),
            }),
            crate::procs::PROC_DISPATCH => Ok(GitHubEffect::Dispatch {
                slug,
                workflow: req_text(node, WORKFLOW_COL, "CALL", "dispatch needs a `workflow`")?,
                ref_name: req_text(node, REF_COL, "CALL", "dispatch needs a `ref`")?,
                inputs: opt_text(node, INPUTS_COL).unwrap_or_else(|| "{}".to_string()),
            }),
            crate::procs::PROC_REVIEW => Ok(GitHubEffect::Review {
                slug,
                number: path.id.clone().ok_or_else(|| {
                    Self::malformed("CALL", node, "review needs the PR number in the path")
                })?,
                event: req_text(node, EVENT_COL, "CALL", "review needs an `event`")?,
                body: opt_text(node, BODY_COL).unwrap_or_default(),
            }),
            _ => Err(GitHubError::UnknownProcedure(proc.to_string())),
        }
    }

    /// Reject a PATCH that changes nothing (no `SET` columns) rather than issuing an empty PATCH.
    fn ensure_patch_changes(self, node: &EffectNode) -> Result<Self, GitHubError> {
        let changes = match &self {
            GitHubEffect::PatchIssue {
                state,
                title,
                body,
                labels,
                ..
            } => state.is_some() || title.is_some() || body.is_some() || labels.is_some(),
            GitHubEffect::PatchPull {
                state, title, body, ..
            } => state.is_some() || title.is_some() || body.is_some(),
            _ => true,
        };
        if changes {
            Ok(self)
        } else {
            Err(Self::malformed(
                "UPDATE",
                node,
                "UPDATE changes nothing (set `state`/`title`/`body`/`labels`)",
            ))
        }
    }

    /// Whether this effect is irreversible (RFD §10/§6): the deletes, plus `merge`/`dispatch`.
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        matches!(
            self,
            GitHubEffect::DeleteComment { .. }
                | GitHubEffect::DeleteRelease { .. }
                | GitHubEffect::DeleteBranch { .. }
                | GitHubEffect::Merge { .. }
                | GitHubEffect::Dispatch { .. }
        )
    }

    /// Whether this effect is a non-idempotent POST the runtime must **never** auto-retry
    /// (at-least-once, RFD §6): a comment post, an open-issue/PR, a create, a review, a merge, a
    /// dispatch. The deletes (DELETE) and the PATCH edits are decided by their HTTP method's
    /// `is_retry_safe`; this is the explicit POST guard for the at-least-once contract.
    #[must_use]
    pub const fn is_at_least_once_post(&self) -> bool {
        matches!(
            self,
            GitHubEffect::PostComment { .. }
                | GitHubEffect::OpenIssue { .. }
                | GitHubEffect::OpenPull { .. }
                | GitHubEffect::CreateRelease { .. }
                | GitHubEffect::CreateBranch { .. }
                | GitHubEffect::Review { .. }
                | GitHubEffect::Merge { .. }
                | GitHubEffect::Dispatch { .. }
        )
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            GitHubEffect::OpenIssue { .. }
            | GitHubEffect::OpenPull { .. }
            | GitHubEffect::PostComment { .. }
            | GitHubEffect::CreateRelease { .. }
            | GitHubEffect::CreateBranch { .. } => "INSERT",
            GitHubEffect::PatchIssue { .. } | GitHubEffect::PatchPull { .. } => "UPDATE",
            GitHubEffect::DeleteComment { .. }
            | GitHubEffect::DeleteRelease { .. }
            | GitHubEffect::DeleteBranch { .. } => "REMOVE",
            GitHubEffect::Merge { .. }
            | GitHubEffect::Dispatch { .. }
            | GitHubEffect::Review { .. } => "CALL",
        }
    }

    fn malformed(verb: &'static str, node: &EffectNode, reason: &str) -> GitHubError {
        GitHubError::MalformedEffect {
            verb,
            path: node.target.path.as_str().to_string(),
            reason: reason.to_string(),
        }
    }

    fn cap_denied(verb: &'static str, node: &EffectNode, ns: Option<Namespace>) -> GitHubError {
        let _ = ns;
        GitHubError::CapabilityDenied {
            verb,
            path: node.target.path.as_str().to_string(),
        }
    }
}

/// Whether the node carries a (possibly empty) column by name.
fn has_col(node: &EffectNode, name: &str) -> bool {
    node.args.schema.columns.iter().any(|c| c.name == name)
}

/// Read a non-empty `Text` value from the node's first row by column name.
fn opt_text(node: &EffectNode, name: &str) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Read a required `Text` column, erroring with `reason` if absent/empty.
fn req_text(
    node: &EffectNode,
    name: &str,
    verb: &'static str,
    reason: &str,
) -> Result<String, GitHubError> {
    opt_text(node, name).ok_or_else(|| GitHubEffect::malformed(verb, node, reason))
}

/// Read a comma-separated `Text` column into a list of trimmed, non-empty items.
fn list_col(node: &EffectNode, name: &str) -> Vec<String> {
    opt_text(node, name)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
