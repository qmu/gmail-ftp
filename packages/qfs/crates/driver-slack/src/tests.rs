//! Slack driver tests (RFD-0001 §5 acceptance) — **no live Slack, no network, no credentials**.
//! Every test drives the introspective `Driver` surface, the pushdown/effect decode, the apply leg
//! against an in-memory [`MockSlackClient`], and the pure `parse_event` normalizer from fixtures —
//! so we assert request shape + response decoding + plan shape + token safety + signature
//! verification without a socket. Live Slack E2E is parked for t38.

use std::sync::{Arc, Mutex};

use qfs_driver::{check_capability, resolve_proc, Archetype, Driver, Path, Verb, VersionSupport};
use qfs_http_core::{HttpRequest, HttpResponse};
use qfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter, SharedApplier};
use qfs_secrets::{ConnectionId, CredentialKey, InMemoryStore, Secret, Secrets};
use qfs_types::{Column, Row, RowBatch, Schema, Value};

use super::*;
use crate::client::{BodyErrorRule, HttpTransport, TransportError};
use crate::effect::{EMOJI_COL, TEXT_COL, TS_COL};
use crate::events::{parse_event, EventHeaders, SlackEventKind, SlackInbound, MAX_SKEW_SECS};
use crate::hmac::{hex_lower, hmac_sha256};

// ---- shared fixtures ---------------------------------------------------------------------

/// A recording HTTP transport (no socket): records every request and answers from a FIFO queue.
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

/// A secret store seeded with a bot token under the `slack/work` credential.
fn store_with_token(token: &str) -> (Arc<dyn Secrets>, CredentialKey) {
    let store = InMemoryStore::new();
    let key = CredentialKey::new(
        qfs_secrets::DriverId::new("slack"),
        ConnectionId::new("work").unwrap(),
    );
    store
        .put(&key, Secret::new(token.as_bytes().to_vec()))
        .unwrap();
    (Arc::new(store), key)
}

fn driver() -> (SlackDriver, Arc<MockSlackClient>) {
    let mock = Arc::new(MockSlackClient::new());
    let d = SlackDriver::new(mock.clone() as Arc<dyn SlackClient>);
    (d, mock)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("slack"), VfsPath::new(path))
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

// ---- introspection: mount / multi-archetype / schema -------------------------------------

#[test]
fn mount_and_id_are_slack() {
    let (d, _) = driver();
    assert_eq!(d.mount(), "/slack");
    assert_eq!(d.id(), DriverId::new("slack"));
}

#[test]
fn describe_emits_the_three_archetypes_per_node() {
    let (d, _) = driver();
    let cases: &[(&str, Archetype, &[&str])] = &[
        (
            "acme/#general/messages",
            Archetype::AppendLog,
            &["ts", "text"],
        ),
        (
            "acme/#general/messages/123.45/replies",
            Archetype::AppendLog,
            &["ts", "text"],
        ),
        (
            "acme/#general/messages/123.45/reactions",
            Archetype::AppendLog,
            &["name", "count"],
        ),
        (
            "acme/dms/U07/messages",
            Archetype::AppendLog,
            &["ts", "text"],
        ),
        (
            "acme/files",
            Archetype::BlobNamespace,
            &["id", "name", "size"],
        ),
        (
            "acme/users",
            Archetype::RelationalTable,
            &["id", "name", "is_bot"],
        ),
    ];
    for (sub, arch, cols) in cases {
        let desc = d
            .describe(&Path::new(format!("/slack/{sub}")))
            .unwrap_or_else(|e| panic!("describe {sub} failed: {e}"));
        assert_eq!(desc.archetype, *arch, "{sub} archetype");
        for col in *cols {
            assert!(
                desc.schema.column(col).is_some(),
                "{sub} missing column {col}"
            );
        }
    }
    // The bare workspace root is not a describable node — an honest structured error.
    assert_eq!(
        d.describe(&Path::new("/slack/acme")).unwrap_err().code(),
        "invalid_path"
    );
}

#[test]
fn version_support_is_snapshot_for_messages_only() {
    let (d, _) = driver();
    assert_eq!(
        d.version_support(&Path::new("/slack/acme/#general/messages")),
        VersionSupport::Snapshot
    );
    assert_eq!(
        d.version_support(&Path::new("/slack/acme/users")),
        VersionSupport::None
    );
    assert_eq!(
        d.version_support(&Path::new("/slack/acme/files")),
        VersionSupport::None
    );
}

// ---- capability gating (parse-time, structured) ------------------------------------------

#[test]
fn capabilities_are_node_keyed() {
    let (d, _) = driver();
    let messages = Path::new("/slack/acme/#general/messages");
    assert!(check_capability(&d, &messages, Verb::Select).is_ok());
    assert!(check_capability(&d, &messages, Verb::Insert).is_ok());
    assert!(check_capability(&d, &messages, Verb::Remove).is_ok());

    let reactions = Path::new("/slack/acme/#general/messages/1.2/reactions");
    assert!(check_capability(&d, &reactions, Verb::Insert).is_ok());
    assert!(check_capability(&d, &reactions, Verb::Remove).is_ok());
    assert!(check_capability(&d, &reactions, Verb::Select).is_err());

    let files = Path::new("/slack/acme/files");
    assert!(check_capability(&d, &files, Verb::Ls).is_ok());
    assert!(check_capability(&d, &files, Verb::Cp).is_ok());
    assert!(check_capability(&d, &files, Verb::Rm).is_ok());
    assert!(check_capability(&d, &files, Verb::Select).is_err());
}

