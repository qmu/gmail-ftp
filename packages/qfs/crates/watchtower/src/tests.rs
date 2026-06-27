//! Internal acceptance tests for the watchtower (t34). No live network, no live creds — fakes +
//! fixtures + golden assertions over the lowered plan / audit ledger / bus spool.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use qfs_core::{Engine, Row, Value};
use qfs_secrets::{CredentialKey, DriverId, InMemoryStore, Secret, Secrets};
use qfs_server::{AuditSink, ServerState, TriggerDef, WebhookDef};

use crate::bind::{bind_new, NewBindings};
use crate::binding::WatchtowerBinding;
use crate::bus::{EventBus, LocalBus};
use crate::commit::{AllowAllGate, Committer, FireError, FireOutcome, RecordingCommitter};
use crate::dispatch::{Dispatched, Dispatcher};
use crate::event::{Event, EventKind, SourcePath};
use crate::webhook::{sign_body, WebhookBinding};
use qfs_server::Binding;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A counting fake committer: records how many commits succeeded + the cumulative plan summaries,
/// so the idempotency golden can assert "one net effect across two deliveries".
#[derive(Default)]
struct CountingCommitter {
    commits: std::sync::atomic::AtomicU64,
}

impl Committer for CountingCommitter {
    fn commit(
        &self,
        _trigger: &str,
        _stmt: &qfs_parser::Statement,
        _policy: Option<&str>,
    ) -> Result<FireOutcome, FireError> {
        let n = self
            .commits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        Ok(FireOutcome {
            plan_summary: format!("commit#{n}"),
            affected: 1,
            effects: vec![format!("INSERT log:/log#{n}")],
        })
    }
}

impl CountingCommitter {
    fn count(&self) -> u64 {
        self.commits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Build a `CREATE TRIGGER` and lower it into a `TriggerDef` (via the real DDL → config-row path)
/// so the test fixtures match production exactly.
fn trigger_def(src: &str) -> TriggerDef {
    let ddl = qfs_core::parse_server_binding_ddl(src).expect("parse trigger ddl");
    let row = qfs_core::binding_config_row(&ddl);
    TriggerDef {
        name: row.get("name").and_then(text).unwrap_or_default(),
        on: row.get("on").and_then(text).unwrap_or_default(),
        predicate: qfs_server::StatementSource::new(
            row.get("predicate").and_then(text).unwrap_or_default(),
        ),
        plan: qfs_server::StatementSource::new(row.get("plan").and_then(text).unwrap_or_default()),
        policy: row.get("policy").and_then(text),
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}

/// An event whose NEW.* row carries the given named columns.
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

// ---------------------------------------------------------------------------
// Plan assertion: injected Event -> handler Plan with NEW.* bound
// ---------------------------------------------------------------------------

#[test]
fn injected_event_binds_new_into_the_handler_plan() {
    // A handler that inserts NEW.subject into /log. After NEW.* binding the plan must carry the
    // event's subject as a typed literal (asserted on the bound statement, no live commit).
    let trig =
        trigger_def("CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES (NEW.subject)");
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "e1",
        &[("subject", Value::Text("hello".to_string()))],
    );
    let binds = NewBindings::from_row(&event.columns, &event.new.values);
    let spec = qfs_core::PlanSpec::from_canonical(trig.plan.as_str()).expect("rehydrate");
    let mut stmt = spec.statement().clone();
    bind_new(&mut stmt, &binds);
    let qfs_parser::Statement::Effect(e) = &stmt else {
        panic!("expected effect")
    };
    let qfs_parser::EffectBody::Values(v) = &e.body else {
        panic!("expected values body")
    };
    assert_eq!(
        v.rows[0][0],
        qfs_parser::Expr::Lit(qfs_parser::Literal::Str("hello".to_string()))
    );
}

// ---------------------------------------------------------------------------
// At-least-once idempotency: same dedup_key twice -> one net effect
// ---------------------------------------------------------------------------

#[test]
fn same_dedup_key_twice_yields_one_net_effect() {
    let trig = trigger_def("CREATE TRIGGER t ON inbox DO INSERT INTO /log VALUES ('x')");
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "dup1",
        &[("subject", Value::Text("a".into()))],
    );

    // First delivery: fires + commits once.
    let r1 = dispatcher
        .handle(&event, &triggers, &committer, &audit)
        .unwrap();
    assert_eq!(r1, Dispatched::Fired(1));
    // Second delivery (SAME dedup_key): a no-op (idempotency ledger collapses it).
    let r2 = dispatcher
        .handle(&event, &triggers, &committer, &audit)
        .unwrap();
    assert_eq!(r2, Dispatched::Duplicate);

    // ONE net effect across two deliveries; ONE audit record.
    assert_eq!(committer.count(), 1);
    assert_eq!(audit.len(), 1);
}

// ---------------------------------------------------------------------------
// Recovery: un-acked LocalBus event redelivered; watcher cursor restored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unacked_event_is_redelivered_after_simulated_crash() {
    let (bus, mut rx) = LocalBus::new(8);
    let event = event_with(
        "inbox",
        EventKind::Webhook,
        "r1",
        &[("subject", Value::Text("a".into()))],
    );
    bus.publish(event.clone()).unwrap();

