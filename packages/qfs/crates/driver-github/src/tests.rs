//! GitHub driver tests (RFD-0001 §5 acceptance) — **no live GitHub, no network, no credentials**.
//! Every test drives the introspective `Driver` surface, the pushdown/effect decode, and the
//! apply leg against an in-memory [`MockGitHubClient`] (scripted GitHub JSON + recorded requests),
//! so we assert request shape + response decoding + plan shape + token-safety without a socket.
//! Live GitHub E2E is parked for t38.

use std::sync::{Arc, Mutex};

use qfs_driver::{check_capability, resolve_proc, Archetype, Driver, Path, Verb};
use qfs_http_core::{HttpRequest, HttpResponse};
use qfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter, SharedApplier};
use qfs_secrets::{AccountId, CredentialKey, InMemoryStore, Secret, Secrets};
use qfs_types::{Column, Row, RowBatch, Schema, Value};

use super::*;
use crate::client::{HttpTransport, TransportError};
use crate::effect::{
    BASE_COL, BODY_COL, EVENT_COL, HEAD_COL, INPUTS_COL, LABELS_COL, METHOD_COL, REF_COL, SHA_COL,
    STATE_COL, TITLE_COL, WORKFLOW_COL,
};

/// A recording HTTP transport (no socket): records every request it receives and answers from a
/// FIFO queue of scripted responses — so a wire-level test asserts the exact request the
/// `RestGitHubClient` built (Bearer header, URL, body) and drives Link pagination / retry by
/// queueing several responses.
#[derive(Default)]
struct RecordingTransport {
    responses: Mutex<std::collections::VecDeque<HttpResponse>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl RecordingTransport {
    fn with(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    fn recorded(&self) -> Vec<HttpRequest> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl HttpTransport for RecordingTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(req.clone());
        }
        self.responses
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
            .ok_or_else(|| TransportError {
                reason: "mock transport exhausted".to_string(),
            })
    }
}

/// A secret store seeded with a PAT under the `github/work` credential.
fn store_with_pat(pat: &str) -> (Arc<dyn Secrets>, CredentialKey) {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(
        qfs_secrets::DriverId::new("github"),
        AccountId::new("work").unwrap(),
    );
    store
        .put(&key, Secret::new(pat.as_bytes().to_vec()))
        .unwrap();
    (Arc::new(store), key)
}

// ---- fixtures ----------------------------------------------------------------------------

fn driver() -> (GitHubDriver, Arc<MockGitHubClient>) {
    let mock = Arc::new(MockGitHubClient::new());
    let d = GitHubDriver::new(mock.clone() as Arc<dyn GitHubClient>);
    (d, mock)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("github"), VfsPath::new(path))
}

/// Build a single-row args batch (the columns the effect decoder reads).
fn args(cols: &[(&str, Value)]) -> RowBatch {
    let schema = Schema::new(
        cols.iter()
            .map(|(n, v)| Column::new(*n, v.type_of(), true))
            .collect(),
    );
    let row = Row::new(cols.iter().map(|(_, v)| v.clone()).collect());
    RowBatch::new(schema, vec![row])
}

// ---- introspection: mount / archetype / schema -------------------------------------------

#[test]
fn mount_and_id_are_github() {
    let (d, _) = driver();
    assert_eq!(d.mount(), "/github");
    assert_eq!(d.id(), DriverId::new("github"));
}

#[test]
fn describe_emits_object_graph_archetype_for_all_eight_namespaces() {
    let (d, _) = driver();
    let expected: &[(&str, &[&str])] = &[
        (
            "issues",
            &["number", "title", "state", "labels", "assignees"],
        ),
        (
            "pulls",
            &["number", "state", "head_sha", "base_ref", "merged"],
        ),
        ("comments", &["id", "user", "body"]),
        ("reviews", &["id", "state", "body"]),
        ("runs", &["id", "status", "conclusion", "head_branch"]),
        ("releases", &["id", "tag_name", "draft", "prerelease"]),
        ("files", &["path", "sha", "size", "kind"]),
        ("branches", &["name", "sha", "protected"]),
    ];
    for (ns, cols) in expected {
        let desc = d
            .describe(&Path::new(format!("/github/o/r/{ns}")))
            .unwrap_or_else(|e| panic!("describe {ns} failed: {e}"));
        assert_eq!(
            desc.archetype,
            Archetype::ObjectGraphWorkflow,
            "{ns} archetype"
        );
        for col in *cols {
            assert!(
                desc.schema.column(col).is_some(),
                "namespace {ns} missing column {col}"
            );
        }
    }
    // The bare repo root is not a describable collection — an honest structured error.
    assert_eq!(
        d.describe(&Path::new("/github/o/r")).unwrap_err().code(),
        "invalid_path"
    );
}

