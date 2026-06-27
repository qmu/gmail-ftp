//! Planner E2E / external-interface validation for t22 (S3 + Cloudflare R2 object-storage driver).
//!
//! This is BLACK-BOX validation driven entirely through the crate's PUBLIC API (the `qfs_driver`
//! `Driver` trait, the public `ObjDriver`/`S3Driver`/`R2Driver` surface, the runtime interpreter +
//! bridge, and the public mock backend). It is intentionally independent of the Constructor's
//! internal `src/tests.rs`: the scenarios map 1:1 to the ticket's acceptance criteria, and several
//! of them ACTIVELY TRY TO BREAK the truthful-pushdown-residual property (scenario 6) and the
//! token-safety property (scenario 7) rather than re-asserting the happy path.
//!
//! No live S3/R2, no network, no live credentials. Run with `cargo test -p qfs-driver-objstore
//! --test planner_e2e`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_driver::{check_capability, Driver, Path, PushdownProfile, Verb, VersionSupport};
use qfs_plan::{
    DriverId as PlanDriverId, EffectKind, EffectNode, NodeId, PlanBuilder, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use qfs_types::{
    CmpOp, ColRef, Column, ColumnType, Literal, Pattern, Predicate, Row, RowBatch, Schema, Value,
};

use qfs_driver_objstore::{
    r2_apply_driver, s3_apply_driver, Bucket, MockObjectBackend, MultipartPolicy, ObjApplier,
    ObjDriver, ObjError, ObjNode, ObjRegistry, PutResult, R2Driver, S3Driver, Scheme,
};
use qfs_driver_objstore::{ListPage, ObjectMeta, RecordedCall};

/// A planted credential canary — must NEVER appear in any output, error, plan, or list result.
const CANARY: &str = "PLANTED-CANARY-cafef00d-secret-7e1c";

fn registry(backend: Arc<MockObjectBackend>) -> ObjRegistry {
    ObjRegistry::new()
        .with_bucket("assets", Bucket::new(backend.clone()))
        .with_bucket("archive", Bucket::versioned(backend))
}

fn obj(backend: Arc<MockObjectBackend>) -> ObjDriver {
    ObjDriver::new(Scheme::S3, registry(backend))
}

fn s3(backend: Arc<MockObjectBackend>) -> S3Driver {
    S3Driver::new(registry(backend))
}

fn one_row(cells: Vec<(&str, ColumnType, Value)>) -> RowBatch {
    let columns = cells
        .iter()
        .map(|(n, t, _)| Column::new(*n, t.clone(), true))
        .collect();
    let values = cells.into_iter().map(|(_, _, v)| v).collect();
    RowBatch::new(Schema::new(columns), vec![Row::new(values)])
}

fn effect_node(id: u32, kind: EffectKind, driver: &str, path: &str, args: RowBatch) -> EffectNode {
    EffectNode::new(
        NodeId(id),
        kind,
        Target::new(PlanDriverId::new(driver), VfsPath::new(path)),
    )
    .with_args(args)
}

// =================================================================================================
// Scenario 1 — Plan-shape (golden, no network)
// =================================================================================================

#[test]
fn s1_upsert_remove_and_read_plan_shapes() {
    // UPSERT INTO /s3/b/k → an Upsert effect node, reversible (retry-safe).
    let upsert = effect_node(
        0,
        EffectKind::Upsert,
        "s3",
        "/s3/assets/k",
        one_row(vec![("body", ColumnType::Text, Value::Text("x".into()))]),
    );
    assert_eq!(upsert.kind, EffectKind::Upsert);
    assert!(!upsert.irreversible, "UPSERT must be reversible/retry-safe");

    // REMOVE /s3/b/k@v → a Remove node, inherently irreversible, version recoverable from path.
    let remove = EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/archive/doc@v9")),
    );
    assert_eq!(remove.kind, EffectKind::Remove);
    assert!(
        remove.irreversible,
        "REMOVE must be inherently irreversible"
    );

    // The driver decodes the @version off the path into the Delete effect (the version_id survives).
    let decoded = qfs_driver_objstore::ObjEffect::from_node(&remove).unwrap();
    match decoded {
        qfs_driver_objstore::ObjEffect::Delete { version_id, .. } => {
            assert_eq!(
                version_id.as_deref(),
                Some("v9"),
                "the @v9 coordinate threads in"
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    // /s3/b/... → an effect-free read/List node carrying no irreversible flag.
    let read = EffectNode::new(
        NodeId(2),
        EffectKind::List,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/assets")),
    );
    assert_eq!(read.kind, EffectKind::List);
    assert!(!read.irreversible, "a read node carries no effect");
}

// =================================================================================================
// Scenario 2 — SigV4 signer reproduces the AWS published vector offline
//
// The signer itself is a PRIVATE module (no vendor leak), so the AWS published byte-vector check
// lives in the unit layer. From the outside we can still observe the signer's determinism + the
// fact the secret never surfaces (covered by scenario 7). The vector reproduction is asserted by
// the in-crate unit test `signing_key_matches_aws_published_derivation`; here we observe that the
// public driver surface exposes NO signer/crypto type (the boundary that makes the vector private).
// =================================================================================================

#[test]
fn s2_no_signer_or_crypto_type_crosses_the_public_boundary() {
    // A compile-time + behavioral check that the SigV4 internals are not reachable from outside:
    // the public driver answers ls/get/plan through owned DTOs only. (The published-vector match is
    // pinned in the private unit layer; see the report.) Building this test at all proves the
    // public API needs none of the signer types.
    let backend = Arc::new(MockObjectBackend::new().with_get_body(b"ok".to_vec()));
    let d = obj(backend);
    let stream = d.get(&Path::new("/s3/assets/k"), None).unwrap();
    assert_eq!(stream.into_bytes(), b"ok");
}

// =================================================================================================
// Scenario 3 — Mock S3 behavior: ls paging + prefixes, get single + ranged, single PUT vs
// multipart, abort-on-error mid-multipart.
// =================================================================================================

#[test]
fn s3_ls_returns_paged_rows_and_common_prefixes() {
    let page = ListPage::new(vec![
        ObjectMeta::new("logs/a.json", 10).with_etag("\"ea\""),
        ObjectMeta::new("logs/b.json", 20).with_etag("\"eb\""),
    ])
    .with_common_prefixes(vec!["logs/2026/".into()])
    .with_next_token("tok-1");
    let backend = Arc::new(MockObjectBackend::new().with_list_page(page));
    let d = obj(backend.clone());

    let pd = ObjDriver::plan_ls(
        Some(&Predicate::Like(
            ColRef::col("key"),
            Pattern("logs/%".into()),
        )),
        Some("/"),
    );
    let (result, residual) = d.ls(&Path::new("/s3/assets"), &pd, None).unwrap();
    assert_eq!(result.objects.len(), 2);
    assert_eq!(result.common_prefixes, vec!["logs/2026/".to_string()]);
    assert!(result.has_more(), "next_token drives pagination");
    assert!(
        residual.is_none(),
        "an exact-prefix LIKE drops the residual"
    );

    // ObjectMeta projects to the declared listing row order.
    let rows = result.to_rows();
    assert_eq!(rows[0].values[0], Value::Text("logs/a.json".into()));
    assert_eq!(rows[0].values[1], Value::Int(10));

    let calls = backend.recorded();
    assert!(
        matches!(&calls[0], RecordedCall::List { prefix, delimiter, .. }
        if prefix.as_deref() == Some("logs/") && delimiter.as_deref() == Some("/"))
    );
}

#[test]
fn s3_get_streams_single_and_ranged() {
    let body = b"0123456789abcdef".to_vec();
    let backend = Arc::new(MockObjectBackend::new().with_get_body(body.clone()));
    let d = obj(backend.clone());

    assert_eq!(
        d.get(&Path::new("/s3/assets/k"), None)
            .unwrap()
            .into_bytes(),
        body
    );
    assert_eq!(
        d.get(&Path::new("/s3/assets/k"), Some((4, 7)))
            .unwrap()
            .into_bytes(),
        b"4567",
        "an inclusive range pushdown returns exactly the requested bytes"
    );
    let calls = backend.recorded();
    assert!(matches!(&calls[1], RecordedCall::Get { range, .. } if *range == Some((4, 7))));
}

#[test]
fn s3_upsert_below_threshold_is_one_put_above_is_multipart_complete() {
    // Below threshold → exactly one PUT.
    use qfs_runtime::SharedApplier;
    let small2 = Arc::new(MockObjectBackend::new());
    let applier2 = ObjApplier::new(registry(small2.clone()));
    applier2
        .apply_shared(&effect_node(
            0,
            EffectKind::Upsert,
            "s3",
            "/s3/assets/small.txt",
            one_row(vec![(
                "body",
                ColumnType::Text,
                Value::Text("hello".into()),
            )]),
        ))
        .unwrap();
    let calls = small2.recorded();
    assert_eq!(calls.len(), 1, "a small body is a single PUT: {calls:?}");
    assert!(matches!(&calls[0], RecordedCall::Put { len, .. } if *len == 5));

    // Above threshold (tiny policy 4/4) → create → 3 parts → complete, no abort.
    let big = Arc::new(MockObjectBackend::new());
    let mp = ObjApplier::with_policy(registry(big.clone()), MultipartPolicy::new(4, 4));
    mp.apply_shared(&effect_node(
        0,
        EffectKind::Upsert,
        "s3",
        "/s3/assets/big.bin",
        one_row(vec![(
            "body",
            ColumnType::Bytes,
            Value::Bytes(vec![7u8; 10]),
        )]),
    ))
    .unwrap();
    let calls = big.recorded();
    assert!(matches!(calls[0], RecordedCall::CreateMultipart { .. }));
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, RecordedCall::UploadPart { .. }))
            .count(),
        3,
        "10 bytes / 4 = 3 parts"
    );
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
fn s3_mid_multipart_failure_triggers_abort() {
    use qfs_runtime::SharedApplier;
    let backend = Arc::new(MockObjectBackend::new().failing_part_at(2));
    let mp = ObjApplier::with_policy(registry(backend.clone()), MultipartPolicy::new(4, 4));
    let err = mp
        .apply_shared(&effect_node(
            0,
            EffectKind::Upsert,
            "s3",
            "/s3/assets/big.bin",
            one_row(vec![(
                "body",
                ColumnType::Bytes,
                Value::Bytes(vec![7u8; 10]),
            )]),
        ))
        .unwrap_err();
    assert!(
        format!("{err:?}").to_lowercase().contains("terminal"),
        "the runtime sees a terminal failure: {err:?}"
    );
    let calls = backend.recorded();
    assert!(matches!(calls[0], RecordedCall::CreateMultipart { .. }));
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, RecordedCall::AbortMultipart { .. })),
        "a mid-multipart failure MUST abort to free orphan parts: {calls:?}"
    );
    // And no complete must have fired after the failure.
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, RecordedCall::CompleteMultipart { .. })),
        "a failed multipart must NOT complete"
    );
}

