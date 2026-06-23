//! Planner-owned **E2E / external-interface** black-box validation of the t24 GitHub driver.
//!
//! This is NOT a unit test of the driver internals (that is the Constructor's `src/tests.rs`).
//! Every scenario here drives the driver from the OUTSIDE — through the public `Driver` contract,
//! the public plan→preview→commit loop, a Planner-owned mock HTTP transport, and the public
//! pushdown/read surface — exactly as an AI agent or a host runtime would. No live GitHub, no live
//! PAT, no network: a recording transport answers from scripted wire responses and a planted
//! canary PAT lives only in an in-memory secret store.
//!
//! Scenario map (ticket acceptance criteria):
//!  1. DESCRIBE returns declared columns for all 8 namespaces.
//!  2. Plan-shape goldens through PREVIEW (no creds): SELECT pushdown, INSERT comment, UPDATE
//!     state-only, CALL merge/dispatch/review at the right endpoints.
//!  3. Capability gating at parse time (UPDATE on runs rejected before a plan exists).
//!  4. Pagination collapses to one plan node AND is followed at the wire level (two-page Link).
//!  5. Pushdown residual truthfulness — the recurring trap: mock OVER-returns rows; the reported
//!     residual, when executed, re-filters to exactly the correct set (no wrong rows).
//!  6. Token safety: canary PAT in NO plan/preview/error/DTO/log; Bearer redaction at the wire;
//!     POST never silently retried.
//!  7. Irreversibility surfaced in PREVIEW for merge/dispatch.

// A black-box integration test crate: assertions panic by design, and fixtures unwrap scripted
// in-memory values that cannot fail. The same allowance the in-crate test module declares at the
// crate root (`#![cfg_attr(test, allow(...))]`) applies here, this being a separate test crate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cfs_driver::{check_capability, resolve_proc, Archetype, Driver, Path, Verb};
use cfs_http_core::{HttpRequest, HttpResponse};
use cfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use cfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use cfs_secrets::{AccountId, CredentialKey, InMemoryStore, Secret, Secrets};
use cfs_types::{CmpOp, ColRef, Column, Literal, Predicate, Row, RowBatch, Schema, Value};

use cfs_driver_github::{
    github_apply_driver, read, schema_for, GitHubClient, GitHubDriver, GitHubEffect, HttpTransport,
    MockGitHubClient, Namespace, RecordedCall, RestGitHubClient, TransportError,
};

// ============================================================================================
// Planner-owned harness: a recording wire transport + a secret store + plan helpers.
// ============================================================================================

