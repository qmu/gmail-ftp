//! Black-box E2E coverage for the t28 interactive shell logic (ticket
//! `20260622214650-t28-cli-interactive-shell`), driving the public `qfs_exec::shell` API as the
//! external surface: [`Session::eval_line`] (the line evaluator the REPL feeds) and
//! [`Completer::complete`] (the tab-completion API). These exercise the two acceptance criteria a
//! single-mount run cannot reach via the real `qfs` binary — **cross-mount `cp`/`mv`** (needs a
//! second driver) and **builtin ≡ typed-statement plan equivalence** — and assert them at the
//! PLAN level (the produced effect `Preview`), per the ticket ("asserted by plan assertions, not
//! live effects"). All drivers are in-memory fakes; no live creds, no network, no real FS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, Driver, DriverId, Engine, NodeDesc,
    Path, PushdownProfile, Row, RowBatch, Schema, Value,
};
use qfs_exec::shell::{Outcome, Session, VfsPath};
use qfs_exec::{Completer, ReadDriver, ReadRegistry};
use qfs_pushdown::ScanNode;

/// A minimal in-memory namespace driver mounted at `mount`, listing the fixed `names`. It is both
/// a `Driver` (so the planner can `describe`/build effect plans against it) and a `ReadDriver`
/// (so `ls`/completion can scan it). It never applies — these tests assert at the PREVIEW level.
struct FakeNs {
    mount: String,
    names: Vec<String>,
}

fn ns_schema() -> Schema {
    Schema::new(vec![Column::new("name", ColumnType::Text, false)])
}

impl Driver for FakeNs {
    fn mount(&self) -> &str {
        &self.mount
    }
    fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::BlobNamespace, ns_schema()))
    }
    fn capabilities(&self, _p: &Path) -> Capabilities {
        Capabilities::none()
            .select()
            .insert()
            .update()
            .upsert()
            .remove()
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        unreachable!("E2E asserts at PREVIEW level; no apply")
    }
}

#[async_trait::async_trait]
impl ReadDriver for FakeNs {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let rows = self
            .names
            .iter()
            .map(|n| Row::new(vec![Value::Text(n.clone())]))
            .collect();
        Ok(RowBatch::new(ns_schema(), rows))
    }
}

/// A two-mount engine + read registry: a `local` namespace and an `other` namespace, so
/// cross-mount addressing is exercisable. Capabilities are `all()` so effect plans build.
fn two_mounts() -> (Engine, ReadRegistry) {
    let mut engine = Engine::new();
    engine
        .mounts
        .register(Arc::new(FakeNs {
            mount: "/local".into(),
            names: vec!["docs".into(), "a.md".into(), "notes".into()],
        }))
        .unwrap();
    engine
        .mounts
        .register(Arc::new(FakeNs {
            mount: "/other".into(),
            names: vec!["dst".into()],
        }))
        .unwrap();
    let reads = ReadRegistry::new()
        .with(
            DriverId::new("local"),
            Arc::new(FakeNs {
                mount: "/local".into(),
                names: vec!["docs".into(), "a.md".into(), "notes".into()],
            }),
        )
        .with(
            DriverId::new("other"),
            Arc::new(FakeNs {
                mount: "/other".into(),
                names: vec!["dst".into()],
            }),
        );
    (engine, reads)
}

/// Cross-mount `cp src /other/dst`: the produced plan routes the copy across drivers (source under
/// `local`, destination under `other`) and the cwd is **unchanged** — asserted via the effect
/// targets in the previewed plan (golden plan snapshot at the target level).
#[test]
fn cross_mount_cp_produces_cross_source_plan_without_changing_cwd() {
    let (engine, reads) = two_mounts();
    let mut s = Session::new(VfsPath::new("local", vec!["docs".into()]), &engine, &reads);

    let out = s
        .eval_line("cp report.md /other/dst/report", false)
        .expect("cp previews");
    let Outcome::Preview(plans) = out else {
        panic!("cp must PREVIEW by default, got {out:?}");
    };
    assert_eq!(plans.len(), 1, "cp is a single copy plan");
    let targets: Vec<String> = plans[0]
        .preview
        .rows
        .iter()
        .map(|r| r.target.to_string())
        .collect();
    // The plan reads from the `local` source and writes (UPSERT) to the `other` destination —
    // two drivers, one plan, no `cd`.
    assert!(
        targets
            .iter()
            .any(|t| t.starts_with("local:/local/docs/report.md")),
        "source must stay under the local mount: {targets:?}"
    );
    assert!(
        targets
            .iter()
            .any(|t| t.starts_with("other:/other/dst/report")),
        "destination must land under the other mount: {targets:?}"
    );
    // cwd is untouched by a cross-mount cp.
    assert_eq!(s.cwd().render(), "/local/docs", "cp must not change cwd");
}

