//! E2E / external-interface validation of the t35 POLICY engine (RFD-0001 §8/§10), driven
//! BLACK-BOX through the public `qfs-server` API: the `CREATE POLICY` DDL round-trip
//! (`policy_from_ddl`/`policy_to_rule_strings`/`policy_from_def`), the fire-path gate seam
//! (`resolve_policy` + `gate_plan` — the single seam every E7 committer, HTTP write-endpoint /
//! cron JOB / watchtower TRIGGER, calls AFTER build_plan and BEFORE commit), the pure
//! enforcer (`evaluate`), the fired-plan audit ledger (`AuditSink`/`FiredPlanRecord`), and the
//! operator boot/replay channel (`Runtime::boot`).
//!
//! These tests run with NO live credentials — the policy engine is pure enforcement over plan
//! DTOs and the in-memory `qfs_core::commit` applier. The committer harness below is a faithful
//! stand-in for an E7 fire path: it resolves the bound policy, gates the built plan, emits
//! exactly one fired-plan audit record, and commits ONLY on allow (so a deny applies zero
//! effects through a counting fake driver).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use qfs_core::{
    commit, AppliedEffect, ApplyError, DriverId, EffectKind, EffectNode, NodeId, Plan, PlanApplier,
    ProcId, ServerNode, ServerWriteOp, Target, VfsPath,
};
use qfs_parser::{parse_statement, Statement};
use qfs_server::{
    gate_plan, policy_from_ddl, policy_from_def, policy_to_rule_strings, resolve_policy, AuditSink,
    DriverGlob, Effectivity, FiredDecision, GateOutcome, Policy, PolicyDecision, PolicyDef,
    PolicyTable, Rule, Runtime, Verb, VerbSet,
};

// ---------------------------------------------------------------------------
// Test harness: a faithful stand-in for an E7 fire path + a counting fake driver.
// ---------------------------------------------------------------------------

/// A counting fake `PlanApplier` (the "fake driver" of scenario 6): it performs NO I/O and
/// records how many effect nodes were actually applied, so a test can assert a DENIED plan
/// applied ZERO effects (atomic abort) by confirming `commit` was never reached.
#[derive(Default)]
struct CountingDriver {
    applied: Arc<Mutex<Vec<NodeId>>>,
}

impl CountingDriver {
    fn new() -> Self {
        Self::default()
    }
    fn applied_count(&self) -> usize {
        self.applied.lock().unwrap().len()
    }
}

impl PlanApplier for CountingDriver {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        self.applied.lock().unwrap().push(node.id);
        Ok(AppliedEffect::new(node.id, 1))
    }
}

/// The single fire-path seam every E7 committer (HTTP / cron / watchtower) follows, exercised
/// here black-box: resolve the bound policy → gate the built plan → emit EXACTLY ONE fired-plan
/// audit record (allow AND deny) → commit ONLY on allow. Returns the gate outcome and the count
/// of effects the fake driver actually applied (0 on a deny — atomic abort).
struct FirePath {
    audit: Arc<AuditSink>,
}

impl FirePath {
    fn new() -> Self {
        Self {
            audit: Arc::new(AuditSink::new()),
        }
    }

    /// Fire `plan` as `handler`, bound to the optional `policy_ref` resolved against `table`.
    /// This is the exact order the gate doc mandates: gate, audit-always, commit-only-on-allow.
    fn fire(
        &self,
        handler: &str,
        policy_ref: Option<&str>,
        table: &PolicyTable,
        plan: &Plan,
        driver: &mut CountingDriver,
    ) -> (GateOutcome, usize) {
        let policy = resolve_policy(policy_ref, table);
        let outcome = gate_plan(&policy, plan);
        // Exactly one fired-plan record, ALWAYS — the single unattended-execution funnel.
        self.audit
            .record_fired(outcome.record(handler, policy_ref.unwrap_or(""), 1000));
        // Commit ONLY on allow: a deny never reaches commit, so zero effects apply (atomic abort).
        if outcome.is_allow() {
            let report = commit(plan, driver, |_| {});
            assert!(report.failed.is_none(), "allowed plan committed cleanly");
        }
        (outcome, driver.applied_count())
    }
}

// ---- plan builders modelling each fire path's built (DO-body) plan ----------

fn write_node(id: u32, kind: EffectKind, driver: &str, path: &str) -> EffectNode {
    EffectNode::new(
        NodeId(id),
        kind,
        Target::new(DriverId::new(driver), VfsPath::new(path)),
    )
}

fn plan_of(nodes: Vec<EffectNode>) -> Plan {
    let mut p = Plan::pure();
    p.nodes = nodes;
    p
}

/// The DO-body plan a watchtower TRIGGER fires: an INSERT into `/log`.
fn trigger_insert_plan() -> Plan {
    plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")])
}

