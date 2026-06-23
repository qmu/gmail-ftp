//! Black-box E2E for the watchtower (t34, RFD-0001 §8). Drives ONLY the PUBLIC `cfs-watchtower`
//! surface — the same composition the `cfs serve` root wires (EventBus/Dispatcher/WebhookBinding/
//! WatchtowerBinding over an in-memory LocalBus + a counting fake committer + an in-memory read
//! driver) — and asserts on externally-observable outputs: the lowered handler plan, the audit
//! ledger, the bus spool, the watcher cursor, and the HTTP ingest status. No live network, no live
//! credentials; the wasm target is NOT built (native test build only).
//!
//! Maps 1:1 to the t34 acceptance criteria + the Architect's O-1 observation (bare-path poll-source
//! classification) + scenario 8 (audit/purity/secret hygiene). For scenarios 2, 4, 5, 7 the tests
//! actively try to DEFEAT the guarantee/classification (many redeliveries, distinct keys, a passing
//! AND failing predicate, a wrong/missing/tampered signature, a quoted webhook `on` contrasted with
//! a bare-path poll `on`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, Engine, NodeDesc, Path, PushdownProfile,
    Row, RowBatch, Schema, Value,
};
use cfs_exec::{ReadDriver, ReadRegistry};
use cfs_pushdown::ScanNode;
use cfs_secrets::{AccountId, CredentialKey, DriverId, InMemoryStore, Secret, Secrets};
use cfs_server::{AuditEntry, AuditSink, Binding, ServerState, TriggerDef, WebhookDef};

use cfs_watchtower::{
    bind_new, sign_body, AllowAllGate, Committer, Dispatched, Dispatcher, Event, EventBus, EventId,
    EventKind, FireError, FireOutcome, LocalBus, MemWatcherStore, SourcePath, WatcherSet,
    WatcherStore, WatchtowerBinding, WebhookBinding, SIGNATURE_HEADER,
};

// ===========================================================================
// fakes (no live network / creds) — the composition root's test doubles
// ===========================================================================

/// A counting fake committer: records each successful commit + the bound statement's rendered
/// text, so the idempotency goldens can assert "one net effect across N deliveries" and prove the
/// NEW.* binding reached the COMMIT boundary with the typed literal.
#[derive(Default)]
struct CountingCommitter {
    commits: AtomicU64,
    last_stmt: Mutex<Option<String>>,
}

impl Committer for CountingCommitter {
    fn commit(&self, stmt: &cfs_watchtower::Statement) -> Result<FireOutcome, FireError> {
        let n = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut g) = self.last_stmt.lock() {
            *g = Some(format!("{stmt:?}"));
        }
        Ok(FireOutcome {
            plan_summary: format!("commit#{n}"),
            affected: 1,
        })
    }
}

impl CountingCommitter {
    fn count(&self) -> u64 {
        self.commits.load(Ordering::SeqCst)
    }
    fn last_stmt(&self) -> Option<String> {
        self.last_stmt.lock().ok().and_then(|g| g.clone())
    }
}

/// A committer that fails EVERY commit (the at-least-once redelivery path: a failed commit returns
/// Err, so the dispatcher does NOT ack, so the event stays in the bus spool for redelivery).
struct FailingCommitter;
impl Committer for FailingCommitter {
    fn commit(&self, _stmt: &cfs_watchtower::Statement) -> Result<FireOutcome, FireError> {
        Err(FireError::Apply("forced commit failure".into()))
    }
}

/// An in-memory read source the real `Watcher::poll_once` polls — the FakeMail pattern from the
/// exec layer (over-returns, the residual restores correctness). Carries a SECRET CANARY in its
/// rows so scenario 8 can prove a payload value never leaks into the audit/error surface.
struct FakeMail {
    mount: String,
    rows: Mutex<Vec<Row>>,
}

fn mail_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("subject", ColumnType::Text, true),
    ])
}

impl FakeMail {
    fn new(rows: Vec<Row>) -> Self {
        Self {
            mount: "/mail".to_string(),
            rows: Mutex::new(rows),
        }
    }
    fn seeded() -> Self {
        Self::new(vec![
            Row::new(vec![Value::Int(1), Value::Text("hello".into())]),
            Row::new(vec![Value::Int(2), Value::Text("world".into())]),
        ])
    }
}