/// Cross-mount `mv` lowers to copy→verify→delete: two plans — an UPSERT into the other driver,
/// then a REMOVE of the source under the original driver — with the source REMOVE flagged
/// irreversible. cwd unchanged.
#[test]
fn cross_mount_mv_lowers_to_copy_then_delete() {
    let (engine, reads) = two_mounts();
    let mut s = Session::new(VfsPath::root("local"), &engine, &reads);

    let out = s
        .eval_line("mv a.md /other/dst/a.md", false)
        .expect("mv previews");
    let Outcome::Preview(plans) = out else {
        panic!("mv must PREVIEW by default, got {out:?}");
    };
    assert_eq!(plans.len(), 2, "mv = copy plan + delete plan");

    // Leg 1: the copy lands on the `other` driver.
    let copy_targets: Vec<String> = plans[0]
        .preview
        .rows
        .iter()
        .map(|r| r.target.to_string())
        .collect();
    assert!(
        copy_targets
            .iter()
            .any(|t| t.starts_with("other:/other/dst/a.md")),
        "copy leg must target the other mount: {copy_targets:?}"
    );

    // Leg 2: the delete is a REMOVE of the source under the original (local) driver, irreversible.
    let del = &plans[1].preview;
    assert!(
        del.rows
            .iter()
            .any(|r| r.verb == "REMOVE" && r.target.to_string().starts_with("local:/local/a.md")),
        "delete leg must REMOVE the local source: {:?}",
        del.rows
    );
    assert!(
        !del.irreversible.is_empty(),
        "the source delete must be flagged irreversible"
    );
    assert_eq!(s.cwd().render(), "/local", "mv must not change cwd");
}

/// Builtin ≡ typed equivalence at the PLAN level: the builtin `cp a.md /other/dst/x` and the raw
/// `UPSERT INTO /other/dst/x /local/a.md` typed at the prompt produce the SAME effect
/// preview (same targets, verbs, irreversibility) — the shell adds no new semantics.
#[test]
fn builtin_cp_matches_typed_upsert_plan() {
    let (engine, reads) = two_mounts();

    let builtin = {
        let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
        s.eval_line("cp a.md /other/dst/x", false).unwrap()
    };
    let typed = {
        let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
        s.eval_line("UPSERT INTO /other/dst/x /local/a.md", false)
            .unwrap()
    };

    let previews = |o: &Outcome| -> Vec<qfs_core::Preview> {
        match o {
            Outcome::Preview(ps) => ps.iter().map(|p| p.preview.clone()).collect(),
            other => panic!("expected PREVIEW, got {other:?}"),
        }
    };
    assert_eq!(
        previews(&builtin),
        previews(&typed),
        "builtin cp must produce the identical plan as the equivalent typed UPSERT"
    );
}

/// Builtin `ls` ≡ the typed `FROM … |> SELECT …` it desugars to, at the row level (the read path
/// equivalence the shell promises for listings).
#[test]
fn builtin_ls_matches_typed_select_rows() {
    let (engine, reads) = two_mounts();

    let by_builtin = {
        let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
        s.eval_line("ls", false).unwrap()
    };
    let by_typed = {
        let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
        s.eval_line("/local |> SELECT name, size, is_dir, modified", false)
            .unwrap()
    };
    let (Outcome::Listing(a), Outcome::Listing(b)) = (&by_builtin, &by_typed) else {
        panic!("both ls and the typed SELECT must list rows");
    };
    assert_eq!(
        a, b,
        "builtin ls must list the same rows as the typed SELECT"
    );
}

