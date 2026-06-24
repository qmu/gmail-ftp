//! Planner-owned **black-box E2E harness** for the t25 Slack driver (qfs-foundation-e0).
//!
//! This is an EXTERNAL-INTERFACE test suite, independent of the Constructor's in-crate unit tests.
//! It drives the driver end-to-end through its **public API** + the **runtime interpreter**
//! (the true `COMMIT` path) + a **Planner-owned mock HTTP transport** (so the real `RestSlackClient`
//! seam — Bearer injection, pagination, BodyErrorRule, retry — is exercised through the wire) and
//! the pure `parse_event` normalizer from event fixtures. NO live Slack, NO live token, NO network.
//!
//! Observation surfaces:
//! - plan shape: the decoded `SlackEffect` + recorded mock calls;
//! - COMMIT disposition: the interpreter `Outcome` ledger (`Applied` vs `Failed`);
//! - read rows: the merged-page `RowBatch`;
//! - signature/event: the `SlackInbound`/`EventError`;
//! - secret safety: planted canaries scanned across plan/preview/error/Debug/log surfaces.

// Conventional test-allow header (the same one every other test crate carries, e.g.
// crates/driver-github/tests/e2e_blackbox.rs): a black-box harness asserts via unwrap/expect/panic,
// which the workspace `-D warnings` clippy gate otherwise rejects.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use qfs_driver::{check_capability, Driver, Path, Verb};
use qfs_http_core::{HttpRequest, HttpResponse};
use qfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter, LegStatus, Outcome, SharedApplier};
use qfs_secrets::{AccountId, CredentialKey, InMemoryStore, Secret, Secrets};
use qfs_types::{CmpOp, ColRef, Column, Literal, Predicate, Row, RowBatch, Schema, Value};

use qfs_driver_slack::{
    parse_event, read, BodyErrorRule, EventHeaders, HttpTransport, NodeKind, ReadPlan,
    RecordedCall, RestSlackClient, SlackApiCall, SlackClient, SlackDriver, SlackEffect, SlackError,
    SlackEventKind, SlackInbound, SlackPath, SlackWsConfig, TransportError,
};

// =====================================================================================
// Planner-owned fixtures (independent of the crate's internal test fixtures)
// =====================================================================================

const BOT_TOKEN: &str = "xoxb-PLANNER-CANARY-bot-token-DO-NOT-LEAK";
const SIGNING_SECRET: &str = "PLANNER-CANARY-signing-secret-DO-NOT-LEAK";

/// A scripted HTTP transport the Planner controls: records every request, answers from a FIFO of
/// responses. No socket. This is the Planner's mock of the wire — the real `RestSlackClient` runs
/// on top of it, so the seam logic (auth header, pagination, retry, BodyErrorRule) is the real code.
#[derive(Default)]
struct ScriptedTransport {
    responses: Mutex<std::collections::VecDeque<HttpResponse>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            recorded: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.recorded.lock().unwrap().push(req.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError {
                reason: "scripted transport exhausted".to_string(),
            })
    }
}

fn store() -> (Arc<dyn Secrets>, CredentialKey) {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(
        qfs_secrets::DriverId::new("slack"),
        AccountId::new("work").unwrap(),
    );
    store
        .put(&key, Secret::new(BOT_TOKEN.as_bytes().to_vec()))
        .unwrap();
    (Arc::new(store), key)
}

fn rest_client(responses: Vec<HttpResponse>) -> (RestSlackClient, Arc<ScriptedTransport>) {
    let (secrets, key) = store();
    let transport = ScriptedTransport::new(responses);
    let client = RestSlackClient::new(transport.clone(), secrets, key, BodyErrorRule::On);
    (client, transport)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("slack"), VfsPath::new(path))
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

fn ok_json(body: &str) -> HttpResponse {
    HttpResponse::new(200, body.as_bytes().to_vec())
}

/// Drive one effect through the **real RestSlackClient over a scripted transport** and observe the
/// COMMIT-path disposition via the runtime's `SharedApplier` (terminal/retryable/applied).
fn commit_one_via_rest(
    node: EffectNode,
    responses: Vec<HttpResponse>,
) -> (
    Result<qfs_runtime::EffectOutput, qfs_runtime::EffectError>,
    Arc<ScriptedTransport>,
) {
    let (client, transport) = rest_client(responses);
    let driver = SlackDriver::new(Arc::new(client) as Arc<dyn SlackClient>);
    let out = driver.slack_applier().apply_shared(&node);
    (out, transport)
}

