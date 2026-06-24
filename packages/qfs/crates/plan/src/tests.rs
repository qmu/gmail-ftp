//! Unit tests for the effect-plan data model (t09 acceptance criteria): plan
//! construction, DAG dependency ordering, irreversible flagging, validation, and the
//! PREVIEW vs COMMIT semantics. Golden snapshots live in `tests/golden_preview.rs`.

use super::*;
use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

/// A two-row insert batch for a `(/driver, path)` target.
fn insert_node(builder: &mut PlanBuilder, driver: &str, path: &str) -> NodeId {
    let id = builder.next_id();
    let schema = Schema::new(vec![Column::new("id", ColumnType::Int, false)]);
    let batch = RowBatch::new(
        schema,
        vec![Row::new(vec![Value::Int(1)]), Row::new(vec![Value::Int(2)])],
    );
    let target = Target::new(DriverId::new(driver), VfsPath::new(path));
    builder.push(EffectNode::new(id, EffectKind::Insert, target).with_args(batch))
}

fn call_send_node(builder: &mut PlanBuilder) -> NodeId {
    let id = builder.next_id();
    let target = Target::new(DriverId::new("mail"), VfsPath::new("/mail/outbox"));
    builder.push(
        EffectNode::new(id, EffectKind::Call(ProcId::new("mail.send")), target)
            .irreversible(true)
            .with_affected(Affected::Exact(1)),
    )
}

#[test]
fn pure_plan_is_empty_and_reversible() {
    let p = Plan::pure();
    assert!(p.nodes().is_empty());
    assert!(p.deps().is_empty());
    assert!(!p.is_irreversible());
    assert!(p.validate().is_ok());
    assert!(preview(&p).is_pure);
}

#[test]
fn leaf_plan_has_one_node() {
    let mut b = PlanBuilder::new();
    let _id = insert_node(&mut b, "sql", "/sql/pg/orders");
    let p = b.build();
    assert_eq!(p.nodes().len(), 1);
    assert!(p.validate().is_ok());
}

#[test]
fn insert_then_call_is_a_two_node_dag_with_one_edge() {
    // Acceptance: INSERT then CALL mail.send → 2-node DAG, one dependency edge.
    let mut b = PlanBuilder::new();
    let insert = insert_node(&mut b, "sql", "/sql/pg/orders");
    let send = call_send_node(&mut b);
    b.depends_on(send, insert); // send depends on insert
    let p = b.build();

    assert_eq!(p.nodes().len(), 2);
    assert_eq!(p.deps().len(), 1);
    assert_eq!(p.deps()[0], (insert, send));
    assert!(p.validate().is_ok());

    // Topo order: insert before send.
    let order = topo_order(&p).unwrap();
    assert_eq!(order, vec![insert, send]);
}

#[test]
fn remove_and_call_send_are_irreversible() {
    // Acceptance: Remove and Call(mail.send) have irreversible == true.
    let mut b = PlanBuilder::new();
    let rid = b.next_id();
    let remove = EffectNode::new(
        rid,
        EffectKind::Remove,
        Target::new(DriverId::new("mail"), VfsPath::new("/mail/spam")),
    );
    assert!(remove.irreversible, "Remove is inherently irreversible");
    b.push(remove);
    let _send = call_send_node(&mut b);
    let p = b.build();
    assert!(p.is_irreversible());
    assert_eq!(preview(&p).irreversible.len(), 2);
}

#[test]
fn insert_is_reversible() {
    let mut b = PlanBuilder::new();
    let _ = insert_node(&mut b, "sql", "/t");
    let p = b.build();
    assert!(!p.is_irreversible());
}

#[test]
fn validate_rejects_dangling_dep() {
    let mut b = PlanBuilder::new();
    let insert = insert_node(&mut b, "sql", "/t");
    let mut p = b.build();
    // Add an edge to a node id that does not exist.
    p = depends_on(p, NodeId(999), insert);
    assert_eq!(
        p.validate(),
        Err(PlanError::DanglingDep {
            child: NodeId(999),
            parent: insert,
        })
    );
}

#[test]
fn validate_rejects_cycle() {
    let mut b = PlanBuilder::new();
    let a = insert_node(&mut b, "sql", "/a");
    let c = insert_node(&mut b, "sql", "/b");
    b.depends_on(c, a); // a -> c
    b.depends_on(a, c); // c -> a  (cycle)
    let p = b.build();
    assert_eq!(p.validate(), Err(PlanError::Cyclic));
    assert!(topo_order(&p).is_none());
}