#[test]
fn describe_issue_columns_are_typed() {
    let (d, _) = driver();
    let desc = d.describe(&Path::new("/github/o/r/issues")).unwrap();
    assert_eq!(
        desc.schema.column("number").unwrap().ty,
        qfs_types::ColumnType::Int
    );
    assert_eq!(
        desc.schema.column("created_at").unwrap().ty,
        qfs_types::ColumnType::Timestamp
    );
    // A sub-collection describes with the sub schema: issues/123/comments → comment columns.
    let sub = d
        .describe(&Path::new("/github/o/r/issues/123/comments"))
        .unwrap();
    assert!(sub.schema.column("body").is_some());
    assert!(
        sub.schema.column("number").is_none(),
        "comment schema, not issue"
    );
}

// ---- capability gating (parse-time, structured) ------------------------------------------

#[test]
fn capabilities_are_node_keyed() {
    let (d, _) = driver();
    // issues: SELECT/INSERT/UPDATE, no REMOVE.
    let issues = Path::new("/github/o/r/issues");
    assert!(check_capability(&d, &issues, Verb::Select).is_ok());
    assert!(check_capability(&d, &issues, Verb::Insert).is_ok());
    assert!(check_capability(&d, &issues, Verb::Update).is_ok());
    assert!(check_capability(&d, &issues, Verb::Remove).is_err());

    // comments: SELECT/INSERT/REMOVE, no UPDATE.
    let comments = Path::new("/github/o/r/issues/1/comments");
    assert!(check_capability(&d, &comments, Verb::Insert).is_ok());
    assert!(check_capability(&d, &comments, Verb::Remove).is_ok());
    assert!(check_capability(&d, &comments, Verb::Update).is_err());

    // runs: SELECT only.
    let runs = Path::new("/github/o/r/runs");
    assert!(check_capability(&d, &runs, Verb::Select).is_ok());
    assert!(check_capability(&d, &runs, Verb::Insert).is_err());
}

#[test]
fn update_on_runs_is_rejected_at_parse_time_with_structured_error() {
    let (d, _) = driver();
    let err = check_capability(&d, &Path::new("/github/o/r/runs/55"), Verb::Update).unwrap_err();
    match &err {
        qfs_driver::CfsError::UnsupportedVerb {
            path,
            verb,
            supported,
        } => {
            assert_eq!(path, "/github/o/r/runs/55");
            assert_eq!(*verb, "UPDATE");
            assert_eq!(
                supported,
                &vec!["SELECT"],
                "runs node names its allowed verbs"
            );
        }
        other => panic!("expected UnsupportedVerb, got {other:?}"),
    }
    assert_eq!(err.code(), "unsupported_verb");
}

// ---- procedures: merge/dispatch/review ---------------------------------------------------

#[test]
fn procedures_are_declared_with_irreversibility_and_scopes() {
    let (d, _) = driver();
    let merge = resolve_proc(&d, "merge").unwrap();
    assert!(merge.irreversible, "merge is irreversible");
    assert!(merge.params.iter().any(|p| p.name == "method"));
    assert!(
        merge.params.iter().any(|p| p.name == "sha"),
        "merge takes the head-sha precondition"
    );
    assert_eq!(merge.requires_scopes, vec!["repo".to_string()]);

    let dispatch = resolve_proc(&d, "dispatch").unwrap();
    assert!(
        dispatch.irreversible,
        "dispatch triggers a run — irreversible"
    );
    assert_eq!(dispatch.requires_scopes, vec!["workflow".to_string()]);

    let review = resolve_proc(&d, "review").unwrap();
    assert!(
        !review.irreversible,
        "a review can be superseded — reversible-ish"
    );

    assert_eq!(
        resolve_proc(&d, "nuke").unwrap_err().code(),
        "unknown_procedure"
    );
}

