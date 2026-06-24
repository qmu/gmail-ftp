//! The qfs-skill **golden example corpus** (ticket t39, RFD §3/§5/§6/§8).
//!
//! Every `SKILL.md` worked example is proven here: it **parses → evaluates → its PREVIEW
//! rendering matches a checked-in golden** — with **NO COMMIT, NO network, no live credentials**.
//! This REUSES the t38 `qfs-test` harness (`assert_plan` / `golden` / `preview_handler`) — it does
//! NOT re-hand-roll a parallel harness.
//!
//! ## Loop uniformity IS the deliverable
//! Each driver's example follows the identical four steps (DESCRIBE → statement → PREVIEW →
//! COMMIT). The describe-facet drivers here are the SAME cred-free facets the binary's
//! `qfs describe` registry uses (mock clients / empty registries / fixture catalog+repo); only the
//! pure introspective half is touched, so the corpus performs no I/O. The mock clients are never
//! invoked — building a plan never reaches the applier seam.
//!
//! ## Coverage (one worked example per required driver)
//! - **mail / drive / slack / git** — full PREVIEW-plan goldens via `assert_plan` (an effect
//!   statement evaluates to a `Plan` whose canonical-JSON PREVIEW is the golden).
//! - **github** — both the write-plan golden (`INSERT INTO …/issues`) AND a resolution check that
//!   `CALL github.merge(method => 'squash')` resolves the declared, irreversible procedure (a
//!   `FROM … |> CALL` pipeline is a pure read producing a Relation, not an effect Plan — the agent
//!   reads its rows; the irreversible-CALL effect path is the runtime's COMMIT leg).
//! - **sql** — a pure read (no effect plan): it resolves against the fixture catalog and yields a
//!   Relation, and its DescribeReport pins the relational + pushdown contract the agent reads.
//! - **server** — a `CREATE TRIGGER` PREVIEW-plan golden via `preview_handler` (pure desugar to one
//!   `/server` config write, the RFD §8 PREVIEW-as-CI-test pattern).
//! - **NEGATIVE** — an unsupported verb for a node fails at resolve time with the structured
//!   `unsupported_verb` error (RFD §5 — the agent-legible failure path), before any plan or I/O.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_core::{Driver, EffectKind, EvalError, EvalValue, Evaluator, MountRegistry, ResolveError};
use qfs_parser::parse_statement;
use qfs_test::{assert_plan, preview_handler};

/// Build a write-capable registry holding the cred-free mock-client drivers (local, mail, drive,
/// github, slack). These give REAL capabilities + procedures, so `assert_plan` builds a real
/// effect `Plan` with no creds and no I/O.
fn service_registry() -> MountRegistry {
    let mut reg = MountRegistry::new();
    let drivers: Vec<Arc<dyn Driver>> = vec![
        Arc::new(qfs_driver_local::LocalFsDriver::new("/")),
        Arc::new(qfs_driver_gmail::GmailDriver::new(Arc::new(
            qfs_driver_gmail::MockGmailClient::new(),
        ))),
        Arc::new(qfs_driver_gdrive::GDriveDriver::new(Arc::new(
            qfs_driver_gdrive::MockDriveClient::default(),
        ))),
        Arc::new(qfs_driver_github::GitHubDriver::new(Arc::new(
            qfs_driver_github::MockGitHubClient::default(),
        ))),
        Arc::new(qfs_driver_slack::SlackDriver::new(Arc::new(
            qfs_driver_slack::MockSlackClient::default(),
        ))),
    ];
    for d in drivers {
        reg.register(d).unwrap();
    }
    reg
}

/// Evaluate `src` against `reg` and return the resolved value (Relation or Plan), proving it
/// resolves + capability-gates with no I/O. Used for the pure-read examples (sql, github CALL).
fn eval_value(src: &str, reg: &MountRegistry) -> EvalValue {
    let stmt = parse_statement(src).unwrap_or_else(|e| panic!("`{src}` did not parse: {e:?}"));
    Evaluator::new(reg)
        .eval(&stmt)
        .unwrap_or_else(|e| panic!("`{src}` did not resolve: {e:?}"))
}