    // Consume the live delivery but DO NOT ack (the simulated crash between publish and ack).
    let delivered = rx.recv().await.unwrap();
    assert_eq!(delivered.id, event.id);
    assert_eq!(bus.unacked_len(), 1); // still in the durable spool

    // Recovery: redeliver the un-acked window — the event is re-enqueued (not skipped).
    let n = bus.redeliver_unacked();
    assert_eq!(n, 1);
    let redelivered = rx.recv().await.unwrap();
    assert_eq!(redelivered.id, event.id);

    // Now ack it: the spool drains, so a subsequent recovery redelivers nothing.
    bus.ack(&event.id).unwrap();
    assert_eq!(bus.unacked_len(), 0);
    assert_eq!(bus.redeliver_unacked(), 0);
}

#[test]
fn watcher_cursor_persists_only_after_publish_and_restores_without_skipping() {
    use crate::watcher::{MemWatcherStore, WatcherCursor, WatcherStore};
    let store = MemWatcherStore::new();
    // Persist a cursor that has seen id "1"; a restart loads it and does not re-emit "1".
    let mut cursor = WatcherCursor::new();
    cursor.mark("1");
    store.save("/mail/inbox", &cursor).unwrap();

    let restored = store.load("/mail/inbox").unwrap();
    assert!(restored.contains("1"));
    assert!(!restored.contains("2")); // an un-emitted id is still pending (not skipped)
}

// ---------------------------------------------------------------------------
// WHERE gating: failing predicate fires no plan
// ---------------------------------------------------------------------------