#[derive(Default)]
struct NoopApplier;
impl cfs_core::PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &cfs_core::EffectNode,
    ) -> Result<cfs_core::AppliedEffect, cfs_core::ApplyError> {
        Ok(cfs_core::AppliedEffect::new(node.id, 0))
    }
}

impl cfs_core::Driver for FakeMail {
    fn mount(&self) -> &str {
        &self.mount
    }
    fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, mail_schema()))
    }
    fn capabilities(&self, _path: &Path) -> Capabilities {
        Capabilities::none().select().insert()
    }
    fn procedures(&self) -> &[cfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn cfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[async_trait::async_trait]
impl ReadDriver for FakeMail {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let rows = self.rows.lock().map(|g| g.clone()).unwrap_or_default();
        Ok(RowBatch::new(mail_schema(), rows))
    }
}

// ===========================================================================
// builders
// ===========================================================================

/// Build a `TriggerDef` from a real `CREATE TRIGGER` through the SAME DDL → config-row path the
/// binding registry uses, so the test fixture is production-identical.
fn trigger_def(src: &str) -> TriggerDef {
    let ddl = cfs_core::parse_server_binding_ddl(src).expect("parse trigger ddl");
    let row = cfs_core::binding_config_row(&ddl);
    let text = |k: &str| match row.get(k) {
        Some(Value::Text(s)) => s.clone(),
        _ => String::new(),
    };
    TriggerDef {
        name: text("name"),
        on: text("on"),
        predicate: cfs_server::StatementSource::new(text("predicate")),
        plan: cfs_server::StatementSource::new(text("plan")),
    }
}

fn event_with(source: &str, kind: EventKind, native_id: &str, fields: &[(&str, Value)]) -> Event {
    let columns: Vec<String> = fields.iter().map(|(n, _)| (*n).to_string()).collect();
    let values: Vec<Value> = fields.iter().map(|(_, v)| v.clone()).collect();
    Event::new(
        format!("{source}#{native_id}"),
        SourcePath::new(source.to_string()),
        kind,
        native_id,
        columns,
        Row::new(values),
        1000,
    )
}

fn secrets_with(handle: &str, secret: &str) -> Arc<dyn Secrets> {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(DriverId::new("webhook"), AccountId::new(handle).unwrap());
    store
        .put(&key, Secret::from_string(secret.to_string()))
        .unwrap();
    Arc::new(store)
}

/// Scan every audit entry's secret-free rendering for a forbidden substring (canary leak check).
fn audit_text(audit: &AuditSink) -> String {
    audit
        .snapshot()
        .iter()
        .map(AuditEntry::summary)
        .collect::<Vec<_>>()
        .join("\n")
}

// ===========================================================================
// Scenario 1 — Plan assertion: NEW.* correctly bound into the lowered handler
// ===========================================================================

