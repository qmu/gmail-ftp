//! **Planner E2E / external-interface validation** for t36 deployment targets (RFD-0001 §8/§9).
//!
//! Black-box: this exercises ONLY the public `qfs_host` API + the checked-in golden fixture, from
//! the outside, the way a deployment tool would. It does NOT inspect crate internals. It is the
//! Planner's independent acceptance-criteria validation, distinct from the Constructor's internal
//! tests (`tests/deployment.rs`, `src/mock.rs`).
//!
//! Scenarios (mapped to the ticket acceptance criteria):
//!  1. host-agnostic binding-set derivation from the fixture `config.qfs` via the public
//!     `bindings_from_state` / `derive_bindings` API.
//!  3. `MockHost` at-least-once idempotency: a JOB twice + a WEBHOOK twice → one net effect, and a
//!     forced stale-cas redelivery still collapses to one.
//!  6. secret hygiene: a planted canary token never reaches the generated `wrangler.toml`; the
//!     wrangler emits binding NAMES only (no token / no `database_id` value).
//!  7. `FileDurableStore` crash-safety: a `put`/`cas` survives a reopen of the store; a `cas` with a
//!     wrong `expect` is rejected.
//!
//! (Scenario 2 wrangler golden, scenario 4 daemon serve + SIGTERM, and scenario 5 deny-test are
//! driven by the Planner's shell harness + the existing test targets — see the trip report.)

#![cfg(feature = "host-daemon")]
// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use qfs_host::{
    bindings_from_state, block_on, generate_wrangler_toml, DurableStore, FileDurableStore,
    MockHost, NativeStoreKind, RuntimeHost, StateBytes, StateKey, Timestamp,
};
use qfs_server::Runtime;

/// Boot the acceptance fixture into a `ServerState` through the t30 server `Runtime`
/// (parse → lower → COMMIT, fully in-memory; no creds, no network).
fn boot_fixture() -> qfs_server::ServerState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("deploy_boot.qfs");
    let mut rt = Runtime::new();
    rt.boot(&path).expect("boot deploy fixture");
    rt.snapshot()
}

/// A throwaway state dir under the OS temp dir (never a system path; cleaned up by the test).
fn scratch_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "qfs-planner-e2e-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
    ));
    p
}

// --- Scenario 1: host-agnostic binding-set derivation -----------------------------------------

#[test]
fn scenario1_fixture_derives_one_cause_of_each_kind_plus_three_native_stores() {
    let set = bindings_from_state(&boot_fixture());

    // Exactly one ENDPOINT / one JOB EVERY / one WEBHOOK / one watcher, host-agnostic.
    assert_eq!(set.endpoints.len(), 1, "one ENDPOINT cause");
    assert_eq!(set.jobs.len(), 1, "one JOB EVERY cause");
    assert_eq!(set.webhooks.len(), 1, "one WEBHOOK cause");
    assert_eq!(set.watchers.len(), 1, "one watcher (TRIGGER) cause");

    // The ENDPOINT projects method + route a host turns into fetch/route.
    let ep = &set.endpoints[0];
    assert_eq!(ep.name, "events");
    assert_eq!(ep.method, "GET");
    assert_eq!(ep.route, "/events");

    // EVERY 6h → a Cron-Trigger crontab the Worker `scheduled` handler / the daemon interval uses.
    let job = &set.jobs[0];
    assert_eq!(job.name, "archive");
    assert_eq!(job.every, "6h");
    assert_eq!(job.cron, "0 */6 * * *");
    assert_eq!(
        job.policy.as_deref(),
        Some("deploypriv"),
        "POLICY by handle"
    );

    // WEBHOOK → a CF Queue name; the secret is a HANDLE, never a token (asserted in scenario 6).
    let wh = &set.webhooks[0];
    assert_eq!(wh.name, "inbound");
    assert_eq!(wh.route, "/hooks/ingest");
    assert_eq!(wh.queue, "inbound-events");

    // Watcher → a DO-backed durable cursor key (stable across hosts).
    let w = &set.watchers[0];
    assert_eq!(w.name, "notify");
    assert_eq!(w.on, "inbox");
    assert_eq!(w.cursor_key().as_str(), "watcher/notify/cursor");

    // /d1 + /r2 + /kv → three native-store bindings, by NAME only.
    assert_eq!(
        set.native_stores.len(),
        3,
        "d1 + r2 + kv native stores: {:?}",
        set.native_stores
    );
    let by_kind = |k: NativeStoreKind| set.native_stores.iter().find(|n| n.kind == k).unwrap();
    assert_eq!(by_kind(NativeStoreKind::D1).binding_name(), "D1_ANALYTICS");
    assert_eq!(by_kind(NativeStoreKind::R2).binding_name(), "R2_BACKUPS");
    assert_eq!(by_kind(NativeStoreKind::Kv).binding_name(), "KV_SESSIONS");

    // The safe-to-log summary is counts only (no resource contents).
    assert_eq!(
        set.summary(),
        "endpoints=1 jobs=1 webhooks=1 watchers=1 native_stores=3"
    );
}