/// Sign a body the way Slack does (Planner's own signer; mirrors Slack's documented scheme).
fn slack_sign(secret: &str, ts: &str, body: &[u8]) -> String {
    // Recompute via the driver's exposed hmac is not public; re-derive with the same documented
    // base string by asking the driver to verify a known-good signature instead. We instead build
    // the signature by round-tripping through verify: but verify needs the signature. So we use the
    // driver's own `parse_event` acceptance as the oracle and construct the signature with a
    // standalone HMAC-SHA256 implementation below to stay fully black-box on the sign side.
    let tag = hmac_sha256(secret.as_bytes(), &{
        let mut b = Vec::new();
        b.extend_from_slice(b"v0:");
        b.extend_from_slice(ts.as_bytes());
        b.push(b':');
        b.extend_from_slice(body);
        b
    });
    format!("v0={}", hex(&tag))
}

// A standalone, dependency-free HMAC-SHA256 + hex so the Planner signer does not borrow the
// driver's internal hmac module (keeping the sign side an independent oracle). Pure RFC-2104/FIPS-180.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = sha256(key);
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_d = sha256(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_d);
    sha256(&outer)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

// =====================================================================================
// Scenario 1: DESCRIBE per-node archetype + schema + capability set
// =====================================================================================

#[test]
fn s1_describe_per_node_archetype_schema_and_capabilities() {
    let d = SlackDriver::new(
        Arc::new(qfs_driver_slack::MockSlackClient::new()) as Arc<dyn SlackClient>
    );

    // Archetype + key schema columns per node (golden-style).
    let cases: &[(&str, &str, &[&str])] = &[
        ("acme/#general/messages", "append_log", &["ts", "text"]),
        (
            "acme/#general/messages/9.9/replies",
            "append_log",
            &["ts", "text"],
        ),
        (
            "acme/#general/messages/9.9/reactions",
            "append_log",
            &["name", "count"],
        ),
        ("acme/dms/U07/messages", "append_log", &["ts", "text"]),
        ("acme/files", "blob_namespace", &["id", "name", "size"]),
        ("acme/users", "relational_table", &["id", "name", "is_bot"]),
    ];
    for (sub, arch, cols) in cases {
        let desc = d.describe(&Path::new(format!("/slack/{sub}"))).unwrap();
        assert_eq!(
            serde_json::to_string(&desc.archetype).unwrap(),
            format!("\"{arch}\""),
            "{sub} archetype"
        );
        for c in *cols {
            assert!(desc.schema.column(c).is_some(), "{sub} missing column {c}");
        }
    }

    // Capability sets are node-keyed (the parse-time gate's data).
    let cap_json = |p: &str| serde_json::to_string(&d.capabilities(&Path::new(p))).unwrap();
    assert!(cap_json("/slack/acme/#general/messages").contains("\"select\":true"));
    assert!(cap_json("/slack/acme/#general/messages").contains("\"insert\":true"));
    assert!(cap_json("/slack/acme/#general/messages").contains("\"remove\":true"));
    // reactions: insert+remove, no select.
    let r = cap_json("/slack/acme/#general/messages/9.9/reactions");
    assert!(
        r.contains("\"insert\":true")
            && r.contains("\"remove\":true")
            && r.contains("\"select\":false")
    );
    // users: select only.
    let u = cap_json("/slack/acme/users");
    assert!(
        u.contains("\"select\":true")
            && u.contains("\"insert\":false")
            && u.contains("\"update\":false")
    );
    // files: ls/cp/rm only.
    let f = cap_json("/slack/acme/files");
    assert!(
        f.contains("\"ls\":true")
            && f.contains("\"cp\":true")
            && f.contains("\"rm\":true")
            && f.contains("\"select\":false")
    );

    // A bare workspace root is not a describable node (honest structured error).
    assert_eq!(
        d.describe(&Path::new("/slack/acme")).unwrap_err().code(),
        "invalid_path"
    );
}

// =====================================================================================
// Scenario 2: plan-shape goldens (no creds, mock client)
// =====================================================================================

#[test]
fn s2_insert_message_yields_postmessage_with_client_msg_id() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("text", Value::Text("hi".into()))]));
    let eff = SlackEffect::from_node(&node).unwrap();
    match eff {
        SlackEffect::PostMessage {
            channel,
            text,
            thread_ts,
            client_msg_id,
            is_dm,
        } => {
            assert_eq!(channel, "#general");
            assert_eq!(text, "hi");
            assert!(thread_ts.is_none());
            assert!(!is_dm);
            assert!(
                client_msg_id.starts_with("qfs-"),
                "idempotency key attached: {client_msg_id}"
            );
            // The redacted-in-logged-form requirement: the token is NOT part of the effect/plan.
            assert!(!format!("{:?}", node).contains(BOT_TOKEN));
        }
        other => panic!("expected PostMessage, got {other:?}"),
    }
}

#[test]
fn s2_insert_reply_carries_thread_ts() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages/111.22/replies"),
    )
    .with_args(args(&[("text", Value::Text("threaded".into()))]));
    match SlackEffect::from_node(&node).unwrap() {
        SlackEffect::PostMessage { thread_ts, .. } => {
            assert_eq!(thread_ts.as_deref(), Some("111.22"));
        }
        other => panic!("expected PostMessage, got {other:?}"),
    }
}