// ---------------------------------------------------------------------------
// mail — append_log (INSERT INTO /mail/drafts).
// ---------------------------------------------------------------------------

#[test]
fn mail_insert_draft_previews_reversible_plan() {
    // Step 2/3: create a draft — a reversible append. (The irreversible `CALL mail.send` is the
    // separate COMMIT-gated step documented in SKILL.md.) No COMMIT, no network.
    assert_plan(
        "INSERT INTO /mail/drafts VALUES ('alice@example.com', 'Hi', 'Body text')",
        &service_registry(),
    )
    .nodes(&[EffectKind::Insert])
    .irreversible(0)
    .no_io_performed()
    .snapshot("plan_mail_insert_draft");
}

// ---------------------------------------------------------------------------
// drive — blob_namespace (UPSERT INTO /drive/Reports/<file>; the retry-safe blob write the
// `cp /local/report.pdf /drive/Reports/` shell verb lowers to).
// ---------------------------------------------------------------------------

#[test]
fn drive_upsert_blob_previews_plan() {
    // Step 2/3: a retry-safe blob write to Drive (UPSERT = the idempotent default, RFD §6). This is
    // the closed-core form the `cp` shell builtin lowers to; SKILL.md shows the `cp` surface.
    assert_plan(
        "UPSERT INTO /drive/my/Reports/report.pdf VALUES ('report-bytes')",
        &service_registry(),
    )
    .nodes(&[EffectKind::Upsert])
    .irreversible(0)
    .no_io_performed()
    .snapshot("plan_drive_upsert_blob");
}

// ---------------------------------------------------------------------------
// github — object_graph_workflow: a write-plan golden (issue creation) PLUS a resolution check
// that `CALL github.merge(method => 'squash')` resolves the declared irreversible procedure.
// ---------------------------------------------------------------------------

#[test]
fn github_insert_issue_previews_plan() {
    // Step 2/3: open an issue — an object-graph INSERT (reversible; issues are closed, not deleted).
    assert_plan(
        "INSERT INTO /github/acme/web/issues VALUES (title) ('Tracking bug')",
        &service_registry(),
    )
    .nodes(&[EffectKind::Insert])
    .no_io_performed()
    .snapshot("plan_github_insert_issue");
}

#[test]
fn github_merge_call_resolves_declared_irreversible_proc() {
    // `FROM …/pulls/42 |> CALL github.merge(method => 'squash')` is a pure pipeline (a Relation):
    // resolution capability-gates the path and proves `github.merge` is a DECLARED procedure. Its
    // irreversibility is asserted on the procedure contract (the COMMIT leg is the runtime's).
    let value = eval_value(
        "FROM /github/acme/web/pulls/42 |> CALL github.merge(method => 'squash')",
        &service_registry(),
    );
    assert!(
        matches!(value, EvalValue::Relation(_)),
        "a `FROM … |> CALL` pipeline is a pure read (Relation)"
    );
    // The declared `merge` procedure is irreversible (the agent's COMMIT gate).
    let reg = service_registry();
    let driver = reg.resolve("/github").unwrap();
    let merge = qfs_core::resolve_proc(driver.as_ref(), "merge").unwrap();
    assert!(merge.irreversible, "github.merge is declared irreversible");
}

// ---------------------------------------------------------------------------
// slack — append_log (INSERT INTO /slack/<ws>/<channel>/messages; a bare channel name is a valid
// ChannelRef resolved at commit — PREVIEW shows the symbolic channel, never an id lookup).
// ---------------------------------------------------------------------------

#[test]
fn slack_insert_message_previews_reversible_append() {
    assert_plan(
        "INSERT INTO /slack/acme/general/messages VALUES ('Deploy finished')",
        &service_registry(),
    )
    .nodes(&[EffectKind::Insert])
    .irreversible(0)
    .no_io_performed()
    .snapshot("plan_slack_insert");
}