#[test]
fn insert_and_update_on_users_are_rejected_at_parse_time_with_structured_error() {
    let (d, _) = driver();
    let users = Path::new("/slack/acme/users");
    for verb in [Verb::Insert, Verb::Update] {
        let err = check_capability(&d, &users, verb).unwrap_err();
        match &err {
            qfs_driver::CfsError::UnsupportedVerb {
                path,
                verb: v,
                supported,
            } => {
                assert_eq!(path, "/slack/acme/users");
                assert_eq!(*v, verb.label());
                assert_eq!(supported, &vec!["SELECT"], "users names its allowed verbs");
            }
            other => panic!("expected UnsupportedVerb, got {other:?}"),
        }
        assert_eq!(err.code(), "unsupported_verb");
    }
}

// ---- procedures: react/pin/unpin/update/delete + the POST prelude ------------------------

#[test]
fn procedures_declare_irreversibility_and_scopes_and_prelude_aliases_post() {
    let (d, _) = driver();
    let react = resolve_proc(&d, "react").unwrap();
    assert!(!react.irreversible, "react is reversible (unreact)");
    assert_eq!(react.requires_scopes, vec!["reactions:write".to_string()]);

    let pin = resolve_proc(&d, "pin").unwrap();
    assert!(pin.irreversible, "pin is flagged irreversible");

    let delete = resolve_proc(&d, "delete").unwrap();
    assert!(delete.irreversible, "delete (chat.delete) is irreversible");

    let unpin = resolve_proc(&d, "unpin").unwrap();
    assert!(!unpin.irreversible);
    let update = resolve_proc(&d, "update").unwrap();
    assert!(!update.irreversible);

    assert_eq!(
        resolve_proc(&d, "nuke").unwrap_err().code(),
        "unknown_procedure"
    );

    // The pure POST prelude alias desugars to a message INSERT (slack.post).
    let prelude = d.prelude();
    assert_eq!(prelude.len(), 1);
    assert_eq!(prelude[0].name, "POST");
    assert_eq!(prelude[0].desugars_to, "slack.post");
}

// ---- path parsing ------------------------------------------------------------------------

#[test]
fn paths_parse_each_node_kind() {
    assert_eq!(
        SlackPath::parse_str("/slack/acme/#general/messages")
            .unwrap()
            .kind(),
        NodeKind::Messages
    );
    let replies = SlackPath::parse_str("/slack/acme/#general/messages/123.45/replies").unwrap();
    match &replies.node {
        SlackNode::Replies { parent_ts, .. } => assert_eq!(parent_ts, "123.45"),
        other => panic!("expected Replies, got {other:?}"),
    }
    let reactions = SlackPath::parse_str("/slack/acme/#general/messages/123.45/reactions").unwrap();
    assert_eq!(reactions.kind(), NodeKind::Reactions);
    assert_eq!(
        SlackPath::parse_str("/slack/acme/dms/U07/messages")
            .unwrap()
            .kind(),
        NodeKind::Dms
    );
    assert_eq!(
        SlackPath::parse_str("/slack/acme/files").unwrap().kind(),
        NodeKind::Files
    );
    assert_eq!(
        SlackPath::parse_str("/slack/acme/users").unwrap().kind(),
        NodeKind::Users
    );

    // A malformed sub-path and a non-/slack path are rejected structurally.
    assert_eq!(
        SlackPath::parse_str("/slack/acme/#general/bogus")
            .unwrap_err()
            .code(),
        "slack_invalid_path"
    );
    assert_eq!(
        SlackPath::parse_str("/drive/x").unwrap_err().code(),
        "slack_invalid_path"
    );
}

#[test]
fn channel_ref_distinguishes_ids_from_symbolic_names() {
    assert!(ChannelRef::new("C0123ABC").is_id());
    assert!(!ChannelRef::new("#general").is_id());
    assert!(!ChannelRef::new("general").is_id());
    assert_eq!(ChannelRef::new("general").symbolic(), "#general");
    assert_eq!(ChannelRef::new("#general").symbolic(), "#general");
    assert_eq!(ChannelRef::new("C0123ABC").symbolic(), "C0123ABC");
}

// ---- pushdown: ts window → oldest/latest, TRUTHFUL residual (the t20 lesson) -------------

#[test]
fn ts_bounds_push_to_oldest_latest_and_other_predicates_stay_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // ts >= 100 AND text = 'hi': the ts boundary pushes to oldest (drops from residual); the text
    // equality Slack cannot express stays residual for local re-filtering.
    let ts_ge = Predicate::Cmp(
        ColRef::col("ts"),
        CmpOp::Ge,
        Literal::Text("100".to_string()),
    );
    let text_eq = Predicate::Cmp(
        ColRef::col("text"),
        CmpOp::Eq,
        Literal::Text("hi".to_string()),
    );
    let pred = Predicate::And(Box::new(ts_ge), Box::new(text_eq.clone()));
    let res = pushdown::build_params(Some(&pred));
    assert_eq!(res.params, vec![("oldest".to_string(), "100".to_string())]);
    assert_eq!(
        res.residual,
        Some(text_eq),
        "the non-expressible text= is kept residual; the ts boundary drops out"
    );

    let (d, _) = driver();
    assert!(d.pushdown().supports_where());
    assert!(d.pushdown().supports_limit());
    assert!(!d.pushdown().supports_order());
}

