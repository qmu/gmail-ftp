//! Drive driver tests (RFD-0001 §5 acceptance) — **no live Drive, no network, no credentials**.
//! Every test drives the introspective `Driver` surface and the apply leg against an in-memory
//! [`MockDriveClient`] (scripted Drive fixtures + recorded calls), so we assert request shape +
//! response decoding + plan shape + token-safety without touching a socket.

use std::sync::Arc;

use qfs_codec::JsonCodec;
use qfs_driver::{check_capability, resolve_proc, Archetype, Driver, Path, Verb, VersionSupport};
use qfs_plan::{
    preview, DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use qfs_types::{Column, Row, RowBatch, Schema, Value};

use super::*;
use crate::client::FilePage;

// ---- fixtures ----------------------------------------------------------------------------

/// A driver over a mock seeded with one folder, one binary file, one Google doc, and a Shared
/// Drive.
fn driver_with_mock() -> (GDriveDriver, Arc<MockDriveClient>) {
    let mock = Arc::new(
        MockDriveClient::new()
            .with_file(FileMeta {
                id: "folder1".to_string(),
                name: "reports".to_string(),
                mime_type: FOLDER_MIME.to_string(),
                parents: vec!["root".to_string()],
                size: 0,
                modified_time: 0,
                md5: String::new(),
                rev: String::new(),
                drive_id: String::new(),
                trashed: false,
            })
            .with_file(FileMeta {
                id: "f1".to_string(),
                name: "data.json".to_string(),
                mime_type: "application/json".to_string(),
                parents: vec!["folder1".to_string()],
                size: 17,
                modified_time: 1_700_000_000_000,
                md5: "abc".to_string(),
                rev: "rev9".to_string(),
                drive_id: String::new(),
                trashed: false,
            })
            .with_file(FileMeta {
                id: "doc1".to_string(),
                name: "notes".to_string(),
                mime_type: "application/vnd.google-apps.document".to_string(),
                parents: vec!["folder1".to_string()],
                size: 0,
                modified_time: 1_700_000_000_000,
                md5: String::new(),
                rev: "rev1".to_string(),
                drive_id: String::new(),
                trashed: false,
            })
            .with_drive(SharedDrive::for_test("d1", "team"))
            .with_download("f1", br#"[{"a":1},{"a":2}]"#.to_vec())
            .with_download("doc1", b"exported-bytes".to_vec()),
    );
    let driver = GDriveDriver::new(mock.clone() as Arc<dyn GDriveClient>);
    (driver, mock)
}

fn target(path: &str) -> Target {
    Target::new(DriverId::new("drive"), VfsPath::new(path))
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

// ---- introspection: mount / archetype / schema / version --------------------------------

#[test]
fn mount_and_id_are_drive() {
    let (d, _) = driver_with_mock();
    assert_eq!(d.mount(), "/drive");
    assert_eq!(d.id(), DriverId::new("drive"));
}

#[test]
fn describe_emits_blob_archetype_and_file_schema() {
    let (d, _) = driver_with_mock();
    let desc = d.describe(&Path::new("/drive/my/reports")).unwrap();
    assert_eq!(desc.archetype, Archetype::BlobNamespace);
    for col in [
        "id",
        "name",
        "mime_type",
        "parents",
        "size",
        "modified_time",
        "md5",
        "is_google_doc",
        "rev",
        "drive_id",
        "trashed",
    ] {
        assert!(desc.schema.column(col).is_some(), "missing column {col}");
    }
    assert_eq!(
        desc.schema.column("modified_time").unwrap().ty,
        qfs_types::ColumnType::Timestamp
    );
    // Drive versions files (RFD §4 @rev).
    assert_eq!(
        d.version_support(&Path::new("/drive/my/reports/data.json")),
        VersionSupport::Versioned
    );
}

// ---- capability golden (path-keyed gate) -------------------------------------------------

#[test]
fn capabilities_are_path_keyed() {
    let (d, _) = driver_with_mock();

    // A corpus root: ls/select only.
    let my_root = Path::new("/drive/my");
    assert!(check_capability(&d, &my_root, Verb::Ls).is_ok());
    assert!(check_capability(&d, &my_root, Verb::Select).is_ok());
    assert!(check_capability(&d, &my_root, Verb::Insert).is_err());

    // A folder path: ls/select/insert/upsert/remove/cp/mv.
    let folder = Path::new("/drive/my/reports");
    assert!(check_capability(&d, &folder, Verb::Insert).is_ok());
    assert!(check_capability(&d, &folder, Verb::Upsert).is_ok());
    assert!(check_capability(&d, &folder, Verb::Mv).is_ok());

    // A file by id: select/upsert/update/remove/cp/mv, NOT a relational insert of columns.
    let file = Path::new("id:f1");
    assert!(check_capability(&d, &file, Verb::Select).is_ok());
    assert!(check_capability(&d, &file, Verb::Remove).is_ok());
    let err = check_capability(&d, &file, Verb::Insert).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
}

#[test]
fn insert_of_columns_into_a_file_is_rejected_structurally() {
    let (d, _) = driver_with_mock();
    let err = check_capability(&d, &Path::new("id:f1"), Verb::Insert).unwrap_err();
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

// ---- procedures (drive.copy) -------------------------------------------------------------

#[test]
fn drive_copy_is_declared_with_drive_scope() {
    let (d, _) = driver_with_mock();
    let copy = resolve_proc(&d, "copy").unwrap();
    assert!(!copy.irreversible, "a copy creates, never destroys");
    assert_eq!(copy.requires_scopes, vec![DRIVE_SCOPE.to_string()]);
    assert_eq!(
        resolve_proc(&d, "nuke").unwrap_err().code(),
        "unknown_procedure"
    );
}

// ---- path parsing: my / shared / id / @rev ----------------------------------------------

#[test]
fn paths_parse_to_corpora_drives_ids_and_revisions() {
    assert_eq!(DrivePath::parse_str("/drive").unwrap(), DrivePath::Root);
    assert_eq!(
        DrivePath::parse_str("/drive/my").unwrap(),
        DrivePath::MyRoot
    );
    assert_eq!(
        DrivePath::parse_str("/drive/shared").unwrap(),
        DrivePath::SharedRoot
    );
    match DrivePath::parse_str("/drive/my/a/b.txt").unwrap() {
        DrivePath::My { segments, revision } => {
            assert_eq!(segments, vec!["a".to_string(), "b.txt".to_string()]);
            assert!(revision.is_none());
        }
        other => panic!("expected My, got {other:?}"),
    }
    // A @rev suffix on the last segment pins a revision.
    match DrivePath::parse_str("/drive/my/a/b.txt@rev7").unwrap() {
        DrivePath::My { segments, revision } => {
            assert_eq!(segments.last().unwrap(), "b.txt");
            assert_eq!(revision.as_deref(), Some("rev7"));
        }
        other => panic!("expected My with revision, got {other:?}"),
    }
    // Shared Drive path names the drive.
    match DrivePath::parse_str("/drive/shared/team/x").unwrap() {
        DrivePath::Shared {
            drive, segments, ..
        } => {
            assert_eq!(drive, "team");
            assert_eq!(segments, vec!["x".to_string()]);
        }
        other => panic!("expected Shared, got {other:?}"),
    }
    // id:<fileId>@rev.
    match DrivePath::parse_str("id:f1@rev9").unwrap() {
        DrivePath::ById { id, revision } => {
            assert_eq!(id, "f1");
            assert_eq!(revision.as_deref(), Some("rev9"));
        }
        other => panic!("expected ById, got {other:?}"),
    }
    // An invalid corpus is rejected.
    assert_eq!(
        DrivePath::parse_str("/drive/bogus").unwrap_err().code(),
        "invalid_path"
    );
}

// ---- pushdown: WHERE → Drive q, TRUTHFUL residual (the t20 lesson) ------------------------

#[test]
fn where_lowers_to_drive_q_with_lossy_residual_kept_local() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // name = 'report.txt' AND name ~ 'rep' AND mime_type = 'text/plain'.
    // `name =` and `mimeType =` are EXACT (drop from residual); `name ~` lowers to the loose
    // `name contains` pre-filter and MUST be kept as residual (over-fetch then filter).
    let name_eq = Predicate::Cmp(
        ColRef::col("name"),
        CmpOp::Eq,
        Literal::Text("report.txt".to_string()),
    );
    let name_match = Predicate::Cmp(
        ColRef::col("name"),
        CmpOp::Match,
        Literal::Text("rep".to_string()),
    );
    let mime_eq = Predicate::Cmp(
        ColRef::col("mime_type"),
        CmpOp::Eq,
        Literal::Text("text/plain".to_string()),
    );
    let pred = Predicate::And(
        Box::new(name_eq.clone()),
        Box::new(Predicate::And(
            Box::new(name_match.clone()),
            Box::new(mime_eq.clone()),
        )),
    );
    let res = query::build_query(Some("folder1"), Some(&pred));
    assert_eq!(
        res.query,
        "'folder1' in parents and name = 'report.txt' and name contains 'rep' and mimeType = 'text/plain'"
    );
    // Only the lossy `name ~ 'rep'` is kept as residual; the two exact terms drop out.
    assert_eq!(
        res.residual,
        Some(name_match),
        "lossy name-contains pre-filter is kept as residual; exact name=/mimeType= drop out"
    );

    let (d, _) = driver_with_mock();
    assert!(d.pushdown().supports_where());
    assert!(d.pushdown().supports_limit());
    assert!(!d.pushdown().supports_order());
}

#[test]
fn exact_predicates_push_fully_with_no_residual() {
    use qfs_types::{CmpOp, ColRef, Literal, Predicate};
    // mime_type = 'folder' AND trashed = false: both EXACT, so the whole predicate pushes and
    // nothing is left to re-check locally — residual is None.
    let pred = Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("mime_type"),
            CmpOp::Eq,
            Literal::Text(FOLDER_MIME.to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("trashed"),
            CmpOp::Eq,
            Literal::Bool(false),
        )),
    );
    let res = query::build_query(None, Some(&pred));
    assert_eq!(
        res.query,
        format!("mimeType = '{FOLDER_MIME}' and trashed = false")
    );
    assert!(
        res.residual.is_none(),
        "exact mimeType/trashed mappings leave no residual to re-check"
    );
}

#[test]
fn lossy_predicate_returns_residual_so_engine_refilters() {
    use qfs_types::{CmpOp, ColRef, Literal, Pattern, Predicate};

    // name LIKE 'weekly' pushes the loose `name contains 'weekly'` but MUST keep the exact LIKE
    // as residual: Drive `contains` also matches "weekly-2024.txt" / a token in a longer name.
    let like = Predicate::Like(ColRef::col("name"), Pattern("weekly".to_string()));
    let res = query::build_query(None, Some(&like));
    assert_eq!(res.query, "name contains 'weekly'");
    assert_eq!(
        res.residual,
        Some(like),
        "LIKE has no Drive operator — `contains` is looser, so kept residual"
    );

    // A modified_time bound is second-granular RFC-3339 — looser than the exact ms comparison,
    // so it is kept as residual.
    let date_gt = Predicate::Cmp(
        ColRef::col("modified_time"),
        CmpOp::Gt,
        Literal::Int(1_700_000_500_500),
    );
    let res = query::build_query(None, Some(&date_gt));
    assert_eq!(res.query, "modifiedTime > '2023-11-14T22:21:40Z'");
    assert_eq!(
        res.residual,
        Some(date_gt),
        "the modifiedTime bound is second-granular/truncated — kept residual for the exact ms compare"
    );

    // An OR stays wholly residual (Drive `and`-joining cannot express it).
    let or_pred = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("name"),
            CmpOp::Eq,
            Literal::Text("a".into()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("name"),
            CmpOp::Eq,
            Literal::Text("b".into()),
        )),
    );
    let res = query::build_query(None, Some(&or_pred));
    assert_eq!(res.query, "", "nothing pushed for an OR");
    assert!(res.residual.is_some(), "OR is residual, filtered locally");
}

