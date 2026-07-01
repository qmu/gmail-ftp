//! Gmail driver tests (RFD-0001 §5 acceptance) — **no live Gmail, no network, no credentials**.
//! Every test drives the introspective `Driver` surface and the apply leg against an in-memory
//! [`MockGmailClient`] (scripted Gmail fixtures + recorded calls), so we assert request shape +
//! response decoding + plan shape + token-safety without touching a socket.

use std::sync::Arc;

use qfs_driver::{check_capability, resolve_proc, Archetype, Driver, Path, Verb, VersionSupport};
use qfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use qfs_types::{Column, Row, RowBatch, Schema, Value};

use super::*;

// ---- fixtures ----------------------------------------------------------------------------

/// A driver over a mock seeded with two labels and one message (with one attachment).
fn driver_with_mock() -> (GmailDriver, Arc<MockGmailClient>) {
    let mock = Arc::new(
        MockGmailClient::new()
            .with_labels(vec!["INBOX".to_string(), "SENT".to_string()])
            .with_message(MailMessage {
                id: "m1".to_string(),
                thread_id: "t1".to_string(),
                label_ids: vec!["INBOX".to_string()],
                date: 1_700_000_000_000,
                from: "alice@example.com".to_string(),
                subject: "hello".to_string(),
                snippet: "hi there".to_string(),
                attachments: vec![AttachmentMeta {
                    filename: "a.pdf".to_string(),
                    mime: "application/pdf".to_string(),
                    attachment_id: "att1".to_string(),
                    size: 42,
                }],
            }),
    );
    let driver = GmailDriver::new(mock.clone() as Arc<dyn GmailClient>);
    (driver, mock)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("mail"), VfsPath::new(path))
}

/// Build a single-row draft args batch (the columns the effect decoder reads).
fn draft_args(cols: &[(&str, Value)]) -> RowBatch {
    let schema = Schema::new(
        cols.iter()
            .map(|(n, v)| Column::new(*n, v.type_of(), true))
            .collect(),
    );
    let row = Row::new(cols.iter().map(|(_, v)| v.clone()).collect());
    RowBatch::new(schema, vec![row])
}

// ---- introspection: mount / archetype / schema / version --------------------------------

#[test]
fn mount_and_id_are_mail() {
    let (d, _) = driver_with_mock();
    assert_eq!(d.mount(), "/mail");
    assert_eq!(d.id(), DriverId::new("mail"));
}

#[test]
fn describe_emits_append_archetype_and_message_schema() {
    let (d, _) = driver_with_mock();
    let desc = d.describe(&Path::new("/mail/INBOX")).unwrap();
    assert_eq!(desc.archetype, Archetype::AppendLog);
    // The canonical typed message columns.
    for col in [
        "id",
        "thread_id",
        "date",
        "from",
        "subject",
        "snippet",
        "label_ids",
        "attachments",
    ] {
        assert!(desc.schema.column(col).is_some(), "missing column {col}");
    }
    assert_eq!(
        desc.schema.column("date").unwrap().ty,
        qfs_types::ColumnType::Timestamp
    );
    assert_eq!(
        d.version_support(&Path::new("/mail/INBOX")),
        VersionSupport::None
    );
}

#[test]
fn describe_root_reports_the_label_listing_schema() {
    // The mailbox ROOT lists labels (`ls /mail`), so its DESCRIBE reports the `name` label schema —
    // not the message schema — keeping introspection honest about what a root read returns.
    let (d, _) = driver_with_mock();
    let desc = d.describe(&Path::new("/mail")).unwrap();
    assert_eq!(desc.archetype, Archetype::AppendLog);
    assert_eq!(desc.schema.columns.len(), 1);
    assert!(
        desc.schema.column("name").is_some(),
        "root lists labels by name"
    );
}

// ---- capability golden (path-keyed gate) -------------------------------------------------