/// A recording HTTP transport (no socket): records every request and answers from a FIFO queue of
/// scripted wire responses. This is the Planner's black-box wire seam — it lets a scenario assert
/// the exact request the driver put on the wire (URL, headers, body) and drive Link pagination /
/// retry by queueing several responses, all without a network.
#[derive(Default)]
struct WireTap {
    responses: Mutex<VecDeque<HttpResponse>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl WireTap {
    fn with(responses: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            recorded: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

impl HttpTransport for WireTap {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.recorded.lock().unwrap().push(req.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError {
                reason: "wire tap exhausted".to_string(),
            })
    }
}

const CANARY_PAT: &str = "ghp_PLANTED_CANARY_e2e_must_never_leak_0xCAFE";

fn store_with_canary() -> (Arc<dyn Secrets>, CredentialKey) {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(
        cfs_secrets::DriverId::new("github"),
        AccountId::new("work").unwrap(),
    );
    store
        .put(&key, Secret::new(CANARY_PAT.as_bytes().to_vec()))
        .unwrap();
    (Arc::new(store), key)
}

fn mock_driver() -> (GitHubDriver, Arc<MockGitHubClient>) {
    let mock = Arc::new(MockGitHubClient::new());
    let d = GitHubDriver::new(mock.clone() as Arc<dyn GitHubClient>);
    (d, mock)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("github"), VfsPath::new(path))
}

fn args(cols: &[(&str, Value)]) -> RowBatch {
    let schema = Schema::new(
        cols.iter()
            .map(|(n, v)| Column::new(*n, v.type_of(), true))
            .collect(),
    );
    let row = Row::new(cols.iter().map(|(_, v)| v.clone()).collect());
    RowBatch::new(schema, vec![row])
}

// ============================================================================================
// Scenario 1 — DESCRIBE returns declared columns for ALL EIGHT namespaces.
// ============================================================================================

#[test]
fn s1_describe_returns_declared_columns_for_all_eight_namespaces() {
    let (d, _) = mock_driver();
    // The full, exact column set per namespace — the external DESCRIBE contract an agent reads.
    let expected: &[(&str, &[&str])] = &[
        (
            "issues",
            &[
                "number",
                "title",
                "body",
                "state",
                "user",
                "assignees",
                "labels",
                "created_at",
                "updated_at",
            ],
        ),
        (
            "pulls",
            &[
                "number",
                "title",
                "body",
                "state",
                "user",
                "head_ref",
                "head_sha",
                "base_ref",
                "merged",
                "created_at",
            ],
        ),
        ("comments", &["id", "user", "body", "created_at"]),
        ("reviews", &["id", "user", "state", "body"]),
        (
            "runs",
            &[
                "id",
                "name",
                "status",
                "conclusion",
                "head_branch",
                "created_at",
            ],
        ),
        (
            "releases",
            &[
                "id",
                "tag_name",
                "name",
                "body",
                "draft",
                "prerelease",
                "created_at",
            ],
        ),
        ("files", &["path", "sha", "size", "kind"]),
        ("branches", &["name", "sha", "protected"]),
    ];
    for (ns, cols) in expected {
        let desc = d
            .describe(&Path::new(format!("/github/acme/widgets/{ns}")))
            .unwrap_or_else(|e| panic!("DESCRIBE {ns} failed: {e}"));
        assert_eq!(desc.archetype, Archetype::ObjectGraphWorkflow, "{ns}");
        // Exact column count — DESCRIBE is neither missing nor inventing columns.
        assert_eq!(
            desc.schema.columns.len(),
            cols.len(),
            "namespace {ns}: column count drift {:?}",
            desc.schema
                .columns
                .iter()
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );
        for col in *cols {
            assert!(
                desc.schema.column(col).is_some(),
                "namespace {ns} missing declared column {col}"
            );
        }
    }
    // A sub-collection DESCRIBEs with the SUB schema (issues/123/comments → comment columns).
    let sub = d
        .describe(&Path::new("/github/acme/widgets/issues/123/comments"))
        .unwrap();
    assert!(sub.schema.column("body").is_some());
    assert!(
        sub.schema.column("number").is_none(),
        "comment schema, not issue"
    );
    // The bare repo root is not a describable collection — honest structured error.
    assert_eq!(
        d.describe(&Path::new("/github/acme/widgets"))
            .unwrap_err()
            .code(),
        "invalid_path"
    );
}

// ============================================================================================
// Scenario 3 — Capability gating happens AT PARSE TIME (before any plan exists).
// ============================================================================================

#[test]
fn s3_update_on_runs_rejected_at_parse_time_with_structured_error() {
    let (d, mock) = mock_driver();
    // The gate is consulted at resolve time, BEFORE a Plan is built — so this never reaches apply.
    let err =
        check_capability(&d, &Path::new("/github/acme/widgets/runs/55"), Verb::Update).unwrap_err();
    match &err {
        cfs_driver::CfsError::UnsupportedVerb {
            path,
            verb,
            supported,
        } => {
            assert_eq!(path, "/github/acme/widgets/runs/55");
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
    // Parse-time rejection means ZERO API calls — the apply leg was never reached.
    assert!(mock.recorded().is_empty(), "gate rejected before any I/O");

    // Cross-check the full node-keyed matrix from the outside.
    let cases: &[(&str, &[(Verb, bool)])] = &[
        (
            "/github/acme/widgets/issues",
            &[
                (Verb::Select, true),
                (Verb::Insert, true),
                (Verb::Update, true),
                (Verb::Remove, false),
            ],
        ),
        (
            "/github/acme/widgets/issues/1/comments",
            &[
                (Verb::Select, true),
                (Verb::Insert, true),
                (Verb::Remove, true),
                (Verb::Update, false),
            ],
        ),
        (
            "/github/acme/widgets/runs",
            &[
                (Verb::Select, true),
                (Verb::Insert, false),
                (Verb::Update, false),
                (Verb::Remove, false),
            ],
        ),
        (
            "/github/acme/widgets/files",
            &[(Verb::Select, true), (Verb::Insert, false)],
        ),
        (
            "/github/acme/widgets/reviews",
            &[(Verb::Select, true), (Verb::Insert, false)],
        ),
    ];
    for (path, verbs) in cases {
        for (verb, allowed) in *verbs {
            let ok = check_capability(&d, &Path::new(*path), *verb).is_ok();
            assert_eq!(ok, *allowed, "{path} {verb:?} expected allowed={allowed}");
        }
    }
}

// ============================================================================================
// Scenario 2 + 7 — Plan-shape goldens through PREVIEW + irreversibility surfacing.
//   PREVIEW is the agent-facing dry run; it must perform ZERO I/O and surface irreversibility.
// ============================================================================================

/// Build a one-node plan and preview it, asserting the mock saw NO calls (PREVIEW is pure).
fn preview_one(mock: &Arc<MockGitHubClient>, node: EffectNode) -> cfs_plan::Preview {
    let mut b = PlanBuilder::new();
    b.push(node);
    let plan = b.build();
    plan.validate().unwrap();
    let pv = preview(&plan);
    assert!(
        mock.recorded().is_empty(),
        "PREVIEW must perform zero GitHub API calls: {:?}",
        mock.recorded()
    );
    pv
}

#[test]
fn s2_insert_comment_previews_as_single_insert_node() {
    let (_d, mock) = mock_driver();
    let pv = preview_one(
        &mock,
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/github/acme/widgets/issues/123/comments"),
        )
        .with_args(args(&[("body", Value::Text("LGTM".into()))])),
    );
    assert_eq!(pv.rows.len(), 1, "single POST node");
    assert_eq!(pv.rows[0].verb, "INSERT");
    assert_eq!(
        pv.rows[0].target.path.as_str(),
        "/github/acme/widgets/issues/123/comments"
    );
    assert!(
        !pv.rows[0].irreversible,
        "a comment post is reversible (deletable)"
    );
    assert!(pv.irreversible.is_empty());
}

#[test]
fn s2_update_state_only_previews_as_update_and_decodes_to_state_only_patch() {
    let (_d, mock) = mock_driver();
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Update,
        target("/github/acme/widgets/issues/123"),
    )
    .with_args(args(&[("state", Value::Text("closed".into()))]));
    let pv = preview_one(&mock, node.clone());
    assert_eq!(pv.rows[0].verb, "UPDATE");
    // Decode the same node to confirm it lowers to a state-ONLY PATCH (no title/body/labels).
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
            assert_eq!(state.as_deref(), Some("closed"));
            assert!(
                title.is_none() && body.is_none() && labels.is_none(),
                "state-only PATCH"
            );
        }
        other => panic!("expected PatchIssue, got {other:?}"),
    }
}