// ---------------------------------------------------------------------------
// sql — relational_table, pushdown. A PURE READ (no effect plan): it resolves against the fixture
// catalog and yields a Relation; the DescribeReport pins the pushdown contract.
// ---------------------------------------------------------------------------

mod sql_fixture {
    use super::*;
    use qfs_core::{Path, Row};
    use qfs_driver_sql::{
        Catalog, ColumnDef, ConnHandle, ConnRegistry, Dialect, DmlOp, Param, RelationKind,
        SqlBackend, SqlDriver, SqlError, TableCatalog,
    };

    /// A no-op SQL backend (no socket, no creds): the introspective `Driver` methods read only the
    /// CACHED catalog (built via `with_catalog`), so `execute_read`/`commit_transaction` are never
    /// reached on the describe/plan path. They return an honest "offline fixture" error if called.
    struct OfflineBackend;

    impl SqlBackend for OfflineBackend {
        fn dialect(&self) -> Dialect {
            Dialect::Postgres
        }
        fn introspect(&self) -> Result<Catalog, SqlError> {
            Ok(Catalog::new(Vec::new()))
        }
        fn execute_read(&self, _sql: &str, _params: &[Param]) -> Result<Vec<Row>, SqlError> {
            Err(SqlError::Backend {
                dialect: "postgres",
                op: "select",
                reason: "offline describe fixture: no live execution".to_string(),
            })
        }
        fn commit_transaction(&self, _ops: &[DmlOp]) -> Result<u64, SqlError> {
            Err(SqlError::Backend {
                dialect: "postgres",
                op: "commit",
                reason: "offline describe fixture: no live commit".to_string(),
            })
        }
    }

    /// A fixture `/sql/pg` driver whose `orders` table catalog is cached (id INT, total INT). The
    /// describe + capability + read-plan paths are pure over this cached catalog.
    fn sql_registry() -> MountRegistry {
        let orders = TableCatalog::new(
            "orders",
            RelationKind::Table,
            vec![
                ColumnDef::new("id", qfs_core::ColumnType::Int, false, true, true),
                ColumnDef::new("total", qfs_core::ColumnType::Int, false, false, false),
            ],
        );
        let catalog = Catalog::new(vec![orders]);
        let handle = ConnHandle::with_catalog(Arc::new(OfflineBackend), catalog);
        let conns = ConnRegistry::new().with("pg", handle);
        let mut reg = MountRegistry::new();
        reg.register(Arc::new(SqlDriver::new(conns))).unwrap();
        reg
    }

    #[test]
    fn sql_read_resolves_against_fixture_catalog_no_io() {
        // FROM /sql/pg/orders |> WHERE total > 100 |> SELECT id, total — a PURE read. It evaluates
        // to a Relation (not an effect Plan), proving the read path resolves with no live backend.
        let value = eval_value(
            "FROM /sql/pg/orders |> WHERE total > 100 |> SELECT id, total",
            &sql_registry(),
        );
        assert!(
            matches!(value, EvalValue::Relation(_)),
            "a SELECT is a pure read, not an effect plan"
        );
    }

    #[test]
    fn sql_describe_reports_relational_pushdown() {
        let reg = sql_registry();
        let (driver, _rest) = reg.resolve_path("/sql/pg/orders").unwrap();
        let report =
            qfs_core::DescribeReport::from_driver(driver.as_ref(), &Path::new("/sql/pg/orders"))
                .expect("/sql/pg/orders describes");
        assert_eq!(report.archetype, qfs_core::Archetype::RelationalTable);
        assert!(report.pushdown.where_, "sql pushes WHERE down");
        assert!(report.pushdown.project, "sql pushes projection down");
        assert!(report.verbs.select);
        // No credential shape in the report (secrets never appear).
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.to_lowercase().contains("password"));
    }
}

// ---------------------------------------------------------------------------
// git — INSERT INTO /git/myrepo/commits PREVIEW-plan golden over a fixture repo.
// ---------------------------------------------------------------------------