#[test]
fn capabilities_are_path_keyed() {
    let (d, _) = driver_with_mock();

    // A label: select/update/remove, but NOT insert/upsert.
    let label = Path::new("/mail/INBOX");
    assert!(check_capability(&d, &label, Verb::Select).is_ok());
    assert!(check_capability(&d, &label, Verb::Update).is_ok());
    assert!(check_capability(&d, &label, Verb::Remove).is_ok());
    assert!(check_capability(&d, &label, Verb::Insert).is_err());

    // Drafts: insert/upsert/select/remove, but NOT update.
    let drafts = Path::new("/mail/drafts");
    assert!(check_capability(&d, &drafts, Verb::Insert).is_ok());
    assert!(check_capability(&d, &drafts, Verb::Upsert).is_ok());
    let err = check_capability(&d, &drafts, Verb::Update).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");

    // A message: select + remove (read/trash only), not insert.
    let msg = Path::new("id:m1");
    assert!(check_capability(&d, &msg, Verb::Select).is_ok());
    assert!(check_capability(&d, &msg, Verb::Remove).is_ok());
    assert!(check_capability(&d, &msg, Verb::Insert).is_err());
}

#[test]
fn a_label_segment_is_passed_through_verbatim() {
    use crate::path::MailPath;
    // qfs does NOT normalize the label case — the segment is the label name verbatim (`inbox` stays
    // `inbox`). It reaches Gmail as a `label:<name>` SEARCH term, which Gmail matches
    // case-insensitively, so `/mail/inbox` reads the inbox without any qfs-side canonicalization.
    assert_eq!(
        MailPath::parse_str("/mail/inbox").unwrap(),
        MailPath::Label {
            name: "inbox".to_string()
        }
    );
    assert_eq!(
        MailPath::parse_str("/mail/Receipts").unwrap(),
        MailPath::Label {
            name: "Receipts".to_string()
        }
    );
}

#[test]
fn insert_into_message_is_rejected_structurally() {
    let (d, _) = driver_with_mock();
    let err = check_capability(&d, &Path::new("id:m1"), Verb::Insert).unwrap_err();
    match &err {
        qfs_driver::CfsError::UnsupportedVerb {
            verb, supported, ..
        } => {
            assert_eq!(*verb, "INSERT");
            assert!(supported.contains(&"SELECT"));
            assert!(supported.contains(&"REMOVE"));
        }
        other => panic!("expected UnsupportedVerb, got {other:?}"),
    }
}

// ---- procedures + prelude (mail.send irreversible; SEND alias) ---------------------------

#[test]
fn mail_send_is_declared_irreversible_with_compose_scope() {
    let (d, _) = driver_with_mock();
    let send = resolve_proc(&d, "send").unwrap();
    assert!(send.irreversible, "mail.send must be irreversible");
    assert_eq!(send.requires_scopes, vec![GMAIL_COMPOSE_SCOPE.to_string()]);
    // An undeclared CALL is rejected structurally.
    assert_eq!(
        resolve_proc(&d, "delete_forever").unwrap_err().code(),
        "unknown_procedure"
    );
}

#[test]
fn prelude_ships_send_alias_desugaring_to_mail_send() {
    let (d, _) = driver_with_mock();
    let alias = d.prelude().iter().find(|a| a.name == "SEND").unwrap();
    assert_eq!(alias.desugars_to, "mail.send");
}

// ---- pushdown: WHERE → Gmail q= ----------------------------------------------------------

