//! End-to-end **wire** test: drive the real `reqwest` client against a LOCAL
//! loopback HTTP server bound to `127.0.0.1` (an ephemeral OS-assigned port). **No live
//! network, no live credentials** — the server is stood up in-process with a raw
//! `tokio::net::TcpListener` and hand-written HTTP/1.1, the auth token comes from an
//! in-memory secrets store, and the whole exchange stays on the loopback interface.
//!
//! This proves the path the [`crate::client::MockHttpClient`] unit tests stub: the driver
//! builds a request, the `ReqwestClient` puts it on the wire (with the injected auth header),
//! the server's JSON response comes back, and it decodes to rows — committed end-to-end
//! through the t10 interpreter and the sync→async `PlanApplierBridge`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_codec::JsonCodec;
use qfs_driver::Driver;
use qfs_driver_http::{
    rest_apply_driver, AuthStrategy, HttpClient, ReqwestClient, ResourceMap, RestApiConfig,
    RestDriver, RestVerb, SecretRef,
};
use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, Target, VfsPath};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use qfs_secrets::{
    AccountId, CredentialKey, DriverId as SecDriverId, InMemoryStore, Secret, Secrets,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOKEN: &str = "WIRE-TEST-TOKEN-abc123";

/// Stand up a one-shot loopback HTTP server: accept a single connection, read the request
/// headers, assert the `Authorization` header carried our token, and reply with a JSON array
/// of two objects. Returns the bound `http://127.0.0.1:<port>` base URL and the server task's
/// join handle (which yields the request line + auth header it saw).
async fn spawn_loopback() -> (String, tokio::task::JoinHandle<(String, Option<String>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let request_line = req.lines().next().unwrap_or("").to_string();
        let auth = req
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .map(|l| {
                l.split_once(':')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
        let body = br#"[{"id":1,"name":"a"},{"id":2,"name":"b"}]"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        sock.flush().await.unwrap();
        (request_line, auth)
    });
    (base, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_over_real_reqwest_against_loopback_decodes_rows_and_injects_auth() {
    let (base, server) = spawn_loopback().await;

    // Secrets: the bearer token, in-memory (no live credential).
    let store = InMemoryStore::new();
    store
        .put(
            &CredentialKey::new(SecDriverId::new("api"), AccountId::new("work").unwrap()),
            Secret::from(TOKEN),
        )
        .unwrap();
    let secrets: Arc<dyn Secrets> = Arc::new(store);

    let config = RestApiConfig::new(
        base,
        vec![ResourceMap::new("things", vec![RestVerb::Select])],
    )
    .with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("api", "work"),
    })
    .with_header("Accept", "application/json");

    let client: Arc<dyn HttpClient> = Arc::new(ReqwestClient::new(10));
    let driver = RestDriver::new(config, Arc::new(JsonCodec), client, secrets);

    // Register the driver's bridged applier and commit a SELECT (Read) plan end-to-end.
    let bridge = rest_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        Target::new(DriverId::new("rest"), VfsPath::new("/rest/api/things")),
    ));
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("rest"), &EffectKind::Read);
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert!(outcome.is_complete(), "the GET leg applied: {outcome:?}");
    // Two JSON rows decoded from the loopback response.
    let entry = &outcome.ledger[0];
    match &entry.status {
        qfs_runtime::LegStatus::Applied { affected, .. } => assert_eq!(*affected, 2),
        other => panic!("expected Applied, got {other:?}"),
    }

    // The server saw a GET and our bearer token on the wire (auth injection works through the
    // real client too).
    let (request_line, auth) = server.await.unwrap();
    assert!(request_line.starts_with("GET "), "saw: {request_line}");
    assert_eq!(auth.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
}