#[test]
fn s2_insert_reaction_and_call_react_are_equivalent_plans() {
    let insert = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[("emoji", Value::Text("tada".into()))]));
    let call = EffectNode::new(
        NodeId(1),
        EffectKind::Call(ProcId::new("slack.react")),
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[
        ("ts", Value::Text("9.9".into())),
        ("emoji", Value::Text("tada".into())),
    ]));
    let expected = SlackEffect::AddReaction {
        channel: "#general".into(),
        ts: "9.9".into(),
        emoji: "tada".into(),
    };
    assert_eq!(SlackEffect::from_node(&insert).unwrap(), expected);
    assert_eq!(
        SlackEffect::from_node(&call).unwrap(),
        expected,
        "CALL ≡ INSERT plan"
    );
}

#[test]
fn s2_call_pin_is_one_irreversible_call_node() {
    let pin = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("slack.pin")),
        target("/slack/acme/#general/messages"),
    )
    .irreversible(true)
    .with_args(args(&[("ts", Value::Text("5.5".into()))]));
    assert!(pin.irreversible);
    let eff = SlackEffect::from_node(&pin).unwrap();
    assert!(matches!(eff, SlackEffect::Pin { .. }));
    assert!(eff.is_irreversible(), "pin tagged irreversible");

    // PREVIEW surfaces the irreversible flag and performs NO I/O (recorded mock untouched).
    let mut b = PlanBuilder::new();
    b.push(pin);
    let pv = preview(&b.build());
    assert_eq!(pv.rows.len(), 1);
    assert!(pv.rows[0].irreversible, "PREVIEW surfaces irreversible pin");
}

#[test]
fn s2_remove_message_is_chat_delete_irreversible() {
    let del = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("ts", Value::Text("5.5".into()))]));
    let eff = SlackEffect::from_node(&del).unwrap();
    assert!(matches!(eff, SlackEffect::DeleteMessage { .. }));
    assert!(eff.is_irreversible(), "chat.delete irreversible=true");
}

// =====================================================================================
// Scenario 3: capability gating at PARSE time
// =====================================================================================

#[test]
fn s3_unsupported_verbs_on_users_rejected_at_parse_with_structured_error() {
    let d = SlackDriver::new(
        Arc::new(qfs_driver_slack::MockSlackClient::new()) as Arc<dyn SlackClient>
    );
    let users = Path::new("/slack/acme/users");
    for verb in [Verb::Insert, Verb::Update] {
        let err = check_capability(&d, &users, verb).unwrap_err();
        assert_eq!(
            err.code(),
            "unsupported_verb",
            "{verb:?} must be a structured capability error"
        );
        match &err {
            qfs_driver::CfsError::UnsupportedVerb {
                path,
                verb: v,
                supported,
            } => {
                assert_eq!(path, "/slack/acme/users");
                assert_eq!(*v, verb.label());
                assert_eq!(supported, &vec!["SELECT"], "lists supported verbs");
            }
            other => panic!("expected UnsupportedVerb, got {other:?}"),
        }
    }
    // Select on users is allowed.
    assert!(check_capability(&d, &users, Verb::Select).is_ok());
}

// =====================================================================================
// Scenario 4: event tests (pure, fixtures) — signature + replay defense
// =====================================================================================

#[test]
fn s4_url_verification_returns_challenge() {
    let ts = "1700000000";
    let body = br#"{"type":"url_verification","challenge":"planner-challenge-xyz"}"#;
    let sig = slack_sign(SIGNING_SECRET, ts, body);
    let h = EventHeaders::new(sig, ts);
    match parse_event(&h, body, SIGNING_SECRET, 1_700_000_000).unwrap() {
        SlackInbound::UrlVerification { challenge } => {
            assert_eq!(challenge, "planner-challenge-xyz")
        }
        other => panic!("expected UrlVerification, got {other:?}"),
    }
}

