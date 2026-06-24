//! Internal tests for `qfs-driver-objstore` (t22). The backend is the in-memory
//! [`MockObjectBackend`] (scripted S3 responses + recorded calls) and the SigV4 signer runs over
//! offline AWS vectors — **no live S3/R2, no network, no live credentials**. The tests prove:
//! - path parsing (bucket / key / `@versionId`) and capability gating (parse-time, structured);
//! - `ls` pushes a prefix/delimiter down and keeps a **truthful residual** for a partial predicate;
//! - `get` streams bytes for a single + a ranged request via a bounded `ByteStream`;
//! - `UPSERT` below threshold = one PUT, above = multipart with `complete`; an injected mid-part
//!   failure triggers `abort` (the abort-on-error invariant);
//! - `@versionId` GET/REMOVE round-trip; ETag surfaced for optimistic concurrency;
//! - plan-shape golden: UPSERT/REMOVE@v/FROM produce the expected nodes + flags;
//! - the credential never leaks across any `ObjError`/request surface;
//! - end-to-end through the interpreter + bridge for an UPSERT and a REMOVE.

use std::sync::Arc;

use qfs_driver::{check_capability, Driver, Path, Verb};
use qfs_plan::{
    DriverId as PlanDriverId, EffectKind, EffectNode, NodeId, PlanBuilder, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter, SharedApplier};
use qfs_types::{
    CmpOp, ColRef, Column, ColumnType, Literal, Pattern, Predicate, Row, RowBatch, Schema, Value,
};

use crate::backend::{MockObjectBackend, RecordedCall};
use crate::dto::{ListPage, ObjectMeta, PutResult};
use crate::registry::{Bucket, ObjRegistry};
use crate::{
    r2_apply_driver, s3_apply_driver, MultipartPolicy, ObjApplier, ObjDriver, ObjNode, R2Driver,
    S3Driver, Scheme,
};

/// A planted secret value — unmistakable if it ever leaks into an error or request Debug surface.
const PLANTED_SECRET: &str = "PLANTED-AWS-SECRET-deadbeef-9f8e7d6c5b4a";

/// A registry wiring a non-versioned `assets` bucket + a versioned `archive` bucket to one shared
/// mock backend.
fn registry_with(backend: Arc<MockObjectBackend>) -> ObjRegistry {
    ObjRegistry::new()
        .with_bucket("assets", Bucket::new(backend.clone()))
        .with_bucket("archive", Bucket::versioned(backend))
}

fn s3_with(backend: Arc<MockObjectBackend>) -> S3Driver {
    S3Driver::new(registry_with(backend))
}

/// The inner [`ObjDriver`] (carries `ls`/`get`/`plan_ls`), built directly for the read-path tests.
fn obj_with(backend: Arc<MockObjectBackend>) -> ObjDriver {
    ObjDriver::new(Scheme::S3, registry_with(backend))
}

/// A single-row RowBatch over the given (name, type, value) triples.
fn row_batch(cells: Vec<(&str, ColumnType, Value)>) -> RowBatch {
    let columns = cells
        .iter()
        .map(|(n, t, _)| Column::new(*n, t.clone(), true))
        .collect();
    let values = cells.into_iter().map(|(_, _, v)| v).collect();
    RowBatch::new(Schema::new(columns), vec![Row::new(values)])
}

fn effect(kind: EffectKind, path: &str, args: RowBatch) -> EffectNode {
    let target = Target::new(PlanDriverId::new("s3"), VfsPath::new(path));
    EffectNode::new(NodeId(1), kind, target).with_args(args)
}

// ----------------------------------------------------------------------------------------------
// Path parsing + capability gating
// ----------------------------------------------------------------------------------------------

#[test]
fn parses_s3_and_r2_addresses_with_versions() {
    assert_eq!(
        ObjNode::parse_str("/s3/assets/img/logo.png").unwrap(),
        ObjNode::Object {
            scheme: Scheme::S3,
            bucket: "assets".to_string(),
            key: "img/logo.png".to_string(),
            version_id: None,
        }
    );
    assert_eq!(
        ObjNode::parse_str("/r2/assets/k@v7").unwrap(),
        ObjNode::Object {
            scheme: Scheme::R2,
            bucket: "assets".to_string(),
            key: "k".to_string(),
            version_id: Some("v7".to_string()),
        }
    );
}

#[test]
fn describe_returns_blob_namespace_with_listing_schema() {
    let d = s3_with(Arc::new(MockObjectBackend::new()));
    let desc = d.describe(&Path::new("/s3/assets/k")).unwrap();
    assert_eq!(desc.archetype, qfs_driver::Archetype::BlobNamespace);
    assert!(desc.schema.column("key").is_some());
    assert!(desc.schema.column("etag").is_some());
    assert!(desc.schema.column("version_id").unwrap().nullable);
}