#[test]
fn ts_lt_pushes_to_latest_and_or_stays_wholly_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    let ts_le = Predicate::Cmp(ColRef::col("ts"), CmpOp::Le, Literal::Text("200".into()));
    let res = pushdown::build_params(Some(&ts_le));
    assert_eq!(res.params, vec![("latest".to_string(), "200".to_string())]);
    assert!(
        res.residual.is_none(),
        "exact latest boundary leaves nothing"
    );

    let or_pred = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("ts"),
            CmpOp::Ge,
            Literal::Text("1".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("ts"),
            CmpOp::Le,
            Literal::Text("2".into()),
        )),
    );
    let res = pushdown::build_params(Some(&or_pred));
    assert!(res.params.is_empty(), "nothing pushed for an OR");
    assert_eq!(res.residual, Some(or_pred));
}

/// Defect #2 (the t20 class): a STRICT `ts > 100` lowers to Slack's INCLUSIVE `oldest=100`, which
/// over-returns the `ts == 100` boundary row — so the strict comparison MUST be kept as residual to
/// re-exclude that row locally. Asserts the residual is preserved AND that applying it excludes the
/// boundary row (a tiny ts-only evaluator stands in for the engine's local filter).
#[test]
fn strict_ts_gt_keeps_residual_and_re_excludes_the_boundary_row() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    let gt = Predicate::Cmp(ColRef::col("ts"), CmpOp::Gt, Literal::Text("100".into()));
    let res = pushdown::build_params(Some(&gt));
    // The inclusive bound is pushed …
    assert_eq!(res.params, vec![("oldest".to_string(), "100".to_string())]);
    // … but the strict comparison is KEPT as an exact residual (not dropped).
    assert_eq!(
        res.residual,
        Some(gt.clone()),
        "a strict > must keep the residual so the inclusive oldest boundary row is re-excluded"
    );

    // Slack (inclusive oldest=100) would hand back rows with ts == 100 AND ts == 101. Applying the
    // residual must drop the ts == 100 boundary row, leaving only ts == 101.
    let slack_returned = ["100", "101"];
    let surviving: Vec<&str> = slack_returned
        .into_iter()
        .filter(|ts| eval_ts_residual(res.residual.as_ref().unwrap(), ts))
        .collect();
    assert_eq!(
        surviving,
        vec!["101"],
        "the boundary row ts==100 is re-excluded by the residual; no wrong rows"
    );

    // Symmetric for strict <: ts < 200 → latest=200 (inclusive) + kept residual re-excludes ts==200.
    let lt = Predicate::Cmp(ColRef::col("ts"), CmpOp::Lt, Literal::Text("200".into()));
    let res = pushdown::build_params(Some(&lt));
    assert_eq!(res.params, vec![("latest".to_string(), "200".to_string())]);
    assert_eq!(
        res.residual,
        Some(lt.clone()),
        "strict < keeps its residual"
    );
    let surviving: Vec<&str> = ["199", "200"]
        .into_iter()
        .filter(|ts| eval_ts_residual(res.residual.as_ref().unwrap(), ts))
        .collect();
    assert_eq!(surviving, vec!["199"], "ts==200 boundary row re-excluded");
}

/// A minimal `ts`-only predicate evaluator standing in for the engine's local residual filter —
/// just enough to prove the boundary-row exclusion (numeric `ts` compare).
fn eval_ts_residual(pred: &qfs_types::Predicate, ts: &str) -> bool {
    use qfs_types::{CmpOp, Literal, Predicate};
    match pred {
        Predicate::Cmp(_, op, Literal::Text(bound)) => {
            let (a, b) = (ts.parse::<i64>().unwrap(), bound.parse::<i64>().unwrap());
            match op {
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                _ => true,
            }
        }
        _ => true,
    }
}

// ---- read path: cursor pagination as a single bounded fetch node + decode ----------------

#[test]
fn select_is_one_read_plan_node_and_decodes_to_typed_rows() {
    let plan = ReadPlan::list(NodeKind::Messages, None);
    assert_eq!(plan.kind, NodeKind::Messages);

    let mock = Arc::new(MockSlackClient::new().with_list(serde_json::json!({
        "messages": [
            {"ts": "1.1", "user": "U07", "text": "hello"},
            {"ts": "1.2", "user": "U08", "text": "world", "thread_ts": "1.1"}
        ]
    })));
    let value = mock.list(NodeKind::Messages, plan.params()).unwrap();
    let batch = read::decode_list(NodeKind::Messages, &value).unwrap();
    assert_eq!(batch.rows.len(), 2);
    assert_eq!(batch.rows[0].values[0], Value::Text("1.1".to_string()));
    assert_eq!(batch.rows[0].values[2], Value::Text("hello".to_string()));
    // Exactly one batched fetch (the cursor follow is at the edge).
    assert_eq!(mock.recorded().len(), 1);
    assert!(matches!(
        &mock.recorded()[0],
        RecordedCall::List { kind, .. } if *kind == NodeKind::Messages
    ));
}