#[test]
fn s1_injected_event_binds_new_into_the_lowered_handler_plan() {
    // The handler inserts NEW.subject into /log; after dispatch the COMMITTED statement must carry
    // the event's subject as a typed string literal — asserted on the statement the fake committer
    // received (the lowered plan), with NO live commit against a real service.
    let trig =
        trigger_def("CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES (NEW.subject)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "e1",
        &[("subject", Value::Text("hello-world".into()))],
    );
    assert_eq!(
        dispatcher
            .handle(&event, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Fired(1)
    );
    // The bound literal reached the COMMIT boundary verbatim (the typed substitution, not text
    // concatenation): the rendered statement carries the value as a Str literal node.
    let rendered = committer.last_stmt().expect("a statement was committed");
    assert!(
        rendered.contains("hello-world"),
        "the NEW.subject value must be bound into the committed plan; got: {rendered}"
    );
    // Independent golden on the pure binder: NEW.subject -> typed Str literal (no re-parse path).
    let mut stmt = cfs_exec::parse("INSERT INTO /log VALUES (NEW.subject)").unwrap();
    let binds = cfs_watchtower::NewBindings::from_row(&event.columns, &event.new.values);
    bind_new(&mut stmt, &binds);
    let cfs_watchtower::Statement::Effect(e) = &stmt else {
        panic!("expected effect")
    };
    let cfs_parser::EffectBody::Values(v) = &e.body else {
        panic!("expected values body")
    };
    assert_eq!(
        v.rows[0][0],
        cfs_parser::Expr::Lit(cfs_parser::Literal::Str("hello-world".to_string()))
    );
}

// ===========================================================================
// Scenario 2 — At-least-once / idempotency (try to break it)
// ===========================================================================

#[test]
fn s2_same_dedup_key_many_redeliveries_yield_one_net_effect() {
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "dup",
        &[("body", Value::Text("payload".into()))],
    );

    // First delivery fires once; the next 9 redeliveries of the SAME dedup_key are all no-ops.
    assert_eq!(
        dispatcher
            .handle(&event, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Fired(1)
    );
    for _ in 0..9 {
        assert_eq!(
            dispatcher
                .handle(&event, &triggers, &committer, &audit)
                .unwrap(),
            Dispatched::Duplicate,
            "a redelivery of an already-committed dedup_key must be a no-op"
        );
    }
    // PUSH HARDER: ONE net effect + ONE audit record across TEN deliveries.
    assert_eq!(
        committer.count(),
        1,
        "exactly one net effect across 10 deliveries"
    );
    assert_eq!(
        audit.len(),
        1,
        "exactly one audit record across 10 deliveries"
    );
    assert!(dispatcher.has_seen("inbox#dup"));
}

#[test]
fn s2_distinct_dedup_keys_each_fire_once() {
    // Try to defeat the ledger from the other side: distinct logical events (distinct dedup_keys)
    // must EACH fire — the ledger collapses redelivery, never two genuinely-distinct events.
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    for i in 0..5 {
        let event = event_with(
            "inbox",
            EventKind::Webhook,
            &format!("id{i}"),
            &[("body", Value::Text(format!("p{i}")))],
        );
        assert_eq!(
            dispatcher
                .handle(&event, &triggers, &committer, &audit)
                .unwrap(),
            Dispatched::Fired(1)
        );
    }
    assert_eq!(committer.count(), 5, "five distinct keys -> five effects");
    assert_eq!(audit.len(), 5);
}

#[test]
fn s2_failed_commit_does_not_advance_the_ledger_so_the_event_redelivers() {
    // The at-least-once core: a COMMIT FAILURE returns Err (NOT a Dispatched), so the event is NOT
    // acked and the dedup_key is NOT recorded — a redelivery RE-ATTEMPTS the commit (never silently
    // dropped). Then a succeeding committer commits it exactly once.
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "retry",
        &[("body", Value::Text("p".into()))],
    );

    // First attempt fails -> Err, not acked, ledger NOT advanced.
    let failing = FailingCommitter;
    assert!(dispatcher
        .handle(&event, &triggers, &failing, &audit)
        .is_err());
    assert!(
        !dispatcher.has_seen("inbox#retry"),
        "a failed commit must not record the key"
    );
    assert_eq!(audit.len(), 0, "a failed commit writes no audit record");

    // Redelivery against a healthy committer fires exactly once.
    let ok = CountingCommitter::default();
    assert_eq!(
        dispatcher.handle(&event, &triggers, &ok, &audit).unwrap(),
        Dispatched::Fired(1)
    );
    assert_eq!(ok.count(), 1);
    assert_eq!(audit.len(), 1);
}

// ===========================================================================
// Scenario 3 — Recovery: un-acked bus event redelivered; cursor restored without skipping
// ===========================================================================

#[tokio::test]
async fn s3_unacked_event_redelivered_after_simulated_crash() {
    let (bus, mut rx) = LocalBus::new(8);
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "r1",
        &[("subject", Value::Text("a".into()))],
    );
    bus.publish(event.clone()).unwrap();

    // Consume the live delivery but DO NOT ack (the crash between publish and ack).
    let delivered = rx.recv().await.unwrap();
    assert_eq!(delivered.id, event.id);
    assert_eq!(
        bus.unacked_len(),
        1,
        "un-acked event stays in the durable spool"
    );

    // Recovery redelivers the un-acked window (never skipped).
    assert_eq!(bus.redeliver_unacked(), 1);
    assert_eq!(rx.recv().await.unwrap().id, event.id);

    // After a real ack the spool drains; recovery then redelivers nothing.
    bus.ack(&event.id).unwrap();
    assert_eq!(bus.unacked_len(), 0);
    assert_eq!(bus.redeliver_unacked(), 0);
}