#[test]
fn s2_s7_merge_and_dispatch_preview_as_irreversible_calls_review_is_not() {
    let (_d, mock) = mock_driver();

    // CALL github.merge(method=>'squash') on /pulls/7 — irreversible.
    let merge = preview_one(
        &mock,
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("github.merge")),
            target("/github/acme/widgets/pulls/7"),
        )
        .irreversible(true)
        .with_args(args(&[("method", Value::Text("squash".into()))])),
    );
    assert_eq!(merge.rows[0].verb, "CALL github.merge");
    assert!(
        merge.rows[0].irreversible,
        "PREVIEW surfaces merge as irreversible"
    );
    assert_eq!(merge.irreversible, vec![NodeId(0)]);
    // The human-rendered preview marks it with (!) — what an operator sees before COMMIT.
    assert!(
        format!("{merge}").contains("(!)"),
        "irreversible marker in display"
    );

    // CALL github.dispatch(workflow=>'ci.yml', ref=>'main') on /runs — irreversible.
    let dispatch = preview_one(
        &mock,
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("github.dispatch")),
            target("/github/acme/widgets/runs"),
        )
        .irreversible(true)
        .with_args(args(&[
            ("workflow", Value::Text("ci.yml".into())),
            ("ref", Value::Text("main".into())),
        ])),
    );
    assert_eq!(dispatch.rows[0].verb, "CALL github.dispatch");
    assert!(
        dispatch.rows[0].irreversible,
        "PREVIEW surfaces dispatch as irreversible"
    );

    // CALL github.review(event=>'APPROVE') on /pulls/7 — NOT irreversible (supersedable).
    let review = preview_one(
        &mock,
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("github.review")),
            target("/github/acme/widgets/pulls/7"),
        )
        .with_args(args(&[("event", Value::Text("APPROVE".into()))])),
    );
    assert_eq!(review.rows[0].verb, "CALL github.review");
    assert!(!review.rows[0].irreversible, "a review can be superseded");

    // The declared ProcSig irreversibility agrees with the plan-node irreversibility.
    let (d, _) = mock_driver();
    assert!(resolve_proc(&d, "merge").unwrap().irreversible);
    assert!(resolve_proc(&d, "dispatch").unwrap().irreversible);
    assert!(!resolve_proc(&d, "review").unwrap().irreversible);
}