// ---- list/search reaches the client + decodes file rows ----------------------------------

#[test]
fn list_files_pushes_q_and_decodes_file_rows() {
    let (_d, mock) = driver_with_mock();
    let client: &dyn GDriveClient = &*mock;
    let _ = mock; // bind
                  // Seed a page and list it with a pushed q + Shared-Drive scope.
    let page = FilePage {
        files: vec![FileMeta::for_test(
            "f1",
            "data.json",
            "application/json",
            vec!["folder1".to_string()],
        )],
        next_page_token: None,
    };
    // re-seed through a fresh mock to control the page queue deterministically
    let mock2 = Arc::new(MockDriveClient::new().with_list_page(page));
    let client2: &dyn GDriveClient = &*mock2;
    let res = client2
        .list_files("'folder1' in parents", Some("d1"), Some(50))
        .unwrap();
    assert_eq!(res.files.len(), 1);
    let row = res.files[0].to_row();
    assert_eq!(row.values[0], Value::Text("f1".to_string()));
    assert_eq!(row.values[1], Value::Text("data.json".to_string()));
    // The mock recorded the exact q + Shared-Drive id + page size.
    assert!(mock2.recorded().contains(&RecordedCall::ListFiles {
        query: "'folder1' in parents".to_string(),
        drive_id: Some("d1".to_string()),
        page_size: Some(50),
    }));
    let _ = client; // bound to prove the seeded mock is distinct.
}