#[test]
fn s4_valid_message_and_reaction_normalize_with_event_id() {
    let ts = "1700000100";
    let now = 1_700_000_100;
    let msg = br#"{"type":"event_callback","event_id":"EvPLAN1","team_id":"T9",
        "event":{"type":"message","channel":"C9","ts":"7.7","user":"U9","text":"hi"}}"#;
    let sig = slack_sign(SIGNING_SECRET, ts, msg);
    match parse_event(&EventHeaders::new(sig, ts), msg, SIGNING_SECRET, now).unwrap() {
        SlackInbound::Event(e) => {
            assert_eq!(e.kind, SlackEventKind::Message);
            assert_eq!(
                e.event_id.as_deref(),
                Some("EvPLAN1"),
                "event_id surfaced for dedupe"
            );
            assert_eq!(e.channel.as_deref(), Some("C9"));
            assert_eq!(e.ts.as_deref(), Some("7.7"));
        }
        other => panic!("expected Event, got {other:?}"),
    }
    let react = br#"{"type":"event_callback","event_id":"EvPLAN2",
        "event":{"type":"reaction_added","user":"U9","reaction":"rocket",
                 "item":{"type":"message","channel":"C9","ts":"7.7"}}}"#;
    let sig = slack_sign(SIGNING_SECRET, ts, react);
    match parse_event(&EventHeaders::new(sig, ts), react, SIGNING_SECRET, now).unwrap() {
        SlackInbound::Event(e) => {
            assert_eq!(e.kind, SlackEventKind::ReactionAdded);
            assert_eq!(e.reaction.as_deref(), Some("rocket"));
            assert_eq!(
                e.channel.as_deref(),
                Some("C9"),
                "channel from item.channel"
            );
            assert_eq!(e.ts.as_deref(), Some("7.7"), "ts from item.ts");
        }
        other => panic!("expected Event, got {other:?}"),
    }
}

#[test]
fn s4_tampered_signature_and_wrong_secret_rejected() {
    let ts = "1700000200";
    let now = 1_700_000_200;
    let body = br#"{"type":"event_callback","event":{"type":"message","text":"x"}}"#;

    // Tamper the body AFTER signing — the signature no longer matches the bytes.
    let sig = slack_sign(SIGNING_SECRET, ts, body);
    let tampered_body = br#"{"type":"event_callback","event":{"type":"message","text":"X"}}"#;
    let err = parse_event(
        &EventHeaders::new(sig.clone(), ts),
        tampered_body,
        SIGNING_SECRET,
        now,
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "slack_event_bad_signature",
        "tampered body fails"
    );

    // Flip a signature hex char.
    let mut bad = sig.clone();
    bad.pop();
    bad.push(if sig.ends_with('0') { '1' } else { '0' });
    let err = parse_event(&EventHeaders::new(bad, ts), body, SIGNING_SECRET, now).unwrap_err();
    assert_eq!(err.code(), "slack_event_bad_signature", "flipped sig fails");

    // Wrong secret fails (constant-time compare).
    let good = slack_sign(SIGNING_SECRET, ts, body);
    let err = parse_event(&EventHeaders::new(good, ts), body, "the-wrong-secret", now).unwrap_err();
    assert_eq!(
        err.code(),
        "slack_event_bad_signature",
        "wrong secret fails"
    );
}

#[test]
fn s4_stale_timestamp_rejected_as_replay_both_directions() {
    let ts = "1700000000";
    let body = br#"{"type":"event_callback","event":{"type":"message"}}"#;
    let sig = slack_sign(SIGNING_SECRET, ts, body);

    // Replay far in the future relative to the signed ts.
    let now_future = 1_700_000_000 + 5 * 60 + 1;
    let err = parse_event(
        &EventHeaders::new(sig.clone(), ts),
        body,
        SIGNING_SECRET,
        now_future,
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "slack_event_stale_timestamp",
        "future skew rejected"
    );

    // A delivery dated in the future relative to now (other side of the window).
    let now_past = 1_700_000_000 - 5 * 60 - 1;
    let err = parse_event(&EventHeaders::new(sig, ts), body, SIGNING_SECRET, now_past).unwrap_err();
    assert_eq!(
        err.code(),
        "slack_event_stale_timestamp",
        "negative skew rejected too"
    );

    // Just inside the window passes (signature valid, ts fresh).
    let fresh_now = 1_700_000_000 + 5 * 60 - 1;
    let sig = slack_sign(SIGNING_SECRET, ts, body);
    assert!(parse_event(&EventHeaders::new(sig, ts), body, SIGNING_SECRET, fresh_now).is_ok());
}

#[test]
fn s4_missing_headers_rejected() {
    let body = br#"{"type":"event_callback"}"#;
    // EventHeaders is #[non_exhaustive]: build via the public ctor then null out one field.
    let mut no_sig = EventHeaders::new("v0=deadbeef", "1700000000");
    no_sig.signature = None;
    assert_eq!(
        parse_event(&no_sig, body, SIGNING_SECRET, 1_700_000_000)
            .unwrap_err()
            .code(),
        "slack_event_missing_header"
    );
    let mut no_ts = EventHeaders::new("v0=deadbeef", "1700000000");
    no_ts.timestamp = None;
    assert_eq!(
        parse_event(&no_ts, body, SIGNING_SECRET, 1_700_000_000)
            .unwrap_err()
            .code(),
        "slack_event_missing_header"
    );
}

// =====================================================================================
// Scenario 5: pagination — 3 cursor pages concatenated; page cap enforced
// =====================================================================================