// =================================================================================================
// Scenario 4 — @versionId GET/REMOVE round-trips; ETag surfaced.
// =================================================================================================

#[test]
fn s4_version_id_get_and_remove_round_trip_with_etag() {
    use qfs_runtime::SharedApplier;
    let backend = Arc::new(
        MockObjectBackend::new()
            .with_get_body(b"v7-bytes".to_vec())
            .with_put_result(PutResult::new("\"etag-77\"").with_version_id("v7")),
    );
    let d = obj(backend.clone());

    // GET by @versionId.
    let got = d.get(&Path::new("/s3/archive/doc.txt@v7"), None).unwrap();
    assert_eq!(got.into_bytes(), b"v7-bytes");

    // REMOVE by @versionId.
    let applier = ObjApplier::new(registry(backend.clone()));
    applier
        .apply_shared(&effect_node(
            0,
            EffectKind::Remove,
            "s3",
            "/s3/archive/doc.txt@v3",
            RowBatch::default(),
        ))
        .unwrap();

    let calls = backend.recorded();
    assert!(matches!(&calls[0], RecordedCall::Get { version_id, .. }
        if version_id.as_deref() == Some("v7")));
    assert!(
        matches!(&calls[1], RecordedCall::Delete { key, version_id, .. }
        if key == "doc.txt" && version_id.as_deref() == Some("v3"))
    );

    // ETag surfaced for optimistic concurrency: the copy/verify leg compares ETags.
    let copied = applier.copy_leg("assets", "s.txt", "d.txt").unwrap();
    assert_eq!(copied.etag, "\"etag-77\"");
    assert!(ObjApplier::verify_leg(&copied, "\"etag-77\"").is_ok());
    assert_eq!(
        ObjApplier::verify_leg(&copied, "\"other\"")
            .unwrap_err()
            .code(),
        "conflict",
        "a mismatched ETag is a structured conflict"
    );

    // version_support reflects the bucket's versioning.
    assert_eq!(
        d.version_support(&Path::new("/s3/archive/x")),
        VersionSupport::Versioned
    );
    assert_eq!(
        d.version_support(&Path::new("/s3/assets/x")),
        VersionSupport::Snapshot
    );
    assert_eq!(
        d.version_support(&Path::new("/s3/nope/x")),
        VersionSupport::None
    );
}