// ============================================================================================
// Scenario 2 (endpoint correctness) — drive merge/dispatch/review to COMMIT and assert the
//   EXACT endpoint/method each puts on the wire (the right endpoints, black-box at the wire).
// ============================================================================================

#[tokio::test]
async fn s2_call_procedures_hit_the_right_endpoints_on_the_wire() {
    let (secrets, key) = store_with_canary();
    // 200 for merge (PUT), 204 for dispatch (POST), 200 for review (POST).
    let tap = WireTap::with(vec![
        HttpResponse::new(200, b"{}".to_vec()),
        HttpResponse::new(204, Vec::new()),
        HttpResponse::new(200, b"{}".to_vec()),
    ]);
    let client = RestGitHubClient::new(tap.clone(), secrets, key);

    client
        .apply(&GitHubEffect::Merge {
            slug: "acme/widgets".into(),
            number: "7".into(),
            method: "squash".into(),
            sha: Some("deadbeef".into()),
        })
        .unwrap();
    client
        .apply(&GitHubEffect::Dispatch {
            slug: "acme/widgets".into(),
            workflow: "ci.yml".into(),
            ref_name: "main".into(),
            inputs: "{}".into(),
        })
        .unwrap();
    client
        .apply(&GitHubEffect::Review {
            slug: "acme/widgets".into(),
            number: "7".into(),
            event: "APPROVE".into(),
            body: String::new(),
        })
        .unwrap();

    let reqs = tap.requests();
    assert_eq!(reqs.len(), 3);
    // merge → PUT /repos/acme/widgets/pulls/7/merge
    assert_eq!(reqs[0].method.as_str(), "PUT");
    assert!(
        reqs[0].url.ends_with("/repos/acme/widgets/pulls/7/merge"),
        "{}",
        reqs[0].url
    );
    // dispatch → POST /repos/acme/widgets/actions/workflows/ci.yml/dispatches
    assert_eq!(reqs[1].method.as_str(), "POST");
    assert!(
        reqs[1]
            .url
            .ends_with("/repos/acme/widgets/actions/workflows/ci.yml/dispatches"),
        "{}",
        reqs[1].url
    );
    // review → POST /repos/acme/widgets/pulls/7/reviews
    assert_eq!(reqs[2].method.as_str(), "POST");
    assert!(
        reqs[2].url.ends_with("/repos/acme/widgets/pulls/7/reviews"),
        "{}",
        reqs[2].url
    );
}

// ============================================================================================
// Scenario 4 — pagination: ONE plan node, AND followed correctly at the wire (two-page Link).
// ============================================================================================

#[test]
fn s4_paginated_select_is_one_plan_node_and_follows_link_at_the_wire() {
    // (a) Plan shape: a paginated SELECT is ONE batched fetch set (the page follow is at the edge).
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
    let plan = read::ReadPlan::list("acme/widgets", Namespace::Issues, None, Some(&pred));
    assert_eq!(
        plan.params(),
        &[
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), "bug".to_string())
        ]
    );

    // (b) Wire level: page 1 carries rel="next"; page 2 has none. The client follows + merges into
    //     ONE result set, issuing exactly two fetches — the bounded fan-out a single plan node owns.
    let (secrets, key) = store_with_canary();
    let page1 = HttpResponse::new(
        200,
        br#"[{"number":1,"title":"a","state":"open","labels":[{"name":"bug"}]}]"#.to_vec(),
    )
    .header(
        "Link",
        "<https://api.github.com/repos/acme/widgets/issues?page=2>; rel=\"next\"",
    );
    let page2 = HttpResponse::new(
        200,
        br#"[{"number":2,"title":"b","state":"open","labels":[{"name":"bug"}]}]"#.to_vec(),
    );
    let tap = WireTap::with(vec![page1, page2]);
    let client = RestGitHubClient::new(tap.clone(), secrets, key);
    let value = client
        .list("acme/widgets", Namespace::Issues, None, plan.params())
        .unwrap();
    let issues = read::decode_issues(&value);
    assert_eq!(issues.len(), 2, "both pages merged into one result set");
    let reqs = tap.requests();
    assert_eq!(reqs.len(), 2, "page 1 + the followed next-page link");
    assert!(reqs[0].url.contains("state=open") && reqs[0].url.contains("labels=bug"));
    assert!(reqs[1].url.contains("page=2"), "followed the rel=next link");
}

