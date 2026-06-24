//! In-crate unit + integration tests for the generic HTTP/REST driver. **No live network**:
//! every test points the driver at the in-memory [`MockHttpClient`] (scripted responses,
//! recorded requests) — asserting the exact request shape the driver built and the response
//! decoding — except the single end-to-end wire test in `tests/wire.rs`, which stands up a
//! LOCAL loopback server on `127.0.0.1` and drives the real `reqwest` client. No live
//! credentials: auth secrets come from a `qfs_secrets::InMemoryStore`.

use super::*;
use qfs_codec::JsonCodec;
use qfs_driver::{check_capability, Archetype, Driver, Path, Verb};
use qfs_plan::{Affected, EffectKind, EffectNode, NodeId, Target, VfsPath};
use qfs_runtime::SharedApplier;
use qfs_secrets::{
    AccountId, CredentialKey, DriverId as SecDriverId, InMemoryStore, Secret, Secrets,
};

/// A planted token, unmistakable if it ever surfaces in a Debug/log surface.
const PLANTED_TOKEN: &str = "PLANTED-BEARER-TOKEN-9f8e7d6c5b4a";

/// An empty secrets store (no credentials) for the no-auth path.
fn empty_secrets() -> Arc<dyn Secrets> {
    Arc::new(InMemoryStore::new())
}

/// A secrets store holding `PLANTED_TOKEN` under `(github, work)` — the auth path.
fn secrets_with_token() -> Arc<dyn Secrets> {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(SecDriverId::new("github"), AccountId::new("work").unwrap());
    store.put(&key, Secret::from(PLANTED_TOKEN)).unwrap();
    Arc::new(store)
}

/// A JSON success response body of two objects.
fn json_two_things() -> HttpResponse {
    HttpResponse::new(
        200,
        br#"[{"id":1,"name":"a"},{"id":2,"name":"b"}]"#.to_vec(),
    )
    .header("content-type", "application/json")
}

/// A config with a `things` resource supporting all four verbs, no auth.
fn things_config() -> RestApiConfig {
    RestApiConfig::new(
        "https://api.example.com/v1",
        vec![ResourceMap::new(
            "things",
            vec![
                RestVerb::Select,
                RestVerb::Insert,
                RestVerb::Upsert,
                RestVerb::Remove,
            ],
        )
        .with_id_field("id")],
    )
    .with_header("Accept", "application/json")
}

/// Build a driver over a mock client returning `responses` (queued FIFO).
fn driver_with(
    config: RestApiConfig,
    mock: MockHttpClient,
    secrets: Arc<dyn Secrets>,
) -> RestDriver {
    let client: Arc<dyn HttpClient> = Arc::new(mock);
    RestDriver::new(config, Arc::new(JsonCodec), client, secrets)
}

/// Reach the mock back out of a built driver is awkward (it is behind an Arc<dyn>); tests that
/// inspect recorded requests keep their own Arc<MockHttpClient> and build the driver from it.
fn driver_from_mock(
    config: RestApiConfig,
    mock: Arc<MockHttpClient>,
    secrets: Arc<dyn Secrets>,
) -> RestDriver {
    let client: Arc<dyn HttpClient> = mock;
    RestDriver::new(config, Arc::new(JsonCodec), client, secrets)
}

fn rest_target(path: &str) -> Target {
    Target::new(qfs_plan::DriverId::new("rest"), VfsPath::new(path))
}

// ---------------------------------------------------------------------------
// Introspection: describe / capabilities / parse-time gate
// ---------------------------------------------------------------------------

#[test]
fn describe_reports_relational_table_with_open_json_schema() {
    let d = driver_with(things_config(), MockHttpClient::new(), empty_secrets());
    assert_eq!(d.mount(), "/rest");
    assert_eq!(d.id(), qfs_plan::DriverId::new("rest"));
    let desc = d.describe(&Path::new("/rest/example/things")).unwrap();
    assert_eq!(desc.archetype, Archetype::RelationalTable);
    // JSON is dynamic — an open `value` json column, not invented typed columns (RFD §4).
    assert_eq!(desc.schema.columns.len(), 1);
    assert_eq!(desc.schema.columns[0].name, "value");
}