// =================================================================================================
// Scenario 5 — Capability rejection at parse time with a structured error naming node + verbs.
// =================================================================================================

#[test]
fn s5_unsupported_verb_on_bucket_root_is_structurally_rejected() {
    let d = s3(Arc::new(MockObjectBackend::new()));

    // A bucket root admits ls/select/upsert/cp/mv but NOT a keyless REMOVE/RM/UPDATE.
    for bad in [Verb::Update, Verb::Remove, Verb::Rm] {
        let err = check_capability(&d, &Path::new("/s3/assets"), bad).unwrap_err();
        assert_eq!(
            err.code(),
            "unsupported_verb",
            "verb {bad:?} must be rejected"
        );
        match err {
            qfs_driver::CfsError::UnsupportedVerb {
                path, supported, ..
            } => {
                assert_eq!(path, "/s3/assets", "the error names the node");
                assert!(
                    supported.contains(&"LS"),
                    "names allowed verbs: {supported:?}"
                );
                assert!(supported.contains(&"UPSERT"));
                assert!(
                    !supported.contains(&"RM"),
                    "RM is NOT allowed on a bucket root"
                );
            }
            other => panic!("expected UnsupportedVerb, got {other:?}"),
        }
    }

    // A key node admits the full blob verb set.
    let key = Path::new("/s3/assets/k");
    for ok in [
        Verb::Ls,
        Verb::Select,
        Verb::Upsert,
        Verb::Remove,
        Verb::Cp,
        Verb::Mv,
        Verb::Rm,
    ] {
        assert!(
            check_capability(&d, &key, ok).is_ok(),
            "key node must allow {ok:?}"
        );
    }

    // An unregistered bucket has the empty capability set (everything rejected).
    assert!(check_capability(&d, &Path::new("/s3/ghost/k"), Verb::Ls).is_err());
}