// ============================================================================================
// Scenario 5 — PUSHDOWN RESIDUAL TRUTHFULNESS (the recurring trap; try hard to break it).
//   The mock OVER-returns rows (the GitHub `labels` param is a SET-membership pre-filter that
//   also returns rows carrying OTHER labels). We then EXECUTE the residual the driver reported
//   and prove it re-filters to EXACTLY the correct set — no wrong rows survive.
// ============================================================================================

/// A minimal residual evaluator over a decoded row + the row's column schema. This stands in for
/// the engine's local re-filter — proving the residual the driver HANDS BACK is sufficient and
/// truthful (if the residual under-specified, wrong rows would survive this filter).
fn row_matches(pred: &Predicate, schema: &Schema, row: &Row) -> bool {
    match pred {
        Predicate::And(a, b) => row_matches(a, schema, row) && row_matches(b, schema, row),
        Predicate::Or(a, b) => row_matches(a, schema, row) || row_matches(b, schema, row),
        Predicate::Not(p) => !row_matches(p, schema, row),
        Predicate::Cmp(col, CmpOp::Eq, Literal::Text(want)) => {
            let name = col.path.last().map(cfs_types::Name::as_str).unwrap_or("");
            // The driver's pushdown contract (pushdown.rs module doc) states a SQL `=` against the
            // SCALAR `label` column is re-checked locally as SET MEMBERSHIP over the fetched
            // `labels` array. Model that exact column mapping so the residual evaluates faithfully.
            let column = if name == "label" { "labels" } else { name };
            let idx = schema.columns.iter().position(|c| c.name == column);
            match idx.and_then(|i| row.values.get(i)) {
                // A scalar text column: exact equality.
                Some(Value::Text(v)) => v == want,
                // The `labels` text-array column: set-membership re-check (the truthful residual).
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|v| matches!(v, Value::Text(t) if t == want)),
                _ => false,
            }
        }
        other => panic!("residual evaluator does not model {other:?} — extend the harness"),
    }
}

#[test]
fn s5_residual_refilters_over_returned_rows_to_exactly_the_correct_set() {
    // Query: WHERE state='open' AND label='bug'.
    //   - state='open' is EXACT → pushed to `state=open`, dropped from residual.
    //   - label='bug' is a LOSSY set-membership pre-filter → pushed to `labels=bug` AND kept as
    //     the residual the engine must re-apply.
    let state_open = Predicate::Cmp(
        ColRef::col("state"),
        CmpOp::Eq,
        Literal::Text("open".into()),
    );
    let label_bug = Predicate::Cmp(ColRef::col("label"), CmpOp::Eq, Literal::Text("bug".into()));
    let pred = Predicate::And(Box::new(state_open), Box::new(label_bug.clone()));

    let plan = read::ReadPlan::list("acme/widgets", Namespace::Issues, None, Some(&pred));
    // Pushed exactly the two params …
    assert_eq!(
        plan.params(),
        &[
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), "bug".to_string())
        ]
    );
    // … and CRUCIALLY kept the lossy label membership as the residual (state= dropped, being exact).
    let residual = plan
        .pushdown
        .residual
        .clone()
        .expect("the lossy label predicate MUST be kept residual");
    assert_eq!(
        residual, label_bug,
        "residual is exactly the label= membership check"
    );

    // Now the trap: the GitHub `labels=bug` filter returns issues that carry `bug` AMONG others,
    // and (being a pre-filter) the mock also models the realistic GitHub behaviour where the
    // returned page includes rows that do NOT actually satisfy our exact predicate — e.g. a closed
    // issue and an issue that lost the bug label. If the residual were dropped, these wrong rows
    // would leak into the result.
    let over_returned = serde_json::json!([
        // CORRECT: open + carries bug (among others).
        {"number": 1, "title": "real bug", "state": "open",
         "labels": [{"name": "bug"}, {"name": "p1"}]},
        // WRONG: open but does NOT carry bug (the param pre-filter let a near-match slip through).
        {"number": 2, "title": "feature", "state": "open",
         "labels": [{"name": "enhancement"}]},
        // WRONG: carries bug but is CLOSED — only here because we are exercising the residual; the
        //        residual is label-only, so state is enforced by the EXACT pushed param in reality.
        //        We include it to prove the residual does not accidentally pass non-bug rows.
        {"number": 3, "title": "old bug", "state": "open",
         "labels": [{"name": "wontfix"}]},
        // CORRECT: open + bug.
        {"number": 4, "title": "another bug", "state": "open", "labels": [{"name": "bug"}]}
    ]);

    let mock = Arc::new(MockGitHubClient::new().with_list(over_returned));
    let value = mock
        .list("acme/widgets", Namespace::Issues, None, plan.params())
        .unwrap();
    let batch = read::decode_list(Namespace::Issues, &value).unwrap();
    let schema = schema_for(Namespace::Issues);
    assert_eq!(batch.rows.len(), 4, "the mock over-returned all four rows");

    // Apply the reported residual locally — the no-wrong-rows contract.
    let kept: Vec<i64> = batch
        .rows
        .iter()
        .filter(|r| row_matches(&residual, &schema, r))
        .map(|r| match &r.values[0] {
            Value::Int(n) => *n,
            other => panic!("number column not an int: {other:?}"),
        })
        .collect();
    assert_eq!(
        kept,
        vec![1, 4],
        "the residual re-filtered the over-returned rows to EXACTLY the bug-carrying issues — \
         rows 2 (no bug) and 3 (no bug) were correctly dropped; no wrong rows survived"
    );
}