#[test]
fn capabilities_reflect_declared_resource_verbs() {
    let d = driver_with(things_config(), MockHttpClient::new(), empty_secrets());
    let p = Path::new("/rest/example/things");
    for v in [Verb::Select, Verb::Insert, Verb::Upsert, Verb::Remove] {
        assert!(
            check_capability(&d, &p, v).is_ok(),
            "{v:?} should be allowed"
        );
    }
    // UPDATE (PATCH) is out of scope — rejected at the parse-time gate.
    let err = check_capability(&d, &p, Verb::Update).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
}

#[test]
fn unconfigured_resource_rejects_every_verb_at_parse_time() {
    // A read-only resource: only SELECT declared.
    let config = RestApiConfig::new(
        "https://api.example.com",
        vec![ResourceMap::new("readonly", vec![RestVerb::Select])],
    );
    let d = driver_with(config, MockHttpClient::new(), empty_secrets());
    let p = Path::new("/rest/example/readonly");
    assert!(check_capability(&d, &p, Verb::Select).is_ok());
    // INSERT is not declared for this resource → structured rejection BEFORE any plan/IO.
    let err = check_capability(&d, &p, Verb::Insert).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    match err {
        qfs_driver::CfsError::UnsupportedVerb { supported, .. } => {
            assert_eq!(supported, vec!["SELECT"]);
        }
        other => panic!("expected UnsupportedVerb, got {other:?}"),
    }
    // A path naming no configured resource → empty caps, everything denied.
    let unknown = Path::new("/rest/example/ghost");
    assert!(check_capability(&d, &unknown, Verb::Select).is_err());
}

// ---------------------------------------------------------------------------
// Verb → method mapping + request shape (plan assertions, no live creds)
// ---------------------------------------------------------------------------

#[test]
fn select_builds_get_to_base_plus_resource() {
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());

    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(out.affected, 2, "two JSON rows decoded");

    let reqs = mock.recorded();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, HttpMethod::Get);
    assert_eq!(reqs[0].url, "https://api.example.com/v1/things");
    // Config default header is present.
    assert_eq!(reqs[0].header_value("Accept"), Some("application/json"));
    assert!(reqs[0].body.is_none());
}