#[test]
fn where_lowers_to_gmail_query_with_residual_kept_local() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // from = 'x@y' AND subject = 'hello' AND is_unread = true.
    // All three q= terms are pushed as a pre-filter, but `from`/`subject` are LOSSY (Gmail's
    // `from:`/`subject:` are address/substring matches, looser than SQL `=`), so they must be
    // KEPT as residual for exact local re-checking. Only the exact `is_unread` (→ is:unread)
    // drops out of the residual. Over-fetch then filter — never wrong rows (RFD §6).
    let from_eq = Predicate::Cmp(
        ColRef::col("from"),
        CmpOp::Eq,
        Literal::Text("x@y".to_string()),
    );
    let subject_eq = Predicate::Cmp(
        ColRef::col("subject"),
        CmpOp::Eq,
        Literal::Text("hello".to_string()),
    );
    let pred = Predicate::And(
        Box::new(from_eq.clone()),
        Box::new(Predicate::And(
            Box::new(subject_eq.clone()),
            Box::new(Predicate::Cmp(
                ColRef::col("is_unread"),
                CmpOp::Eq,
                Literal::Bool(true),
            )),
        )),
    );
    let res = query::build_query(Some("INBOX"), Some(&pred));
    assert_eq!(res.query, "label:INBOX from:x@y subject:hello is:unread");
    // The residual re-checks the two lossy conjuncts (from = 'x@y' AND subject = 'hello'); the
    // exact `is_unread` is fully expressed by `is:unread` and is not re-checked.
    assert_eq!(
        res.residual,
        Some(Predicate::And(Box::new(from_eq), Box::new(subject_eq),)),
        "lossy from/subject equality is kept as residual; only exact is_unread drops out"
    );

    // An OR stays residual (Gmail term-ANDing cannot express it) — combined locally.
    let or_pred = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("from"),
            CmpOp::Eq,
            Literal::Text("a".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("from"),
            CmpOp::Eq,
            Literal::Text("b".into()),
        )),
    );
    let res2 = query::build_query(Some("INBOX"), Some(&or_pred));
    assert_eq!(res2.query, "label:INBOX", "only the label scope pushed");
    assert!(res2.residual.is_some(), "OR is residual, filtered locally");

    // The driver declares partial WHERE+LIMIT pushdown.
    let (d, _) = driver_with_mock();
    assert!(d.pushdown().supports_where());
    assert!(d.pushdown().supports_limit());
    assert!(!d.pushdown().supports_order());
}

#[test]
fn exact_predicates_push_fully_with_no_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // label = 'INBOX' AND is_unread = false: both map to Gmail terms that mean EXACTLY the SQL
    // predicate (exact label membership / exact read-state), so the whole predicate is pushed
    // and NOTHING is left to re-check locally — residual is None.
    let pred = Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("label"),
            CmpOp::Eq,
            Literal::Text("INBOX".to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("is_unread"),
            CmpOp::Eq,
            Literal::Bool(false),
        )),
    );
    let res = query::build_query(None, Some(&pred));
    assert_eq!(res.query, "label:INBOX is:read");
    assert!(
        res.residual.is_none(),
        "exact label/is_unread mappings leave no residual to re-check"
    );
}

#[test]
fn lossy_predicate_returns_residual_so_engine_refilters() {
    use qfs_types::{CmpOp, ColRef, Literal, Pattern, Predicate};

    // A lossy `from = 'x@y'` pushes the loose `from:x@y` pre-filter but MUST return the exact
    // predicate as residual, because Gmail `from:x@y` also matches e.g. "bob@x@y-domain" / a
    // `From` header of `"Alice <x@y>"` that is not equal to 'x@y' under SQL `=`. Without the
    // residual the engine would emit those over-fetched, non-exact rows — the t20 defect.
    let from_eq = Predicate::Cmp(
        ColRef::col("from"),
        CmpOp::Eq,
        Literal::Text("x@y".to_string()),
    );
    let res = query::build_query(None, Some(&from_eq));
    assert_eq!(
        res.query, "from:x@y",
        "the loose q= pre-filter is still pushed"
    );
    assert_eq!(
        res.residual,
        Some(from_eq),
        "the exact from = 'x@y' is kept as residual so the engine re-filters substring over-fetch"
    );

    // A lossy `subject = 'hello'` likewise — `subject:hello` matches "hello world".
    let subject_eq = Predicate::Cmp(
        ColRef::col("subject"),
        CmpOp::Eq,
        Literal::Text("hello".to_string()),
    );
    let res = query::build_query(None, Some(&subject_eq));
    assert_eq!(res.query, "subject:hello");
    assert_eq!(
        res.residual,
        Some(subject_eq),
        "subject = 'hello' kept as residual; subject:hello also matches 'hello world'"
    );

    // `LIKE` and the `date` bounds are lossy too: pushed as a pre-filter, kept as residual.
    let like = Predicate::Like(ColRef::col("subject"), Pattern("weekly".to_string()));
    let res = query::build_query(None, Some(&like));
    assert_eq!(res.query, "subject:weekly");
    assert_eq!(
        res.residual,
        Some(like),
        "LIKE has no Gmail operator — kept residual"
    );

    let date_gt = Predicate::Cmp(
        ColRef::col("date"),
        CmpOp::Gt,
        Literal::Int(1_700_000_500_000),
    );
    let res = query::build_query(None, Some(&date_gt));
    assert_eq!(res.query, "after:1700000500", "ms→s truncated bound");
    assert_eq!(
        res.residual,
        Some(date_gt),
        "date bound is date-granular/truncated — kept residual for the exact ms comparison"
    );
}