/// The PREVIEW→COMMIT gate at the API level: an `rm` defaults to PREVIEW (no commit), and only an
/// explicit `commit=true` produces a COMMITTED outcome. There is no builtin path that yields a
/// COMMITTED outcome with `commit=false` — the safety invariant.
#[test]
fn rm_gate_requires_explicit_commit() {
    let (engine, reads) = two_mounts();
    let mut s = Session::new(VfsPath::root("local"), &engine, &reads);

    let preview = s.eval_line("rm a.md", false).unwrap();
    assert!(
        matches!(preview, Outcome::Preview(_)),
        "rm must PREVIEW by default, got {preview:?}"
    );

    let committed = s.eval_line("rm a.md", true).unwrap();
    assert!(
        matches!(committed, Outcome::Committed(_)),
        "explicit commit must reach COMMITTED, got {committed:?}"
    );
}

/// No effectful builtin can ever return a `Committed` outcome under the default (commit=false)
/// gate — exhaustively across `cp`/`mv`/`rm`. This is the "attempt to break the safety invariant"
/// scenario: prove the gate cannot be shortcut by any builtin shape.
#[test]
fn no_effect_builtin_commits_under_default_gate() {
    let (engine, reads) = two_mounts();
    for line in [
        "rm a.md",
        "rm a.md notes",
        "cp a.md /other/dst/x",
        "mv a.md /other/dst/x",
    ] {
        let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
        let out = s.eval_line(line, false).unwrap();
        assert!(
            matches!(out, Outcome::Preview(_)),
            "`{line}` must PREVIEW (never COMMIT) under the default gate, got {out:?}"
        );
    }
}

/// Tab completion returns builtin names, mount names, and path segments (local-driver only),
/// without live creds — and the bounded scan returns promptly (no hang).
#[test]
fn completion_covers_builtins_mounts_and_segments() {
    let (engine, reads) = two_mounts();
    let c = Completer::new(&engine, &reads);
    let cwd = VfsPath::root("local");

    // Builtin names at the head.
    let head = c.complete("c", &cwd);
    for want in ["cat", "cd", "cp"] {
        assert!(
            head.contains(&want.to_string()),
            "head completion: {head:?}"
        );
    }

    // Mount names for an absolute fragment.
    let mounts = c.complete("cd /o", &cwd);
    assert_eq!(mounts, vec!["/other"], "mount completion: {mounts:?}");

    // Path segments under the resolved parent via a bounded `ls`.
    let segs = c.complete("ls do", &cwd);
    assert_eq!(segs, vec!["docs"], "segment completion: {segs:?}");
}

/// Completion against an unmounted parent degrades to empty (no read driver) — it never hangs or
/// errors. Bounds the latency concern (hard part (b)) at the API level.
#[test]
fn completion_degrades_for_unknown_parent() {
    let (engine, reads) = two_mounts();
    let c = Completer::new(&engine, &reads);
    let got = c.complete("ls /nope/x", &VfsPath::root("local"));
    assert!(
        got.is_empty(),
        "unknown parent must degrade to empty: {got:?}"
    );
}

/// Secret-safety: the previewed plan target rendering carries identity + path only (no token /
/// credential material). A cross-mount cp preview is a representative effect; assert its rendered
/// targets contain only driver:path, never anything secret-shaped.
#[test]
fn preview_targets_are_secret_free() {
    let (engine, reads) = two_mounts();
    let mut s = Session::new(VfsPath::root("local"), &engine, &reads);
    let out = s.eval_line("cp a.md /other/dst/x", false).unwrap();
    let Outcome::Preview(plans) = out else {
        panic!("expected preview");
    };
    for p in &plans {
        for row in &p.preview.rows {
            let t = row.target.to_string();
            assert!(
                t.starts_with("local:") || t.starts_with("other:"),
                "target must be driver:path only: {t}"
            );
            for banned in ["token", "secret", "password", "Bearer", "key="] {
                assert!(
                    !t.to_ascii_lowercase()
                        .contains(&banned.to_ascii_lowercase()),
                    "preview target must not leak `{banned}`: {t}"
                );
            }
        }
    }
}