#[tokio::test]
async fn s3_real_watcher_persists_cursor_only_after_publish_and_resumes_without_skipping() {
    // A REAL Watcher::poll_once over an in-memory read source: first poll emits both rows + persists
    // the cursor; a restart with the SAME store re-runs poll_once and emits NOTHING (no skip, no
    // dupe). Then a new row appears -> only the new row is emitted.
    let engine = {
        let mut e = Engine::new();
        e.mounts.register(Arc::new(FakeMail::seeded())).unwrap();
        e
    };
    let reads = ReadRegistry::new().with(DriverId::new("mail"), Arc::new(FakeMail::seeded()));
    let (bus, mut rx) = LocalBus::new(16);
    let store = MemWatcherStore::new();
    let watcher = cfs_watchtower::Watcher::new("/mail/inbox", 60, "id");

    // First poll: two new rows -> two events published, cursor saved AFTER publish.
    let emitted = watcher
        .poll_once(&engine, &reads, &bus, &store, 1000)
        .await
        .unwrap();
    assert_eq!(emitted, 2, "first poll emits both rows");
    assert_eq!(bus.unacked_len(), 2);
    let cursor = store.load("/mail/inbox").unwrap();
    assert!(
        cursor.contains("1") && cursor.contains("2"),
        "cursor advanced only after publish"
    );
    // Drain the two delivered events.
    let _ = rx.recv().await.unwrap();
    let _ = rx.recv().await.unwrap();

    // RESTART: a fresh watcher + the SAME store re-polls the SAME source -> emits NOTHING (the
    // cursor restore prevents re-emitting the already-seen window; no skip either).
    let watcher2 = cfs_watchtower::Watcher::new("/mail/inbox", 60, "id");
    let again = watcher2
        .poll_once(&engine, &reads, &bus, &store, 1001)
        .await
        .unwrap();
    assert_eq!(
        again, 0,
        "a restart re-polling the same rows emits nothing (no skip, no dupe)"
    );

    // A NEW row appears at the source -> only it is emitted (the bounded new window).
    let engine3 = {
        let mut e = Engine::new();
        e.mounts.register(Arc::new(FakeMail::seeded())).unwrap();
        e
    };
    let reads3 = ReadRegistry::new().with(
        DriverId::new("mail"),
        Arc::new(FakeMail::new(vec![
            Row::new(vec![Value::Int(1), Value::Text("hello".into())]),
            Row::new(vec![Value::Int(2), Value::Text("world".into())]),
            Row::new(vec![Value::Int(3), Value::Text("new".into())]),
        ])),
    );
    let _ = engine3;
    let watcher3 = cfs_watchtower::Watcher::new("/mail/inbox", 60, "id");
    let n3 = watcher3
        .poll_once(&engine, &reads3, &bus, &store, 1002)
        .await
        .unwrap();
    assert_eq!(n3, 1, "only the newly-appeared row is emitted");
    assert!(store.load("/mail/inbox").unwrap().contains("3"));
}

#[tokio::test]
async fn s3_publish_failure_does_not_advance_the_watcher_cursor() {
    // Defeat-test the recovery invariant: if the bus publish FAILS, the cursor must NOT advance, so
    // the un-published window re-emits next poll (never silently skipped). A closed-channel bus
    // (receiver dropped) makes publish fail.
    let engine = {
        let mut e = Engine::new();
        e.mounts.register(Arc::new(FakeMail::seeded())).unwrap();
        e
    };
    let reads = ReadRegistry::new().with(DriverId::new("mail"), Arc::new(FakeMail::seeded()));
    let (bus, rx) = LocalBus::new(16);
    drop(rx); // close the channel so publish() returns Err(Closed)
    let store = MemWatcherStore::new();
    let watcher = cfs_watchtower::Watcher::new("/mail/inbox", 60, "id");

    let res = watcher.poll_once(&engine, &reads, &bus, &store, 1000).await;
    assert!(res.is_err(), "a publish failure surfaces as an error");
    // The cursor must NOT have been persisted with the un-published id (re-emits next poll).
    let cursor = store.load("/mail/inbox").unwrap_or_default();
    assert!(
        !cursor.contains("1") && !cursor.contains("2"),
        "a failed publish must not advance the cursor (the window re-emits, never skips)"
    );
}