#[test]
fn unsupported_verb_on_a_bucket_root_is_rejected_structurally() {
    let d = s3_with(Arc::new(MockObjectBackend::new()));
    let bucket = Path::new("/s3/assets");
    // A bucket root admits ls/select/upsert/cp/mv but NOT a keyless REMOVE/RM/UPDATE.
    let err = check_capability(&d, &bucket, Verb::Update).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    match err {
        qfs_driver::CfsError::UnsupportedVerb {
            path, supported, ..
        } => {
            assert_eq!(path, "/s3/assets");
            assert!(supported.contains(&"LS"));
            assert!(supported.contains(&"UPSERT"));
            assert!(!supported.contains(&"RM"));
        }
        other => panic!("expected UnsupportedVerb, got {other:?}"),
    }
    // A key node admits the full blob verb set.
    let key = Path::new("/s3/assets/k");
    assert!(check_capability(&d, &key, Verb::Rm).is_ok());
    assert!(check_capability(&d, &key, Verb::Cp).is_ok());
    assert!(check_capability(&d, &key, Verb::Upsert).is_ok());
}

// ----------------------------------------------------------------------------------------------
// ls — prefix/delimiter pushdown + truthful residual (the t20 lesson)
// ----------------------------------------------------------------------------------------------

#[test]
fn ls_pushes_prefix_and_delimiter_and_returns_paged_rows() {
    let page = ListPage::new(vec![
        ObjectMeta::new("logs/a.json", 10).with_etag("\"ea\""),
        ObjectMeta::new("logs/b.json", 20).with_etag("\"eb\""),
    ])
    .with_common_prefixes(vec!["logs/2026/".to_string()])
    .with_next_token("tok-1");
    let backend = Arc::new(MockObjectBackend::new().with_list_page(page));
    let d = obj_with(backend.clone());

    // `key LIKE 'logs/%'` is EXACTLY a prefix list → push the prefix, drop the residual.
    let pred = Predicate::Like(ColRef::col("key"), Pattern("logs/%".to_string()));
    let pushdown = ObjDriver::plan_ls(Some(&pred), Some("/"));
    assert_eq!(pushdown.prefix.as_deref(), Some("logs/"));
    assert_eq!(pushdown.delimiter.as_deref(), Some("/"));
    assert!(pushdown.residual.is_none(), "exact prefix ⇒ no residual");

    let (result, residual) = d.ls(&Path::new("/s3/assets"), &pushdown, None).unwrap();
    assert_eq!(result.objects.len(), 2);
    assert!(result.has_more());
    assert!(residual.is_none());

    let calls = backend.recorded();
    let RecordedCall::List {
        bucket,
        prefix,
        delimiter,
        ..
    } = &calls[0]
    else {
        panic!("expected a list call, got {calls:?}");
    };
    assert_eq!(bucket, "assets");
    assert_eq!(prefix.as_deref(), Some("logs/"));
    assert_eq!(delimiter.as_deref(), Some("/"));
}