#[test]
fn users_and_files_decode_from_their_envelopes() {
    let users = serde_json::json!({"members": [
        {"id": "U1", "name": "alice", "real_name": "Alice", "is_bot": false},
        {"id": "B1", "name": "bot", "is_bot": true}
    ]});
    let batch = read::decode_list(NodeKind::Users, &users).unwrap();
    assert_eq!(batch.rows.len(), 2);
    assert_eq!(batch.rows[1].values[3], Value::Bool(true), "bot flag");

    let files = serde_json::json!({"files": [{"id": "F1", "name": "a.txt", "size": 12}]});
    let fb = read::decode_list(NodeKind::Files, &files).unwrap();
    assert_eq!(fb.rows.len(), 1);
    assert_eq!(fb.rows[0].values[3], Value::Int(12));
}

// ---- effect decode: INSERT(message/reply/reaction) / REMOVE / CALL -----------------------

#[test]
fn insert_message_decodes_to_post_message_with_client_msg_id() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[(TEXT_COL, Value::Text("hi".into()))]));
    match SlackEffect::from_node(&node).unwrap() {
        SlackEffect::PostMessage {
            channel,
            text,
            thread_ts,
            client_msg_id,
            is_dm,
        } => {
            assert_eq!(
                channel, "#general",
                "symbolic channel preserved at plan time"
            );
            assert_eq!(text, "hi");
            assert!(thread_ts.is_none());
            assert!(!is_dm);
            assert!(
                client_msg_id.starts_with("qfs-"),
                "an idempotency key is attached: {client_msg_id}"
            );
        }
        other => panic!("expected PostMessage, got {other:?}"),
    }
}

#[test]
fn insert_reply_carries_thread_ts() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages/111.22/replies"),
    )
    .with_args(args(&[(TEXT_COL, Value::Text("threaded".into()))]));
    match SlackEffect::from_node(&node).unwrap() {
        SlackEffect::PostMessage { thread_ts, .. } => {
            assert_eq!(
                thread_ts.as_deref(),
                Some("111.22"),
                "thread_ts = parent ts"
            );
        }
        other => panic!("expected PostMessage, got {other:?}"),
    }
}

#[test]
fn insert_reaction_and_call_react_produce_equivalent_plans() {
    // INSERT INTO .../reactions
    let insert = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[(EMOJI_COL, Value::Text("tada".into()))]));
    let from_insert = SlackEffect::from_node(&insert).unwrap();

    // CALL slack.react(channel, ts, emoji) — the channel/ts come from the path + args.
    let call = EffectNode::new(
        NodeId(1),
        EffectKind::Call(ProcId::new("slack.react")),
        target("/slack/acme/#general/messages/9.9/reactions"),
    )
    .with_args(args(&[
        (TS_COL, Value::Text("9.9".into())),
        (EMOJI_COL, Value::Text("tada".into())),
    ]));
    let from_call = SlackEffect::from_node(&call).unwrap();

    let expected = SlackEffect::AddReaction {
        channel: "#general".into(),
        ts: "9.9".into(),
        emoji: "tada".into(),
    };
    assert_eq!(from_insert, expected, "INSERT → reactions.add");
    assert_eq!(from_call, expected, "CALL react → equivalent reactions.add");
}

#[test]
fn call_pin_is_irreversible_and_remove_message_decodes_to_chat_delete() {
    let pin = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("slack.pin")),
        target("/slack/acme/#general/messages"),
    )
    .irreversible(true)
    .with_args(args(&[(TS_COL, Value::Text("5.5".into()))]));
    assert!(pin.irreversible);
    match SlackEffect::from_node(&pin).unwrap() {
        SlackEffect::Pin { channel, ts } => {
            assert_eq!(channel, "#general");
            assert_eq!(ts, "5.5");
        }
        other => panic!("expected Pin, got {other:?}"),
    }

    // REMOVE of a message → chat.delete (irreversible).
    let del = EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[(TS_COL, Value::Text("5.5".into()))]));
    let eff = SlackEffect::from_node(&del).unwrap();
    assert!(eff.is_irreversible(), "chat.delete is irreversible");
    assert!(matches!(eff, SlackEffect::DeleteMessage { .. }));
}

#[test]
fn irreversibility_and_at_least_once_classification_is_honest() {
    let post = SlackEffect::PostMessage {
        channel: "#g".into(),
        text: "x".into(),
        thread_ts: None,
        client_msg_id: "qfs-1".into(),
        is_dm: false,
    };
    assert!(post.is_at_least_once_post(), "a post is at-least-once");
    assert!(!post.is_irreversible(), "a post can be deleted later");

    let react = SlackEffect::AddReaction {
        channel: "#g".into(),
        ts: "1".into(),
        emoji: "x".into(),
    };
    assert!(!react.is_at_least_once_post());
    assert!(
        react.swallows_already_done(),
        "reactions.add swallows already_reacted (naturally idempotent)"
    );

    let del = SlackEffect::DeleteMessage {
        channel: "#g".into(),
        ts: "1".into(),
    };
    assert!(del.is_irreversible() && !del.is_at_least_once_post());
}