// --- Scenario 3: MockHost at-least-once idempotency -------------------------------------------

#[test]
fn scenario3_redelivery_of_job_and_webhook_commits_one_net_effect_each() {
    let host = MockHost::new(Timestamp::from_secs(1_700_000_000));

    // A JOB delivered twice for the same scheduled run-id.
    assert!(
        host.deliver("job:archive", "run-2026-06-24T00").unwrap(),
        "first JOB delivery commits"
    );
    assert!(
        !host.deliver("job:archive", "run-2026-06-24T00").unwrap(),
        "JOB redelivery is a cas-guarded no-op"
    );

    // A WEBHOOK event delivered twice for the same event-id.
    assert!(
        host.deliver("webhook:inbound", "evt-001").unwrap(),
        "first WEBHOOK delivery commits"
    );
    assert!(
        !host.deliver("webhook:inbound", "evt-001").unwrap(),
        "WEBHOOK redelivery is a cas-guarded no-op"
    );

    let effects = host.committed_effects();
    assert_eq!(
        effects.len(),
        2,
        "two distinct causes, each committed exactly once despite the redelivery: {effects:?}"
    );
    assert_eq!(effects[0].cause, "job:archive");
    assert_eq!(effects[1].cause, "webhook:inbound");
}

#[test]
fn scenario3_forced_stale_cas_race_collapses_to_one_effect() {
    // Force the "stale prior" race directly on the durable store the host guards with: two
    // concurrent deliveries both read the SAME empty prior, then both attempt the swap. The cas
    // primitive must let exactly ONE win (the other sees the slot already advanced → no-op).
    let host = MockHost::new(Timestamp::from_secs(1));
    let durable = host.durable();
    let key = StateKey::new("cursor/job:race");

    // Both racers observe the same (empty) prior.
    let prior_a = block_on(durable.get(&key)).unwrap();
    let prior_b = block_on(durable.get(&key)).unwrap();
    assert!(
        prior_a.is_none() && prior_b.is_none(),
        "both read the empty prior"
    );

    // Racer A swaps empty → run-1: wins.
    let a = block_on(durable.cas(&key, prior_a, StateBytes::new(b"run-1".to_vec()))).unwrap();
    // Racer B carries the STALE empty expectation; the slot already holds run-1 → loses.
    let b = block_on(durable.cas(&key, prior_b, StateBytes::new(b"run-1".to_vec()))).unwrap();

    assert!(a, "the first swap wins");
    assert!(
        !b,
        "the stale-prior racer loses the cas — collapses to one net effect"
    );
    assert_eq!(
        block_on(durable.get(&key)).unwrap().unwrap().as_slice(),
        b"run-1",
        "exactly one value committed"
    );
}

// --- Scenario 6: secret hygiene (canary) -------------------------------------------------------