// ===========================================================================
// Scenario 4 — WHERE gating (try to defeat it both directions)
// ===========================================================================

#[test]
fn s4_where_gating_blocks_failing_and_fires_passing() {
    // The CREATE TRIGGER … ON … WHERE NEW.<field> … DO … form the parser wired: round-trips through
    // the real DDL path and gates correctly.
    let trig = trigger_def(
        "CREATE TRIGGER hot ON inbox WHERE NEW.priority > 3 DO UPSERT INTO /log VALUES (NEW.body)",
    );
    // The predicate round-tripped into a non-empty spec (the WHERE clause survived the DDL path).
    assert!(
        !trig.predicate.as_str().is_empty(),
        "the WHERE clause round-tripped into the spec"
    );
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    // FAILING predicate (priority 1 ≤ 3): zero fire, zero audit, zero commit (zero driver calls).
    let low = event_with(
        "inbox",
        EventKind::Webhook,
        "g1",
        &[
            ("priority", Value::Int(1)),
            ("body", Value::Text("x".into())),
        ],
    );
    assert_eq!(
        dispatcher
            .handle(&low, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Gated
    );
    assert_eq!(committer.count(), 0, "a failing WHERE fires no plan");
    assert_eq!(audit.len(), 0, "a failing WHERE writes no audit record");

    // PASSING predicate (priority 5 > 3): exactly one fire.
    let high = event_with(
        "inbox",
        EventKind::Webhook,
        "g2",
        &[
            ("priority", Value::Int(5)),
            ("body", Value::Text("y".into())),
        ],
    );
    assert_eq!(
        dispatcher
            .handle(&high, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Fired(1)
    );
    assert_eq!(committer.count(), 1);
    assert_eq!(audit.len(), 1);
}

#[test]
fn s4_boundary_and_missing_field_fail_closed() {
    // Defeat attempts: the exact boundary (priority == 3, NOT > 3) must NOT fire; a missing field
    // must NOT fire (fail-closed, never a panic).
    let trig = trigger_def(
        "CREATE TRIGGER hot ON inbox WHERE NEW.priority > 3 DO UPSERT INTO /log VALUES ('x')",
    );
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    let boundary = event_with(
        "inbox",
        EventKind::Webhook,
        "b",
        &[("priority", Value::Int(3))],
    );
    assert_eq!(
        dispatcher
            .handle(&boundary, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Gated,
        "the > boundary must exclude equality"
    );
    let missing = event_with(
        "inbox",
        EventKind::Webhook,
        "m",
        &[("other", Value::Int(99))],
    );
    assert_eq!(
        dispatcher
            .handle(&missing, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Gated,
        "a missing guarded field fails closed (never fires, never panics)"
    );
    assert_eq!(committer.count(), 0);
    assert_eq!(audit.len(), 0);
}

// ===========================================================================
// Scenario 5 — Webhook signature (signed -> 202+1; bad/missing/tampered -> 401+0)
// ===========================================================================

fn webhook_state(name: &str, route: &str, secret_handle: &str) -> ServerState {
    let mut state = ServerState::new();
    state.webhooks.insert(
        name.to_string(),
        WebhookDef {
            name: name.to_string(),
            route: route.to_string(),
            secret: secret_handle.to_string(),
        },
    );
    state
}

#[test]
fn s5_signed_request_enqueues_one_event_returns_2xx() {
    let secret = "topsecret";
    let secrets = secrets_with("inbox-hook", secret);
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut binding = WebhookBinding::new(secrets, Arc::clone(&bus));
    binding
        .reconcile(&webhook_state("inbox", "/hooks/inbox", "inbox-hook"))
        .unwrap();

    let body = br#"{"subject":"hi"}"#;
    let mut headers = BTreeMap::new();
    headers.insert(
        SIGNATURE_HEADER.to_string(),
        sign_body(secret.as_bytes(), body),
    );
    let out = binding.ingest("/hooks/inbox", &headers, body, 2000);
    assert_eq!(out.status, 202);
    assert!(out.published);
    assert_eq!(bus.unacked_len(), 1, "exactly one event enqueued");
}

#[test]
fn s5_wrong_missing_and_tampered_signatures_all_401_enqueue_nothing() {
    let secret = "topsecret";
    let secrets = secrets_with("inbox-hook", secret);
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut binding = WebhookBinding::new(secrets, Arc::clone(&bus));
    binding
        .reconcile(&webhook_state("inbox", "/hooks/inbox", "inbox-hook"))
        .unwrap();
    let body = br#"{"subject":"hi"}"#;

    // (a) Wrong secret.
    let mut h = BTreeMap::new();
    h.insert(SIGNATURE_HEADER.to_string(), sign_body(b"wrong", body));
    assert_eq!(binding.ingest("/hooks/inbox", &h, body, 2000).status, 401);

    // (b) Missing header.
    assert_eq!(
        binding
            .ingest("/hooks/inbox", &BTreeMap::new(), body, 2000)
            .status,
        401
    );

    // (c) Tampered body: a valid signature over the ORIGINAL body, but a MUTATED body delivered.
    let mut h2 = BTreeMap::new();
    h2.insert(
        SIGNATURE_HEADER.to_string(),
        sign_body(secret.as_bytes(), body),
    );
    let tampered = br#"{"subject":"HACKED"}"#;
    assert_eq!(
        binding.ingest("/hooks/inbox", &h2, tampered, 2000).status,
        401
    );

    // (d) Malformed prefix (no `v0=`).
    let mut h3 = BTreeMap::new();
    h3.insert(SIGNATURE_HEADER.to_string(), "deadbeef".to_string());
    assert_eq!(binding.ingest("/hooks/inbox", &h3, body, 2000).status, 401);

    // NOTHING was ever enqueued across all four rejections.
    assert_eq!(
        bus.unacked_len(),
        0,
        "no event enqueued for any rejected request"
    );

    // A request to an UNRECONCILED route is 404 (no route), still zero events.
    let mut h4 = BTreeMap::new();
    h4.insert(
        SIGNATURE_HEADER.to_string(),
        sign_body(secret.as_bytes(), body),
    );
    assert_eq!(
        binding.ingest("/hooks/missing", &h4, body, 2000).status,
        404
    );
    assert_eq!(bus.unacked_len(), 0);
}

// ===========================================================================
// Scenario 6 — Reconcile: add/remove spawns/cancels; idempotent re-reconcile is a no-op
// ===========================================================================

#[test]
fn s6_reconcile_converges_routes_and_watchers_and_is_idempotent() {
    let secrets = secrets_with("h", "s");
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut wt = WatchtowerBinding::new(secrets, bus);

    // Empty -> zero routes, zero watchers, zero triggers.
    wt.reconcile(&ServerState::new()).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 0);
    assert_eq!(wt.current_watchers().len(), 0);
    assert_eq!(wt.current_triggers().len(), 0);

    // Add a webhook + a poll-source trigger + an event-kind (non-watcher) trigger.
    let mut state = webhook_state("inbox", "/hooks/inbox", "h");
    state.triggers.insert(
        "poll".to_string(),
        TriggerDef {
            name: "poll".to_string(),
            on: "/mail/inbox".to_string(),
            predicate: Default::default(),
            plan: Default::default(),
        },
    );
    state.triggers.insert(
        "on_hook".to_string(),
        TriggerDef {
            name: "on_hook".to_string(),
            on: "webhook".to_string(),
            predicate: Default::default(),
            plan: Default::default(),
        },
    );
    wt.reconcile(&state).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 1);
    assert_eq!(
        wt.current_watchers().len(),
        1,
        "only the poll-source trigger spawns a watcher"
    );
    assert_eq!(
        wt.current_triggers().len(),
        2,
        "both triggers are in the live dispatch set"
    );

    // Idempotent re-reconcile: identical counts (a no-op convergence for the daemon).
    wt.reconcile(&state).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 1);
    assert_eq!(wt.current_watchers().len(), 1);
    assert_eq!(wt.current_triggers().len(), 2);

    // Remove everything -> back to zero (cancel).
    wt.reconcile(&ServerState::new()).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 0);
    assert_eq!(wt.current_watchers().len(), 0);
    assert_eq!(wt.current_triggers().len(), 0);
}