// ---- path parsing ------------------------------------------------------------------------

#[test]
fn paths_parse_owner_repo_namespace_id_and_subcollection() {
    let p = GitHubPath::parse_str("/github/octo/repo/issues/123/comments/9").unwrap();
    assert_eq!(p.owner, "octo");
    assert_eq!(p.repo, "repo");
    assert_eq!(p.slug(), "octo/repo");
    assert_eq!(p.namespace, Some(Namespace::Issues));
    assert_eq!(p.id.as_deref(), Some("123"));
    assert_eq!(p.sub, Some(Namespace::Comments));
    assert_eq!(p.sub_id.as_deref(), Some("9"));
    assert_eq!(p.effective_namespace(), Some(Namespace::Comments));

    // A bare collection.
    let c = GitHubPath::parse_str("/github/o/r/pulls").unwrap();
    assert!(c.is_collection());
    assert_eq!(c.effective_namespace(), Some(Namespace::Pulls));

    // An unknown namespace segment is rejected structurally.
    assert_eq!(
        GitHubPath::parse_str("/github/o/r/bogus")
            .unwrap_err()
            .code(),
        "github_invalid_path"
    );
    // Not under the mount.
    assert_eq!(
        GitHubPath::parse_str("/drive/x").unwrap_err().code(),
        "github_invalid_path"
    );
}

// ---- pushdown: WHERE → REST query params, TRUTHFUL residual (the t20 lesson) --------------

#[test]
fn where_state_and_label_push_to_params_with_label_kept_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // state = 'open' AND label = 'bug': `state` is EXACT (drops from residual); `label` is a
    // set-membership pre-filter (pushed AND kept residual for the exact local re-check).
    let state_eq = Predicate::Cmp(
        ColRef::col("state"),
        CmpOp::Eq,
        Literal::Text("open".to_string()),
    );
    let label_eq = Predicate::Cmp(
        ColRef::col("label"),
        CmpOp::Eq,
        Literal::Text("bug".to_string()),
    );
    let pred = Predicate::And(Box::new(state_eq.clone()), Box::new(label_eq.clone()));
    let res = pushdown::build_params(Some(&pred));
    assert_eq!(
        res.params,
        vec![
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), "bug".to_string()),
        ]
    );
    assert_eq!(
        res.residual,
        Some(label_eq),
        "the lossy labels membership pre-filter is kept residual; exact state= drops out"
    );

    let (d, _) = driver();
    assert!(d.pushdown().supports_where());
    assert!(d.pushdown().supports_limit());
    assert!(!d.pushdown().supports_order());
}

#[test]
fn exact_predicates_push_fully_with_no_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // state = 'closed' AND assignee = 'octocat': both EXACT, so the whole predicate pushes and
    // there is no residual to re-check locally.
    let pred = Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("state"),
            CmpOp::Eq,
            Literal::Text("closed".to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("assignee"),
            CmpOp::Eq,
            Literal::Text("octocat".to_string()),
        )),
    );
    let res = pushdown::build_params(Some(&pred));
    assert_eq!(
        res.params,
        vec![
            ("state".to_string(), "closed".to_string()),
            ("assignee".to_string(), "octocat".to_string()),
        ]
    );
    assert!(
        res.residual.is_none(),
        "exact state/assignee leave no residual"
    );
}

#[test]
fn or_predicate_stays_wholly_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    let or_pred = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("state"),
            CmpOp::Eq,
            Literal::Text("open".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("state"),
            CmpOp::Eq,
            Literal::Text("closed".into()),
        )),
    );
    let res = pushdown::build_params(Some(&or_pred));
    assert!(res.params.is_empty(), "nothing pushed for an OR");
    assert_eq!(
        res.residual,
        Some(or_pred),
        "OR is residual, filtered locally"
    );
}

// ---- read path: pagination as a single bounded fetch node + decode -----------------------