#[test]
fn list_drives_returns_shared_drives() {
    let (_d, mock) = driver_with_mock();
    let client: &dyn GDriveClient = &*mock;
    let drives = client.list_drives().unwrap();
    assert_eq!(drives, vec![SharedDrive::for_test("d1", "team")]);
    assert!(mock.recorded().contains(&RecordedCall::ListDrives));
}

// ---- read path: download+decode, and export of a google doc ------------------------------

#[test]
fn download_binary_then_decode_to_rows_via_codec() {
    let (_d, mock) = driver_with_mock();
    let client: &dyn GDriveClient = &*mock;
    let file = client.get_file("f1").unwrap();
    // A binary file plans a raw download.
    let plan = plan_read(&file, None, None).unwrap();
    assert_eq!(
        plan,
        ReadPlan::Download {
            id: "f1".to_string(),
            revision: None
        }
    );
    let bytes = client.download("f1", None).unwrap();
    // Decode the JSON body to rows via the codec (the bytes → rows boundary).
    let batch = decode_body(&JsonCodec, &bytes).unwrap();
    assert_eq!(batch.rows.len(), 2);
    assert!(mock.recorded().contains(&RecordedCall::Download {
        id: "f1".to_string(),
        revision: None,
    }));
}

#[test]
fn google_doc_plans_an_export_to_docx_by_default_and_honors_override() {
    let (_d, mock) = driver_with_mock();
    let client: &dyn GDriveClient = &*mock;
    let doc = client.get_file("doc1").unwrap();
    assert!(doc.is_google_doc(), "a google doc has no raw bytes");
    // Default export → docx.
    let plan = plan_read(&doc, None, None).unwrap();
    match plan {
        ReadPlan::Export { id, target } => {
            assert_eq!(id, "doc1");
            assert_eq!(target.suffix, "docx");
            assert!(target.mime.contains("wordprocessingml"));
        }
        other => panic!("expected Export, got {other:?}"),
    }
    // An explicit override → pdf.
    let plan = plan_read(&doc, None, Some("pdf")).unwrap();
    match plan {
        ReadPlan::Export { target, .. } => {
            assert_eq!(target.mime, "application/pdf");
            assert_eq!(target.suffix, "pdf");
        }
        other => panic!("expected Export, got {other:?}"),
    }
    // The export call reaches the client.
    let _ = client.export("doc1", "application/pdf").unwrap();
    assert!(mock.recorded().contains(&RecordedCall::Export {
        id: "doc1".to_string(),
        export_mime: "application/pdf".to_string(),
    }));
}