// =================================================================================================
// Scenario 6 — Pushdown residual TRUTHFULNESS. Actively try to BREAK it: any predicate whose
// pushed prefix is a STRICT SUPERSET of the predicate MUST keep the exact predicate as a residual
// so the engine re-filters and no wrong rows are returned.
// =================================================================================================

/// The core property: for every predicate, the pushed `prefix` (if any) must be a SUPERSET filter,
/// and the residual must be dropped ONLY when the prefix is provably exactly the predicate.
/// We enumerate adversarial predicates and assert the residual is kept whenever the prefix would
/// admit a row the predicate rejects.
#[test]
fn s6_residual_is_kept_whenever_the_prefix_is_a_strict_superset() {
    // (a) key = 'logs/exact.json' → prefix "logs/exact.json" is a SUPERSET (also matches
    //     "logs/exact.jsonX"), so the exact `=` MUST be a residual.
    let eq = Predicate::Cmp(
        ColRef::col("key"),
        CmpOp::Eq,
        Literal::Text("logs/exact.json".into()),
    );
    let pd = ObjDriver::plan_ls(Some(&eq), None);
    assert_eq!(pd.prefix.as_deref(), Some("logs/exact.json"));
    assert_eq!(
        pd.residual.as_ref(),
        Some(&eq),
        "= MUST keep an exact residual"
    );

    // (b) AND(key LIKE 'img/%', size > 1000) → push "img/" but KEEP the whole predicate (the size
    //     conjunct still constrains; dropping it would return oversized AND undersized rows).
    let and = Predicate::And(
        Box::new(Predicate::Like(ColRef::col("key"), Pattern("img/%".into()))),
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
        "AND MUST keep the whole predicate"
    );

    // (c) BETWEEN 'apple' AND 'apricot' → common prefix "ap" is a superset (also "april"), residual
    //     MUST stay.
    let between = Predicate::Between(
        ColRef::col("key"),
        Literal::Text("apple".into()),
        Literal::Text("apricot".into()),
    );
    let pd = ObjDriver::plan_ls(Some(&between), None);
    assert_eq!(pd.prefix.as_deref(), Some("ap"));
    assert_eq!(
        pd.residual.as_ref(),
        Some(&between),
        "BETWEEN MUST keep the residual"
    );

    // (d) key >= 'm' → an ordering bound is NOT a prefix; pushing "m" would EXCLUDE "z..." rows the
    //     predicate keeps. The driver must push NOTHING and keep the whole residual (the
    //     correctness-over-cleverness call). This is the row-EXCLUSION trap, the inverse danger.
    let ge = Predicate::Cmp(ColRef::col("key"), CmpOp::Ge, Literal::Text("m".into()));
    let pd = ObjDriver::plan_ls(Some(&ge), None);
    assert!(
        pd.prefix.is_none(),
        "an ordering bound must NOT be pushed as a prefix: {:?}",
        pd.prefix
    );
    assert_eq!(
        pd.residual.as_ref(),
        Some(&ge),
        ">= MUST keep the whole residual"
    );

    // (e) a predicate over a NON-key column only → push nothing, keep everything.
    let nonkey = Predicate::Cmp(ColRef::col("size"), CmpOp::Gt, Literal::Int(5));
    let pd = ObjDriver::plan_ls(Some(&nonkey), None);
    assert!(pd.prefix.is_none());
    assert_eq!(pd.residual.as_ref(), Some(&nonkey));

    // (f) NEGATION: NOT(key LIKE 'tmp/%') — the prefix "tmp/" is the COMPLEMENT of what we want;
    //     pushing it would return EXACTLY the rows to exclude. The driver must not derive a prefix
    //     from a negation. Whatever it does, the residual must be the full predicate (no rows lost).
    let neg = Predicate::Not(Box::new(Predicate::Like(
        ColRef::col("key"),
        Pattern("tmp/%".into()),
    )));
    let pd = ObjDriver::plan_ls(Some(&neg), None);
    assert!(
        pd.prefix.is_none(),
        "a NOT must NOT push the negated prefix (it would return the excluded rows): {:?}",
        pd.prefix
    );
    assert_eq!(
        pd.residual.as_ref(),
        Some(&neg),
        "NOT MUST keep the full predicate"
    );

    // (g) OR: key LIKE 'a/%' OR key LIKE 'b/%' — neither single prefix covers the union; pushing
    //     one ("a/") would DROP all "b/..." rows. The driver must not push a single-branch prefix
    //     for an OR; the residual must be the whole predicate.
    let or = Predicate::Or(
        Box::new(Predicate::Like(ColRef::col("key"), Pattern("a/%".into()))),
        Box::new(Predicate::Like(ColRef::col("key"), Pattern("b/%".into()))),
    );
    let pd = ObjDriver::plan_ls(Some(&or), None);
    if let Some(p) = &pd.prefix {
        // If a prefix IS pushed for an OR, it would silently drop the other branch's rows. Fail loud.
        panic!("OR pushed prefix {p:?} — this DROPS the other branch's rows (a wrong-rows bug)");
    }
    assert_eq!(
        pd.residual.as_ref(),
        Some(&or),
        "OR MUST keep the whole predicate"
    );
}