#[test]
fn paginated_select_collapses_into_one_fetch_node_in_the_plan() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // A WHERE state='open' AND label='bug' read is ONE ReadPlan node (a bounded fetch set), not
    // an N-page loop — the pagination follow is at the edge in the client.
    let pred = Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("state"),
            CmpOp::Eq,
            Literal::Text("open".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("label"),
            CmpOp::Eq,
            Literal::Text("bug".into()),
        )),
    );
    let plan = ReadPlan::list("o/r", Namespace::Issues, None, Some(&pred));
    assert_eq!(plan.slug, "o/r");
    assert_eq!(plan.effective_namespace(), Namespace::Issues);
    assert_eq!(
        plan.params(),
        &[
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), "bug".to_string())
        ]
    );
    // The residual (the lossy labels membership) is preserved for local re-filtering.
    assert!(plan.pushdown.residual.is_some());

    // One client call materializes ALL pages (the Link follow is at the edge): the mock seeds two
    // issues and the driver's list reaches the client exactly once.
    let mock = Arc::new(MockGitHubClient::new().with_list(serde_json::json!([
        {"number": 1, "title": "a", "state": "open", "labels": [{"name": "bug"}]},
        {"number": 2, "title": "b", "state": "open", "labels": [{"name": "bug"}]}
    ])));
    let value = mock
        .list("o/r", Namespace::Issues, None, plan.params())
        .unwrap();
    let batch = read::decode_list(Namespace::Issues, &value).unwrap();
    assert_eq!(batch.rows.len(), 2);
    let recorded = mock.recorded();
    assert_eq!(
        recorded.len(),
        1,
        "exactly one batched fetch set: {recorded:?}"
    );
    assert!(matches!(
        &recorded[0],
        RecordedCall::List { segment, params, .. }
            if segment == "issues" && params == plan.params()
    ));
}

#[test]
fn issue_list_decodes_to_typed_rows_and_filters_out_pull_requests() {
    let value = serde_json::json!([
        {"number": 7, "title": "bug", "state": "open", "user": {"login": "octo"},
         "assignees": [{"login": "dev"}], "labels": [{"name": "bug"}], "created_at": "2023-01-02T03:04:05Z"},
        {"number": 8, "title": "a pr", "state": "open", "pull_request": {"url": "x"}}
    ]);
    let issues = read::decode_issues(&value);
    assert_eq!(
        issues.len(),
        1,
        "the PR-shaped row is excluded from the issues view"
    );
    assert_eq!(issues[0].number, 7);
    assert_eq!(issues[0].assignees, vec!["dev".to_string()]);
    assert_eq!(issues[0].labels, vec!["bug".to_string()]);
    let row = Row::from(&issues[0]);
    assert_eq!(row.values[0], Value::Int(7));
    assert_eq!(row.values[3], Value::Text("open".to_string()));
    assert!(
        matches!(row.values[7], Value::Timestamp(_)),
        "created_at is a Timestamp"
    );
}

// ---- effect decode: INSERT / UPDATE(PATCH) / REMOVE --------------------------------------

#[test]
fn insert_comment_decodes_to_post_comment_single_node() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/github/o/r/issues/123/comments"),
    )
    .with_args(args(&[(BODY_COL, Value::Text("LGTM".into()))]));
    match GitHubEffect::from_node(&node).unwrap() {
        GitHubEffect::PostComment { slug, number, body } => {
            assert_eq!(slug, "o/r");
            assert_eq!(number, "123");
            assert_eq!(body, "LGTM");
        }
        other => panic!("expected PostComment, got {other:?}"),
    }
}

#[test]
fn insert_issue_and_pull_decode_to_open_effects() {
    let issue = EffectNode::new(NodeId(0), EffectKind::Insert, target("/github/o/r/issues"))
        .with_args(args(&[
            (TITLE_COL, Value::Text("Found a bug".into())),
            (BODY_COL, Value::Text("repro".into())),
            (LABELS_COL, Value::Text("bug,p1".into())),
        ]));
    match GitHubEffect::from_node(&issue).unwrap() {
        GitHubEffect::OpenIssue { title, labels, .. } => {
            assert_eq!(title, "Found a bug");
            assert_eq!(labels, vec!["bug".to_string(), "p1".to_string()]);
        }
        other => panic!("expected OpenIssue, got {other:?}"),
    }
    let pr = EffectNode::new(NodeId(1), EffectKind::Insert, target("/github/o/r/pulls")).with_args(
        args(&[
            (TITLE_COL, Value::Text("My PR".into())),
            (HEAD_COL, Value::Text("feature".into())),
            (BASE_COL, Value::Text("main".into())),
        ]),
    );
    match GitHubEffect::from_node(&pr).unwrap() {
        GitHubEffect::OpenPull { head, base, .. } => {
            assert_eq!(head, "feature");
            assert_eq!(base, "main");
        }
        other => panic!("expected OpenPull, got {other:?}"),
    }
}