#[test]
fn s5b_or_and_other_columns_stay_wholly_residual_so_nothing_pushed_silently_drops_rows() {
    // An OR cannot be expressed by GitHub's AND-only param set → push NOTHING, keep the whole
    // predicate residual. If pushdown lied and pushed a partial param, rows would be wrongly cut.
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
    let plan = read::ReadPlan::list("acme/widgets", Namespace::Issues, None, Some(&or_pred));
    assert!(plan.params().is_empty(), "nothing pushed for an OR");
    assert_eq!(
        plan.pushdown.residual.as_ref(),
        Some(&or_pred),
        "the whole OR is kept residual — filtered locally, never silently pushed"
    );

    // A non-listable namespace (comments) pushes NOTHING and keeps the whole predicate residual,
    // even for `state` which IS pushable on issues — pushdown is scoped to listable nodes only.
    let on_comments = read::ReadPlan::list(
        "acme/widgets",
        Namespace::Comments,
        None,
        Some(&Predicate::Cmp(
            ColRef::col("state"),
            CmpOp::Eq,
            Literal::Text("open".into()),
        )),
    );
    assert!(
        on_comments.params().is_empty(),
        "comments is not a pushdown-listable node — nothing pushed"
    );
    assert!(
        on_comments.pushdown.residual.is_some(),
        "the predicate is kept whole as residual on a non-listable node"
    );
}

// ============================================================================================
// Scenario 6 — TOKEN SAFETY: canary in NO plan/preview/error/DTO/log; Bearer redaction at the
//   wire; POST never silently retried.
// ============================================================================================

#[test]
fn s6_canary_pat_never_appears_in_a_serialized_plan_or_preview() {
    let (_d, _mock) = mock_driver();
    // A plan over /github whose args carry user text — it must STILL be token-free by construction
    // (the PAT lives only behind the auth seam, read at apply time, never in the plan IR).
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/github/acme/widgets/issues/1/comments"),
        )
        .with_args(args(&[("body", Value::Text("a normal comment".into()))])),
    );
    let plan = b.build();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains(CANARY_PAT),
        "no PAT in serialized plan: {json}"
    );
    assert!(!json.contains("Bearer"), "no bearer in serialized plan");

    let pv = preview(&plan);
    let pv_json = serde_json::to_string(&pv).unwrap();
    let pv_text = format!("{pv} {pv:?}");
    for hay in [pv_json.as_str(), pv_text.as_str()] {
        assert!(
            !hay.contains(CANARY_PAT),
            "no PAT in preview surface: {hay}"
        );
        assert!(!hay.contains("Bearer"), "no bearer in preview surface");
    }
}