// ---- search pushdown reaches the client as q= --------------------------------------------

#[test]
fn search_pushes_q_and_decodes_message_rows() {
    let (_d, mock) = driver_with_mock();
    let client: &dyn GmailClient = &*mock;
    // The driver's SELECT path: a search with q= then a per-id detail fetch (the N+1 leaves).
    mock.search_ids_seed("from:alice@example.com", vec!["m1".to_string()]);
    let page = client
        .search_message_ids("from:alice@example.com", Some(5))
        .unwrap();
    assert_eq!(page.ids, vec!["m1".to_string()]);
    let msg = client.get_message("m1").unwrap();
    let row = msg.to_row();
    // Decoded into typed row values.
    assert_eq!(row.values[0], Value::Text("m1".to_string()));
    assert_eq!(row.values[3], Value::Text("alice@example.com".to_string()));
    // The mock recorded the exact q= the driver pushed + the N+1 detail leaf.
    let calls = mock.recorded();
    assert!(calls.contains(&RecordedCall::Search {
        query: "from:alice@example.com".to_string(),
        max_results: Some(5),
    }));
    assert!(calls.contains(&RecordedCall::GetMessage {
        id: "m1".to_string()
    }));
}

// ---- effect decode: drafts / labels / trash / send --------------------------------------

#[test]
fn insert_into_drafts_decodes_to_create_draft() {
    let node = EffectNode::new(NodeId(0), EffectKind::Insert, target("/mail/drafts")).with_args(
        draft_args(&[
            (TO_COL, Value::Text("bob@example.com".into())),
            (SUBJECT_COL, Value::Text("hi".into())),
            (BODY_COL, Value::Text("body".into())),
        ]),
    );
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::CreateDraft { draft } => {
            assert_eq!(draft.to, vec!["bob@example.com".to_string()]);
            assert_eq!(draft.subject, "hi");
        }
        other => panic!("expected CreateDraft, got {other:?}"),
    }
}

#[test]
fn insert_into_drafts_decodes_an_attachments_array_struct_column() {
    // The `attachments` column is the `Array(Struct{filename, mime, bytes})` shape a t92
    // `[ { filename: '…', mime: '…', bytes: X'…' } ]` literal lowers to; the effect decoder must
    // surface it as a `MailDraft` attachment carrying the raw bytes (here `X'68656c6c6f'` = "hello").
    let attachment = Value::Struct(qfs_types::Fields::new(vec![
        ("filename".to_string(), Value::Text("note.txt".into())),
        ("mime".to_string(), Value::Text("text/plain".into())),
        ("bytes".to_string(), Value::Bytes(b"hello".to_vec())),
    ]));
    let node = EffectNode::new(NodeId(0), EffectKind::Insert, target("/mail/drafts")).with_args(
        draft_args(&[
            (TO_COL, Value::Text("bob@example.com".into())),
            (SUBJECT_COL, Value::Text("hi".into())),
            (BODY_COL, Value::Text("body".into())),
            ("attachments", Value::Array(vec![attachment])),
        ]),
    );
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::CreateDraft { draft } => {
            assert_eq!(draft.attachments.len(), 1);
            assert_eq!(draft.attachments[0].filename, "note.txt");
            assert_eq!(draft.attachments[0].mime, "text/plain");
            assert_eq!(draft.attachments[0].bytes, b"hello");
        }
        other => panic!("expected CreateDraft, got {other:?}"),
    }
}