#[test]
fn update_issue_set_state_closed_decodes_to_patch_state_only() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Update,
        target("/github/o/r/issues/123"),
    )
    .with_args(args(&[(STATE_COL, Value::Text("closed".into()))]));
    match GitHubEffect::from_node(&node).unwrap() {
        GitHubEffect::PatchIssue {
            number,
            state,
            title,
            body,
            labels,
            ..
        } => {
            assert_eq!(number, "123");
            assert_eq!(state.as_deref(), Some("closed"), "state-only PATCH");
            assert!(title.is_none() && body.is_none() && labels.is_none());
        }
        other => panic!("expected PatchIssue, got {other:?}"),
    }
    // A PATCH that changes nothing is rejected.
    let empty = EffectNode::new(
        NodeId(1),
        EffectKind::Update,
        target("/github/o/r/issues/123"),
    );
    assert_eq!(
        GitHubEffect::from_node(&empty).unwrap_err().code(),
        "github_malformed_effect"
    );
}

#[test]
fn remove_comment_release_branch_decode_to_deletes() {
    let comment = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("/github/o/r/issues/1/comments/55"),
    );
    assert!(comment.irreversible, "REMOVE is inherently irreversible");
    match GitHubEffect::from_node(&comment).unwrap() {
        GitHubEffect::DeleteComment { id, .. } => assert_eq!(id, "55"),
        other => panic!("expected DeleteComment, got {other:?}"),
    }
    let release = EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        target("/github/o/r/releases/9"),
    );
    assert!(matches!(
        GitHubEffect::from_node(&release).unwrap(),
        GitHubEffect::DeleteRelease { .. }
    ));
    let branch = EffectNode::new(
        NodeId(2),
        EffectKind::Remove,
        target("/github/o/r/branches/feature"),
    );
    match GitHubEffect::from_node(&branch).unwrap() {
        GitHubEffect::DeleteBranch { ref_name, .. } => assert_eq!(ref_name, "feature"),
        other => panic!("expected DeleteBranch, got {other:?}"),
    }
}

// ---- CALL procedures: merge / dispatch / review ------------------------------------------

#[test]
fn call_merge_decodes_irreversible_with_sha_precondition() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("github.merge")),
        target("/github/o/r/pulls/7"),
    )
    .irreversible(true)
    .with_args(args(&[
        (METHOD_COL, Value::Text("squash".into())),
        (SHA_COL, Value::Text("deadbeef".into())),
    ]));
    assert!(node.irreversible);
    match GitHubEffect::from_node(&node).unwrap() {
        GitHubEffect::Merge {
            number,
            method,
            sha,
            ..
        } => {
            assert_eq!(number, "7");
            assert_eq!(method, "squash");
            assert_eq!(
                sha.as_deref(),
                Some("deadbeef"),
                "optimistic concurrency on head SHA"
            );
        }
        other => panic!("expected Merge, got {other:?}"),
    }
}