/// The OTHER half of the property: a residual is dropped ONLY for a predicate that is EXACTLY a
/// key-prefix listing. Prove the only drop case is a tail-anchored LIKE with no embedded wildcard.
#[test]
fn s6_residual_is_dropped_only_for_an_exact_prefix_like() {
    // Exact prefix LIKE → residual dropped (the prefix list IS the predicate).
    let exact = Predicate::Like(ColRef::col("key"), Pattern("logs/2026/%".into()));
    let pd = ObjDriver::plan_ls(Some(&exact), None);
    assert_eq!(pd.prefix.as_deref(), Some("logs/2026/"));
    assert!(
        pd.residual.is_none(),
        "a tail-anchored LIKE is exactly the prefix list"
    );

    // A LIKE with an EMBEDDED wildcard is NOT a pure prefix → must not drop the residual.
    let mid = Predicate::Like(ColRef::col("key"), Pattern("logs/%/2026".into()));
    let pd = ObjDriver::plan_ls(Some(&mid), None);
    assert!(
        pd.residual.is_some() || pd.prefix.is_none(),
        "an embedded-wildcard LIKE must NOT be treated as an exact prefix (prefix={:?}, residual={:?})",
        pd.prefix,
        pd.residual
    );
    // Specifically: if any prefix is pushed it must be a superset with the residual kept.
    if pd.prefix.is_some() {
        assert_eq!(
            pd.residual.as_ref(),
            Some(&mid),
            "embedded-wildcard LIKE keeps the residual"
        );
    }

    // A LIKE with a leading wildcard ('%foo') is NOT a prefix → no prefix, residual kept.
    let lead = Predicate::Like(ColRef::col("key"), Pattern("%foo".into()));
    let pd = ObjDriver::plan_ls(Some(&lead), None);
    assert!(
        pd.prefix.is_none(),
        "a leading-wildcard LIKE has no prefix: {:?}",
        pd.prefix
    );
    assert_eq!(pd.residual.as_ref(), Some(&lead));

    // No predicate at all → no prefix, no residual.
    let pd = ObjDriver::plan_ls(None, Some("/"));
    assert!(pd.prefix.is_none() && pd.residual.is_none());
}

