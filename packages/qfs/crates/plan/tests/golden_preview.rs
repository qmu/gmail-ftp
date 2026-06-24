//! Golden snapshot tests for `PREVIEW` rendering (t09 acceptance criterion).
//!
//! A representative **mixed** plan — read + insert + remove + call(mail.send) — is
//! previewed and both its human `Display` text and its `-json` serialization are
//! pinned byte-for-byte. This locks the deterministic ordering, the per-node affected
//! counts, the irreversible-warning section, and `total_affected` (RFD §6/§7/§10): any
//! refactor that perturbs the dry-run surface trips the compare. **No live
//! credentials** and no network are used.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_plan::{
    preview, Affected, DriverId, EffectKind, EffectNode, NodeId, Plan, PlanBuilder, ProcId, Target,
    VfsPath,
};
use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

/// Build the representative mixed plan: a `Read` feeding an `Insert`, an independent
/// `Remove`, and a `Call mail.send` that depends on the `Insert`.
fn mixed_plan() -> Plan {
    let mut b = PlanBuilder::new();

    // #0 READ /sql/pg/source — a pure data-acquisition dependency.
    let read = b.next_id();
    b.push(
        EffectNode::new(
            read,
            EffectKind::Read,
            Target::new(DriverId::new("sql"), VfsPath::new("/sql/pg/source")),
        )
        .with_affected(Affected::AtMost(10)),
    );

    // #1 INSERT /sql/pg/orders — two literal rows (Exact 2).
    let insert = b.next_id();
    let schema = Schema::new(vec![Column::new("id", ColumnType::Int, false)]);
    let batch = RowBatch::new(
        schema,
        vec![Row::new(vec![Value::Int(1)]), Row::new(vec![Value::Int(2)])],
    );
    b.push(
        EffectNode::new(
            insert,
            EffectKind::Insert,
            Target::new(DriverId::new("sql"), VfsPath::new("/sql/pg/orders")),
        )
        .with_args(batch),
    );

    // #2 REMOVE /mail/spam — irreversible, AtMost 5.
    let remove = b.next_id();
    b.push(
        EffectNode::new(
            remove,
            EffectKind::Remove,
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/spam")),
        )
        .with_affected(Affected::AtMost(5)),
    );

    // #3 CALL mail.send — irreversible, Exact 1, depends on the insert.
    let send = b.next_id();
    b.push(
        EffectNode::new(
            send,
            EffectKind::Call(ProcId::new("mail.send")),
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/outbox")),
        )
        .irreversible(true)
        .with_affected(Affected::Exact(1)),
    );

    b.depends_on(insert, read); // read before insert
    b.depends_on(send, insert); // insert before send
    b.build()
}

#[test]
fn mixed_plan_is_valid() {
    assert!(mixed_plan().validate().is_ok());
}

#[test]
fn golden_preview_display() {
    let plan = mixed_plan();
    let pv = preview(&plan);
    let rendered = pv.to_string();
    let golden = "\
PREVIEW: 4 effect(s)
  #0 READ -> sql:/sql/pg/source [affected <=10]
  #1 INSERT -> sql:/sql/pg/orders [affected 2]
  #2 REMOVE -> mail:/mail/spam [affected <=5] (!)
  #3 CALL mail.send -> mail:/mail/outbox [affected 1] (!)
  (!) irreversible: 2 node(s) [#2, #3]
  total affected: <=18";
    assert_eq!(rendered, golden, "PREVIEW Display drifted from golden");
}

#[test]
fn golden_preview_json() {
    let plan = mixed_plan();
    let pv = preview(&plan);
    let json = serde_json::to_string(&pv).unwrap();
    let golden = r#"{"rows":[{"id":0,"verb":"READ","target":{"driver":"sql","path":"/sql/pg/source"},"affected":{"at_most":10},"irreversible":false},{"id":1,"verb":"INSERT","target":{"driver":"sql","path":"/sql/pg/orders"},"affected":{"exact":2},"irreversible":false},{"id":2,"verb":"REMOVE","target":{"driver":"mail","path":"/mail/spam"},"affected":{"at_most":5},"irreversible":true},{"id":3,"verb":"CALL mail.send","target":{"driver":"mail","path":"/mail/outbox"},"affected":{"exact":1},"irreversible":true}],"irreversible":[2,3],"total_affected":{"at_most":18},"is_pure":false}"#;
    assert_eq!(json, golden, "PREVIEW JSON drifted from golden");
}

#[test]
fn preview_ordering_is_deterministic_across_runs() {
    let a = serde_json::to_string(&preview(&mixed_plan())).unwrap();
    let b = serde_json::to_string(&preview(&mixed_plan())).unwrap();
    assert_eq!(a, b);
}

#[test]
fn nodeid_serializes_as_plain_index() {
    // The preview JSON addresses each node by its raw index (observability).
    assert_eq!(serde_json::to_string(&NodeId(7)).unwrap(), "7");
}