#[test]
fn s5_three_cursor_pages_concatenate_through_rest_client() {
    let p1 = ok_json(
        r#"{"ok":true,"messages":[{"ts":"1.1","text":"a"}],"response_metadata":{"next_cursor":"C2"}}"#,
    );
    let p2 = ok_json(
        r#"{"ok":true,"messages":[{"ts":"1.2","text":"b"}],"response_metadata":{"next_cursor":"C3"}}"#,
    );
    let p3 = ok_json(
        r#"{"ok":true,"messages":[{"ts":"1.3","text":"c"}],"response_metadata":{"next_cursor":""}}"#,
    );
    let (client, transport) = rest_client(vec![p1, p2, p3]);

    let plan = ReadPlan::list(NodeKind::Messages, None);
    let value = client.list(NodeKind::Messages, plan.params()).unwrap();
    let batch = read::decode_list(NodeKind::Messages, &value).unwrap();
    assert_eq!(batch.rows.len(), 3, "3 cursor pages concatenated");
    assert_eq!(batch.rows[0].values[0], Value::Text("1.1".into()));
    assert_eq!(batch.rows[2].values[0], Value::Text("1.3".into()));

    let reqs = transport.requests();
    assert_eq!(reqs.len(), 3, "exactly 3 page fetches");
    assert!(reqs[1].url.contains("cursor=C2"));
    assert!(reqs[2].url.contains("cursor=C3"));
}

#[test]
fn s5_page_cap_bounds_runaway_pagination() {
    // Every page advertises a non-empty next_cursor → would loop forever without a cap.
    let pages: Vec<HttpResponse> = (0..100)
        .map(|_| ok_json(r#"{"ok":true,"messages":[{"ts":"x","text":"loop"}],"response_metadata":{"next_cursor":"MORE"}}"#))
        .collect();
    let (client, transport) = rest_client(pages);
    let value = client.list(NodeKind::Messages, &[]).unwrap();
    let batch = read::decode_list(NodeKind::Messages, &value).unwrap();
    // MAX_PAGES is 50 — the cap halts the fan-out well before the 100 scripted pages.
    let fetched = transport.requests().len();
    assert!(
        fetched <= 50,
        "page cap enforced: fetched {fetched} pages (cap 50)"
    );
    assert_eq!(
        batch.rows.len(),
        fetched,
        "one row per fetched page, bounded"
    );
}

// =====================================================================================
// Scenario 6: BodyErrorRule — HTTP 200 + ok:false → terminal error (read + write)
// =====================================================================================

#[test]
fn s6_body_error_on_write_path_is_terminal_not_false_success() {
    let resp = ok_json(r#"{"ok":false,"error":"not_in_channel"}"#);
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("text", Value::Text("hi".into()))]));
    let (out, transport) = commit_one_via_rest(node, vec![resp]);
    let err = out.expect_err("ok:false must NOT be a success");
    assert_eq!(
        err.code(),
        "terminal",
        "BodyError surfaces terminal (no retry)"
    );
    assert!(
        format!("{err}").contains("not_in_channel"),
        "carries Slack's error code"
    );
    assert_eq!(transport.requests().len(), 1, "issued once, never retried");
}

#[test]
fn s6_body_error_on_read_path_is_terminal() {
    let resp = ok_json(r#"{"ok":false,"error":"channel_not_found"}"#);
    let (client, _t) = rest_client(vec![resp]);
    let err = client.list(NodeKind::Messages, &[]).unwrap_err();
    assert_eq!(
        err.code(),
        "slack_body_error",
        "read path BodyError is structured terminal"
    );
    assert!(format!("{err}").contains("channel_not_found"));
}

// =====================================================================================
// Scenario 7 (THE FLAGGED REGRESSION): remove-side idempotency
//   REMOVE reaction already absent (no_reaction) and slack.unpin already-unpinned (not_pinned)
//   Per the idempotency contract these SHOULD be swallowed no-ops, NOT terminal errors.
// =====================================================================================

#[test]
fn s7_remove_absent_reaction_no_reaction_outcome() {
    // REMOVE a reaction that is already absent: Slack returns ok:false error=no_reaction.
    let resp = ok_json(r#"{"ok":false,"error":"no_reaction"}"#);
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[("emoji", Value::Text("tada".into()))]));
    let (out, _t) = commit_one_via_rest(node, vec![resp]);

    // Document EXACTLY what happens (no assertion of success — we report the observed reality).
    match &out {
        Ok(o) => println!(
            "S7 no_reaction: SWALLOWED as no-op (affected={})",
            o.affected
        ),
        Err(e) => println!("S7 no_reaction: SURFACED as {} -> {e}", e.code()),
    }
    // The idempotency contract expectation:
    assert!(
        out.is_ok(),
        "CONTRACT: REMOVE of an already-absent reaction (no_reaction) should be a swallowed no-op, \
         but it surfaced as: {:?}",
        out.err()
    );
}