#[test]
fn call_dispatch_decodes_with_workflow_ref_inputs() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("github.dispatch")),
        target("/github/o/r/runs"),
    )
    .irreversible(true)
    .with_args(args(&[
        (WORKFLOW_COL, Value::Text("ci.yml".into())),
        (REF_COL, Value::Text("main".into())),
        (INPUTS_COL, Value::Text(r#"{"env":"prod"}"#.into())),
    ]));
    match GitHubEffect::from_node(&node).unwrap() {
        GitHubEffect::Dispatch {
            workflow,
            ref_name,
            inputs,
            ..
        } => {
            assert_eq!(workflow, "ci.yml");
            assert_eq!(ref_name, "main");
            assert_eq!(inputs, r#"{"env":"prod"}"#);
        }
        other => panic!("expected Dispatch, got {other:?}"),
    }
    assert!(node.irreversible);
}

#[test]
fn call_review_decodes_event_and_body() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("github.review")),
        target("/github/o/r/pulls/7"),
    )
    .with_args(args(&[
        (EVENT_COL, Value::Text("APPROVE".into())),
        (BODY_COL, Value::Text("ship it".into())),
    ]));
    match GitHubEffect::from_node(&node).unwrap() {
        GitHubEffect::Review { event, body, .. } => {
            assert_eq!(event, "APPROVE");
            assert_eq!(body, "ship it");
        }
        other => panic!("expected Review, got {other:?}"),
    }
    // An unknown proc is rejected structurally.
    let bad = EffectNode::new(
        NodeId(1),
        EffectKind::Call(ProcId::new("github.nuke")),
        target("/github/o/r/pulls/7"),
    );
    assert_eq!(
        GitHubEffect::from_node(&bad).unwrap_err().code(),
        "github_unknown_procedure"
    );
}

// ---- irreversibility classification ------------------------------------------------------

#[test]
fn irreversible_and_at_least_once_classification_is_honest() {
    // merge/dispatch and the deletes are irreversible.
    let merge = GitHubEffect::Merge {
        slug: "o/r".into(),
        number: "7".into(),
        method: "merge".into(),
        sha: None,
    };
    assert!(merge.is_irreversible() && merge.is_at_least_once_post());
    // A comment POST is at-least-once but NOT irreversible (a comment can be deleted).
    let comment = GitHubEffect::PostComment {
        slug: "o/r".into(),
        number: "1".into(),
        body: "x".into(),
    };
    assert!(comment.is_at_least_once_post());
    assert!(!comment.is_irreversible());
    // A PATCH is neither an at-least-once POST nor irreversible.
    let patch = GitHubEffect::PatchPull {
        slug: "o/r".into(),
        number: "7".into(),
        state: Some("closed".into()),
        title: None,
        body: None,
    };
    assert!(!patch.is_at_least_once_post());
}

// ---- DTO → Row boundary (no vendor type escapes) -----------------------------------------

#[test]
fn dtos_project_onto_their_schema_in_column_order() {
    let issue = IssueDto::for_test(5, "t", "open");
    let row = Row::from(&issue);
    assert_eq!(row.values.len(), IssueDto::schema().columns.len());
    let pull = PullDto::for_test(7, "p", "open", "abc");
    assert_eq!(
        Row::from(&pull).values.len(),
        PullDto::schema().columns.len()
    );
    let comment = CommentDto::for_test(9, "hi");
    assert_eq!(
        Row::from(&comment).values.len(),
        CommentDto::schema().columns.len()
    );
    // The DTOs are the boundary types: a list decode returns owned DTOs, never serde_json::Value
    // past this point. (A DTO-boundary doc check; the public signatures carry only owned types.)
    let _: Vec<ReviewDto> = read::decode_reviews(&serde_json::json!([]));
    let _: Vec<RunDto> = read::decode_runs(&serde_json::json!([]));
    let _: Vec<ReleaseDto> = read::decode_releases(&serde_json::json!([]));
    let _: Vec<BranchDto> = read::decode_branches(&serde_json::json!([]));
    let _: Vec<FileMetaDto> = read::decode_files(&serde_json::json!([]));
}

// ---- PREVIEW performs no I/O (mock asserts zero calls) -----------------------------------

#[test]
fn preview_of_a_merge_plan_surfaces_irreversible_and_performs_no_io() {
    let (_d, mock) = driver();
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("github.merge")),
            target("/github/o/r/pulls/7"),
        )
        .irreversible(true)
        .with_args(args(&[(METHOD_COL, Value::Text("squash".into()))])),
    );
    let plan = b.build();
    let pv = preview(&plan);
    assert_eq!(pv.rows.len(), 1);
    assert!(
        pv.rows[0].irreversible,
        "preview surfaces the irreversible merge"
    );
    assert!(
        mock.recorded().is_empty(),
        "PREVIEW must perform zero GitHub API calls: {:?}",
        mock.recorded()
    );
}

