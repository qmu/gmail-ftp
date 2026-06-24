//! Constructor-owned **internal** integration tests for the t36 deployment host-adapter layer.
//!
//! These run under the `host-daemon` feature (the conversion from `qfs_server::ServerState` lives
//! there; the wasm-clean core derivation is unit-tested in `src/derive.rs`). No live creds, no
//! network: a fixture `config.qfs` is booted into a `ServerState` through the t30 server `Runtime`
//! (parse → lower → COMMIT, all in-memory), then the host-agnostic [`BindingSet`] is derived from
//! it and the `wrangler.toml` is generated.
//!
//! Acceptance coverage:
//!  1. host-agnostic binding-set assertion: the fixture's one ENDPOINT / one JOB EVERY / one
//!     WEBHOOK / one watcher + `/d1`+`/r2`+`/kv` refs → the produced binding set.
//!  2. wrangler.toml golden (Cron expr, Queue, DO class, d1/r2/kv binding names) vs the checked-in
//!     fixture.
//!
//! (MockHost at-least-once idempotency is a unit test in `src/mock.rs`.)

#![cfg(feature = "host-daemon")]
// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use qfs_host::{bindings_from_state, generate_wrangler_toml, NativeStoreKind};
use qfs_server::Runtime;

/// Boot the deployment fixture into a `ServerState` snapshot via the t30 server `Runtime`.
fn boot_fixture() -> qfs_server::ServerState {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("deploy_boot.qfs");
    let mut rt = Runtime::new();
    rt.boot(&path).expect("boot deploy fixture");
    rt.snapshot()
}

#[test]
fn fixture_yields_the_expected_host_agnostic_binding_set() {
    // Acceptance 1: the binding set derived from the fixture has exactly one cause of each kind
    // and three native stores (d1 + r2 + kv), each by NAME only.
    let state = boot_fixture();
    let set = bindings_from_state(&state);

    assert_eq!(set.endpoints.len(), 1, "one ENDPOINT");
    let ep = &set.endpoints[0];
    assert_eq!(ep.name, "events");
    assert_eq!(ep.method, "GET");
    assert_eq!(ep.route, "/events");

    assert_eq!(set.jobs.len(), 1, "one JOB EVERY");
    let job = &set.jobs[0];
    assert_eq!(job.name, "archive");
    assert_eq!(job.every, "6h");
    assert_eq!(job.cron, "0 */6 * * *", "EVERY 6h → Cron Trigger crontab");
    assert_eq!(job.policy.as_deref(), Some("deploypriv"));

    assert_eq!(set.webhooks.len(), 1, "one WEBHOOK");
    let wh = &set.webhooks[0];
    assert_eq!(wh.name, "inbound");
    assert_eq!(wh.route, "/hooks/ingest");
    assert_eq!(wh.queue, "inbound-events", "WEBHOOK → CF Queue name");

    assert_eq!(set.watchers.len(), 1, "one watcher (TRIGGER)");
    let w = &set.watchers[0];
    assert_eq!(w.name, "notify");
    assert_eq!(w.on, "inbox");
    assert_eq!(
        w.cursor_key().as_str(),
        "watcher/notify/cursor",
        "watcher → DO-backed durable cursor key"
    );

    // Three native stores, sorted (D1 < R2 < Kv by enum order), each name-only.
    assert_eq!(
        set.native_stores.len(),
        3,
        "d1 + r2 + kv: {:?}",
        set.native_stores
    );
    let kinds: Vec<NativeStoreKind> = set.native_stores.iter().map(|n| n.kind).collect();
    assert!(kinds.contains(&NativeStoreKind::D1));
    assert!(kinds.contains(&NativeStoreKind::R2));
    assert!(kinds.contains(&NativeStoreKind::Kv));
    let d1 = set
        .native_stores
        .iter()
        .find(|n| n.kind == NativeStoreKind::D1)
        .unwrap();
    assert_eq!(d1.resource, "analytics");
    assert_eq!(d1.binding_name(), "D1_ANALYTICS");
    let r2 = set
        .native_stores
        .iter()
        .find(|n| n.kind == NativeStoreKind::R2)
        .unwrap();
    assert_eq!(r2.resource, "backups");
    assert_eq!(r2.binding_name(), "R2_BACKUPS");
    let kv = set
        .native_stores
        .iter()
        .find(|n| n.kind == NativeStoreKind::Kv)
        .unwrap();
    assert_eq!(kv.resource, "sessions");
    assert_eq!(kv.binding_name(), "KV_SESSIONS");
}

#[test]
fn wrangler_toml_matches_the_checked_in_golden() {
    // Acceptance 2: the generated wrangler.toml (Cron expr, Queue, DO class, d1/r2/kv binding
    // names) byte-matches the checked-in golden fixture. Regenerate with QFS_BLESS_GOLDEN=1.
    let state = boot_fixture();
    let set = bindings_from_state(&state);
    let generated = generate_wrangler_toml("qfs-deploy", &set);

    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("wrangler.golden.toml");

    if std::env::var("QFS_BLESS_GOLDEN").is_ok() {
        std::fs::write(&golden_path, &generated).expect("bless golden");
    }

    let golden = std::fs::read_to_string(&golden_path).expect("read wrangler golden");
    assert_eq!(
        generated, golden,
        "generated wrangler.toml drifted from the checked-in golden \
         (re-bless with QFS_BLESS_GOLDEN=1 if intended)"
    );

    // Sanity: the golden carries the load-bearing deployment facts and NO secret value.
    assert!(
        golden.contains("crons = [\"0 */6 * * *\"]"),
        "Cron expr present"
    );
    assert!(
        golden.contains("class_name = \"WatchtowerState\""),
        "DO class present"
    );
    assert!(
        golden.contains("queue = \"inbound-events\""),
        "Queue present"
    );
    assert!(
        golden.contains("binding = \"D1_ANALYTICS\""),
        "d1 binding name"
    );
    assert!(
        golden.contains("binding = \"R2_BACKUPS\""),
        "r2 binding name"
    );
    assert!(
        golden.contains("binding = \"KV_SESSIONS\""),
        "kv binding name"
    );
    assert!(
        golden.contains("database_id = \"\""),
        "no database_id value embedded"
    );
}