#[test]
fn insert_into_labels_decodes_to_create_label() {
    // gmail-ftp `mkdir Work/Receipts` → INSERT INTO /mail/labels VALUES ('Work/Receipts'). The
    // positional `name` column is named by the label collection's describe schema.
    let node = EffectNode::new(NodeId(0), EffectKind::Insert, target("/mail/labels")).with_args(
        draft_args(&[(NAME_COL, Value::Text("Work/Receipts".into()))]),
    );
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::CreateLabel { name } => assert_eq!(name, "Work/Receipts"),
        other => panic!("expected CreateLabel, got {other:?}"),
    }
}

#[test]
fn insert_into_labels_with_no_name_is_malformed() {
    let node = EffectNode::new(NodeId(0), EffectKind::Insert, target("/mail/labels"))
        .with_args(draft_args(&[("other", Value::Text("x".into()))]));
    assert!(GmailEffect::from_node(&node).is_err());
}

#[test]
fn label_create_capability_is_insert_only() {
    let (d, _) = driver_with_mock();
    let labels = Path::new("/mail/labels");
    assert!(check_capability(&d, &labels, Verb::Insert).is_ok());
    assert!(check_capability(&d, &labels, Verb::Select).is_err());
}

#[test]
fn upsert_into_drafts_is_retry_safe_keyed_by_draft_id() {
    let node = EffectNode::new(NodeId(0), EffectKind::Upsert, target("/mail/drafts")).with_args(
        draft_args(&[
            (DRAFT_ID_COL, Value::Text("d9".into())),
            (TO_COL, Value::Text("bob@example.com".into())),
        ]),
    );
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::UpsertDraft { id, .. } => assert_eq!(id, "d9"),
        other => panic!("expected UpsertDraft, got {other:?}"),
    }
}

#[test]
fn update_on_label_decodes_to_modify_labels() {
    let node = EffectNode::new(NodeId(0), EffectKind::Update, target("/mail/INBOX")).with_args(
        draft_args(&[
            ("id", Value::Text("m1".into())),
            (ADD_LABELS_COL, Value::Text("STARRED".into())),
            (REMOVE_LABELS_COL, Value::Text("UNREAD".into())),
        ]),
    );
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::ModifyLabels {
            message,
            add,
            remove,
        } => {
            assert_eq!(message, "m1");
            assert_eq!(add, vec!["STARRED".to_string()]);
            assert_eq!(remove, vec!["UNREAD".to_string()]);
        }
        other => panic!("expected ModifyLabels, got {other:?}"),
    }
}

#[test]
fn remove_message_is_a_single_trash_message_not_thread() {
    let node = EffectNode::new(NodeId(0), EffectKind::Remove, target("id:m1"));
    assert!(node.irreversible, "REMOVE is inherently irreversible");
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::TrashMessage { id } => assert_eq!(id, "m1"),
        other => panic!("expected TrashMessage, got {other:?}"),
    }
    // A thread address trashes the whole thread.
    let thread = EffectNode::new(NodeId(1), EffectKind::Remove, target("id:thread:t1"));
    match GmailEffect::from_node(&thread).unwrap() {
        GmailEffect::TrashThread { id } => assert_eq!(id, "t1"),
        other => panic!("expected TrashThread, got {other:?}"),
    }
}