// ---- effect decode: upload / upsert / move / trash / hard-delete / copy -------------------

#[test]
fn insert_into_folder_decodes_to_upload() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Insert,
        target("/drive/my/reports/x.txt"),
    )
    .with_args(args(&[
        (PARENT_ID_COL, Value::Text("folder1".into())),
        (MIME_COL, Value::Text("text/plain".into())),
        (BYTES_COL, Value::Bytes(b"hi".to_vec())),
    ]));
    match DriveEffect::from_node(&node).unwrap() {
        DriveEffect::Upload {
            parent,
            name,
            mime,
            bytes,
        } => {
            assert_eq!(parent, "folder1");
            assert_eq!(name, "x.txt", "name falls back to the path leaf segment");
            assert_eq!(mime, "text/plain");
            assert_eq!(bytes, b"hi");
        }
        other => panic!("expected Upload, got {other:?}"),
    }
}

#[test]
fn upsert_with_file_id_is_retry_safe_content_replace() {
    let node = EffectNode::new(NodeId(0), EffectKind::Upsert, target("id:f1")).with_args(args(&[
        (FILE_ID_COL, Value::Text("f1".into())),
        (MIME_COL, Value::Text("application/json".into())),
        (BYTES_COL, Value::Bytes(b"[]".to_vec())),
    ]));
    match DriveEffect::from_node(&node).unwrap() {
        DriveEffect::Update { id, .. } => assert_eq!(id, "f1"),
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn update_decodes_to_rename_and_move() {
    let node = EffectNode::new(NodeId(0), EffectKind::Update, target("id:f1")).with_args(args(&[
        (FILE_ID_COL, Value::Text("f1".into())),
        (NAME_COL, Value::Text("renamed.json".into())),
        (ADD_PARENTS_COL, Value::Text("folder2".into())),
        (REMOVE_PARENTS_COL, Value::Text("folder1".into())),
    ]));
    match DriveEffect::from_node(&node).unwrap() {
        DriveEffect::Move {
            id,
            new_name,
            add_parents,
            remove_parents,
        } => {
            assert_eq!(id, "f1");
            assert_eq!(new_name.as_deref(), Some("renamed.json"));
            assert_eq!(add_parents, vec!["folder2".to_string()]);
            assert_eq!(remove_parents, vec!["folder1".to_string()]);
        }
        other => panic!("expected Move, got {other:?}"),
    }
}

#[test]
fn remove_defaults_to_trash_not_permanent_delete() {
    let node = EffectNode::new(NodeId(0), EffectKind::Remove, target("id:f1"));
    assert!(node.irreversible, "REMOVE is inherently irreversible");
    match DriveEffect::from_node(&node).unwrap() {
        DriveEffect::Trash { id } => assert_eq!(id, "f1"),
        other => panic!("expected Trash (recoverable), got {other:?}"),
    }
    // An explicit hard_delete flag selects the permanent, irreversible delete.
    let hard = EffectNode::new(NodeId(1), EffectKind::Remove, target("id:f1"))
        .with_args(args(&[(HARD_DELETE_COL, Value::Bool(true))]));
    match DriveEffect::from_node(&hard).unwrap() {
        DriveEffect::Delete { id } => assert_eq!(id, "f1"),
        other => panic!("expected Delete, got {other:?}"),
    }
}

#[test]
fn call_drive_copy_decodes_and_unknown_proc_rejected() {
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Call(ProcId::new("drive.copy")),
        target("id:f1"),
    )
    .with_args(args(&[
        (FILE_ID_COL, Value::Text("f1".into())),
        (PARENT_ID_COL, Value::Text("folder2".into())),
        (NAME_COL, Value::Text("copy.json".into())),
    ]));
    match DriveEffect::from_node(&node).unwrap() {
        DriveEffect::Copy { id, parent, name } => {
            assert_eq!(id, "f1");
            assert_eq!(parent, "folder2");
            assert_eq!(name, "copy.json");
        }
        other => panic!("expected Copy, got {other:?}"),
    }
    let bad = EffectNode::new(
        NodeId(1),
        EffectKind::Call(ProcId::new("drive.nuke")),
        target("id:f1"),
    );
    assert_eq!(
        DriveEffect::from_node(&bad).unwrap_err().code(),
        "unknown_procedure"
    );
}

// ---- PREVIEW performs no I/O (mock asserts zero calls) -----------------------------------

#[test]
fn preview_of_a_trash_plan_performs_no_io() {
    let (_d, mock) = driver_with_mock();
    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("id:f1"),
    ));
    let plan = b.build();
    let pv = preview(&plan);
    assert_eq!(pv.rows.len(), 1);
    assert!(
        pv.rows[0].irreversible,
        "preview surfaces the irreversible REMOVE (trash)"
    );
    assert!(
        mock.recorded().is_empty(),
        "PREVIEW must perform zero Drive API calls: {:?}",
        mock.recorded()
    );
}