#[test]
fn validate_rejects_duplicate_id() {
    let target = Target::new(DriverId::new("x"), VfsPath::new("/x"));
    let p = Plan {
        nodes: vec![
            EffectNode::new(NodeId(0), EffectKind::Insert, target.clone()),
            EffectNode::new(NodeId(0), EffectKind::Update, target),
        ],
        deps: vec![],
        returning: None,
    };
    assert_eq!(p.validate(), Err(PlanError::DuplicateId(NodeId(0))));
}

#[test]
fn topo_is_deterministic_across_runs() {
    // Build a diamond and confirm two independent runs produce identical order.
    let build = || {
        let mut b = PlanBuilder::new();
        let root = insert_node(&mut b, "d", "/root");
        let left = insert_node(&mut b, "d", "/left");
        let right = insert_node(&mut b, "d", "/right");
        let sink = insert_node(&mut b, "d", "/sink");
        b.depends_on(left, root);
        b.depends_on(right, root);
        b.depends_on(sink, left);
        b.depends_on(sink, right);
        b.build()
    };
    let p1 = build();
    let p2 = build();
    let o1 = topo_order(&p1).unwrap();
    let o2 = topo_order(&p2).unwrap();
    assert_eq!(o1, o2);
    // Root first, sink last; the two middle nodes in NodeId order.
    assert_eq!(o1, vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)]);
}

#[test]
fn then_combinator_sequences_subplans() {
    let mut b1 = PlanBuilder::new();
    let insert = insert_node(&mut b1, "sql", "/t");
    let p1 = b1.build();
    let mut b2 = PlanBuilder::new();
    // Continue id allocation so ids stay disjoint.
    let _ = b2.next_id();
    let send_id = b2.next_id();
    let p2 = Plan::leaf(
        EffectNode::new(
            send_id,
            EffectKind::Call(ProcId::new("mail.send")),
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/outbox")),
        )
        .irreversible(true),
    );
    let combined = p1.then(p2);
    assert_eq!(combined.nodes().len(), 2);
    assert_eq!(combined.deps(), &[(insert, send_id)]);
    assert!(combined.validate().is_ok());
}

#[test]
fn merge_combinator_unions_independent_subplans() {
    let mut b1 = PlanBuilder::new();
    let a = insert_node(&mut b1, "d", "/a");
    let p1 = b1.build();
    let mut b2 = PlanBuilder::new();
    let _ = b2.next_id(); // skip 0 to keep ids disjoint
    let c = insert_node(&mut b2, "d", "/b");
    let p2 = b2.build();
    let merged = p1.merge(p2);
    assert_eq!(merged.nodes().len(), 2);
    assert!(merged.deps().is_empty());
    assert!(merged.validate().is_ok());
    // Independent: both are roots; order is NodeId-ascending.
    assert_eq!(topo_order(&merged).unwrap(), vec![a, c]);
}

#[test]
fn affected_combines_honestly() {
    assert_eq!(
        Affected::Exact(2).combine(Affected::Exact(3)),
        Affected::Exact(5)
    );
    assert_eq!(
        Affected::Exact(2).combine(Affected::AtMost(3)),
        Affected::AtMost(5)
    );
    assert_eq!(
        Affected::Exact(2).combine(Affected::Unknown),
        Affected::Unknown
    );
}

#[test]
fn preview_reports_counts_and_irreversible_without_applying() {
    // PREVIEW semantics: no side effects, honest counts, irreversible called out.
    let mut b = PlanBuilder::new();
    let insert = insert_node(&mut b, "sql", "/sql/pg/orders");
    let send = call_send_node(&mut b);
    b.depends_on(send, insert);
    let p = b.build();

    let pv = preview(&p);
    assert!(!pv.is_pure);
    assert_eq!(pv.rows.len(), 2);
    // Order respects dependency: insert (Exact 2) before send (Exact 1).
    assert_eq!(pv.rows[0].id, insert);
    assert_eq!(pv.rows[0].affected, Affected::Exact(2));
    assert_eq!(pv.rows[1].id, send);
    assert_eq!(pv.irreversible, vec![send]);
    assert_eq!(pv.total_affected, Affected::Exact(3));
}