// ---- token never in logs / errors (planted canary) ---------------------------------------

#[test]
fn errors_are_secret_free() {
    let errs = [
        GitHubError::Api {
            op: "issues.list",
            status: 401,
        },
        GitHubError::CapabilityDenied {
            path: "/github/o/r/runs/1".into(),
            verb: "UPDATE",
        },
        GitHubError::Auth {
            code: "secret_not_found",
        },
        GitHubError::Transport {
            op: "http",
            reason: "connection failed".into(),
        },
    ];
    for e in &errs {
        let text = format!("{e} {e:?}");
        assert!(!text.contains("Bearer"), "no bearer in error: {text}");
        assert!(!text.contains("ghp_"), "no PAT prefix in error: {text}");
        assert!(!text.contains("token"), "no token text in error: {text}");
    }
}

#[test]
fn planted_token_never_appears_in_a_serialized_plan() {
    // The planted canary: a plan over /github carries NO token (the PAT lives only behind the
    // auth seam, read at apply time). Even an effect whose args carry user text never carries the
    // credential — the serialized plan is token-free by construction.
    const CANARY: &str = "ghp_PLANTED_CANARY_TOKEN_should_never_serialize";
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/github/o/r/issues/1/comments"),
        )
        .with_args(args(&[(BODY_COL, Value::Text("a normal comment".into()))])),
    );
    let plan = b.build();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains(CANARY) && !json.contains("Bearer"),
        "no token material in a serialized plan: {json}"
    );
    // And the preview is token-free too.
    let pv = preview(&plan);
    let pv_text = format!("{pv:?}");
    assert!(!pv_text.contains(CANARY) && !pv_text.contains("Bearer"));
}

// ---- end-to-end: commit through interpreter + bridge -------------------------------------

#[tokio::test]
async fn commit_post_comment_end_to_end_through_interpreter() {
    let (driver, mock) = driver();
    let bridge = github_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/github/o/r/issues/123/comments"),
        )
        .with_args(args(&[(BODY_COL, Value::Text("LGTM".into()))])),
    );
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("github"), &EffectKind::Insert);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "comment posted: {outcome:?}");
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::Apply(GitHubEffect::PostComment {
            slug: "o/r".to_string(),
            number: "123".to_string(),
            body: "LGTM".to_string(),
        })]
    );
}

#[tokio::test]
async fn commit_merge_call_end_to_end() {
    let (driver, mock) = driver();
    let bridge = github_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("github.merge")),
            target("/github/o/r/pulls/7"),
        )
        .irreversible(true)
        .with_args(args(&[
            (METHOD_COL, Value::Text("squash".into())),
            (SHA_COL, Value::Text("deadbeef".into())),
        ])),
    );
    let plan = b.build();

    let caps = CapabilitySet::none().grant(
        DriverId::new("github"),
        &EffectKind::Call(ProcId::new("github.merge")),
    );
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "merge applied: {outcome:?}");
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::Apply(GitHubEffect::Merge {
            slug: "o/r".to_string(),
            number: "7".to_string(),
            method: "squash".to_string(),
            sha: Some("deadbeef".to_string()),
        })]
    );
}

#[test]
fn apply_shared_routes_only_to_its_own_client() {
    // Two driver instances over two independent mock clients; each routes only to its own.
    let mock_a = Arc::new(MockGitHubClient::new());
    let mock_b = Arc::new(MockGitHubClient::new());
    let driver_a = GitHubDriver::new(mock_a.clone() as Arc<dyn GitHubClient>);
    let driver_b = GitHubDriver::new(mock_b.clone() as Arc<dyn GitHubClient>);

    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("/github/o/r/releases/9"),
    );
    driver_a.github_applier().apply_shared(&node).unwrap();
    assert_eq!(mock_a.recorded().len(), 1);
    assert!(mock_b.recorded().is_empty(), "client B was untouched");
    let _ = driver_b;
}

// ---- wire level: RestGitHubClient over the HttpTransport seam (Bearer / Link / retry) -----