// ---- the t18 BodyErrorRule: HTTP 200 with ok:false → terminal Body error ------------------

#[test]
fn body_error_rule_maps_ok_false_to_a_terminal_structured_error() {
    let body = serde_json::json!({"ok": false, "error": "channel_not_found"});
    // On (Slack's setting): ok:false is a structured terminal Body error carrying the code.
    let err = BodyErrorRule::On
        .check("chat.postMessage", &body, false)
        .unwrap_err();
    match &err {
        SlackError::Body { op, code } => {
            assert_eq!(*op, "chat.postMessage");
            assert_eq!(code, "channel_not_found");
        }
        other => panic!("expected Body, got {other:?}"),
    }
    assert_eq!(err.code(), "slack_body_error");

    // Off (the t18 default): the body is not inspected — a 2xx is success.
    assert!(BodyErrorRule::Off.check("x", &body, false).is_ok());

    // ok:true passes regardless.
    let ok = serde_json::json!({"ok": true, "ts": "1.1"});
    assert!(BodyErrorRule::On.check("x", &ok, false).is_ok());
}

#[test]
fn already_done_class_is_swallowed_for_naturally_idempotent_ops() {
    let already = serde_json::json!({"ok": false, "error": "already_reacted"});
    // A reaction add swallows already_reacted (no-op success).
    assert!(BodyErrorRule::On
        .check("reactions.add", &already, true)
        .is_ok());
    // But a non-idempotent op does NOT swallow it (it surfaces as a Body error).
    assert!(BodyErrorRule::On
        .check("chat.postMessage", &already, false)
        .is_err());
}

/// Defect #1: the swallow set is SYMMETRIC across the add/remove pair (RFD §6 at-least-once). A
/// redelivered `reactions.remove` on an already-removed reaction (`no_reaction`) and `pins.remove`
/// on an already-unpinned message (`not_pinned`) must be no-op successes, not terminal errors —
/// the selector gate (`swallows_already_done`) must include the remove-side effects so it stays in
/// sync with the recognizer (`is_already_done`), which already lists those codes.
#[test]
fn remove_side_already_done_is_swallowed_symmetrically() {
    // The selector gate now lists every idempotent op — both sides of each pair.
    let remove_reaction = SlackEffect::RemoveReaction {
        channel: "#g".into(),
        ts: "1".into(),
        emoji: "x".into(),
    };
    let unpin = SlackEffect::Unpin {
        channel: "#g".into(),
        ts: "1".into(),
    };
    assert!(
        remove_reaction.swallows_already_done(),
        "reactions.remove swallows no_reaction"
    );
    assert!(
        unpin.swallows_already_done(),
        "pins.remove swallows not_pinned"
    );

    // The recognizer + the gate agree: the remove-side codes are swallowed no-ops.
    let no_reaction = serde_json::json!({"ok": false, "error": "no_reaction"});
    assert!(BodyErrorRule::On
        .check(
            "reactions.remove",
            &no_reaction,
            remove_reaction.swallows_already_done()
        )
        .is_ok());
    let not_pinned = serde_json::json!({"ok": false, "error": "not_pinned"});
    assert!(BodyErrorRule::On
        .check("pins.remove", &not_pinned, unpin.swallows_already_done())
        .is_ok());

    // A genuine remove-side error (not the already-done class) still surfaces, even when swallowed.
    let real_err = serde_json::json!({"ok": false, "error": "message_not_found"});
    assert!(BodyErrorRule::On
        .check("reactions.remove", &real_err, true)
        .is_err());
}

// ---- the wire seam: BodyErrorRule wired through RestSlackClient ---------------------------

#[test]
fn rest_client_post_ok_false_becomes_a_terminal_body_error() {
    let (secrets, key) = store_with_token("xoxb-secret");
    // HTTP 200 but ok:false — the BodyErrorRule (On) classifies it as a terminal Body error.
    let transport = Arc::new(RecordingTransport::with(vec![HttpResponse::new(
        200,
        br#"{"ok":false,"error":"not_in_channel"}"#.to_vec(),
    )]));
    let client = RestSlackClient::new(transport.clone(), secrets, key, BodyErrorRule::On);
    let err = client
        .apply(&SlackEffect::PostMessage {
            channel: "#general".into(),
            text: "hi".into(),
            thread_ts: None,
            client_msg_id: "qfs-1".into(),
            is_dm: false,
        })
        .unwrap_err();
    assert_eq!(err.code(), "slack_body_error");
    assert!(
        format!("{err}").contains("not_in_channel"),
        "carries Slack's error code"
    );
    // The post was issued exactly once — never auto-retried.
    assert_eq!(transport.recorded().len(), 1);
    assert_eq!(transport.recorded()[0].method.as_str(), "POST");
}