#[test]
fn insert_builds_post_upsert_put_remove_delete_with_irreversible() {
    // INSERT → POST
    let mock =
        Arc::new(MockHttpClient::new().with_response(HttpResponse::new(201, b"{}".to_vec())));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let body = http_body_args(br#"{"name":"x"}"#);
    let insert = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        rest_target("/rest/example/things"),
    )
    .with_args(body.clone());
    d.rest_applier().apply_shared(&insert).unwrap();
    assert_eq!(mock.recorded()[0].method, HttpMethod::Post);
    assert_eq!(
        mock.recorded()[0].body.as_deref(),
        Some(&br#"{"name":"x"}"#[..])
    );

    // UPSERT → PUT
    let mock =
        Arc::new(MockHttpClient::new().with_response(HttpResponse::new(200, b"{}".to_vec())));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let upsert = EffectNode::new(
        NodeId(1),
        EffectKind::Upsert,
        rest_target("/rest/example/things"),
    )
    .with_args(body);
    d.rest_applier().apply_shared(&upsert).unwrap();
    assert_eq!(mock.recorded()[0].method, HttpMethod::Put);

    // REMOVE → DELETE, and the node is inherently irreversible.
    let mock =
        Arc::new(MockHttpClient::new().with_response(HttpResponse::new(204, b"{}".to_vec())));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let remove = EffectNode::new(
        NodeId(2),
        EffectKind::Remove,
        rest_target("/rest/example/things"),
    );
    assert!(remove.irreversible, "REMOVE is inherently irreversible");
    let effect = HttpEffect::from_node(&remove).unwrap();
    assert!(effect.irreversible);
    d.rest_applier().apply_shared(&remove).unwrap();
    assert_eq!(mock.recorded()[0].method, HttpMethod::Delete);
}

#[test]
fn update_and_call_are_terminal_decode_failures() {
    let update = EffectNode::new(
        NodeId(0),
        EffectKind::Update,
        rest_target("/rest/example/things"),
    );
    assert!(HttpEffect::from_node(&update).is_err());
    let call = EffectNode::new(
        NodeId(1),
        EffectKind::Call(qfs_plan::ProcId::new("x.y")),
        rest_target("/rest/example/things"),
    );
    assert!(HttpEffect::from_node(&call).is_err());
}

/// Build a body-carrying RowBatch under the `__http_body` column the effect decoder reads.
fn http_body_args(body: &[u8]) -> RowBatch {
    RowBatch::new(
        Schema::new(vec![Column::new("__http_body", ColumnType::Bytes, false)]),
        vec![Row::new(vec![Value::Bytes(body.to_vec())])],
    )
}

// ---------------------------------------------------------------------------
// Auth via secrets + redaction (never log the token)
// ---------------------------------------------------------------------------

#[test]
fn bearer_auth_header_is_injected_from_a_secret() {
    let config = things_config().with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("github", "work"),
    });
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(config, Arc::clone(&mock), secrets_with_token());

    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    d.rest_applier().apply_shared(&node).unwrap();

    let req = &mock.recorded()[0];
    // The resolved token IS on the wire (functional correctness).
    assert_eq!(
        req.header_value("Authorization"),
        Some(format!("Bearer {PLANTED_TOKEN}").as_str())
    );
}

#[test]
fn custom_header_auth_is_injected_from_a_secret() {
    let config = things_config().with_auth(AuthStrategy::Header {
        name: "X-Api-Key".to_string(),
        secret_ref: SecretRef::new("github", "work"),
    });
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(config, Arc::clone(&mock), secrets_with_token());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(
        mock.recorded()[0].header_value("X-Api-Key"),
        Some(PLANTED_TOKEN)
    );
}

#[test]
fn the_auth_token_never_appears_in_debug_or_log_surfaces() {
    let config = things_config().with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("github", "work"),
    });
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(config, Arc::clone(&mock), secrets_with_token());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    d.rest_applier().apply_shared(&node).unwrap();

    // The request carried the token on the wire, but its Debug rendering MUST redact it —
    // this is the surface a log line / error / {:?} dump would expose.
    let req = &mock.recorded()[0];
    let dbg = format!("{req:?}");
    assert!(
        !dbg.contains(PLANTED_TOKEN),
        "SECRET LEAK: token surfaced in request Debug: {dbg}"
    );
    assert!(
        !dbg.contains("9f8e7d6c5b4a"),
        "SECRET LEAK: token fragment in: {dbg}"
    );
    // The redaction marker rendered where the Authorization value would have been.
    assert!(
        dbg.contains(qfs_secrets::REDACTED),
        "redaction marker present: {dbg}"
    );
    // The header NAME is still surfaced (presence is observable, value is not).
    assert!(dbg.contains("Authorization"));
}

#[test]
fn missing_credential_is_a_structured_auth_error_not_a_panic() {
    let config = things_config().with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("github", "work"),
    });
    // empty_secrets has no credential → resolution fails with a secret-free code.
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(config, Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let err = d.rest_applier().apply_shared(&node).unwrap_err();
    // Terminal (a missing credential is not transient) and secret-free.
    assert_eq!(err.code(), "terminal");
    assert!(!format!("{err:?}").contains(PLANTED_TOKEN));
    // The mock never received a request (auth failed before send).
    assert!(mock.recorded().is_empty());
}