#[test]
fn s7_unpin_already_unpinned_not_pinned_outcome() {
    // slack.unpin on an already-unpinned message: Slack returns ok:false error=not_pinned.
    let resp = ok_json(r#"{"ok":false,"error":"not_pinned"}"#);
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("slack.unpin")),
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("ts", Value::Text("5.5".into()))]));
    let (out, _t) = commit_one_via_rest(node, vec![resp]);

    match &out {
        Ok(o) => println!(
            "S7 not_pinned: SWALLOWED as no-op (affected={})",
            o.affected
        ),
        Err(e) => println!("S7 not_pinned: SURFACED as {} -> {e}", e.code()),
    }
    assert!(
        out.is_ok(),
        "CONTRACT: slack.unpin on an already-unpinned message (not_pinned) should be a swallowed \
         no-op, but it surfaced as: {:?}",
        out.err()
    );
}

/// The symmetric ADD side, as the control: already_reacted / already_pinned ARE swallowed.
#[test]
fn s7_control_add_side_already_done_is_swallowed() {
    let add = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[("emoji", Value::Text("tada".into()))]));
    let (out, _t) = commit_one_via_rest(
        add,
        vec![ok_json(r#"{"ok":false,"error":"already_reacted"}"#)],
    );
    assert!(
        out.is_ok(),
        "already_reacted on ADD is swallowed (control passes)"
    );

    let pin = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("slack.pin")),
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("ts", Value::Text("5.5".into()))]));
    let (out, _t) = commit_one_via_rest(
        pin,
        vec![ok_json(r#"{"ok":false,"error":"already_pinned"}"#)],
    );
    assert!(
        out.is_ok(),
        "already_pinned on PIN is swallowed (control passes)"
    );
}

/// The swallow must be the **already-satisfied class only**, not "swallow all remove errors". A
/// genuine remove failure (e.g. `message_not_found` on a reaction remove) must STILL surface as a
/// terminal error — otherwise the fix would mask real failures.
#[test]
fn s7_genuine_remove_failure_still_surfaces_terminal() {
    // reactions.remove that genuinely fails (the message does not exist) — NOT an already-done code.
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[("emoji", Value::Text("tada".into()))]));
    let (out, _t) = commit_one_via_rest(
        node,
        vec![ok_json(r#"{"ok":false,"error":"message_not_found"}"#)],
    );
    let err = out.expect_err("a genuine remove failure must NOT be swallowed");
    assert_eq!(err.code(), "terminal", "real remove failure stays terminal");
    assert!(
        format!("{err}").contains("message_not_found"),
        "carries the real Slack error code, not a silent success"
    );

    // Symmetrically, an unpin that genuinely fails (bad channel) must also surface terminal.
    let unpin = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("slack.unpin")),
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[("ts", Value::Text("5.5".into()))]));
    let (out, _t) = commit_one_via_rest(
        unpin,
        vec![ok_json(r#"{"ok":false,"error":"channel_not_found"}"#)],
    );
    let err = out.expect_err("a genuine unpin failure must NOT be swallowed");
    assert_eq!(err.code(), "terminal");
    assert!(format!("{err}").contains("channel_not_found"));
}

// =====================================================================================
// Scenario 8: pushdown residual truthfulness
//   ts boundary (pushable) mixed with a non-pushable predicate keeps the exact residual.
//   Also probe: strict `>` lowering to inclusive `oldest`.
// =====================================================================================

#[test]
fn s8_ge_boundary_drops_and_nonpushable_stays_residual() {
    // ts >= 100 AND text = 'hi' : oldest=100 pushed (inclusive boundary == `>=`, exact), text= residual.
    let ts_ge = Predicate::Cmp(ColRef::col("ts"), CmpOp::Ge, Literal::Text("100".into()));
    let text_eq = Predicate::Cmp(ColRef::col("text"), CmpOp::Eq, Literal::Text("hi".into()));
    let pred = Predicate::And(Box::new(ts_ge), Box::new(text_eq.clone()));
    let plan = ReadPlan::list(NodeKind::Messages, Some(&pred));
    assert_eq!(plan.params(), &[("oldest".to_string(), "100".to_string())]);
    assert_eq!(
        plan.pushdown.residual,
        Some(text_eq),
        "the non-pushable text= MUST stay residual so the engine re-filters (no wrong rows)"
    );
}

#[test]
fn s8_or_predicate_stays_wholly_residual() {
    let or = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("ts"),
            CmpOp::Ge,
            Literal::Text("1".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("text"),
            CmpOp::Eq,
            Literal::Text("z".into()),
        )),
    );
    let plan = ReadPlan::list(NodeKind::Messages, Some(&or));
    assert!(plan.params().is_empty(), "nothing pushed for an OR");
    assert_eq!(
        plan.pushdown.residual,
        Some(or),
        "the whole OR stays residual"
    );
}