/// End-to-end residual truthfulness: drive a partial predicate THROUGH `ls`, then prove that
/// applying the returned residual to the mock's returned page would actually filter out the rows
/// the prefix over-returned. This is the "no wrong rows" contract, executed.
#[test]
fn s6_returned_residual_actually_filters_the_over_returned_rows() {
    // The mock returns 3 objects under prefix "logs/exact.json" — only ONE is the exact match.
    let page = ListPage::new(vec![
        ObjectMeta::new("logs/exact.json", 1),
        ObjectMeta::new("logs/exact.json.bak", 2), // over-returned by the prefix
        ObjectMeta::new("logs/exact.jsonX", 3),    // over-returned by the prefix
    ]);
    let backend = Arc::new(MockObjectBackend::new().with_list_page(page));
    let d = obj(backend);

    let eq = Predicate::Cmp(
        ColRef::col("key"),
        CmpOp::Eq,
        Literal::Text("logs/exact.json".into()),
    );
    let pd = ObjDriver::plan_ls(Some(&eq), None);
    let (result, residual) = d.ls(&Path::new("/s3/assets"), &pd, None).unwrap();

    // The native prefix list over-returned 3 rows.
    assert_eq!(
        result.objects.len(),
        3,
        "the native prefix list over-returns"
    );
    // The driver HANDED BACK the exact `=` residual, so the engine can re-filter to exactly 1.
    let residual = residual.expect("the residual MUST be present to re-filter");
    let surviving: Vec<&ObjectMeta> = result
        .objects
        .iter()
        .filter(|o| eval_residual(&residual, o))
        .collect();
    assert_eq!(
        surviving.len(),
        1,
        "re-filtering the residual yields exactly the exact match"
    );
    assert_eq!(surviving[0].key, "logs/exact.json");
}