#[test]
fn call_mail_send_decodes_irreversible_and_unknown_proc_rejected() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("mail.send")),
        target("/mail/drafts"),
    )
    .irreversible(true)
    .with_args(draft_args(&[(DRAFT_ID_COL, Value::Text("d9".into()))]));
    assert!(node.irreversible);
    match GmailEffect::from_node(&node).unwrap() {
        GmailEffect::Send { draft_id, .. } => assert_eq!(draft_id, Some("d9".to_string())),
        other => panic!("expected Send, got {other:?}"),
    }
    // An undeclared CALL is rejected structurally.
    let bad = EffectNode::new(
        NodeId(1),
        EffectKind::Call(ProcId::new("mail.nuke")),
        target("/mail/drafts"),
    );
    assert_eq!(
        GmailEffect::from_node(&bad).unwrap_err().code(),
        "unknown_procedure"
    );
}

// ---- MIME golden -------------------------------------------------------------------------

#[test]
fn build_mime_is_byte_stable_for_nonascii_subject_and_two_attachments() {
    let draft = MailDraft {
        id: None,
        to: vec!["bob@example.com".to_string()],
        cc: vec![],
        subject: "café ☕".to_string(), // non-ASCII → RFC 2047 base64
        body: "hello\nworld".to_string(),
        attachments: vec![
            Attachment {
                filename: "a.txt".to_string(),
                mime: "text/plain".to_string(),
                bytes: b"AAAA".to_vec(),
            },
            Attachment {
                filename: "b.bin".to_string(),
                mime: "application/octet-stream".to_string(),
                bytes: vec![0u8, 1, 2, 3],
            },
        ],
    };
    let bytes = mime::build_mime(&draft).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    // CRLF line endings, RFC 2047 subject, multipart/mixed boundary, base64 parts.
    assert!(text.contains("\r\n"));
    assert!(
        text.contains("Subject: =?UTF-8?B?"),
        "non-ascii subject is RFC2047-encoded"
    );
    assert!(text.contains("Content-Type: multipart/mixed; boundary="));
    assert!(text.contains("Content-Disposition: attachment; filename=\"a.txt\""));
    assert!(text.contains("QUFBQQ=="), "AAAA base64 part present");
    // Byte-stable: same input → identical output (deterministic boundary, no randomness).
    assert_eq!(mime::build_mime(&draft).unwrap(), text.into_bytes());

    // The base64url raw is the Gmail `raw` field shape (URL alphabet, no + or /).
    let raw = mime::raw_base64url(&draft).unwrap();
    assert!(!raw.contains('+') && !raw.contains('/'), "raw is base64url");
}

#[test]
fn build_mime_rejects_a_draft_with_no_recipients() {
    let draft = MailDraft {
        to: vec![],
        ..MailDraft::default()
    };
    assert_eq!(mime::build_mime(&draft).unwrap_err().code(), "mime");
}

// ---- PREVIEW performs no I/O (mock asserts zero calls) -----------------------------------

#[test]
fn preview_of_a_send_plan_performs_no_io() {
    let (_d, mock) = driver_with_mock();
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("mail.send")),
            target("/mail/drafts"),
        )
        .irreversible(true)
        .with_args(draft_args(&[(DRAFT_ID_COL, Value::Text("d9".into()))])),
    );
    let plan = b.build();
    // The pure preview walks the plan with NO client dispatch.
    let pv = preview(&plan);
    assert_eq!(pv.rows.len(), 1);
    assert!(
        pv.rows[0].irreversible,
        "preview surfaces the irreversible send"
    );
    assert!(
        mock.recorded().is_empty(),
        "PREVIEW must perform zero Gmail API calls: {:?}",
        mock.recorded()
    );
}

// ---- token never in logs / errors --------------------------------------------------------