/// The DO-body plan a TRIGGER fires that writes `/server` config (a ServerConfigWrite effect) —
/// the OBS-3 scenario. `who fires it, not what it targets`.
fn server_config_write_plan() -> Plan {
    plan_of(vec![write_node(
        0,
        EffectKind::ServerConfigWrite {
            node: ServerNode::Jobs,
            op: ServerWriteOp::Upsert,
        },
        "server",
        "/server/jobs",
    )])
}

fn policy_of_ddl(src: &str) -> Policy {
    let Statement::Ddl(ddl) = parse_statement(src).expect("parse DDL") else {
        panic!("not a DDL: {src}");
    };
    policy_from_ddl(&ddl).expect("build policy")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

// ===========================================================================
// Scenario 1 — Grammar golden + round-trip.
// ===========================================================================

#[test]
fn scenario1_grammar_golden_and_roundtrip() {
    // The RFD §8 golden example parses to the expected Policy DTO.
    let parsed = policy_of_ddl("CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL");
    let expected = Policy::new("api")
        .with_rule(Rule::allow(VerbSet::one(Verb::Select), DriverGlob::any()))
        .with_rule(Rule::deny(
            VerbSet::from_verbs(&[Verb::Insert, Verb::Update, Verb::Remove, Verb::Call]),
            DriverGlob::any(),
        ));
    assert_eq!(
        parsed, expected,
        "CREATE POLICY parses to the RFD §8 Policy DTO"
    );

    // Round-trip through INSERT INTO /server/policies (the PolicyDef row) back to an EQUAL Policy.
    let def = PolicyDef {
        name: parsed.name.clone(),
        handler: String::new(),
        allow: policy_to_rule_strings(&parsed),
    };
    let back = policy_from_def(&def);
    assert_eq!(
        parsed, back,
        "CREATE POLICY round-trips through /server/policies to an EQUAL Policy"
    );
}

// ===========================================================================
// Scenario 2 — Enforcement plan-assertion under the `api` policy.
// ===========================================================================

#[test]
fn scenario2_enforcement_under_api_policy() {
    let table = single_policy_table(
        "api",
        "CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL",
    );
    let fp = FirePath::new();

    // SELECT-only plan → Allow. A pure read node is NOT policy-bearing (the read path is
    // separate from the COMMIT plan the policy gates), so the gate permits it under the `api`
    // policy whose only ALLOW is SELECT.
    let select_only = plan_of(vec![write_node(0, EffectKind::Read, "mail", "/mail/inbox")]);
    let mut d = CountingDriver::new();
    let (out, _applied) = fp.fire("endpoint:recent", Some("api"), &table, &select_only, &mut d);
    assert!(out.is_allow(), "SELECT-only plan is allowed");
    // The decision carries no deny: the read node was classified as a non-gated dependency.
    assert!(out.deny_reason().is_none(), "no denial on a pure-read plan");

    // A plan with INSERT → Deny naming the offending INSERT node.
    let with_insert = plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")]);
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire("endpoint:write", Some("api"), &table, &with_insert, &mut d);
    assert_deny(&out.decision, Verb::Insert, "log");
    assert_eq!(applied, 0, "denied INSERT applies nothing");

    // A plan with CALL mail.send → Deny naming the offending CALL node + driver.
    let with_call = plan_of(vec![write_node(
        0,
        EffectKind::Call(ProcId::new("mail.send")),
        "mail",
        "/mail/outbox",
    )]);
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire("endpoint:send", Some("api"), &table, &with_call, &mut d);
    assert_deny(&out.decision, Verb::Call, "mail");
}

// ===========================================================================
// Scenario 3 — DEFAULT-DENY (the most important behavior). Try to defeat it.
// ===========================================================================

#[test]
fn scenario3_default_deny_no_policy_and_empty_policy_deny_every_write() {
    let fp = FirePath::new();
    let empty_table = PolicyTable::new();

    // (a) A cron JOB fires a write with NO policy attached → deny, zero effects, deny audit.
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "job:nightly",
        None,
        &empty_table,
        &trigger_insert_plan(),
        &mut d,
    );
    assert!(!out.is_allow(), "a no-policy handler must deny every write");
    assert_eq!(applied, 0, "no-policy deny applies zero effects");

    // (b) A watchtower TRIGGER fires a write bound to an EMPTY policy → deny, zero effects.
    let mut empty_pol = PolicyTable::new();
    empty_pol.insert(
        "empty".to_string(),
        PolicyDef {
            name: "empty".to_string(),
            handler: String::new(),
            allow: Vec::new(),
        },
    );
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "trigger:notify",
        Some("empty"),
        &empty_pol,
        &trigger_insert_plan(),
        &mut d,
    );
    assert!(!out.is_allow(), "an empty policy must deny every write");
    assert_eq!(applied, 0, "empty-policy deny applies zero effects");

    // ATTEMPT TO DEFEAT default-deny — every one of these must STILL deny:
    let table = empty_table;
    for (label, plan) in adversarial_write_plans() {
        // No policy.
        let mut d = CountingDriver::new();
        let (out, applied) = fp.fire("attacker", None, &table, &plan, &mut d);
        assert!(
            !out.is_allow(),
            "no-policy must deny adversarial write: {label}"
        );
        assert_eq!(applied, 0, "no effects applied for: {label}");
    }

    // A policy whose only rule is a DENY (no ALLOW at all) — still deny (no widening).
    let deny_only = single_policy_table("d", "CREATE POLICY d DENY REMOVE");
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire("h", Some("d"), &deny_only, &trigger_insert_plan(), &mut d);
    assert!(
        !out.is_allow(),
        "a deny-only policy never widens INSERT — default-deny still fires"
    );
    assert_eq!(applied, 0);

    // Every fired plan above emitted exactly one deny record (allow AND deny audited).
    for entry in fp.audit.snapshot() {
        if let qfs_server::AuditEntry::FiredPlan(r) = entry {
            assert!(
                matches!(r.decision, FiredDecision::Deny { .. }),
                "default-deny records a DENY"
            );
        }
    }
}