/// FIX RE-TEST (#8): a STRICT `ts > 100` must push `oldest=100` (Slack's inclusive bound is a fine
/// pre-filter — over-fetch is safe) BUT must KEEP the strict `ts > 100` residual so the engine
/// re-excludes the `ts == 100` boundary row Slack would otherwise over-return. This is the
/// truthful-residual invariant: a pre-filter looser than the SQL predicate may never drop the
/// predicate.
#[test]
fn s8_strict_gt_pushes_oldest_but_keeps_strict_residual() {
    let ts_gt = Predicate::Cmp(ColRef::col("ts"), CmpOp::Gt, Literal::Text("100".into()));
    let plan = ReadPlan::list(NodeKind::Messages, Some(&ts_gt));
    assert_eq!(
        plan.params(),
        &[("oldest".to_string(), "100".to_string())],
        "strict > still pushes oldest=100 as a pre-filter (over-fetch is safe)"
    );
    assert_eq!(
        plan.pushdown.residual,
        Some(Predicate::Cmp(
            ColRef::col("ts"),
            CmpOp::Gt,
            Literal::Text("100".into())
        )),
        "the strict `ts > 100` MUST stay residual so the ts==100 boundary row is re-excluded → no wrong rows"
    );

    // Symmetric strict `<` keeps its residual too.
    let ts_lt = Predicate::Cmp(ColRef::col("ts"), CmpOp::Lt, Literal::Text("200".into()));
    let plan = ReadPlan::list(NodeKind::Messages, Some(&ts_lt));
    assert_eq!(plan.params(), &[("latest".to_string(), "200".to_string())]);
    assert_eq!(
        plan.pushdown.residual,
        Some(Predicate::Cmp(
            ColRef::col("ts"),
            CmpOp::Lt,
            Literal::Text("200".into())
        )),
        "strict < keeps its residual symmetrically"
    );
}

/// CONTROL (#8): an INCLUSIVE `ts >= 100` still correctly DROPS its residual (the inclusive Slack
/// `oldest` boundary means exactly `>=`, so re-checking would be redundant). The fix must not have
/// made the inclusive case over-conservative.
#[test]
fn s8_inclusive_ge_still_drops_residual() {
    let ts_ge = Predicate::Cmp(ColRef::col("ts"), CmpOp::Ge, Literal::Text("100".into()));
    let plan = ReadPlan::list(NodeKind::Messages, Some(&ts_ge));
    assert_eq!(plan.params(), &[("oldest".to_string(), "100".to_string())]);
    assert!(
        plan.pushdown.residual.is_none(),
        "inclusive >= is exact against Slack's inclusive oldest → residual correctly dropped"
    );

    let ts_le = Predicate::Cmp(ColRef::col("ts"), CmpOp::Le, Literal::Text("200".into()));
    let plan = ReadPlan::list(NodeKind::Messages, Some(&ts_le));
    assert_eq!(plan.params(), &[("latest".to_string(), "200".to_string())]);
    assert!(
        plan.pushdown.residual.is_none(),
        "inclusive <= drops residual symmetrically"
    );
}

// =====================================================================================
// Scenario 9: token + signing-secret safety — planted canaries everywhere
// =====================================================================================

#[test]
fn s9_bot_token_canary_absent_from_plan_preview_and_serialized_plan() {
    // A real INSERT plan over /slack carries no token.
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/slack/acme/#general/messages"),
        )
        .with_args(args(&[("text", Value::Text("a normal message".into()))])),
    );
    let plan = b.build();

    let serialized = serde_json::to_string(&plan).unwrap();
    assert!(
        !serialized.contains(BOT_TOKEN),
        "no bot token in serialized plan"
    );
    assert!(
        !serialized.contains("Bearer"),
        "no Bearer material in serialized plan"
    );

    let pv = preview(&plan);
    let pv_dbg = format!("{pv:?}");
    assert!(!pv_dbg.contains(BOT_TOKEN), "no bot token in PREVIEW");
    assert!(
        !pv_dbg.contains(SIGNING_SECRET),
        "no signing secret in PREVIEW"
    );
}