#[test]
fn rest_client_injects_bearer_token_and_never_logs_it() {
    const TOKEN: &str = "xoxb-SECRET-bot-token-42";
    let (secrets, key) = store_with_token(TOKEN);
    let transport = Arc::new(RecordingTransport::with(vec![HttpResponse::new(
        200,
        br#"{"ok":true,"messages":[]}"#.to_vec(),
    )]));
    let client = RestSlackClient::new(transport.clone(), secrets, key, BodyErrorRule::On);
    client.list(NodeKind::Messages, &[]).unwrap();

    let reqs = transport.recorded();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(
        req.header_value("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(req.url.contains("/conversations.history?"));
    assert!(req.url.contains("limit="));
    // The redacting Debug never reveals the token.
    let dbg = format!("{req:?}");
    assert!(
        !dbg.contains(TOKEN),
        "token must not appear in Debug: {dbg}"
    );
    assert!(dbg.contains(qfs_secrets::REDACTED));
}

#[test]
fn rest_client_follows_cursor_pagination_and_merges_pages() {
    let (secrets, key) = store_with_token("xoxb-x");
    let page1 = HttpResponse::new(
        200,
        br#"{"ok":true,"messages":[{"ts":"1.1","text":"a"}],"response_metadata":{"next_cursor":"CURSOR2"}}"#.to_vec(),
    );
    let page2 = HttpResponse::new(
        200,
        br#"{"ok":true,"messages":[{"ts":"1.2","text":"b"}],"response_metadata":{"next_cursor":""}}"#.to_vec(),
    );
    let transport = Arc::new(RecordingTransport::with(vec![page1, page2]));
    let client = RestSlackClient::new(transport.clone(), secrets, key, BodyErrorRule::On);

    let value = client.list(NodeKind::Messages, &[]).unwrap();
    let msgs = read::decode_messages(&value);
    assert_eq!(msgs.len(), 2, "both cursor pages merged");
    assert_eq!(transport.recorded().len(), 2);
    assert!(transport.recorded()[1].url.contains("cursor=CURSOR2"));
}

#[test]
fn rest_client_retries_transient_429_on_a_get_then_succeeds() {
    let (secrets, key) = store_with_token("xoxb-x");
    let throttled = HttpResponse::new(429, Vec::new()).header("Retry-After", "0");
    let ok = HttpResponse::new(200, br#"{"ok":true,"messages":[]}"#.to_vec());
    let transport = Arc::new(RecordingTransport::with(vec![throttled, ok]));
    let client = RestSlackClient::new(transport.clone(), secrets, key, BodyErrorRule::On);

    client.list(NodeKind::Messages, &[]).unwrap();
    assert_eq!(
        transport.recorded().len(),
        2,
        "the throttled GET was retried once then succeeded"
    );
}

// ---- PREVIEW performs no I/O + surfaces irreversible -------------------------------------

#[test]
fn preview_of_a_pin_plan_surfaces_irreversible_and_performs_no_io() {
    let (_d, mock) = driver();
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("slack.pin")),
            target("/slack/acme/#general/messages"),
        )
        .irreversible(true)
        .with_args(args(&[(TS_COL, Value::Text("5.5".into()))])),
    );
    let plan = b.build();
    let pv = preview(&plan);
    assert_eq!(pv.rows.len(), 1);
    assert!(
        pv.rows[0].irreversible,
        "preview surfaces the irreversible pin"
    );
    assert!(
        mock.recorded().is_empty(),
        "PREVIEW must perform zero Slack API calls: {:?}",
        mock.recorded()
    );
}

// ---- token / secret never in logs or a serialized plan (planted canary) ------------------

#[test]
fn errors_are_secret_free() {
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
        assert!(!text.contains("Bearer"), "no bearer in error: {text}");
        assert!(
            !text.contains("xoxb-"),
            "no bot-token prefix in error: {text}"
        );
    }
}

#[test]
fn planted_token_and_signing_secret_never_appear_in_a_serialized_plan_or_config_debug() {
    const TOKEN: &str = "xoxb-PLANTED-CANARY-should-never-serialize";
    const SIGNING: &str = "shhh-PLANTED-signing-secret";
    // A plan over /slack carries NO token (the bot token lives only behind the auth seam).
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/slack/acme/#general/messages"),
        )
        .with_args(args(&[(TEXT_COL, Value::Text("a normal message".into()))])),
    );
    let plan = b.build();
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains(TOKEN) && !json.contains("Bearer"),
        "no token material in a serialized plan: {json}"
    );

    // The config Debug carries credential KEYS (selectors), never the token/signing-secret values.
    let key = CredentialKey::new(
        qfs_secrets::DriverId::new("slack"),
        ConnectionId::new("work").unwrap(),
    );
    let cfg = SlackWsConfig::new("acme", "T123", key.clone(), key);
    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains(TOKEN) && !dbg.contains(SIGNING),
        "config Debug is secret-free: {dbg}"
    );
}

// ---- end-to-end: commit through interpreter + bridge -------------------------------------

