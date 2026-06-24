//! The fired-plan audit record (RFD §6 audit ledger / §10): exactly one
//! [`FiredPlanRecord`] per evaluated handler plan — allow AND deny. Secret-free: driver name
//! + path + verb + rule index only, NEVER a payload or credential.

use super::enforce::PolicyDecision;

/// One fired-plan audit record (RFD §6/§10). Emitted for EVERY plan a handler fires — both
/// permitted and denied — so the ledger is the single funnel of unattended execution. Carries
/// the secret-free effect summaries only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredPlanRecord {
    /// The handler that fired the plan (`"job:nightly"`, `"trigger:notify"`,
    /// `"endpoint:write"`). A label, never a credential.
    pub handler: String,
    /// The bound policy name (the `/server/policies` row), or empty when none was attached
    /// (the fail-closed default-deny path).
    pub policy: String,
    /// Whether the plan was allowed; on deny, the verb/driver/rule coordinates.
    pub decision: FiredDecision,
    /// The secret-free per-effect summaries (`"INSERT mail:/mail/outbox"`). Driver + path +
    /// verb only — NEVER the row payload (RFD §10).
    pub effects: Vec<String>,
    /// The epoch second the plan fired (the receipt clock).
    pub ts: i64,
}

/// The decision half of a [`FiredPlanRecord`] — a secret-free projection of [`PolicyDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiredDecision {
    /// The plan was permitted.
    Allow,
    /// The plan was denied; carries the offending verb/driver and rule index (or `None` for
    /// the default-deny with no matching rule).
    Deny {
        /// The denied effect's verb label (`"REMOVE"`, `"CALL"`, …).
        verb: String,
        /// The denied effect's driver (secret-free name).
        driver: String,
        /// The matching rule index, or `None` for the default-deny.
        rule: Option<usize>,
    },
}

impl FiredDecision {
    /// Project a [`PolicyDecision`] into the audit decision (secret-free).
    #[must_use]
    pub fn from_decision(decision: &PolicyDecision) -> Self {
        match decision {
            PolicyDecision::Allow => FiredDecision::Allow,
            PolicyDecision::Deny {
                verb, driver, rule, ..
            } => FiredDecision::Deny {
                verb: verb.label().to_string(),
                driver: driver.clone(),
                rule: *rule,
            },
        }
    }

    /// Whether this records a permitted fire.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, FiredDecision::Allow)
    }
}

impl FiredPlanRecord {
    /// A one-line, secret-free rendering for the drain log / operator output.
    #[must_use]
    pub fn summary(&self) -> String {
        let pol = if self.policy.is_empty() {
            "-"
        } else {
            self.policy.as_str()
        };
        match &self.decision {
            FiredDecision::Allow => format!(
                "ALLOW handler={} policy={pol} effects={} ts={}",
                self.handler,
                self.effects.len(),
                self.ts
            ),
            FiredDecision::Deny { verb, driver, rule } => format!(
                "DENY handler={} policy={pol} verb={verb} driver={driver} rule={} ts={}",
                self.handler,
                rule.map(|r| r.to_string())
                    .unwrap_or_else(|| "default".to_string()),
                self.ts
            ),
        }
    }
}