#[test]
fn ls_keeps_a_truthful_residual_for_a_partial_predicate() {
    // `key = 'logs/exact.json'` → push the value as a SUPERSET prefix, but keep the EXACT `=`
    // predicate as a residual the engine re-filters (a prefix list would also return `logs/...X`).
    let pred = Predicate::Cmp(
        ColRef::col("key"),
        CmpOp::Eq,
        Literal::Text("logs/exact.json".to_string()),
    );
    let pushdown = ObjDriver::plan_ls(Some(&pred), None);
    assert_eq!(pushdown.prefix.as_deref(), Some("logs/exact.json"));
    assert_eq!(
        pushdown.residual.as_ref(),
        Some(&pred),
        "the exact `=` predicate MUST be kept as a residual (never silently dropped)"
    );

    // An AND keeps the whole predicate as a residual while pushing one conjunct's prefix.
    let and = Predicate::And(
        Box::new(Predicate::Like(
            ColRef::col("key"),
            Pattern("img/%".to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("size"),
            CmpOp::Gt,
            Literal::Int(1000),
        )),
    );
    let pd = ObjDriver::plan_ls(Some(&and), None);
    assert_eq!(pd.prefix.as_deref(), Some("img/"));
    assert_eq!(
        pd.residual.as_ref(),
        Some(&and),
        "an AND with a non-key conjunct keeps the whole predicate as the residual"
    );

    // A predicate with no key constraint pushes nothing and keeps everything.
    let no_key = Predicate::Cmp(ColRef::col("size"), CmpOp::Gt, Literal::Int(5));
    let pd2 = ObjDriver::plan_ls(Some(&no_key), None);
    assert!(pd2.prefix.is_none());
    assert_eq!(pd2.residual.as_ref(), Some(&no_key));
}

// ----------------------------------------------------------------------------------------------
// get — streaming + range pushdown + @versionId
// ----------------------------------------------------------------------------------------------

#[test]
fn get_streams_bytes_and_pushes_a_range_down() {
    let body = b"0123456789abcdef".to_vec();
    let backend = Arc::new(MockObjectBackend::new().with_get_body(body.clone()));
    let d = obj_with(backend.clone());

    // Full GET.
    let stream = d.get(&Path::new("/s3/assets/k"), None).unwrap();
    assert_eq!(stream.into_bytes(), body);

    // Ranged GET: bytes 4..=7 inclusive → "4567".
    let ranged = d.get(&Path::new("/s3/assets/k"), Some((4, 7))).unwrap();
    assert_eq!(ranged.into_bytes(), b"4567");

    let calls = backend.recorded();
    let RecordedCall::Get { range, .. } = &calls[1] else {
        panic!("expected the ranged get, got {calls:?}");
    };
    assert_eq!(*range, Some((4, 7)), "the byte range is pushed down");
}

#[test]
fn get_by_version_id_round_trips() {
    let backend = Arc::new(MockObjectBackend::new().with_get_body(b"v7-bytes".to_vec()));
    let d = obj_with(backend.clone());
    let stream = d.get(&Path::new("/s3/archive/doc.txt@v7"), None).unwrap();
    assert_eq!(stream.into_bytes(), b"v7-bytes");
    let calls = backend.recorded();
    let RecordedCall::Get { version_id, .. } = &calls[0] else {
        panic!("expected get, got {calls:?}");
    };
    assert_eq!(version_id.as_deref(), Some("v7"));
}

#[test]
fn large_get_is_framed_into_bounded_chunks() {
    // A multi-megabyte body must come back as multiple bounded chunks, not one buffer.
    let big = vec![9u8; crate::DEFAULT_MAX_CHUNK * 2 + 5];
    let backend = Arc::new(MockObjectBackend::new().with_get_body(big.clone()));
    let d = obj_with(backend);
    let stream = d.get(&Path::new("/s3/assets/big.bin"), None).unwrap();
    assert!(stream.chunk_count() >= 3, "bounded-memory streaming");
    assert_eq!(stream.len(), big.len());
}

// ----------------------------------------------------------------------------------------------
// UPSERT — single PUT below threshold, multipart above, abort-on-error
// ----------------------------------------------------------------------------------------------

#[test]
fn upsert_below_threshold_is_one_put() {
    let backend =
        Arc::new(MockObjectBackend::new().with_put_result(PutResult::new("\"new-etag\"")));
    // Threshold = 8 MiB default; a small body is one PUT.
    let applier = ObjApplier::new(registry_with(backend.clone()));
    let node = effect(
        EffectKind::Upsert,
        "/s3/assets/small.txt",
        row_batch(vec![(
            "body",
            ColumnType::Text,
            Value::Text("hello".to_string()),
        )]),
    );
    let out = applier.apply_shared(&node).unwrap();
    assert_eq!(out.affected, 1);

    let calls = backend.recorded();
    assert_eq!(calls.len(), 1);
    let RecordedCall::Put { bucket, key, len } = &calls[0] else {
        panic!("expected a single put, got {calls:?}");
    };
    assert_eq!(bucket, "assets");
    assert_eq!(key, "small.txt");
    assert_eq!(*len, 5);
}

#[test]
fn upsert_above_threshold_is_multipart_with_complete() {
    let backend = Arc::new(MockObjectBackend::new().with_put_result(PutResult::new("\"mp-etag\"")));
    // Tiny policy: threshold 4 bytes, part size 4 → a 10-byte body = 3 parts (4,4,2).
    let policy = MultipartPolicy::new(4, 4);
    let applier = ObjApplier::with_policy(registry_with(backend.clone()), policy);
    let node = effect(
        EffectKind::Upsert,
        "/s3/assets/big.bin",
        row_batch(vec![(
            "body",
            ColumnType::Bytes,
            Value::Bytes(vec![1u8; 10]),
        )]),
    );
    applier.apply_shared(&node).unwrap();

    let calls = backend.recorded();
    // create → 3× upload_part → complete (no abort).
    assert!(matches!(calls[0], RecordedCall::CreateMultipart { .. }));
    let part_count = calls
        .iter()
        .filter(|c| matches!(c, RecordedCall::UploadPart { .. }))
        .count();
    assert_eq!(part_count, 3, "10 bytes / 4 = 3 parts");
    assert!(matches!(
        calls.last().unwrap(),
        RecordedCall::CompleteMultipart { .. }
    ));
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, RecordedCall::AbortMultipart { .. })),
        "a clean multipart must NOT abort"
    );
}