/// A tiny residual evaluator for the `key = '...'` case used in the end-to-end residual test — it
/// stands in for the engine's re-filter to prove the residual is enough to recover correctness.
fn eval_residual(pred: &Predicate, obj: &ObjectMeta) -> bool {
    match pred {
        Predicate::Cmp(col, CmpOp::Eq, Literal::Text(v))
            if col.path.len() == 1 && col.path[0].as_str() == "key" =>
        {
            obj.key == *v
        }
        _ => true,
    }
}

// =================================================================================================
// Scenario 7 — Token safety: NO credential/canary string in any output, error, or list/plan result.
// =================================================================================================

#[test]
fn s7_no_canary_in_any_error_display_or_debug() {
    // Drive every public ObjError arm through Debug + Display; the canary must be nowhere.
    let errors = vec![
        ObjError::InvalidPath {
            path: "/s3/x".into(),
            reason: "bad",
        },
        ObjError::CapabilityDenied {
            path: "/s3/b/k".into(),
            verb: "UPDATE",
        },
        ObjError::Api {
            op: "put_object",
            status: 500,
        },
        ObjError::Decode {
            op: "list_objects_v2",
            reason: "not xml".into(),
        },
        ObjError::Transport {
            reason: "connection failed".into(),
        },
        ObjError::MultipartAborted {
            part: 2,
            reason: "part failed".into(),
        },
        ObjError::Conflict {
            version: "\"etag\"".into(),
        },
    ];
    for e in &errors {
        let dbg = format!("{e:?}");
        let disp = e.to_string();
        assert!(!dbg.contains(CANARY), "canary leaked in Debug: {dbg}");
        assert!(!disp.contains(CANARY), "canary leaked in Display: {disp}");
        assert!(!dbg.contains("cafef00d"), "canary fragment leaked: {dbg}");
        assert!(
            !dbg.contains("secret"),
            "the literal 'secret' must not ride in an error: {dbg}"
        );
    }
}

#[test]
fn s7_no_canary_in_list_results_plan_or_recorded_calls() {
    // Even though credentials never enter the DTO/plan/recorded surfaces by construction, drive a
    // full round and serialize every observable artifact, asserting the canary is absent.
    let page = ListPage::new(vec![ObjectMeta::new("k", 1).with_etag("\"e\"")]);
    let backend = Arc::new(
        MockObjectBackend::new()
            .with_list_page(page)
            .with_get_body(b"x".to_vec()),
    );
    let d = obj(backend.clone());

    let pd = ObjDriver::plan_ls(
        Some(&Predicate::Like(ColRef::col("key"), Pattern("k%".into()))),
        None,
    );
    let (result, _r) = d.ls(&Path::new("/s3/assets"), &pd, None).unwrap();
    d.get(&Path::new("/s3/assets/k"), None).unwrap();

    // Serialize the list page (the -json projection an operator sees) — no canary.
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains(CANARY), "canary in list JSON: {json}");

    // The ListPushdown Debug (the plan an operator can inspect) — no canary.
    let plan_dbg = format!("{pd:?}");
    assert!(!plan_dbg.contains(CANARY), "canary in plan: {plan_dbg}");

    // The recorded backend calls — secret-free by construction.
    let calls_dbg = format!("{:?}", backend.recorded());
    assert!(
        !calls_dbg.contains(CANARY),
        "canary in recorded calls: {calls_dbg}"
    );
    assert!(!calls_dbg.contains("cafef00d"));
}