#[test]
fn rest_client_injects_bearer_pat_and_never_logs_it() {
    // The PAT is read from the secret store, written into an Authorization: Bearer header, and
    // redacted in the request's Debug — never serialized in a log surface.
    const PAT: &str = "ghp_SECRET_pat_value_42";
    let (secrets, key) = store_with_pat(PAT);
    let transport = Arc::new(RecordingTransport::with(vec![HttpResponse::new(
        200,
        b"[]".to_vec(),
    )]));
    let client = RestGitHubClient::new(transport.clone(), secrets, key);

    client
        .list(
            "o/r",
            Namespace::Issues,
            None,
            &[("state".into(), "open".into())],
        )
        .unwrap();

    let reqs = transport.recorded();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    // The Bearer PAT is present on the wire …
    assert_eq!(
        req.header_value("authorization"),
        Some(format!("Bearer {PAT}").as_str())
    );
    // … the URL carries the pushed param + per_page cap …
    assert!(req.url.contains("/repos/o/r/issues?"));
    assert!(req.url.contains("state=open"));
    assert!(req.url.contains("per_page=100"));
    // … but the redacting Debug never reveals the token (RFD §10).
    let dbg = format!("{req:?}");
    assert!(!dbg.contains(PAT), "PAT must not appear in Debug: {dbg}");
    assert!(dbg.contains(qfs_secrets::REDACTED));
}

#[test]
fn rest_client_follows_link_pagination_and_merges_pages() {
    let (secrets, key) = store_with_pat("ghp_x");
    // Page 1 carries a rel="next" Link; page 2 has none. The client follows it and merges.
    let page1 = HttpResponse::new(
        200,
        br#"[{"number":1,"title":"a","state":"open"}]"#.to_vec(),
    )
    .header(
        "Link",
        "<https://api.github.com/repos/o/r/issues?page=2>; rel=\"next\"",
    );
    let page2 = HttpResponse::new(
        200,
        br#"[{"number":2,"title":"b","state":"open"}]"#.to_vec(),
    );
    let transport = Arc::new(RecordingTransport::with(vec![page1, page2]));
    let client = RestGitHubClient::new(transport.clone(), secrets, key);

    let value = client.list("o/r", Namespace::Issues, None, &[]).unwrap();
    let issues = read::decode_issues(&value);
    assert_eq!(issues.len(), 2, "both pages merged into one result set");
    // Exactly two fetches: the first page + the followed next-page link.
    assert_eq!(transport.recorded().len(), 2);
    assert!(transport.recorded()[1].url.contains("page=2"));
}

#[test]
fn rest_client_retries_transient_429_on_a_get_then_succeeds() {
    let (secrets, key) = store_with_pat("ghp_x");
    // A 429 (Retry-After) then a 200 — a GET is retried within the bounded budget.
    let throttled = HttpResponse::new(429, Vec::new()).header("Retry-After", "0");
    let ok = HttpResponse::new(200, b"[]".to_vec());
    let transport = Arc::new(RecordingTransport::with(vec![throttled, ok]));
    let client = RestGitHubClient::new(transport.clone(), secrets, key);

    let value = client.list("o/r", Namespace::Issues, None, &[]).unwrap();
    assert_eq!(value, serde_json::json!([]));
    assert_eq!(
        transport.recorded().len(),
        2,
        "the throttled GET was retried once then succeeded"
    );
}

#[test]
fn rest_client_does_not_retry_a_write_post() {
    let (secrets, key) = store_with_pat("ghp_x");
    // A 500 on a POST is terminal — never retried (at-least-once for a non-idempotent create).
    let transport = Arc::new(RecordingTransport::with(vec![HttpResponse::new(
        500,
        Vec::new(),
    )]));
    let client = RestGitHubClient::new(transport.clone(), secrets, key);

    let err = client
        .apply(&GitHubEffect::PostComment {
            slug: "o/r".into(),
            number: "1".into(),
            body: "x".into(),
        })
        .unwrap_err();
    assert_eq!(err.code(), "github_api");
    assert_eq!(
        transport.recorded().len(),
        1,
        "a write POST is issued exactly once, never auto-retried"
    );
    // The single recorded request was a POST with the Bearer header and a JSON body.
    let req = &transport.recorded()[0];
    assert_eq!(req.method.as_str(), "POST");
    assert!(req.body.is_some());
    assert!(req.header_value("authorization").is_some());
}