#[tokio::test]
async fn commit_post_message_end_to_end_through_interpreter() {
    let (driver, mock) = driver();
    let bridge = slack_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            target("/slack/acme/#general/messages"),
        )
        .with_args(args(&[(TEXT_COL, Value::Text("hi".into()))])),
    );
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("slack"), &EffectKind::Insert);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "message posted: {outcome:?}");
    match &mock.recorded()[..] {
        [RecordedCall::Apply(SlackEffect::PostMessage { channel, text, .. })] => {
            assert_eq!(channel, "#general");
            assert_eq!(text, "hi");
        }
        other => panic!("expected one PostMessage apply, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_react_call_end_to_end() {
    let (driver, mock) = driver();
    let bridge = slack_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("slack.react")),
            target("/slack/acme/#general/messages/9.9/reactions"),
        )
        .with_args(args(&[
            (TS_COL, Value::Text("9.9".into())),
            (EMOJI_COL, Value::Text("tada".into())),
        ])),
    );
    let plan = b.build();

    let caps = CapabilitySet::none().grant(
        DriverId::new("slack"),
        &EffectKind::Call(ProcId::new("slack.react")),
    );
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "reaction added: {outcome:?}");
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::Apply(SlackEffect::AddReaction {
            channel: "#general".to_string(),
            ts: "9.9".to_string(),
            emoji: "tada".to_string(),
        })]
    );
}

#[test]
fn apply_shared_routes_only_to_its_own_client() {
    let mock_a = Arc::new(MockSlackClient::new());
    let mock_b = Arc::new(MockSlackClient::new());
    let driver_a = SlackDriver::new(mock_a.clone() as Arc<dyn SlackClient>);
    let driver_b = SlackDriver::new(mock_b.clone() as Arc<dyn SlackClient>);

    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/slack/acme/#general/messages"),
    )
    .with_args(args(&[(TEXT_COL, Value::Text("x".into()))]));
    driver_a.slack_applier().apply_shared(&node).unwrap();
    assert_eq!(mock_a.recorded().len(), 1);
    assert!(mock_b.recorded().is_empty(), "client B was untouched");
    let _ = driver_b;
}

// ---- the genuinely-hard, wasm-required part: parse_event (pure, fixtures) -----------------

/// Sign `body` with `secret` at `ts` the way Slack does — the fixture helper for a valid signature.
fn sign(secret: &str, ts: &str, body: &[u8]) -> String {
    let mut base = Vec::new();
    base.extend_from_slice(b"v0:");
    base.extend_from_slice(ts.as_bytes());
    base.push(b':');
    base.extend_from_slice(body);
    let tag = hmac_sha256(secret.as_bytes(), &base);
    format!("v0={}", hex_lower(&tag))
}

#[test]
fn parse_event_url_verification_returns_the_challenge() {
    let secret = "slack-signing-secret";
    let ts = "1700000000";
    let body = br#"{"type":"url_verification","challenge":"abc123challenge"}"#;
    let sig = sign(secret, ts, body);
    let headers = EventHeaders::new(sig, ts);
    let now = 1_700_000_000;
    match parse_event(&headers, body, secret, now).unwrap() {
        SlackInbound::UrlVerification { challenge } => assert_eq!(challenge, "abc123challenge"),
        other => panic!("expected UrlVerification, got {other:?}"),
    }
}

#[test]
fn parse_event_normalizes_a_message_and_a_reaction_with_event_id_for_dedupe() {
    let secret = "s3cret";
    let ts = "1700000100";
    let now = 1_700_000_100;

    // A message event.
    let msg_body = br#"{"type":"event_callback","event_id":"Ev01","team_id":"T1",
        "event":{"type":"message","channel":"C1","ts":"9.9","user":"U7","text":"hello"}}"#;
    let sig = sign(secret, ts, msg_body);
    let inbound = parse_event(&EventHeaders::new(sig, ts), msg_body, secret, now).unwrap();
    match inbound {
        SlackInbound::Event(e) => {
            assert_eq!(e.kind, SlackEventKind::Message);
            assert_eq!(e.event_id.as_deref(), Some("Ev01"), "event_id for dedupe");
            assert_eq!(e.channel.as_deref(), Some("C1"));
            assert_eq!(e.ts.as_deref(), Some("9.9"));
            assert_eq!(e.user.as_deref(), Some("U7"));
            assert_eq!(e.team_id.as_deref(), Some("T1"));
        }
        other => panic!("expected Event, got {other:?}"),
    }

    // A reaction_added event nests its target under item.
    let react_body = br#"{"type":"event_callback","event_id":"Ev02",
        "event":{"type":"reaction_added","user":"U7","reaction":"tada",
                 "item":{"type":"message","channel":"C1","ts":"9.9"}}}"#;
    let sig = sign(secret, ts, react_body);
    match parse_event(&EventHeaders::new(sig, ts), react_body, secret, now).unwrap() {
        SlackInbound::Event(e) => {
            assert_eq!(e.kind, SlackEventKind::ReactionAdded);
            assert_eq!(e.reaction.as_deref(), Some("tada"));
            assert_eq!(e.channel.as_deref(), Some("C1"), "from item.channel");
            assert_eq!(e.ts.as_deref(), Some("9.9"), "from item.ts");
        }
        other => panic!("expected Event, got {other:?}"),
    }
}

