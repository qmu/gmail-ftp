//! [`Committer`] — the **injected commit seam** (RFD §3/§6 purity invariant), and [`PolicyGate`]
//! — the policy hook every fired plan passes through (the engine is t35; this calls the hook).
//!
//! The dispatcher constructs no effects and runs nothing itself: it builds the `NEW.*`-bound
//! handler `Statement`, asks the [`PolicyGate`] whether the fire is permitted, then hands the
//! statement to a [`Committer`]. The REAL committer (the runtime interpreter with live drivers) is
//! provided by the composition root (the `qfs` binary) — this keeps `qfs-watchtower` off
//! `qfs-runtime` and the dispatch logic pure (the exact qfs-cron pattern).

use qfs_parser::Statement;

/// The outcome of committing a fired handler plan: a secret-free summary. NO plan payload, NO
/// secrets (RFD §10) — only what the audit ledger may keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireOutcome {
    /// A stable, secret-free fingerprint of the committed plan (counts/hashes only).
    pub plan_summary: String,
    /// How many effects the commit applied (the safe-to-log count).
    pub affected: u64,
    /// The secret-free per-effect summaries (`"INSERT log:/log"`) for the fired-plan audit
    /// record — driver + path + verb only, never a payload (RFD §10).
    pub effects: Vec<String>,
}

/// A structured, secret-free fire error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FireError {
    /// The handler body could not be lowered to a plan (resolve / capability / plan construction).
    #[error("plan build failed: {0}")]
    Build(String),
    /// The bound POLICY denied the fired plan (t35). Atomic abort: ZERO effects applied. Carries
    /// the secret-free reason + the per-effect summaries so the dispatcher emits the one deny
    /// fired-plan record.
    #[error("policy denied: {reason}")]
    PolicyDenied {
        /// The secret-free denial reason (verb / driver / rule index).
        reason: String,
        /// The denied effect's verb label (`"REMOVE"`, `"CALL"`, …).
        verb: String,
        /// The denied effect's driver (secret-free name).
        driver: String,
        /// The matching rule index, or `None` for the default-deny (no matching rule).
        rule: Option<usize>,
        /// The secret-free per-effect summaries of the (aborted) plan.
        effects: Vec<String>,
    },
    /// A leg of the plan failed to apply at commit (the reason is already secret-free).
    #[error("commit failed: {0}")]
    Apply(String),
    /// t37: the fired plan carries an irreversible effect (REMOVE / declared-irreversible CALL)
    /// and the server is firing it UNATTENDED (`RunMode::Server`) without an explicit ack — so it
    /// is refused, fail-closed. Atomic abort: ZERO effects applied. The reason is secret-free.
    #[error("irreversible blocked: {0}")]
    IrreversibleBlocked(String),
}

/// The statement-level policy gate hook (RFD §10) retained from t34 for the dispatcher's
/// pre-commit shape. The REAL plan-level enforcement (t35) lives in the [`Committer`] (where the
/// built `Plan` exists): the committer resolves the trigger's bound policy, runs the pure
/// `qfs_server::evaluate` over the plan, emits the [`qfs_server::FiredPlanRecord`], and aborts
/// atomically on deny. This stmt-level gate is therefore a NO-OP pass-through ([`AllowAllGate`])
/// in the live composition — the plan-level engine is the load-bearing one. Kept so the
/// dispatcher's gate seam (and the WHERE-guard tests) are unchanged.
pub trait PolicyGate: Send + Sync {
    /// Decide whether firing `stmt` under the named `trigger` is permitted at the statement
    /// level. `Ok(())` permits (the plan-level engine then enforces in the committer).
    ///
    /// # Errors
    /// A secret-free denial reason if the fire is not permitted.
    fn check(&self, trigger: &str, stmt: &Statement) -> Result<(), String>;
}

/// The default gate: permits every fire at the statement level (the load-bearing plan-level
/// POLICY engine is in the committer, t35). Pure, no state.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllGate;

impl PolicyGate for AllowAllGate {
    fn check(&self, _trigger: &str, _stmt: &Statement) -> Result<(), String> {
        Ok(())
    }
}

/// The injected commit seam. The dispatcher calls `commit` with the **already-`NEW.*`-bound**
/// handler statement plus the trigger's identity + bound POLICY ref (t35); the implementor
/// builds the plan, enforces the policy against it (default-deny / atomic abort), emits the one
/// fired-plan audit record, and runs the real (or recording) applier — never widening scope.
pub trait Committer: Send + Sync {
    /// Commit the bound handler `stmt` for `trigger` under the bound `policy` ref (the
    /// `/server/policies` row name, or `None` for no attached policy ⇒ fail-closed default-deny).
    /// Returns a secret-free [`FireOutcome`].
    ///
    /// # Errors
    /// [`FireError::PolicyDenied`] if the bound policy denies the plan (ZERO effects applied);
    /// [`FireError::Build`]/[`FireError::Apply`] on a build / apply failure (a failed commit is
    /// NOT acked, so the event is redelivered — at-least-once).
    fn commit(
        &self,
        trigger: &str,
        stmt: &Statement,
        policy: Option<&str>,
    ) -> Result<FireOutcome, FireError>;
}