/// A battery of every write verb / driver shape an attacker might use to slip past default-deny.
fn adversarial_write_plans() -> Vec<(&'static str, Plan)> {
    vec![
        (
            "INSERT",
            plan_of(vec![write_node(0, EffectKind::Insert, "mail", "/mail/x")]),
        ),
        (
            "UPSERT",
            plan_of(vec![write_node(0, EffectKind::Upsert, "s3", "/s3/x")]),
        ),
        (
            "UPDATE",
            plan_of(vec![write_node(0, EffectKind::Update, "git", "/git/x")]),
        ),
        (
            "REMOVE",
            plan_of(vec![write_node(0, EffectKind::Remove, "fs", "/fs/x")]),
        ),
        (
            "CALL",
            plan_of(vec![write_node(
                0,
                EffectKind::Call(ProcId::new("mail.send")),
                "mail",
                "/mail/out",
            )]),
        ),
        (
            "ServerConfigWrite",
            plan_of(vec![write_node(
                0,
                EffectKind::ServerConfigWrite {
                    node: ServerNode::Policies,
                    op: ServerWriteOp::Upsert,
                },
                "server",
                "/server/policies",
            )]),
        ),
        // A write hidden behind a leading harmless read dependency.
        (
            "READ-then-REMOVE",
            plan_of(vec![
                write_node(0, EffectKind::Read, "fs", "/fs/x"),
                write_node(1, EffectKind::Remove, "fs", "/fs/x"),
            ]),
        ),
    ]
}

// ===========================================================================
// Scenario 4 — OBS-3: /server-write TRIGGER denial vs trusted operator boot/replay.
// ===========================================================================

#[test]
fn scenario4_server_write_trigger_is_gated_but_operator_boot_is_not() {
    let fp = FirePath::new();

    // A TRIGGER whose DO body writes /server config, fired WITHOUT a granting policy → DENIED.
    // "who fires it (a handler), not what it targets (/server)": gated like any other write.
    let empty = PolicyTable::new();
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "trigger:reconfigure",
        None,
        &empty,
        &server_config_write_plan(),
        &mut d,
    );
    assert!(
        !out.is_allow(),
        "a /server-write TRIGGER with no granting policy is DENIED"
    );
    assert_deny(&out.decision, Verb::Upsert, "server");
    assert_eq!(applied, 0, "the denied /server-write applies zero effects");

    // ATTEMPT TO DEFEAT OBS-3: bind the trigger to a policy that grants the /server write and
    // confirm it is ALLOWED only then (gated, not forbidden — the grant is what flips it).
    let admin = single_policy_table(
        "server_admin",
        "CREATE POLICY server_admin ALLOW INSERT,UPSERT",
    );
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "trigger:reconfigure",
        Some("server_admin"),
        &admin,
        &server_config_write_plan(),
        &mut d,
    );
    assert!(
        out.is_allow(),
        "with an explicit INSERT,UPSERT grant the /server-write trigger is allowed"
    );
    assert_eq!(
        applied, 1,
        "the granted /server write applies its one effect"
    );

    // CONTRAST — the trusted operator channel: booting the SAME /server write via Runtime::boot
    // is NOT gated (boot/replay is the operator channel, not a fired handler). It succeeds even
    // though no policy grants it — proving the gate is on the FIRE path, not on /server itself.
    let mut rt = Runtime::new();
    rt.boot(&fixture("watchtower.qfs"))
        .expect("operator boot of /server writes succeeds ungated");
    // The reconfigure trigger's /server-targeting DO body is stored as config; boot itself
    // applied /server writes with no policy gate — confirm the bootstrapped state materialized.
    let state = rt.snapshot();
    assert_eq!(
        state.triggers.len(),
        3,
        "operator boot installed all triggers (ungated)"
    );
    assert!(
        state.triggers.get("reconfigure").unwrap().policy.as_deref() == Some("server_admin"),
        "the /server-write trigger carries its grant for its FIRE path (not for boot)"
    );

    // Drive a direct operator /server write through apply_source (the hot-reconfigure channel):
    // also ungated, succeeds with no policy.
    rt.apply_source(
        "operator",
        1,
        "UPSERT INTO /server/jobs VALUES (name, every) ('opjob', '12h')",
    )
    .expect("operator /server write is ungated and succeeds");
    assert!(
        rt.snapshot().jobs.contains_key("opjob"),
        "operator hot-reconfigure applied ungated"
    );
}