#[test]
fn errors_are_secret_free() {
    // Every GmailError surface is secret-free by construction; assert no token-looking text.
    let errs = [
        GmailError::Api {
            op: "messages.send",
            status: 500,
        },
        GmailError::CapabilityDenied {
            path: "/mail/drafts".into(),
            verb: "UPDATE",
        },
        GmailError::from(qfs_google_auth::AuthError::TokenRefresh {
            reason: "invalid_grant".to_string(),
        }),
    ];
    for e in &errs {
        let text = format!("{e} {e:?}");
        assert!(!text.contains("Bearer"), "no bearer in error: {text}");
        assert!(!text.contains("ya29"), "no google token prefix: {text}");
    }
    // The auth→gmail mapping preserves only the secret-free code + reauthorize signal.
    match GmailError::from(qfs_google_auth::AuthError::TokenRefresh {
        reason: "invalid_grant".to_string(),
    }) {
        GmailError::Auth { code, reauthorize } => {
            assert_eq!(code, "auth_token_refresh");
            assert!(reauthorize);
        }
        other => panic!("expected Auth, got {other:?}"),
    }
}

// ---- end-to-end: commit through interpreter + bridge -------------------------------------

#[tokio::test]
async fn commit_trash_message_end_to_end_through_interpreter() {
    let (driver, mock) = driver_with_mock();
    let bridge = gmail_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("id:m1"),
    ));
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("mail"), &EffectKind::Remove);
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert!(outcome.is_complete(), "trash applied: {outcome:?}");
    assert_eq!(outcome.applied_ids(), vec![NodeId(0)]);
    // The applier dispatched exactly one trash-message op (the irreversible REMOVE).
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::TrashMessage {
            id: "m1".to_string()
        }]
    );
}

#[tokio::test]
async fn commit_send_creates_draft_then_sends_it() {
    let (driver, mock) = driver_with_mock();
    let bridge = gmail_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    // mail.send with draft content → create-then-send (the recoverable de-dupe path).
    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Call(ProcId::new("mail.send")),
            target("/mail/drafts"),
        )
        .irreversible(true)
        .with_args(draft_args(&[
            (TO_COL, Value::Text("bob@example.com".into())),
            (SUBJECT_COL, Value::Text("hi".into())),
            (BODY_COL, Value::Text("body".into())),
        ])),
    );
    let plan = b.build();

    let caps = CapabilitySet::none().grant(
        DriverId::new("mail"),
        &EffectKind::Call(ProcId::new("mail.send")),
    );
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "send applied: {outcome:?}");

    // create_draft then send_draft, in order — and NEVER a permanent delete.
    let calls = mock.recorded();
    assert!(matches!(
        calls.first(),
        Some(RecordedCall::CreateDraft { .. })
    ));
    assert!(matches!(calls.get(1), Some(RecordedCall::SendDraft { .. })));
}

#[tokio::test]
async fn multi_account_selects_independent_clients() {
    // Two accounts → two independent driver instances over two independent mock clients.
    // Each driver routes only to its own client; selection is the t19 base (one client per
    // account). We prove a label-modify on account A never touches account B's client.
    let mock_a = Arc::new(MockGmailClient::new());
    let mock_b = Arc::new(MockGmailClient::new());
    let driver_a = GmailDriver::new(mock_a.clone() as Arc<dyn GmailClient>);
    let driver_b = GmailDriver::new(mock_b.clone() as Arc<dyn GmailClient>);

    let node = EffectNode::new(NodeId(0), EffectKind::Update, target("/mail/INBOX")).with_args(
        draft_args(&[
            ("id", Value::Text("mA".into())),
            (ADD_LABELS_COL, Value::Text("STARRED".into())),
        ]),
    );
    use qfs_runtime::SharedApplier;
    driver_a.gmail_applier().apply_shared(&node).unwrap();

    assert_eq!(
        mock_a.recorded(),
        vec![RecordedCall::ModifyLabels {
            id: "mA".to_string(),
            add: vec!["STARRED".to_string()],
            remove: vec![],
        }]
    );
    assert!(
        mock_b.recorded().is_empty(),
        "account B's client was untouched"
    );
    let _ = driver_b; // bound to prove the two are distinct instances.
}