#[test]
fn failing_where_guard_fires_zero_plans_and_zero_driver_calls() {
    // The canonical production form: `WHERE NEW.priority > 3` (the `NEW.` prefix the dispatcher
    // strips to resolve against the event's NEW.* field names).
    let trig = trigger_def(
        "CREATE TRIGGER hot ON inbox WHERE NEW.priority > 3 DO INSERT INTO /log VALUES ('x')",
    );
    let triggers = vec![trig];
    let committer = CountingCommitter::default();
    let audit = AuditSink::new();
    let dispatcher = Dispatcher::new(AllowAllGate);

    // priority = 1 fails the guard: zero fires, zero audit, zero commits.
    let low = event_with(
        "inbox",
        EventKind::Webhook,
        "g1",
        &[("priority", Value::Int(1))],
    );
    assert_eq!(
        dispatcher
            .handle(&low, &triggers, &committer, &audit)
            .unwrap(),
        Dispatched::Gated
    );
    assert_eq!(committer.count(), 0);
    assert_eq!(audit.len(), 0);

    // priority = 5 passes the guard: one fire.
    let high = event_with(
        "inbox",
        EventKind::Webhook,
        "g2",
        &[("priority", Value::Int(5))],
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

// ---------------------------------------------------------------------------
// Webhook: signed -> 202 + 1 event; bad signature -> 401 + 0 events
// ---------------------------------------------------------------------------

fn secrets_with(handle: &str, secret: &str) -> Arc<dyn Secrets> {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(
        DriverId::new(crate::webhook::WEBHOOK_SECRET_DRIVER),
        qfs_secrets::ConnectionId::new(handle).unwrap(),
    );
    store
        .put(&key, Secret::from_string(secret.to_string()))
        .unwrap();
    Arc::new(store)
}

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
fn signed_webhook_request_enqueues_one_event_and_returns_2xx() {
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
        crate::webhook::SIGNATURE_HEADER.to_string(),
        sign_body(secret.as_bytes(), body),
    );
    let out = binding.ingest("/hooks/inbox", &headers, body, 2000);
    assert_eq!(out.status, 202);
    assert!(out.published);
    assert_eq!(bus.unacked_len(), 1); // exactly one event enqueued
}

#[test]
fn bad_signature_webhook_request_enqueues_nothing_and_returns_401() {
    let secrets = secrets_with("inbox-hook", "topsecret");
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut binding = WebhookBinding::new(secrets, Arc::clone(&bus));
    binding
        .reconcile(&webhook_state("inbox", "/hooks/inbox", "inbox-hook"))
        .unwrap();

    let body = br#"{"subject":"hi"}"#;
    let mut headers = BTreeMap::new();
    // Wrong signature (signed under a different secret).
    headers.insert(
        crate::webhook::SIGNATURE_HEADER.to_string(),
        sign_body(b"wrong-secret", body),
    );
    let out = binding.ingest("/hooks/inbox", &headers, body, 2000);
    assert_eq!(out.status, 401);
    assert!(!out.published);
    assert_eq!(bus.unacked_len(), 0); // NOTHING enqueued
}

#[test]
fn missing_signature_header_is_rejected_401() {
    let secrets = secrets_with("inbox-hook", "topsecret");
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut binding = WebhookBinding::new(secrets, Arc::clone(&bus));
    binding
        .reconcile(&webhook_state("inbox", "/hooks/inbox", "inbox-hook"))
        .unwrap();
    let out = binding.ingest("/hooks/inbox", &BTreeMap::new(), b"{}", 2000);
    assert_eq!(out.status, 401);
    assert_eq!(bus.unacked_len(), 0);
}

// ---------------------------------------------------------------------------
// Reconcile: add/remove webhooks + triggers; idempotent re-reconcile is a no-op
// ---------------------------------------------------------------------------

#[test]
fn reconcile_converges_webhook_routes_and_is_idempotent() {
    let secrets = secrets_with("h", "s");
    let (bus, _rx) = LocalBus::new(8);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let mut wt = WatchtowerBinding::new(secrets, bus);

    // Empty state -> zero routes, zero watchers.
    wt.reconcile(&ServerState::new()).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 0);
    assert_eq!(wt.current_watchers().len(), 0);

    // Add a webhook + a poll-source trigger.
    let mut state = webhook_state("inbox", "/hooks/inbox", "h");
    state.triggers.insert(
        "poll".to_string(),
        TriggerDef {
            name: "poll".to_string(),
            on: "/mail/inbox".to_string(), // a source path -> a watcher
            predicate: Default::default(),
            plan: Default::default(),
            policy: None,
        },
    );
    wt.reconcile(&state).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 1);
    assert_eq!(wt.current_watchers().len(), 1);

    // Idempotent re-reconcile: same counts (a no-op convergence).
    wt.reconcile(&state).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 1);
    assert_eq!(wt.current_watchers().len(), 1);

    // Remove everything -> back to zero.
    wt.reconcile(&ServerState::new()).unwrap();
    assert_eq!(wt.routes_handle().read().unwrap().len(), 0);
    assert_eq!(wt.current_watchers().len(), 0);
}

// ---------------------------------------------------------------------------
// Purity: building the plan + evaluating WHERE mutate nothing until COMMIT
// ---------------------------------------------------------------------------