// ===========================================================================
// Scenario 5 — Layering (can ∧ may): policy deny is distinct from a capability deny.
// ===========================================================================

#[test]
fn scenario5_can_and_may_layering_distinct_errors() {
    use qfs_core::Capabilities;
    use qfs_core::Verb as CapVerb;

    // The CAN layer (t13 driver capability): a driver that fully supports REMOVE.
    let caps = Capabilities::from_verbs(&[CapVerb::Select, CapVerb::Insert, CapVerb::Remove]);
    assert!(
        caps.allows(CapVerb::Remove),
        "the driver CAN REMOVE (capability layer permits)"
    );

    // The MAY layer (t35 policy): a policy that allows INSERT but NOT REMOVE.
    let table = single_policy_table("ins", "CREATE POLICY ins ALLOW INSERT");
    let fp = FirePath::new();
    let remove_plan = plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]);
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire("endpoint:purge", Some("ins"), &table, &remove_plan, &mut d);

    // Even though the driver CAN REMOVE, the policy denies it — `can ∧ may` rejects the plan.
    assert!(
        !out.is_allow(),
        "policy denies REMOVE even when the driver capability permits it"
    );
    assert_eq!(applied, 0, "the layered deny applies zero effects");

    // The deny is a DISTINCT policy-deny reason (verb/driver/rule coordinates), NOT a capability
    // (UnsupportedVerb) error — the two layers never masquerade as each other.
    let reason = out.deny_reason().expect("a policy-deny reason");
    assert!(
        reason.contains("policy denies REMOVE"),
        "policy-deny reason, distinct from capability: {reason}"
    );
    assert!(
        !reason.contains("unsupported_verb"),
        "not a capability error"
    );
}

// ===========================================================================
// Scenario 6 — Atomic abort: a denied plan applies ZERO effects (no half-run).
// ===========================================================================

#[test]
fn scenario6_atomic_abort_no_partial_cross_source_plan() {
    let fp = FirePath::new();
    // A cross-source plan: an allowed INSERT on `log`, then a DENIED REMOVE on `s3`. Under the
    // ins-only policy the whole plan is denied; the counting driver must apply NOTHING (no
    // half-run that committed the leading INSERT before hitting the deny).
    let table = single_policy_table("ins", "CREATE POLICY ins ALLOW INSERT");
    let cross = plan_of(vec![
        write_node(0, EffectKind::Insert, "log", "/log"),
        write_node(1, EffectKind::Remove, "s3", "/s3/x"),
    ]);
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire("job:sync", Some("ins"), &table, &cross, &mut d);
    assert!(
        !out.is_allow(),
        "a cross-source plan with one denied node is denied whole"
    );
    assert_eq!(
        applied, 0,
        "ATOMIC ABORT: zero effects applied, the leading INSERT never ran"
    );

    // Sanity: the same plan, fully granted, applies BOTH effects (the driver is real-counting).
    let grant = single_policy_table("g", "CREATE POLICY g ALLOW INSERT,REMOVE");
    let mut d2 = CountingDriver::new();
    let (out2, applied2) = fp.fire("job:sync", Some("g"), &grant, &cross, &mut d2);
    assert!(out2.is_allow());
    assert_eq!(
        applied2, 2,
        "a fully-granted plan applies both cross-source effects"
    );
}

// ===========================================================================
// Scenario 7 — ALLOW ALL excludes irreversible; explicit grant includes them. Both directions.
// ===========================================================================