// ---------------------------------------------------------------------------
// Response decode via codec
// ---------------------------------------------------------------------------

#[test]
fn response_body_decodes_to_rows_via_the_json_codec() {
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    // Two JSON array elements → two rows (the JsonCodec contract).
    assert_eq!(out.affected, 2);
}

// ---------------------------------------------------------------------------
// Error responses → structured errors + retry classification
// ---------------------------------------------------------------------------

#[test]
fn client_4xx_is_terminal_and_carries_no_token() {
    let config = things_config().with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("github", "work"),
    });
    let mock = Arc::new(MockHttpClient::new().with_response(HttpResponse::new(
        403,
        br#"{"message":"forbidden"}"#.to_vec(),
    )));
    let d = driver_from_mock(config, Arc::clone(&mock), secrets_with_token());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let err = d.rest_applier().apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "terminal", "a 4xx is terminal, never retried");
    // The 401/403 reason mentions the status + URL but NOT the auth token.
    assert!(!format!("{err:?}").contains(PLANTED_TOKEN));
}

#[test]
fn server_5xx_on_a_get_is_retryable() {
    let mock =
        Arc::new(MockHttpClient::new().with_response(HttpResponse::new(503, b"oops".to_vec())));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let err = d.rest_applier().apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "retryable", "a 5xx on a GET is transient");
}

#[test]
fn server_5xx_on_a_post_is_terminal_never_retried() {
    // POST is not idempotent — a 5xx (or timeout) must NOT be auto-retried (RFD §6).
    let mock =
        Arc::new(MockHttpClient::new().with_response(HttpResponse::new(503, b"oops".to_vec())));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        rest_target("/rest/example/things"),
    )
    .with_args(http_body_args(b"{}"));
    let err = d.rest_applier().apply_shared(&node).unwrap_err();
    assert_eq!(
        err.code(),
        "terminal",
        "a 5xx on a POST is terminal — never re-sent"
    );
}