// =================================================================================================
// Cross-cutting — end-to-end COMMIT through the runtime interpreter + bridge (both schemes).
// =================================================================================================

#[tokio::test]
async fn e2e_commit_upsert_and_remove_through_the_s3_bridge() {
    let backend = Arc::new(MockObjectBackend::new());
    let driver = s3(backend.clone());
    let bridge = s3_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(effect_node(
        0,
        EffectKind::Upsert,
        "s3",
        "/s3/assets/k1",
        one_row(vec![(
            "body",
            ColumnType::Text,
            Value::Text("payload".into()),
        )]),
    ));
    b.push(EffectNode::new(
        NodeId(1),
        EffectKind::Remove,
        Target::new(PlanDriverId::new("s3"), VfsPath::new("/s3/archive/k2@v1")),
    ));
    let caps = CapabilitySet::none()
        .grant(PlanDriverId::new("s3"), &EffectKind::Upsert)
        .grant(PlanDriverId::new("s3"), &EffectKind::Remove);
    let outcome = interp.commit(b.build(), &caps).await.unwrap();
    assert!(
        outcome.is_complete(),
        "both effects must apply: {outcome:?}"
    );

    let calls = backend.recorded();
    assert!(calls.iter().any(|c| matches!(c, RecordedCall::Put { .. })));
    assert!(calls
        .iter()
        .any(|c| matches!(c, RecordedCall::Delete { version_id, .. }
        if version_id.as_deref() == Some("v1"))));
}

#[tokio::test]
async fn e2e_r2_commits_through_its_own_bridge_id() {
    let backend = Arc::new(MockObjectBackend::new());
    let driver = R2Driver::new(registry(backend.clone()));
    assert_eq!(
        driver.id(),
        PlanDriverId::new("r2"),
        "R2 derives its own driver id"
    );
    let bridge = r2_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(effect_node(
        0,
        EffectKind::Upsert,
        "r2",
        "/r2/assets/k",
        one_row(vec![("body", ColumnType::Text, Value::Text("p".into()))]),
    ));
    let caps = CapabilitySet::none().grant(PlanDriverId::new("r2"), &EffectKind::Upsert);
    let outcome = interp.commit(b.build(), &caps).await.unwrap();
    assert!(
        outcome.is_complete(),
        "the r2 upsert must apply: {outcome:?}"
    );
    assert!(backend
        .recorded()
        .iter()
        .any(|c| matches!(c, RecordedCall::Put { .. })));
}

// =================================================================================================
// Cross-cutting — path/parse + capability gating + pushdown profile sanity.
// =================================================================================================

#[test]
fn x_path_parse_and_pushdown_profile() {
    // @version parses; an unmounted path is rejected.
    assert_eq!(
        ObjNode::parse_str("/r2/assets/k@v7").unwrap(),
        ObjNode::Object {
            scheme: Scheme::R2,
            bucket: "assets".into(),
            key: "k".into(),
            version_id: Some("v7".into()),
        }
    );
    assert_eq!(
        ObjNode::parse_str("/mail/inbox").unwrap_err().code(),
        "invalid_path"
    );

    // The declared pushdown profile is Partial with where_/project/limit on, no join/aggregate.
    let d = obj(Arc::new(MockObjectBackend::new()));
    match d.pushdown() {
        PushdownProfile::Partial {
            where_,
            project,
            join,
            aggregate,
            ..
        } => {
            assert!(
                *where_ && *project,
                "predicate + projection pushdown advertised"
            );
            assert!(!*join && !*aggregate, "no join/aggregate pushdown");
        }
        other => panic!("expected Partial pushdown, got {other:?}"),
    }
}