#[test]
fn commit_applies_in_dependency_order() {
    // COMMIT semantics: applier called per node in topo order.
    let mut b = PlanBuilder::new();
    let insert = insert_node(&mut b, "sql", "/t");
    let send = call_send_node(&mut b);
    b.depends_on(send, insert);
    let p = b.build();

    let mut applier = RecordingApplier::new();
    let mut ledger: Vec<NodeId> = Vec::new();
    let report = commit(&p, &mut applier, |eff| ledger.push(eff.id));

    assert!(report.is_complete());
    assert_eq!(applier.applied, vec![insert, send]);
    assert_eq!(ledger, vec![insert, send]);
    assert_eq!(report.applied.len(), 2);
    // Declared Affected flows through to the applied count.
    assert_eq!(report.applied[0].affected, 2);
    assert_eq!(report.applied[1].affected, 1);
}

#[test]
fn commit_skips_dependents_of_a_failed_node() {
    // Acceptance: when a parent fails, dependents are reported skipped (not applied).
    let mut b = PlanBuilder::new();
    let insert = insert_node(&mut b, "sql", "/t");
    let send = call_send_node(&mut b);
    b.depends_on(send, insert);
    let p = b.build();

    let mut applier = RecordingApplier::new().failing_on(insert);
    let report = commit(&p, &mut applier, |_| {});

    // The failed parent was attempted; the dependent was NOT applied.
    assert_eq!(applier.applied, vec![insert]);
    assert!(report.applied.is_empty());
    assert!(report.failed.is_some());
    assert_eq!(
        report.skipped,
        vec![(send, SkipReason::DependencyFailed(insert))]
    );
    assert!(!report.is_complete());
}

#[test]
fn commit_of_pure_plan_does_nothing() {
    let p = Plan::pure();
    let mut applier = RecordingApplier::new();
    let report = commit(&p, &mut applier, |_| {});
    assert!(applier.applied.is_empty());
    assert!(report.is_complete());
}

#[test]
fn kind_for_verb_maps_all_write_verbs() {
    assert_eq!(kind_for_verb(WriteVerb::Insert), EffectKind::Insert);
    assert_eq!(kind_for_verb(WriteVerb::Upsert), EffectKind::Upsert);
    assert_eq!(kind_for_verb(WriteVerb::Update), EffectKind::Update);
    assert_eq!(kind_for_verb(WriteVerb::Remove), EffectKind::Remove);
    // Remove built from the verb is irreversible.
    let node = EffectNode::new(
        NodeId(0),
        kind_for_verb(WriteVerb::Remove),
        Target::new(DriverId::new("mail"), VfsPath::new("/mail/spam")),
    );
    assert!(node.irreversible);
}

#[test]
fn upsert_is_distinct_from_insert() {
    // Idempotency: Upsert modelled distinctly so retry-safe effects are first-class.
    assert_ne!(EffectKind::Insert, EffectKind::Upsert);
}

#[test]
fn server_config_write_is_pure_data_and_reversible() {
    // t30: the /server self-config write is a closed-set EffectKind variant carrying only
    // vendor-free coordinates (which node + which op); the config DTO rides in the node's
    // RowBatch args. It is NOT inherently irreversible (a config write is undoable).
    use crate::{ServerNode, ServerWriteOp};
    let kind = EffectKind::ServerConfigWrite {
        node: ServerNode::Jobs,
        op: ServerWriteOp::Upsert,
    };
    assert!(!kind.is_inherently_irreversible());
    assert_eq!(kind.label(), "SERVER_JOBS_UPSERT");

    // Distinct collections / ops are distinct kinds (so coalescing groups them separately).
    assert_ne!(
        kind,
        EffectKind::ServerConfigWrite {
            node: ServerNode::Triggers,
            op: ServerWriteOp::Upsert
        }
    );
    assert_ne!(
        kind,
        EffectKind::ServerConfigWrite {
            node: ServerNode::Jobs,
            op: ServerWriteOp::Remove
        }
    );

    // The node coordinate maps to its /server path + segment (single source of truth).
    assert_eq!(ServerNode::Jobs.path(), "/server/jobs");
    assert_eq!(
        ServerNode::from_segment("materialized_views"),
        Some(ServerNode::Views)
    );
    assert_eq!(ServerWriteOp::Remove.label(), "REMOVE");
}