// ---- token never in logs / errors --------------------------------------------------------

#[test]
fn errors_are_secret_free() {
    let errs = [
        DriveError::Api {
            op: "files.list",
            status: 500,
        },
        DriveError::CapabilityDenied {
            path: "id:f1".into(),
            verb: "INSERT",
        },
        DriveError::from(qfs_google_auth::AuthError::TokenRefresh {
            reason: "invalid_grant".to_string(),
        }),
    ];
    for e in &errs {
        let text = format!("{e} {e:?}");
        assert!(!text.contains("Bearer"), "no bearer in error: {text}");
        assert!(!text.contains("ya29"), "no google token prefix: {text}");
    }
    match DriveError::from(qfs_google_auth::AuthError::TokenRefresh {
        reason: "invalid_grant".to_string(),
    }) {
        DriveError::Auth { code, reauthorize } => {
            assert_eq!(code, "auth_token_refresh");
            assert!(reauthorize);
        }
        other => panic!("expected Auth, got {other:?}"),
    }
}

// ---- end-to-end: commit through interpreter + bridge -------------------------------------

#[tokio::test]
async fn commit_trash_file_end_to_end_through_interpreter() {
    let (driver, mock) = driver_with_mock();
    let bridge = gdrive_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        target("id:f1"),
    ));
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none().grant(DriverId::new("drive"), &EffectKind::Remove);
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert!(outcome.is_complete(), "trash applied: {outcome:?}");
    assert_eq!(outcome.applied_ids(), vec![NodeId(0)]);
    // The applier trashed (not permanently deleted) exactly one file.
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::Trash {
            id: "f1".to_string()
        }]
    );
}