// ===========================================================================
// Scenario 7 — O-1: bare-path poll-source classification (try to defeat it)
// ===========================================================================

#[test]
fn s7_bare_path_on_round_trips_with_leading_slash_and_classifies_as_a_watcher() {
    // The parser fix the Architect ruled required: `ON /mail/inbox` (a bare Token::Path) round-trips
    // WITH the leading slash through the real DDL path, and `WatcherSet::from_state` classifies it
    // as a poll watcher.
    let poll = trigger_def(
        "CREATE TRIGGER archive_new_mail ON /mail/inbox DO UPSERT INTO /archive VALUES (NEW.id)",
    );
    assert_eq!(
        poll.on, "/mail/inbox",
        "a bare-path `on` must round-trip WITH the leading slash (the O-1 fix)"
    );

    let mut state = ServerState::new();
    state.triggers.insert(poll.name.clone(), poll);
    let watchers = WatcherSet::from_state(&state);
    assert_eq!(
        watchers.len(),
        1,
        "the leading-slash `on` classifies as a poll watcher"
    );
    assert_eq!(watchers.watchers[0].source, "/mail/inbox");
}

#[test]
fn s7_quoted_webhook_event_on_is_not_classified_as_a_poll_watcher() {
    // Contrast: a TRIGGER whose `on` is an event-kind label (`webhook`, a bare ident — no leading
    // slash) must NOT be classified as a poll watcher (it dispatches off the bus instead).
    let hook =
        trigger_def("CREATE TRIGGER on_webhook ON webhook DO UPSERT INTO /audit VALUES (NEW.body)");
    assert_eq!(
        hook.on, "webhook",
        "an event-kind `on` carries no leading slash"
    );

    let mut state = ServerState::new();
    state.triggers.insert(hook.name.clone(), hook);
    assert_eq!(
        WatcherSet::from_state(&state).len(),
        0,
        "an event-kind `on` must NOT register a poll watcher"
    );
}