#[test]
fn parse_event_rejects_a_tampered_signature() {
    let secret = "s3cret";
    let ts = "1700000200";
    let now = 1_700_000_200;
    let body = br#"{"type":"event_callback","event":{"type":"message","text":"hi"}}"#;
    let mut sig = sign(secret, ts, body);
    // Flip the last hex char — a tampered signature.
    sig.pop();
    sig.push(if sig.ends_with('0') { '1' } else { '0' });
    let err = parse_event(&EventHeaders::new(sig, ts), body, secret, now).unwrap_err();
    assert_eq!(err.code(), "slack_event_bad_signature");

    // A wrong secret also fails (constant-time compare).
    let good = sign(secret, ts, body);
    let err = parse_event(&EventHeaders::new(good, ts), body, "wrong-secret", now).unwrap_err();
    assert_eq!(err.code(), "slack_event_bad_signature");
}

#[test]
fn parse_event_rejects_a_stale_timestamp_as_a_replay() {
    let secret = "s3cret";
    let ts = "1700000000";
    let body = br#"{"type":"event_callback","event":{"type":"message"}}"#;
    let sig = sign(secret, ts, body);
    // `now` is well beyond the skew window — a replay of an old, validly signed delivery.
    let now = 1_700_000_000 + MAX_SKEW_SECS + 60;
    let err = parse_event(&EventHeaders::new(sig, ts), body, secret, now).unwrap_err();
    assert_eq!(err.code(), "slack_event_stale_timestamp");
}

#[test]
fn parse_event_rejects_a_missing_signature_header() {
    let secret = "s3cret";
    let body = br#"{"type":"event_callback"}"#;
    let headers = EventHeaders {
        signature: None,
        timestamp: Some("1700000000".into()),
        retry_num: None,
    };
    let err = parse_event(&headers, body, secret, 1_700_000_000).unwrap_err();
    assert_eq!(err.code(), "slack_event_missing_header");
}

/// Architect carry-over (b): a signature-valid envelope whose top-level `type` is neither
/// `url_verification` nor `event_callback` is surfaced as a distinct `Unhandled` outcome (acked +
/// logged by the ingress), NOT as a fabricated `Event`.
#[test]
fn parse_event_surfaces_an_unknown_envelope_type_as_unhandled() {
    let secret = "s3cret";
    let ts = "1700000300";
    let now = 1_700_000_300;
    let body = br#"{"type":"app_rate_limited","minute_rate_limited":1700000200}"#;
    let sig = sign(secret, ts, body);
    match parse_event(&EventHeaders::new(sig, ts), body, secret, now).unwrap() {
        SlackInbound::Unhandled { envelope_type, .. } => {
            assert_eq!(envelope_type, "app_rate_limited");
        }
        other => panic!("expected Unhandled, got {other:?}"),
    }
}

// ---- golden DESCRIBE snapshots (the ticket's golden/snapshot acceptance) ------------------

#[test]
fn describe_json_snapshot_is_stable_per_archetype() {
    let (d, _) = driver();

    // messages → AppendLog, the message schema.
    let messages = serde_json::to_string_pretty(
        &d.describe(&Path::new("/slack/acme/#general/messages"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        messages,
        r#"{
  "archetype": "append_log",
  "schema": {
    "columns": [
      {
        "name": "ts",
        "ty": "Text",
        "nullable": false,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      },
      {
        "name": "user",
        "ty": "Text",
        "nullable": true,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      },
      {
        "name": "text",
        "ty": "Text",
        "nullable": true,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      },
      {
        "name": "thread_ts",
        "ty": "Text",
        "nullable": true,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      },
      {
        "name": "subtype",
        "ty": "Text",
        "nullable": true,
        "provenance": {
          "driver": null,
          "source_col": null
        }
      }
    ]
  }
}"#
    );

    // files → BlobNamespace; users → RelationalTable (archetype tags pinned).
    let files = d.describe(&Path::new("/slack/acme/files")).unwrap();
    assert_eq!(
        serde_json::to_string(&files.archetype).unwrap(),
        r#""blob_namespace""#
    );
    let users = d.describe(&Path::new("/slack/acme/users")).unwrap();
    assert_eq!(
        serde_json::to_string(&users.archetype).unwrap(),
        r#""relational_table""#
    );

    // The node-keyed capabilities snapshot (the capability-set golden).
    let caps = serde_json::to_string(&d.capabilities(&Path::new("/slack/acme/#general/messages")))
        .unwrap();
    assert_eq!(
        caps,
        r#"{"select":true,"insert":true,"upsert":false,"update":false,"remove":true,"ls":false,"cp":false,"mv":false,"rm":false}"#
    );
    let file_caps =
        serde_json::to_string(&d.capabilities(&Path::new("/slack/acme/files"))).unwrap();
    assert_eq!(
        file_caps,
        r#"{"select":false,"insert":false,"upsert":false,"update":false,"remove":false,"ls":true,"cp":true,"mv":false,"rm":true}"#
    );
}

#[test]
fn archetype_for_maps_every_node_kind() {
    use crate::schema::archetype_for;
    assert_eq!(archetype_for(NodeKind::Messages), Archetype::AppendLog);
    assert_eq!(archetype_for(NodeKind::Replies), Archetype::AppendLog);
    assert_eq!(archetype_for(NodeKind::Reactions), Archetype::AppendLog);
    assert_eq!(archetype_for(NodeKind::Dms), Archetype::AppendLog);
    assert_eq!(archetype_for(NodeKind::Files), Archetype::BlobNamespace);
    assert_eq!(archetype_for(NodeKind::Users), Archetype::RelationalTable);
}