#[test]
fn s9_bot_token_canary_redacted_in_request_debug_after_real_injection() {
    // Drive the REAL client so the token is genuinely injected as a header, then assert the
    // recorded request's Debug redacts it.
    let (client, transport) = rest_client(vec![ok_json(r#"{"ok":true,"messages":[]}"#)]);
    client.list(NodeKind::Messages, &[]).unwrap();
    let reqs = transport.requests();
    assert_eq!(reqs.len(), 1);
    // The header value is genuinely the token (auth works)...
    assert_eq!(
        reqs[0].header_value("authorization"),
        Some(format!("Bearer {BOT_TOKEN}").as_str())
    );
    // ...but the redacting Debug never reveals it.
    let dbg = format!("{:?}", reqs[0]);
    assert!(
        !dbg.contains(BOT_TOKEN),
        "token must not appear in request Debug: {dbg}"
    );
    assert!(
        dbg.contains(qfs_secrets::REDACTED),
        "redaction marker present"
    );
}

#[test]
fn s9_signing_secret_canary_absent_from_event_error_and_inbound_debug() {
    // A BAD signature must produce an error that carries NO signing-secret material.
    let ts = "1700000000";
    let body = br#"{"type":"event_callback","event":{"type":"message","text":"x"}}"#;
    let err = parse_event(
        &EventHeaders::new("v0=deadbeef", ts),
        body,
        SIGNING_SECRET,
        1_700_000_000,
    )
    .unwrap_err();
    let err_text = format!("{err} {err:?}");
    assert!(
        !err_text.contains(SIGNING_SECRET),
        "signing secret must NOT appear in EventError: {err_text}"
    );

    // A GOOD parse: the normalized SlackInbound Debug must not echo the signing secret either.
    let sig = slack_sign(SIGNING_SECRET, ts, body);
    let inbound = parse_event(
        &EventHeaders::new(sig, ts),
        body,
        SIGNING_SECRET,
        1_700_000_000,
    )
    .unwrap();
    let inbound_dbg = format!("{inbound:?}");
    assert!(
        !inbound_dbg.contains(SIGNING_SECRET),
        "signing secret must NOT appear in SlackInbound: {inbound_dbg}"
    );
}

#[test]
fn s9_slack_errors_are_secret_free() {
    // Construct the structured errors a failing call would surface and scan for canaries.
    let errs = [
        SlackError::Http {
            op: "chat.postMessage",
            status: 401,
        },
        SlackError::Body {
            op: "chat.postMessage",
            code: "not_authed".into(),
        },
        SlackError::Auth {
            code: "secret_not_found",
        },
        SlackError::Transport {
            op: "http",
            reason: "connection failed".into(),
        },
    ];
    for e in &errs {
        let text = format!("{e} {e:?}");
        assert!(
            !text.contains(BOT_TOKEN) && !text.contains("xoxb-"),
            "no bot token in error: {text}"
        );
        assert!(
            !text.contains(SIGNING_SECRET),
            "no signing secret in error: {text}"
        );
        assert!(!text.contains("Bearer"), "no Bearer in error: {text}");
    }

    // Config Debug carries credential KEYS (selectors), never values.
    let (_secrets, key) = store();
    let cfg = SlackWsConfig::new("acme", "T123", key.clone(), key);
    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains(BOT_TOKEN) && !dbg.contains(SIGNING_SECRET),
        "config Debug secret-free: {dbg}"
    );
}

// =====================================================================================
// Bonus: a true end-to-end COMMIT through the async interpreter (mock client) — the success path
// =====================================================================================

#[tokio::test]
async fn e2e_commit_post_message_through_interpreter_is_complete() {
    let mock = Arc::new(qfs_driver_slack::MockSlackClient::new());
    let driver = SlackDriver::new(mock.clone() as Arc<dyn SlackClient>);
    let bridge = qfs_driver_slack::slack_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/slack/acme/#general/messages"),
        )
        .with_args(args(&[("text", Value::Text("hi".into()))])),
    );
    let plan = b.build();
    plan.validate().unwrap();
    let caps = CapabilitySet::none().grant(DriverId::new("slack"), &EffectKind::Insert);
    let outcome: Outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "post committed: {outcome:?}");
    assert!(matches!(
        mock.recorded().as_slice(),
        [RecordedCall::Apply(SlackEffect::PostMessage { .. })]
    ));
    // Ledger leg is Applied.
    assert!(matches!(
        outcome.ledger[0].status,
        LegStatus::Applied { .. }
    ));
}

/// A NEGATIVE end-to-end: an `ok:false` write through the FULL interpreter records a Failed leg.
#[tokio::test]
async fn e2e_commit_body_error_records_failed_leg() {
    let (client, _t) = rest_client(vec![ok_json(r#"{"ok":false,"error":"not_in_channel"}"#)]);
    let driver = SlackDriver::new(Arc::new(client) as Arc<dyn SlackClient>);
    let bridge = qfs_driver_slack::slack_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/slack/acme/#general/messages"),
        )
        .with_args(args(&[("text", Value::Text("hi".into()))])),
    );
    let plan = b.build();
    let caps = CapabilitySet::none().grant(DriverId::new("slack"), &EffectKind::Insert);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(!outcome.is_complete(), "ok:false must not complete");
    assert_eq!(
        outcome.failed_count(),
        1,
        "the BodyError leg failed terminally"
    );
    match &outcome.ledger[0].status {
        LegStatus::Failed { error, .. } => {
            assert_eq!(error.code(), "terminal");
            assert!(format!("{error}").contains("not_in_channel"));
        }
        other => panic!("expected Failed leg, got {other:?}"),
    }
}

// Keep unused-import noise out if a path helper is only used in some configs.
#[allow(dead_code)]
fn _touch(_p: &SlackPath, _c: SlackApiCall) {}