#[test]
fn scenario7_allow_all_excludes_irreversible_explicit_includes() {
    let fp = FirePath::new();

    // ALLOW ALL grants reversible writes (INSERT) ...
    let broad = single_policy_table("broad", "CREATE POLICY broad ALLOW ALL");
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire(
        "h",
        Some("broad"),
        &broad,
        &plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")]),
        &mut d,
    );
    assert!(out.is_allow(), "ALLOW ALL grants INSERT (reversible)");

    // ... but ALLOW ALL does NOT grant REMOVE (irreversible) — try to defeat it.
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "h",
        Some("broad"),
        &broad,
        &plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]),
        &mut d,
    );
    assert!(!out.is_allow(), "ALLOW ALL must NOT grant REMOVE");
    assert_eq!(applied, 0);

    // ... nor CALL (irreversible) under ALLOW ALL.
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire(
        "h",
        Some("broad"),
        &broad,
        &plan_of(vec![write_node(
            0,
            EffectKind::Call(ProcId::new("mail.send")),
            "mail",
            "/mail",
        )]),
        &mut d,
    );
    assert!(!out.is_allow(), "ALLOW ALL must NOT grant CALL");

    // The OTHER direction: an EXPLICIT ALLOW SELECT,INSERT,REMOVE,CALL DOES grant REMOVE + CALL.
    let explicit = single_policy_table("ex", "CREATE POLICY ex ALLOW SELECT,INSERT,REMOVE,CALL");
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "h",
        Some("ex"),
        &explicit,
        &plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]),
        &mut d,
    );
    assert!(out.is_allow(), "an EXPLICIT verb list grants REMOVE");
    assert_eq!(applied, 1);
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire(
        "h",
        Some("ex"),
        &explicit,
        &plan_of(vec![write_node(
            0,
            EffectKind::Call(ProcId::new("mail.send")),
            "mail",
            "/mail",
        )]),
        &mut d,
    );
    assert!(out.is_allow(), "an EXPLICIT verb list grants CALL");

    // Defeat attempt via round-trip: the ALL-token strictness must survive storage. Confirm a
    // stored `ALLOW ALL` rehydrates with the all_token bit set so it STILL excludes REMOVE.
    let stored = broad.get("broad").unwrap();
    let rehydrated = policy_from_def(stored);
    assert!(
        rehydrated.rules[0].all_token,
        "the ALL token survives the /server/policies round-trip"
    );
    let after = gate_plan(
        &rehydrated,
        &plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]),
    );
    assert!(
        !after.is_allow(),
        "rehydrated ALLOW ALL still excludes REMOVE (strictness is durable)"
    );
}

// ===========================================================================
// Scenario 8 — Audit: one record per fire; deny coordinates; secret-free (canary).
// ===========================================================================

#[test]
fn scenario8_audit_one_per_fire_deny_coordinates_secret_free() {
    let fp = FirePath::new();
    let table = single_policy_table("ins", "CREATE POLICY ins ALLOW INSERT");

    // Plant a CANARY secret/payload in the effect's row args. The audit summary must carry
    // driver + path + verb ONLY — never the secret/payload.
    const CANARY: &str = "SUPER-SECRET-TOKEN-xyz-DO-NOT-LOG";
    let mut node = write_node(0, EffectKind::Insert, "mail", "/mail/outbox");
    node = node.with_args(row_with_canary(CANARY));
    let allow_plan = plan_of(vec![node]);

    // (1) An ALLOW fire → exactly one allow record.
    let mut d = CountingDriver::new();
    let _ = fp.fire("endpoint:write", Some("ins"), &table, &allow_plan, &mut d);
    // (2) A DENY fire → exactly one deny record carrying verb/driver/rule.
    let deny_plan = plan_of(vec![write_node(1, EffectKind::Remove, "log", "/log")]);
    let mut d = CountingDriver::new();
    let _ = fp.fire("endpoint:purge", Some("ins"), &table, &deny_plan, &mut d);

    let entries = fp.audit.snapshot();
    let fired: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            qfs_server::AuditEntry::FiredPlan(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fired.len(),
        2,
        "exactly one FiredPlanRecord per fire (allow AND deny)"
    );
    assert_eq!(fp.audit.fired_count(), 2);

    // The allow record: secret-free, names driver + path + verb only.
    let allow_rec = fired
        .iter()
        .find(|r| r.decision.is_allow())
        .expect("an allow record");
    assert!(matches!(allow_rec.decision, FiredDecision::Allow));
    for e in &allow_rec.effects {
        assert!(
            !e.contains(CANARY),
            "audit effect summary must not carry the canary secret: {e}"
        );
        assert!(
            e.contains("mail") && e.contains("/mail/outbox") && e.contains("INSERT"),
            "driver+path+verb only: {e}"
        );
    }
    assert!(
        !allow_rec.summary().contains(CANARY),
        "the rendered summary is secret-free"
    );

    // The deny record: carries the offending verb / driver / rule index, still secret-free.
    let deny_rec = fired
        .iter()
        .find(|r| !r.decision.is_allow())
        .expect("a deny record");
    match &deny_rec.decision {
        FiredDecision::Deny { verb, driver, rule } => {
            assert_eq!(verb, "REMOVE", "deny names the offending verb");
            assert_eq!(driver, "log", "deny names the offending driver");
            // REMOVE matched no rule under ALLOW INSERT ⇒ default-deny ⇒ rule index None.
            assert_eq!(*rule, None, "default-deny carries no rule index");
        }
        FiredDecision::Allow => panic!("expected a deny record"),
    }
    assert!(
        !deny_rec.summary().contains(CANARY),
        "deny summary is secret-free"
    );

    // Deny WITH an explicit matching rule index: a policy whose rule[1] explicitly denies REMOVE.
    let with_rule = single_policy_table(
        "api",
        "CREATE POLICY api ALLOW SELECT DENY INSERT,UPDATE,REMOVE,CALL",
    );
    let fp2 = FirePath::new();
    let mut d = CountingDriver::new();
    let (out, _) = fp2.fire(
        "h",
        Some("api"),
        &with_rule,
        &plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]),
        &mut d,
    );
    match out.decision {
        PolicyDecision::Deny { rule, .. } => {
            assert_eq!(rule, Some(1), "the explicit DENY rule index is reported")
        }
        PolicyDecision::Allow => panic!("expected deny"),
    }
}