// ===========================================================================
// Scenario 8 — Audit + purity + secret hygiene (canary)
// ===========================================================================

#[test]
fn s8_exactly_one_audit_record_per_fired_plan_with_event_trigger_outcome() {
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "a1",
        &[("body", Value::Text("hi".into()))],
    );
    dispatcher
        .handle(&event, &triggers, &committer, &audit)
        .unwrap();
    assert_eq!(audit.len(), 1);
    // The single record names the event id + trigger + kind (outcome cause), per RFD §6.
    let snap = audit.snapshot();
    let AuditEntry::PlanFired { cause } = &snap[0] else {
        panic!("expected a PlanFired audit entry")
    };
    assert!(cause.contains("trigger:t"), "audit names the trigger");
    assert!(cause.contains("event:inbox#a1"), "audit names the event id");
    assert!(cause.contains("kind:webhook"), "audit names the event kind");
}

#[test]
fn s8_build_and_guard_are_pure_no_commit_until_the_boundary() {
    // A committer that PANICS if reached proves the build + guard path performs no commit for a
    // gated event (the COMMIT is the only effect; a gated event never reaches it).
    struct PanicCommitter;
    impl Committer for PanicCommitter {
        fn commit(&self, _stmt: &cfs_watchtower::Statement) -> Result<FireOutcome, FireError> {
            panic!("commit must not be called for a gated event");
        }
    }
    let trig = trigger_def(
        "CREATE TRIGGER hot ON inbox WHERE NEW.priority > 3 DO UPSERT INTO /log VALUES ('x')",
    );
    let triggers = vec![trig];
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);
    let low = event_with(
        "inbox",
        EventKind::Webhook,
        "p1",
        &[("priority", Value::Int(1))],
    );
    assert_eq!(
        dispatcher
            .handle(&low, &triggers, &PanicCommitter, &audit)
            .unwrap(),
        Dispatched::Gated
    );
    assert_eq!(audit.len(), 0);
}