#[test]
fn transport_error_on_a_get_is_retryable() {
    let mock = Arc::new(MockHttpClient::new().with_error(HttpError::Transport {
        method: "GET".into(),
        url: "https://api.example.com/v1/things".into(),
        reason: "connection failed".into(),
    }));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let err = d.rest_applier().apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "retryable");
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[test]
fn cursor_pagination_concatenates_pages_and_enforces_the_cap() {
    let config = things_config().with_pagination(Pagination::Cursor {
        next_field: "next_cursor".to_string(),
        param: "cursor".to_string(),
        max_pages: 5,
    });
    // Three pages: pages 1+2 carry a next_cursor in the JSON object body, page 3 does not.
    // The JsonCodec turns each top-level object into exactly one row, so 3 pages → 3 rows.
    let mock = Arc::new(MockHttpClient::new());
    mock.push_response(HttpResponse::new(
        200,
        br#"{"next_cursor":"c2","p":1}"#.to_vec(),
    ));
    mock.push_response(HttpResponse::new(
        200,
        br#"{"next_cursor":"c3","p":2}"#.to_vec(),
    ));
    mock.push_response(HttpResponse::new(200, br#"{"p":3}"#.to_vec()));

    let d = driver_from_mock(config, Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(out.affected, 3, "three pages concatenated into three rows");

    let reqs = mock.recorded();
    assert_eq!(reqs.len(), 3, "stopped when the cursor was absent");
    // Page 1 has no cursor; pages 2 and 3 carry the prior page's cursor.
    assert!(!reqs[0].url.contains("cursor="));
    assert!(reqs[1].url.contains("cursor=c2"));
    assert!(reqs[2].url.contains("cursor=c3"));
}

#[test]
fn cursor_pagination_stops_at_the_page_cap() {
    let config = things_config().with_pagination(Pagination::Cursor {
        next_field: "next_cursor".to_string(),
        param: "cursor".to_string(),
        max_pages: 2,
    });
    let mock = Arc::new(MockHttpClient::new());
    // Every page advertises a next cursor; only the cap stops the loop.
    mock.push_response(HttpResponse::new(
        200,
        br#"{"next_cursor":"c2","p":1}"#.to_vec(),
    ));
    mock.push_response(HttpResponse::new(
        200,
        br#"{"next_cursor":"c3","p":2}"#.to_vec(),
    ));
    mock.push_response(HttpResponse::new(
        200,
        br#"{"next_cursor":"c4","p":3}"#.to_vec(),
    ));
    let d = driver_from_mock(config, Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(out.affected, 2, "the cap halts the follow loop");
    assert_eq!(
        mock.recorded().len(),
        2,
        "exactly max_pages requests issued"
    );
}

#[test]
fn link_header_pagination_follows_rel_next() {
    let config = things_config().with_pagination(Pagination::LinkHeader { max_pages: 5 });
    let mock = Arc::new(MockHttpClient::new());
    mock.push_response(HttpResponse::new(200, br#"[{"p":1}]"#.to_vec()).header(
        "Link",
        "<https://api.example.com/v1/things?page=2>; rel=\"next\"",
    ));
    mock.push_response(HttpResponse::new(200, br#"[{"p":2}]"#.to_vec())); // no Link → stop
    let d = driver_from_mock(config, Arc::clone(&mock), empty_secrets());
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(out.affected, 2);
    let reqs = mock.recorded();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].url, "https://api.example.com/v1/things?page=2");
}

// ---------------------------------------------------------------------------
// http.get TVF
// ---------------------------------------------------------------------------

#[test]
fn http_get_tvf_issues_a_no_config_get_and_decodes_rows() {
    // A driver whose config base URL would be IGNORED by the override URL the TVF carries.
    let mock = Arc::new(MockHttpClient::new().with_response(json_two_things()));
    let d = driver_from_mock(things_config(), Arc::clone(&mock), empty_secrets());

    let node = http_get_node(
        NodeId(0),
        "https://other.example.org/probe",
        &[("Accept".to_string(), "application/json".to_string())],
    );
    let out = d.rest_applier().apply_shared(&node).unwrap();
    assert_eq!(out.affected, 2);

    let req = &mock.recorded()[0];
    assert_eq!(req.method, HttpMethod::Get);
    // The override URL is used verbatim — NOT joined to the config base.
    assert_eq!(req.url, "https://other.example.org/probe");
    assert_eq!(req.header_value("Accept"), Some("application/json"));
}

// ---------------------------------------------------------------------------
// Config serde + secret-free invariant
// ---------------------------------------------------------------------------

#[test]
fn config_round_trips_through_serde_without_any_token() {
    let config = things_config().with_auth(AuthStrategy::Bearer {
        secret_ref: SecretRef::new("github", "work"),
    });
    let json = serde_json::to_string(&config).unwrap();
    // The config carries a secret_ref selector, never the token itself.
    assert!(json.contains("secret_ref"));
    assert!(json.contains("github"));
    assert!(!json.contains(PLANTED_TOKEN));
    let back: RestApiConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, config);
}

#[test]
fn pagination_labels_and_caps_are_stable() {
    assert_eq!(Pagination::None.label(), "none");
    assert_eq!(Pagination::None.max_pages(), 1);
    let cur = Pagination::Cursor {
        next_field: "n".into(),
        param: "c".into(),
        max_pages: 7,
    };
    assert_eq!(cur.label(), "cursor");
    assert_eq!(cur.max_pages(), 7);
    assert_eq!(
        Pagination::LinkHeader { max_pages: 3 }.label(),
        "link-header"
    );
}

#[test]
fn affected_estimate_is_honest_for_a_filtered_get() {
    // A SELECT whose count is unknown until apply carries an honest estimate on the node.
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        rest_target("/rest/example/things"),
    )
    .with_affected(Affected::Unknown);
    assert_eq!(node.est_affected, Affected::Unknown);
}