#[tokio::test]
async fn commit_upsert_replaces_content_by_id() {
    let (driver, mock) = driver_with_mock();
    let bridge = gdrive_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(NodeId(0), EffectKind::Upsert, target("id:f1")).with_args(args(&[
            (FILE_ID_COL, Value::Text("f1".into())),
            (MIME_COL, Value::Text("application/json".into())),
            (BYTES_COL, Value::Bytes(b"[]".to_vec())),
        ])),
    );
    let plan = b.build();

    let caps = CapabilitySet::none().grant(DriverId::new("drive"), &EffectKind::Upsert);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete(), "upsert applied: {outcome:?}");
    assert_eq!(
        mock.recorded(),
        vec![RecordedCall::UpdateContent {
            id: "f1".to_string(),
            mime: "application/json".to_string(),
            len: 2,
        }]
    );
}

#[tokio::test]
async fn multi_account_selects_independent_clients() {
    // Two accounts → two independent driver instances over two independent mock clients. Each
    // driver routes only to its own client; selection is the t19 base (one client per account).
    let mock_a = Arc::new(MockDriveClient::new());
    let mock_b = Arc::new(MockDriveClient::new());
    let driver_a = GDriveDriver::new(mock_a.clone() as Arc<dyn GDriveClient>);
    let driver_b = GDriveDriver::new(mock_b.clone() as Arc<dyn GDriveClient>);

    let node = EffectNode::new(NodeId(0), EffectKind::Remove, target("id:fA"));
    use qfs_runtime::SharedApplier;
    driver_a.drive_applier().apply_shared(&node).unwrap();

    assert_eq!(
        mock_a.recorded(),
        vec![RecordedCall::Trash {
            id: "fA".to_string()
        }]
    );
    assert!(
        mock_b.recorded().is_empty(),
        "account B's client was untouched"
    );
    let _ = driver_b;
}