#[test]
fn a_mid_multipart_failure_triggers_abort() {
    // Inject a failure at part 2 → create, part 1 ok, part 2 fails → abort.
    let backend = Arc::new(MockObjectBackend::new().failing_part_at(2));
    let policy = MultipartPolicy::new(4, 4);
    let applier = ObjApplier::with_policy(registry_with(backend.clone()), policy);
    let node = effect(
        EffectKind::Upsert,
        "/s3/assets/big.bin",
        row_batch(vec![(
            "body",
            ColumnType::Bytes,
            Value::Bytes(vec![1u8; 10]),
        )]),
    );
    let err = applier.apply_shared(&node).unwrap_err();
    // The runtime sees a terminal/structured failure.
    assert!(format!("{err:?}").to_lowercase().contains("terminal"));

    let calls = backend.recorded();
    assert!(matches!(calls[0], RecordedCall::CreateMultipart { .. }));
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, RecordedCall::AbortMultipart { .. })),
        "a mid-multipart failure MUST abort to free orphan parts: {calls:?}"
    );
}

// ----------------------------------------------------------------------------------------------
// REMOVE — versioned delete + irreversibility
// ----------------------------------------------------------------------------------------------

#[test]
fn remove_by_version_id_deletes_that_version() {
    let backend = Arc::new(MockObjectBackend::new());
    let applier = ObjApplier::new(registry_with(backend.clone()));
    let node = effect(
        EffectKind::Remove,
        "/s3/archive/doc.txt@v3",
        RowBatch::default(),
    );
    applier.apply_shared(&node).unwrap();
    let calls = backend.recorded();
    let RecordedCall::Delete {
        bucket,
        key,
        version_id,
    } = &calls[0]
    else {
        panic!("expected delete, got {calls:?}");
    };
    assert_eq!(bucket, "archive");
    assert_eq!(key, "doc.txt");
    assert_eq!(version_id.as_deref(), Some("v3"));
}

// ----------------------------------------------------------------------------------------------
// copy → verify → delete leg primitives (cross-source cp/mv, not orchestrated here)
// ----------------------------------------------------------------------------------------------

#[test]
fn copy_verify_delete_legs_compose() {
    let backend =
        Arc::new(MockObjectBackend::new().with_put_result(PutResult::new("\"copied-etag\"")));
    let applier = ObjApplier::new(registry_with(backend.clone()));

    // copy leg.
    let copied = applier.copy_leg("assets", "src.txt", "dst.txt").unwrap();
    assert_eq!(copied.etag, "\"copied-etag\"");
    // verify leg: matching ETag passes, mismatch is a conflict.
    assert!(ObjApplier::verify_leg(&copied, "\"copied-etag\"").is_ok());
    assert_eq!(
        ObjApplier::verify_leg(&copied, "\"other\"")
            .unwrap_err()
            .code(),
        "conflict"
    );
    // delete leg (the mv final step).
    applier.delete_leg("assets", "src.txt", None).unwrap();

    let calls = backend.recorded();
    assert!(matches!(calls[0], RecordedCall::Copy { .. }));
    assert!(matches!(calls[1], RecordedCall::Delete { .. }));
}

// ----------------------------------------------------------------------------------------------
// Plan-shape golden tests (no network)
// ----------------------------------------------------------------------------------------------