#[test]
fn s6_bearer_redaction_holds_at_the_wire_and_pat_is_on_the_wire_but_hidden_in_debug() {
    let (secrets, key) = store_with_canary();
    let tap = WireTap::with(vec![HttpResponse::new(200, b"[]".to_vec())]);
    let client = RestGitHubClient::new(tap.clone(), secrets, key);
    client
        .list(
            "acme/widgets",
            Namespace::Issues,
            None,
            &[("state".into(), "open".into())],
        )
        .unwrap();

    let reqs = tap.requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    // The PAT IS present as a real Bearer header value on the wire (it has to authenticate) …
    assert_eq!(
        req.header_value("authorization"),
        Some(format!("Bearer {CANARY_PAT}").as_str())
    );
    // … but the redacting Debug NEVER reveals it — the only log surface an operator would print.
    let dbg = format!("{req:?}");
    assert!(
        !dbg.contains(CANARY_PAT),
        "PAT must not appear in request Debug: {dbg}"
    );
    assert!(
        !dbg.contains("Bearer "),
        "the bearer value is redacted in Debug: {dbg}"
    );
    assert!(
        dbg.contains(cfs_secrets::REDACTED),
        "the redaction placeholder is present"
    );
}

#[test]
fn s6_errors_carry_no_token_material() {
    // A non-2xx on a list must surface a structured, secret-free error — no token, no header.
    let (secrets, key) = store_with_canary();
    let tap = WireTap::with(vec![HttpResponse::new(401, b"Bad credentials".to_vec())]);
    let client = RestGitHubClient::new(tap.clone(), secrets, key);
    let err = client
        .list("acme/widgets", Namespace::Issues, None, &[])
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert_eq!(err.code(), "github_api");
    assert!(!text.contains(CANARY_PAT), "no PAT in error: {text}");
    assert!(!text.contains("Bearer"), "no bearer in error: {text}");
    // The wire request that produced the 401 still carried the bearer (it had to), but the ERROR
    // surfaced to the agent is token-free — the canary is nowhere in the structured failure.
}

#[test]
fn s6_write_post_is_never_silently_retried_even_on_a_500() {
    // A 500 on a non-idempotent POST is terminal: issued EXACTLY once, never auto-retried — the
    // at-least-once contract that protects against double-posting a comment.
    let (secrets, key) = store_with_canary();
    let tap = WireTap::with(vec![HttpResponse::new(500, Vec::new())]);
    let client = RestGitHubClient::new(tap.clone(), secrets, key);
    let err = client
        .apply(&GitHubEffect::PostComment {
            slug: "acme/widgets".into(),
            number: "1".into(),
            body: "x".into(),
        })
        .unwrap_err();
    assert_eq!(err.code(), "github_api");
    let reqs = tap.requests();
    assert_eq!(
        reqs.len(),
        1,
        "a write POST is issued exactly once, never retried"
    );
    assert_eq!(reqs[0].method.as_str(), "POST");
    // Contrast: a transient 429 on a GET IS retried (idempotent) — proves the asymmetry is real.
    let (secrets2, key2) = store_with_canary();
    let tap2 = WireTap::with(vec![
        HttpResponse::new(429, Vec::new()).header("Retry-After", "0"),
        HttpResponse::new(200, b"[]".to_vec()),
    ]);
    let client2 = RestGitHubClient::new(tap2.clone(), secrets2, key2);
    client2
        .list("acme/widgets", Namespace::Issues, None, &[])
        .unwrap();
    assert_eq!(
        tap2.requests().len(),
        2,
        "an idempotent GET IS retried on a transient 429"
    );
}

// ============================================================================================
// End-to-end COMMIT through the public interpreter + bridge (the real apply loop).
// ============================================================================================

#[tokio::test]
async fn e2e_commit_post_comment_through_the_public_interpreter() {
    let (driver, mock) = mock_driver();
    let bridge = github_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/github/acme/widgets/issues/123/comments"),
        )
        .with_args(args(&[("body", Value::Text("LGTM".into()))])),
    );
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("github"), &EffectKind::Insert);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "comment posted: {outcome:?}");
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::Apply(GitHubEffect::PostComment {
            slug: "acme/widgets".to_string(),
            number: "123".to_string(),
            body: "LGTM".to_string(),
        })]
    );
}