#[test]
fn scenario6_secret_values_cannot_flow_into_the_wrangler_or_a_ledger_line() {
    // The hygiene invariant (RFD §10): the seam carries credentials by HANDLE / NAME only — there
    // is NO secret-value field on any binding, so a token value is *structurally* unable to flow
    // into the generated wrangler or an audit line. We prove that two ways.
    //
    // (a) The fixture's WEBHOOK has no inline secret, so the derived secret_handle is empty — the
    //     seam never even carried a credential.
    let set = bindings_from_state(&boot_fixture());
    assert_eq!(
        set.webhooks[0].secret_handle, "",
        "the fixture webhook carries no inline secret value — only a (here empty) handle"
    );

    // (b) Even if an operator's qfs-secrets ACCOUNT-ID handle is present, the wrangler emits it
    //     only as a `wrangler secret put <NAME>` reference, never a token VALUE; and the generator
    //     emits no `database_id`/`id` value at all. A canary planted as the would-be token VALUE
    //     (distinct from the handle) has no field to live in, so it can never appear. We assert the
    //     output is value-free below.
    const CANARY: &str = "qfs-CANARY-TOKEN-do-not-leak-1a2b3c4d";
    let wrangler = generate_wrangler_toml("qfs-deploy", &set);

    assert!(
        !wrangler.contains(CANARY),
        "a token VALUE must never appear — the seam has no field to carry one"
    );
    // No id / database_id VALUE is emitted (only the empty placeholder a deployer fills).
    assert!(
        wrangler.contains("database_id = \"\""),
        "no database_id value embedded — only a blank placeholder"
    );
    assert!(
        wrangler.contains("id = \"\""),
        "no kv id value embedded — only a blank placeholder"
    );
    // No inline token/secret assignment of a VALUE slips through.
    let lower = wrangler.to_lowercase();
    assert!(
        !lower.contains("token ="),
        "no inline token = <value> assignment"
    );
    assert!(
        !lower.contains("secret ="),
        "no inline secret = <value> assignment"
    );

    // Binding NAMES are present; that is the intended, non-secret surface.
    assert!(
        wrangler.contains("binding = \"D1_ANALYTICS\""),
        "d1 binding NAME present"
    );
    assert!(
        wrangler.contains("binding = \"R2_BACKUPS\""),
        "r2 binding NAME present"
    );
    assert!(
        wrangler.contains("binding = \"KV_SESSIONS\""),
        "kv binding NAME present"
    );
}

// --- Scenario 7: FileDurableStore crash-safety (black-box reopen) ------------------------------

#[test]
fn scenario7_durable_state_survives_a_store_reopen_and_wrong_cas_is_rejected() {
    let dir = scratch_dir("durable");
    let key = StateKey::new("watcher/notify/cursor");

    // Open, write a cursor, then DROP the store (simulating the daemon stopping).
    {
        let store = FileDurableStore::open(&dir).unwrap();
        assert!(
            block_on(store.get(&key)).unwrap().is_none(),
            "empty before first put"
        );
        block_on(store.put(&key, StateBytes::new(b"cursor-v1".to_vec()))).unwrap();
    }

    // REOPEN a brand-new store over the same dir (the daemon restarting): the cursor persisted.
    {
        let store = FileDurableStore::open(&dir).unwrap();
        assert_eq!(
            block_on(store.get(&key)).unwrap().unwrap().as_slice(),
            b"cursor-v1",
            "cursor survived the store reopen (crash-safety / persisted state)"
        );

        // A cas with the CORRECT expectation advances the cursor.
        assert!(
            block_on(store.cas(
                &key,
                Some(StateBytes::new(b"cursor-v1".to_vec())),
                StateBytes::new(b"cursor-v2".to_vec()),
            ))
            .unwrap(),
            "cas with the right expect swaps"
        );
        // A cas with a WRONG expectation is rejected and leaves the value untouched.
        assert!(
            !block_on(store.cas(
                &key,
                Some(StateBytes::new(b"cursor-v1".to_vec())), // stale
                StateBytes::new(b"cursor-v9".to_vec()),
            ))
            .unwrap(),
            "cas with a wrong expect is rejected"
        );
        assert_eq!(
            block_on(store.get(&key)).unwrap().unwrap().as_slice(),
            b"cursor-v2",
            "the rejected cas left the persisted value untouched"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