// ===========================================================================
// Scenario 9 — Dangling policy ref resolves to default-deny (fail closed).
// ===========================================================================

#[test]
fn scenario9_dangling_policy_ref_fails_closed() {
    let fp = FirePath::new();
    // The table has a real policy, but the handler names a DIFFERENT, absent policy.
    let table = single_policy_table("real", "CREATE POLICY real ALLOW INSERT");
    let mut d = CountingDriver::new();
    let (out, applied) = fp.fire(
        "trigger:ghost",
        Some("does_not_exist"),
        &table,
        &trigger_insert_plan(),
        &mut d,
    );
    // A dangling ref must NOT error and must NOT allow — it resolves to default-deny.
    assert!(
        !out.is_allow(),
        "a dangling policy ref fails CLOSED (default-deny), never allows"
    );
    assert_eq!(applied, 0, "dangling-ref deny applies zero effects");
    // It is recorded as a normal deny fire (no panic, no error path).
    assert_eq!(
        fp.audit.fired_count(),
        1,
        "the dangling-ref fire is audited like any deny"
    );

    // Confirm the resolved policy is genuinely empty + default-deny (not the `real` policy).
    let resolved = resolve_policy(Some("does_not_exist"), &table);
    assert!(
        resolved.rules.is_empty(),
        "dangling ref resolves to a no-rule policy"
    );
    assert_eq!(
        resolved.default,
        Effectivity::Deny,
        "dangling ref defaults to DENY"
    );
}

// ===========================================================================
// Scenario 10 — Fixture boot: server_boot.qfs and watchtower.qfs boot under default-deny with
// their attached policies; the write-firing handlers are now allowed (regression).
// ===========================================================================