/// A no-creds, no-network test committer (the PREVIEW path), gated behind `native` because it
/// consumes `qfs-exec` (the evaluator pulls tokio, no-wasm). Builds the plan via
/// `qfs_exec::build_plan` over an injected engine and commits over a `RecordingApplier`. Counts
/// the effects applied across ALL commits (so the idempotency golden can assert "one net effect
/// across two deliveries"). The pure wasm core ships only the [`Committer`] trait; a wasm consumer
/// provides its own committer.
#[cfg(feature = "native")]
type PolicyTableHandle = std::sync::Arc<std::sync::RwLock<std::sync::Arc<qfs_server::PolicyTable>>>;

#[cfg(feature = "native")]
pub struct RecordingCommitter {
    engine: qfs_core::Engine,
    /// When set, every commit fails with this reason (the forced-failure / retry path).
    fail_reason: Option<String>,
    /// The cumulative count of effects applied across all commits (for the idempotency golden).
    applied: std::sync::atomic::AtomicU64,
    /// The live `/server/policies` table the trigger's bound policy ref resolves against (t35).
    /// `None` (the bare test path) ⇒ an empty table ⇒ a named ref dangles to default-deny.
    policies: Option<PolicyTableHandle>,
}

#[cfg(feature = "native")]
impl RecordingCommitter {
    /// A recording committer over an empty engine (sufficient for a `VALUES`-bodied effect).
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: qfs_core::Engine::new(),
            fail_reason: None,
            applied: std::sync::atomic::AtomicU64::new(0),
            policies: None,
        }
    }

    /// A recording committer over a pre-built engine (with the test's fake mounts registered).
    #[must_use]
    pub fn with_engine(engine: qfs_core::Engine) -> Self {
        Self {
            engine,
            fail_reason: None,
            applied: std::sync::atomic::AtomicU64::new(0),
            policies: None,
        }
    }

    /// A recording committer that fails every commit (the retry / dead-letter tests).
    #[must_use]
    pub fn failing(reason: impl Into<String>) -> Self {
        Self {
            engine: qfs_core::Engine::new(),
            fail_reason: Some(reason.into()),
            applied: std::sync::atomic::AtomicU64::new(0),
            policies: None,
        }
    }

    /// Attach the live `/server/policies` table the trigger's bound policy ref resolves against.
    #[must_use]
    pub fn with_policies(mut self, policies: PolicyTableHandle) -> Self {
        self.policies = Some(policies);
        self
    }

    /// The cumulative effects applied across all commits so far (test/observability aid).
    #[must_use]
    pub fn total_applied(&self) -> u64 {
        self.applied.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Snapshot the live policy table (clones the inner `Arc`; the guard drops at once).
    fn policy_snapshot(&self) -> qfs_server::PolicyTable {
        match &self.policies {
            Some(h) => h.read().map(|g| (**g).clone()).unwrap_or_default(),
            None => qfs_server::PolicyTable::new(),
        }
    }
}

#[cfg(feature = "native")]
impl Default for RecordingCommitter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "native")]
impl Committer for RecordingCommitter {
    fn commit(
        &self,
        trigger: &str,
        stmt: &Statement,
        policy: Option<&str>,
    ) -> Result<FireOutcome, FireError> {
        if let Some(reason) = &self.fail_reason {
            return Err(FireError::Apply(reason.clone()));
        }
        let _ = trigger;
        let plan = qfs_exec::build_plan(stmt, &self.engine)
            .map_err(|e| FireError::Build(e.to_string()))?;

        // t35 policy gate (RFD §10): resolve the trigger's bound policy against the live table
        // and run the PURE enforcer over the built plan BEFORE any apply. No policy / a dangling
        // ref ⇒ fail-closed default-deny. On deny, RETURN before the apply (atomic abort: ZERO
        // effects) — the dispatcher emits the ONE deny fired-plan record from the carried fields.
        let table = self.policy_snapshot();
        let resolved = qfs_server::resolve_policy(policy, &table);
        let gate = qfs_server::gate_plan(&resolved, &plan);
        let effects = gate.effects.clone();
        if let qfs_server::PolicyDecision::Deny {
            verb, driver, rule, ..
        } = &gate.decision
        {
            return Err(FireError::PolicyDenied {
                reason: gate.deny_reason().unwrap_or_default(),
                verb: verb.label().to_string(),
                driver: driver.clone(),
                rule: *rule,
                effects,
            });
        }

        // t37 irreversible gate (RFD §6/§10): the server fires handler plans UNATTENDED
        // (`RunMode::Server`), so an irreversible REMOVE / declared-irreversible CALL is refused
        // fail-closed without an explicit ack — exactly like CI. Reversible plans pass untouched.
        if let Err(needs) = qfs_core::IrreversibleGuard::require_ack(
            &plan,
            qfs_core::RunMode::Server,
            qfs_core::Ack::Absent,
        ) {
            return Err(FireError::IrreversibleBlocked(needs.reason().to_string()));
        }

        let preview = qfs_core::preview(&plan);
        let plan_summary = qfs_crypto_core::sha256_hex(format!("{preview:?}").as_bytes());
        // Commit over a recording applier (no creds, no network — the PREVIEW path).
        let mut applier = qfs_core::RecordingApplier::new();
        let report = qfs_core::commit(&plan, &mut applier, |_| {});
        if let Some(err) = report.failed {
            return Err(FireError::Apply(err.to_string()));
        }
        let affected = report.applied.len() as u64;
        self.applied
            .fetch_add(affected, std::sync::atomic::Ordering::SeqCst);
        Ok(FireOutcome {
            plan_summary,
            affected,
            effects,
        })
    }
}