#[test]
fn s8_signing_secret_never_appears_in_audit_or_event() {
    // Plant a canary signing secret; a signed webhook fires a trigger; assert the secret never
    // appears in the event payload, the dedup_key, or the audit ledger.
    const CANARY: &str = "CANARY-SIGNING-SECRET-do-not-leak";
    let secrets = secrets_with("inbox-hook", CANARY);
    let (bus, mut rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut binding = WebhookBinding::new(secrets, Arc::clone(&bus));
    binding
        .reconcile(&webhook_state("inbox", "/hooks/inbox", "inbox-hook"))
        .unwrap();

    let body = br#"{"subject":"normal"}"#;
    let mut headers = BTreeMap::new();
    headers.insert(
        SIGNATURE_HEADER.to_string(),
        sign_body(CANARY.as_bytes(), body),
    );
    assert_eq!(
        binding.ingest("/hooks/inbox", &headers, body, 2000).status,
        202
    );

    // The enqueued event must not carry the secret anywhere.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let event = rt.block_on(async { rx.recv().await.unwrap() });
    let rendered = format!("{event:?}");
    assert!(
        !rendered.contains(CANARY),
        "the signing secret must never reach the event DTO"
    );
    assert!(
        !event.dedup_key.contains(CANARY),
        "the secret must never reach the dedup_key"
    );

    // Fire it and assert the audit ledger is secret-free.
    let trig =
        trigger_def("CREATE TRIGGER t ON /hooks/inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);
    dispatcher
        .handle(&event, &triggers, &committer, &audit)
        .unwrap();
    assert!(
        !audit_text(&audit).contains(CANARY),
        "the secret must never reach the audit ledger"
    );
}

#[test]
fn s8_payload_canary_never_leaks_into_audit() {
    // A trigger fires on a payload that carries a canary value; the audit record (which names the
    // trigger/event/kind/outcome, not the row contents) must not echo the payload value.
    const CANARY: &str = "PAYLOAD-CANARY-xyz";
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO UPSERT INTO /log VALUES (NEW.body)");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "c1",
        &[("body", Value::Text(CANARY.into()))],
    );
    dispatcher
        .handle(&event, &triggers, &committer, &audit)
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert!(
        !audit_text(&audit).contains(CANARY),
        "the audit ledger records names/ops/outcome counts, never the row payload"
    );
}

// ===========================================================================
// Scenario 9 — Fixture boot: fixtures/watchtower.cfs boots deterministically
// ===========================================================================

#[test]
fn s9_shipped_fixture_boots_and_reconciles_deterministically() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("watchtower.cfs");
    let secrets = secrets_with("inbox-signing", "s");
    let (bus, _rx) = LocalBus::new(16);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let wt = WatchtowerBinding::new(secrets, bus);

    let mut rt = cfs_server::Runtime::new().with_binding(Box::new(wt));
    rt.boot(&fixture)
        .expect("watchtower fixture boots through the COMMIT path");
    let state = rt.snapshot();

    // Deterministic regression target: 1 webhook + 3 triggers; the webhook carries the HANDLE (not
    // the secret); the guarded trigger carries its predicate spec; exactly one poll-source watcher.
    assert_eq!(state.webhooks.len(), 1, "one webhook reconciled");
    assert_eq!(state.triggers.len(), 3, "three triggers");
    assert_eq!(
        state.webhooks.get("inbox_hook").unwrap().secret,
        "inbox-signing",
        "the webhook carries the signing-secret HANDLE, never the secret"
    );
    assert!(
        !state
            .triggers
            .get("on_urgent_webhook")
            .unwrap()
            .predicate
            .as_str()
            .is_empty(),
        "the WHERE-guarded trigger stored its predicate spec"
    );
    let watchers = WatcherSet::from_state(&state);
    assert_eq!(watchers.len(), 1, "one poll-source watcher (/mail/inbox)");
    assert_eq!(watchers.watchers[0].source, "/mail/inbox");
}

// A compile-time touch of EventId so the import is load-bearing (the ack key the bus dedupes on).
#[test]
fn s_event_id_is_the_ack_key() {
    let e = event_with("inbox", EventKind::Webhook, "x", &[]);
    assert_eq!(e.id, EventId::new("inbox#x"));
}