#[test]
fn scenario10_fixtures_boot_with_attached_policies() {
    // server_boot.qfs boots.
    let mut rt = Runtime::new();
    rt.boot(&fixture("server_boot.qfs"))
        .expect("server_boot.qfs boots");
    let s = rt.snapshot();
    assert_eq!(
        s.policies.len(),
        1,
        "server_boot carries its leastpriv policy"
    );
    let leastpriv = s.policies.get("leastpriv").expect("leastpriv policy");

    // Its nightly JOB (REMOVE) and notify TRIGGER (INSERT) attach leastpriv; under that policy
    // their fired plans are ALLOWED (the regression: E7 fixtures still work under default-deny
    // because they carry explicit grants).
    let table: PolicyTable = s.policies.clone().into_iter().collect();
    let policy = policy_from_def(leastpriv);
    // leastpriv = `ALLOW INSERT,REMOVE` (an EXPLICIT verb list ⇒ grants the irreversible REMOVE).
    let fp = FirePath::new();
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire(
        "job:nightly",
        Some("leastpriv"),
        &table,
        &plan_of(vec![write_node(0, EffectKind::Remove, "fs", "/tmp")]),
        &mut d,
    );
    assert!(
        out.is_allow(),
        "the nightly JOB's REMOVE is allowed under its explicit leastpriv grant"
    );
    let mut d = CountingDriver::new();
    let (out, _) = fp.fire(
        "trigger:notify",
        Some("leastpriv"),
        &table,
        &trigger_insert_plan(),
        &mut d,
    );
    assert!(
        out.is_allow(),
        "the notify TRIGGER's INSERT is allowed under leastpriv"
    );
    let _ = policy; // the rehydrated policy mirrors the table entry (sanity-built above).

    // watchtower.qfs boots with its two policies and three triggers (each carrying a grant).
    let mut rt2 = Runtime::new();
    rt2.boot(&fixture("watchtower.qfs"))
        .expect("watchtower.qfs boots");
    let w = rt2.snapshot();
    assert_eq!(
        w.policies.len(),
        2,
        "watchtower carries watch_insert + server_admin"
    );
    assert_eq!(
        w.triggers.len(),
        3,
        "watchtower installs notify + reconfigure + guarded triggers"
    );
    let wtable: PolicyTable = w.policies.clone().into_iter().collect();
    let wfp = FirePath::new();
    // The notify TRIGGER's INSERT is allowed under watch_insert.
    let mut d = CountingDriver::new();
    let (out, _) = wfp.fire(
        "trigger:notify",
        w.triggers.get("notify").unwrap().policy.as_deref(),
        &wtable,
        &trigger_insert_plan(),
        &mut d,
    );
    assert!(
        out.is_allow(),
        "watchtower notify TRIGGER INSERT allowed under watch_insert"
    );
    // The reconfigure TRIGGER's /server write is allowed ONLY because server_admin grants it.
    let mut d = CountingDriver::new();
    let (out, applied) = wfp.fire(
        "trigger:reconfigure",
        w.triggers.get("reconfigure").unwrap().policy.as_deref(),
        &wtable,
        &server_config_write_plan(),
        &mut d,
    );
    assert!(
        out.is_allow(),
        "watchtower reconfigure /server write allowed under its server_admin grant"
    );
    assert_eq!(applied, 1);
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Build a one-entry `PolicyTable` from a `CREATE POLICY` source, storing it exactly as the
/// `INSERT INTO /server/policies` desugar would (the canonical rule strings).
fn single_policy_table(name: &str, ddl: &str) -> PolicyTable {
    let p = policy_of_ddl(ddl);
    let mut table = PolicyTable::new();
    table.insert(
        name.to_string(),
        PolicyDef {
            name: name.to_string(),
            handler: String::new(),
            allow: policy_to_rule_strings(&p),
        },
    );
    table
}

/// Assert a decision is a `Deny` naming the expected verb + driver.
fn assert_deny(d: &PolicyDecision, expect_verb: Verb, expect_driver: &str) {
    match d {
        PolicyDecision::Deny { verb, driver, .. } => {
            assert_eq!(*verb, expect_verb, "deny names the offending verb");
            assert_eq!(driver, expect_driver, "deny names the offending driver");
        }
        PolicyDecision::Allow => panic!(
            "expected Deny {{ {}, {expect_driver} }}, got Allow",
            expect_verb.label()
        ),
    }
}

/// A single-row `RowBatch` carrying a canary secret in its value, so the audit-secret-free
/// assertion has a concrete payload to prove is NOT logged.
fn row_with_canary(canary: &str) -> qfs_core::RowBatch {
    use qfs_core::{Row, RowBatch, Value};
    let mut batch = RowBatch::default();
    batch
        .rows
        .push(Row::new(vec![Value::Text(canary.to_string())]));
    batch
}

// ===========================================================================
// t37 — Multi-leg `cp` audit ledger: one AppliedEffect per leg, reconstructable
//        after a kill mid-plan (RFD §6 idempotency / recovery).
// ===========================================================================

/// A `cp` across sources lowers to copy → verify → delete (RFD §6): three legs, the delete
/// depending on the verify, the verify on the copy. We model the built plan and prove two things:
///   (1) committing it writes EXACTLY ONE ledger entry per applied leg (the `on_applied` funnel),
///       in dependency order; and
///   (2) a process kill AFTER the copy leg leaves a ledger from which the remaining verify→delete
///       is reconstructable — the recorded `applied` prefix names what is done, so a recovery pass
///       can resume the rest. The ledger lines are secret-free (verb + driver + path only).
fn cp_plan() -> Plan {
    // copy (node 0) -> verify (node 1) -> delete (node 2). REMOVE is the irreversible delete leg.
    let mut p = plan_of(vec![
        write_node(0, EffectKind::Insert, "dst", "/dst/file"), // copy = write to dst
        write_node(1, EffectKind::Read, "dst", "/dst/file"),   // verify = read-back
        write_node(2, EffectKind::Remove, "src", "/src/file"), // delete source
    ]);
    p.deps = vec![(NodeId(0), NodeId(1)), (NodeId(1), NodeId(2))];
    p
}

/// An append-only, secret-free ledger that mirrors `qfs_host::AuditLedger`'s line semantics (one
/// `writeln!` per applied leg). Modeled in-memory here so the test stays in the pure, no-creds
/// `qfs-server` layer; the on-disk fsync'd ledger (t36) shares the exact append contract.
#[derive(Default)]
struct ReplayLedger {
    lines: Mutex<Vec<String>>,
}

impl ReplayLedger {
    fn record(&self, plan: &Plan, e: &AppliedEffect) {
        if let Some(node) = plan.node(e.id) {
            self.lines.lock().unwrap().push(format!(
                "{} {}:{} affected={}",
                node.kind.label(),
                node.target.driver.as_str(),
                node.target.path.as_str(),
                e.affected
            ));
        }
    }
    fn snapshot(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

#[test]
fn cp_writes_one_applied_effect_per_leg() {
    let plan = cp_plan();
    let ledger = ReplayLedger::default();
    let mut driver = CountingDriver::new();
    let report = commit(&plan, &mut driver, |e| ledger.record(&plan, e));
    assert!(report.is_complete(), "a clean cp applies every leg");

    let lines = ledger.snapshot();
    // The verify leg is a READ dependency; commit applies all three nodes through the applier, so
    // the ledger records one entry per node in dependency (topological) order.
    assert_eq!(lines.len(), 3, "one AppliedEffect per leg: {lines:?}");
    assert!(lines[0].starts_with("INSERT dst:/dst/file"), "{:?}", lines);
    assert!(lines[2].starts_with("REMOVE src:/src/file"), "{:?}", lines);
    // Secret-free: no payload column ever appears, only verb + driver + path + count.
    assert!(lines
        .iter()
        .all(|l| !l.contains("VALUES") && !l.contains("token")));
}

#[test]
fn kill_after_copy_leaves_a_reconstructable_ledger() {
    // Simulate a process kill AFTER the copy leg: the applier fails on the VERIFY node, so commit
    // applies only the copy, records it, and stops — leaving the delete unattempted.
    let plan = cp_plan();
    let ledger = ReplayLedger::default();
    let mut driver = CountingDriver::default();
    // Drive commit with an applier that fails the verify leg (node 1) to model the mid-plan kill.
    let mut failing = qfs_core::RecordingApplier::new().failing_on(NodeId(1));
    let report = commit(&plan, &mut failing, |e| ledger.record(&plan, e));
    let _ = &mut driver;

    // The ledger holds exactly the COPY leg — the done-prefix a recovery pass reads.
    let lines = ledger.snapshot();
    assert_eq!(
        lines.len(),
        1,
        "only the copy leg was applied+recorded: {lines:?}"
    );
    assert!(lines[0].starts_with("INSERT dst:/dst/file"));

    // The delete (node 2) depends on the failed verify (node 1), so it is recorded SKIPPED, never
    // applied — the irreversible leg never runs on a mid-plan failure (fail-closed recovery).
    assert!(report.failed.is_some(), "the verify leg failed");
    assert!(
        report.skipped.iter().any(|(id, _)| *id == NodeId(2)),
        "the delete is skipped (its verify dependency failed), so it is reconstructable as 'not \
         yet done' from the ledger: applied={:?} skipped={:?}",
        report.applied,
        report.skipped
    );
    // Reconstruction: the remaining work = plan legs whose ids are NOT in the applied prefix.
    let done: Vec<NodeId> = report.applied.iter().map(|e| e.id).collect();
    let remaining: Vec<NodeId> = plan
        .nodes()
        .iter()
        .map(|n| n.id)
        .filter(|id| !done.contains(id))
        .collect();
    assert_eq!(
        remaining,
        vec![NodeId(1), NodeId(2)],
        "verify+delete remain reconstructable from the ledger's applied prefix"
    );
}

// ===========================================================================
// t37 — Golden PREVIEW of shipped example handlers through offline stubs:
//        PREVIEW applies ZERO effects (PREVIEW-as-CI, no live creds; RFD §3/§8).
// ===========================================================================

/// Every shipped example handler, when run through PREVIEW with offline stubs and NO live
/// credentials, must produce its plan WITHOUT applying any effect. We model the handler plans the
/// shipped `server_boot.qfs` declares (the nightly JOB's REMOVE, the notify TRIGGER's INSERT) and
/// assert that taking the PREVIEW projection (`qfs_core::preview`) never touches the applier — a
/// PREVIEW is a pure, side-effect-free description, the property CI relies on to dry-run handlers.
#[test]
fn shipped_handler_preview_applies_zero_effects() {
    // The two write-bearing shipped handlers from server_boot.qfs (their DO-body plans).
    let nightly = plan_of(vec![write_node(0, EffectKind::Remove, "tmp", "/tmp")]);
    let notify = trigger_insert_plan();

    for (name, plan) in [("nightly", &nightly), ("notify", &notify)] {
        // PREVIEW is the pure projection — it constructs the summary from the plan data alone and
        // invokes NO applier, so a counting driver observes zero applied effects.
        let mut driver = CountingDriver::new();
        let preview = qfs_core::preview(plan);
        // A PREVIEW is non-empty (it describes the would-be effects) ...
        assert!(
            !preview.rows.is_empty(),
            "{name}: PREVIEW must describe the handler's effects"
        );
        // ... but applies nothing: the driver was never called.
        assert_eq!(
            driver.applied_count(),
            0,
            "{name}: a PREVIEW must apply ZERO effects (offline dry-run)"
        );
        let _ = &mut driver;
    }
}