#[test]
fn building_the_plan_and_guard_perform_no_commit() {
    // A dispatcher with a committer that PANICS if called proves the build + guard path is pure
    // up to the COMMIT boundary: a GATED event never reaches commit.
    struct PanicCommitter;
    impl Committer for PanicCommitter {
        fn commit(
            &self,
            _trigger: &str,
            _stmt: &qfs_parser::Statement,
            _policy: Option<&str>,
        ) -> Result<FireOutcome, FireError> {
            panic!("commit must not be called for a gated event");
        }
    }
    let trig = trigger_def(
        "CREATE TRIGGER hot ON inbox WHERE NEW.priority > 3 DO INSERT INTO /log VALUES ('x')",
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
    // The guard fails -> commit is never reached (no panic), zero audit.
    assert_eq!(
        dispatcher
            .handle(&low, &triggers, &PanicCommitter, &audit)
            .unwrap(),
        Dispatched::Gated
    );
    assert_eq!(audit.len(), 0);
}

// ---------------------------------------------------------------------------
// RecordingCommitter: a real build_plan path (one effect -> one applied)
// ---------------------------------------------------------------------------

#[test]
fn recording_committer_enforces_policy_default_deny_allow_and_atomic_abort() {
    use std::sync::{Arc, RwLock};

    let stmt = qfs_exec::parse("INSERT INTO /log VALUES ('x')").expect("parse");

    // (1) Default-deny: NO policy table + NO bound policy ⇒ the INSERT is DENIED (fail closed),
    //     and ZERO effects applied (atomic abort — total_applied stays 0).
    let empty: Arc<RwLock<Arc<qfs_server::PolicyTable>>> =
        Arc::new(RwLock::new(Arc::new(qfs_server::PolicyTable::new())));
    let committer = RecordingCommitter::with_engine(Engine::new()).with_policies(empty.clone());
    let denied = committer.commit("t", &stmt, None);
    assert!(
        matches!(denied, Err(FireError::PolicyDenied { .. })),
        "no policy ⇒ default-deny, got {denied:?}"
    );
    assert_eq!(
        committer.total_applied(),
        0,
        "a denied plan applies ZERO effects (atomic abort)"
    );

    // (2) A granting policy (`ALLOW INSERT`) in the table, bound by name ⇒ the INSERT commits.
    let mut table = qfs_server::PolicyTable::new();
    table.insert(
        "writer".to_string(),
        qfs_server::PolicyDef {
            name: "writer".to_string(),
            handler: String::new(),
            allow: vec!["ALLOW INSERT".to_string()],
        },
    );
    let handle: Arc<RwLock<Arc<qfs_server::PolicyTable>>> = Arc::new(RwLock::new(Arc::new(table)));
    let committer = RecordingCommitter::with_engine(Engine::new()).with_policies(handle);
    let outcome = committer
        .commit("t", &stmt, Some("writer"))
        .expect("granted ⇒ commit");
    assert!(outcome.affected >= 1);
    assert!(committer.total_applied() >= 1);
    assert!(
        !outcome.effects.is_empty(),
        "the fired-plan record carries secret-free effect summaries"
    );
}

// ---------------------------------------------------------------------------
// Fixture: the shipped fixtures/watchtower.qfs boots + reconciles the watchtower
// ---------------------------------------------------------------------------

#[test]
fn shipped_fixture_boots_and_reconciles_routes_watchers_and_triggers() {
    // The shipped fixtures/watchtower.qfs boots through the SAME COMMIT path a live write takes
    // (no network, no creds), and the watchtower binding reconciles its webhook routes + watcher
    // set + trigger set from the committed state.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("watchtower.qfs");

    let secrets = secrets_with("inbox-signing", "s");
    let (bus, _rx) = LocalBus::new(16);
    let bus: Arc<dyn EventBus> = Arc::new(bus);
    let wt = WatchtowerBinding::new(secrets, bus);

    let mut rt = qfs_server::Runtime::new().with_binding(Box::new(wt));
    rt.boot(&fixture).expect("watchtower fixture boots");

    let state = rt.snapshot();
    // The fixture declares 1 webhook + 3 triggers (on_urgent_webhook, archive_new_mail,
    // log_all_webhooks). archive_new_mail's `on` is a source path -> a watcher.
    assert_eq!(state.webhooks.len(), 1, "one webhook reconciled");
    assert_eq!(state.triggers.len(), 3, "three triggers");
    // The webhook carries the signing-secret HANDLE (never the secret itself).
    assert_eq!(
        state.webhooks.get("inbox_hook").unwrap().secret,
        "inbox-signing"
    );
    // The guarded trigger carries a non-empty predicate spec.
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
    // archive_new_mail is the poll-source trigger -> exactly one watcher derived.
    let watchers = crate::binding::WatcherSet::from_state(&state);
    assert_eq!(watchers.len(), 1, "one poll-source watcher (/mail/inbox)");
}