mod git_fixture {
    use super::*;
    use qfs_driver_git::{GitApplier, GitDriver, LooseObjectDb, Repo, RepoResolver};

    /// A fixture `/git` driver with one registered empty repo `myrepo` (no creds, no real .git on
    /// disk). The commit-log node then declares INSERT, so an `INSERT INTO /git/myrepo/commits`
    /// resolves + builds a plan with no I/O.
    fn git_registry() -> MountRegistry {
        let repo = Repo::new(Arc::new(LooseObjectDb::new()));
        let repos = RepoResolver::new().with_repo("myrepo", repo);
        let driver = GitDriver::new(repos, GitApplier::new());
        let mut reg = MountRegistry::new();
        reg.register(Arc::new(driver)).unwrap();
        reg
    }

    #[test]
    fn git_insert_commit_previews_plan_no_io() {
        // INSERT INTO /git/myrepo/commits — record a commit (history is append-only). Built purely;
        // nothing is committed, no .git is touched.
        assert_plan(
            "INSERT INTO /git/myrepo/commits VALUES ('add feature', 'main')",
            &git_registry(),
        )
        .nodes(&[EffectKind::Insert])
        .no_io_performed()
        .snapshot("plan_git_insert_commit");
    }
}

// ---------------------------------------------------------------------------
// server — CREATE TRIGGER PREVIEW-plan golden via preview_handler (pure desugar).
// ---------------------------------------------------------------------------

#[test]
fn server_trigger_previews_single_config_write() {
    // CREATE TRIGGER desugars to exactly one /server config write (RFD §8 PREVIEW-as-CI-test). No
    // socket, no backend — the binding's plan is asserted directly.
    let plan = preview_handler(
        "CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('mail arrived')",
    );
    assert_eq!(
        plan.nodes().len(),
        1,
        "exactly one /server config-write node"
    );
    assert!(
        matches!(plan.nodes()[0].kind, EffectKind::ServerConfigWrite { .. }),
        "the trigger desugars to a /server config write"
    );
    assert!(!plan.is_irreversible(), "a config write is reversible");
    plan.validate().expect("desugared plan is a valid DAG");
}

// ---------------------------------------------------------------------------
// NEGATIVE golden — an unsupported verb fails at resolve time, structured.
// ---------------------------------------------------------------------------

#[test]
fn negative_unsupported_verb_fails_structurally() {
    // A Slack channel message log is an APPEND log: it supports INSERT(append) but NOT UPDATE.
    // Planning an UPDATE must fail at resolve time with the structured `unsupported_verb` error
    // (RFD §5) — the agent-legible failure path — BEFORE any plan or I/O exists.
    let reg = service_registry();
    let stmt = parse_statement("UPDATE /slack/acme/general/messages SET text = 'x' WHERE id = 1")
        .expect("the statement parses (UPDATE is closed-core; the NODE rejects it)");
    let err = Evaluator::new(&reg)
        .eval(&stmt)
        .expect_err("UPDATE on an append-log node must be rejected at resolve time");
    let code = err.code();
    assert_eq!(
        code, "unsupported_verb",
        "expected a structured unsupported_verb error, got code `{code}`: {err:?}"
    );
    // The error is genuinely AGENT-LEGIBLE (RFD §5) WITHOUT string-parsing: its structured fields
    // name the rejected verb AND carry the `supported` set the agent picks a valid verb from (the
    // SKILL.md quick-ref promise "Unsupported verb = structured error … pick from it"). UPDATE is
    // rejected on the slack append-log node; INSERT is offered as a supported alternative.
    let EvalError::Resolve(ResolveError::UnsupportedVerb {
        verb, supported, ..
    }) = &err
    else {
        panic!("expected EvalError::Resolve(UnsupportedVerb {{ .. }}), got: {err:?}");
    };
    assert_eq!(
        *verb, "UPDATE",
        "the rejected verb is named for agent recovery"
    );
    assert!(
        supported.contains(&"INSERT"),
        "the supported-verb set must offer INSERT for agent recovery, got: {supported:?}"
    );
}