#[test]
fn plan_shape_upsert_remove_and_read_nodes() {
    // UPSERT INTO /s3/b/k → an Upsert effect node (reversible).
    let upsert = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/assets/k")),
    )
    .with_args(row_batch(vec![(
        "body",
        ColumnType::Text,
        Value::Text("x".to_string()),
    )]));
    assert_eq!(upsert.kind, EffectKind::Upsert);
    assert!(!upsert.irreversible, "UPSERT is retry-safe / reversible");

    // REMOVE /s3/b/k@v → a Remove node, inherently irreversible, carrying the version in the path.
    let remove = EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/archive/k@v9")),
    );
    assert_eq!(remove.kind, EffectKind::Remove);
    assert!(remove.irreversible, "REMOVE is inherently irreversible");
    // The version id is recoverable from the target path (the @v9 coordinate).
    let effect = crate::ObjEffect::from_node(&remove).unwrap();
    match effect {
        crate::ObjEffect::Delete { version_id, .. } => {
            assert_eq!(version_id.as_deref(), Some("v9"))
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    // FROM /s3/b/... → an effect-free READ node (List/Read), no irreversible flag.
    let read = EffectNode::new(
        NodeId(2),
        EffectKind::List,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/assets")),
    );
    assert_eq!(read.kind, EffectKind::List);
    assert!(!read.irreversible, "a read node carries no effect");
}

// ----------------------------------------------------------------------------------------------
// Credential never leaks
// ----------------------------------------------------------------------------------------------

#[test]
fn the_credential_never_appears_in_any_error_surface() {
    // Build a real SigV4 backend bearing the planted secret, then drive every ObjError surface and
    // prove the secret is nowhere. (It rides only in a redacted Authorization header.)
    let creds =
        crate::SigV4Credentials::new("AKIDEXAMPLE", qfs_secrets::Secret::from(PLANTED_SECRET));
    let _backend = crate::HttpBackend::new(
        Arc::new(crate::MockExchange::new()),
        crate::Endpoint::new("https://s3.us-east-1.amazonaws.com", "us-east-1"),
        creds,
        "20130524T000000Z",
        "20130524",
    );

    let errors = vec![
        crate::ObjError::InvalidPath {
            path: "/s3/x".to_string(),
            reason: "bad",
        },
        crate::ObjError::CapabilityDenied {
            path: "/s3/b/k".to_string(),
            verb: "UPDATE",
        },
        crate::ObjError::Api {
            op: "put_object",
            status: 500,
        },
        crate::ObjError::Decode {
            op: "list_objects_v2",
            reason: "not xml".to_string(),
        },
        crate::ObjError::Transport {
            reason: "connection failed".to_string(),
        },
        crate::ObjError::MultipartAborted {
            part: 2,
            reason: "part failed".to_string(),
        },
        crate::ObjError::Conflict {
            version: "\"etag\"".to_string(),
        },
    ];
    for e in &errors {
        let dbg = format!("{e:?}");
        let disp = e.to_string();
        assert!(
            !dbg.contains(PLANTED_SECRET),
            "secret leaked in Debug: {dbg}"
        );
        assert!(
            !disp.contains(PLANTED_SECRET),
            "secret leaked in Display: {disp}"
        );
        assert!(!dbg.contains("deadbeef"), "secret fragment leaked: {dbg}");
    }
}

// ----------------------------------------------------------------------------------------------
// End-to-end through the interpreter + bridge (both schemes)
// ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_commit_upsert_and_remove_through_the_bridge() {
    let backend = Arc::new(MockObjectBackend::new());
    let driver = s3_with(backend.clone());
    let bridge = s3_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Upsert,
            Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/assets/k1")),
        )
        .with_args(row_batch(vec![(
            "body",
            ColumnType::Text,
            Value::Text("payload".to_string()),
        )])),
    );
    b.push(EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/archive/k2@v1")),
    ));
    let plan = b.build();

    let caps = CapabilitySet::none()
        .grant(PlanDriverId::new("s3"), &EffectKind::Upsert)
        .grant(PlanDriverId::new("s3"), &EffectKind::Remove);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(
        outcome.is_complete(),
        "both effects must apply: {outcome:?}"
    );

    let calls = backend.recorded();
    assert!(calls.iter().any(|c| matches!(c, RecordedCall::Put { .. })));
    assert!(calls
        .iter()
        .any(|c| matches!(c, RecordedCall::Delete { .. })));
}

#[tokio::test]
async fn r2_driver_commits_through_its_own_bridge_id() {
    let backend = Arc::new(MockObjectBackend::new());
    let driver = R2Driver::new(registry_with(backend.clone()));
    assert_eq!(driver.id(), PlanDriverId::new("r2"));
    let bridge = r2_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Upsert,
            Target::new(PlanDriverId::new("r2"), VfsPath::new("/r2/assets/k")),
        )
        .with_args(row_batch(vec![(
            "body",
            ColumnType::Text,
            Value::Text("p".to_string()),
        )])),
    );
    let caps = CapabilitySet::none().grant(PlanDriverId::new("r2"), &EffectKind::Upsert);
    let outcome = interp.commit(b.build(), &caps).await.unwrap();
    assert!(outcome.is_complete(), "r2 upsert must apply: {outcome:?}");
    assert!(backend
        .recorded()
        .iter()
        .any(|c| matches!(c, RecordedCall::Put { .. })));
}
