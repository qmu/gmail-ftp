//! The irreversible-effect commit gate (t37, RFD-0001 §6/§10).
//!
//! `qfs` runs cross-service effect-plans **unattended** — a large blast radius. Some effects
//! are *irreversible*: a `REMOVE` (delete) or a `CALL mail.send` cannot be undone (the
//! `irreversible` bit is set by the planner per proc/verb and surfaced on
//! [`Plan::is_irreversible`]). This module is the single, pure gate that decides whether a
//! plan carrying an irreversible effect may proceed to `COMMIT`, given **how** qfs is being
//! driven ([`RunMode`]).
//!
//! ## Why it lives in `qfs-core` (the hub)
//! Both faces of the binary commit through `qfs-core`'s [`Plan`]: the CLI one-shot
//! (`qfs run … --commit`, via `qfs-exec`) and the serve fire paths (HTTP / cron / watchtower
//! committers, via `qfs-server`). Placing the guard on the hub's `Plan` lets *both* reuse one
//! pure decision rather than each re-deriving "is this safe to apply unattended?". It is pure
//! data-in/data-out (no I/O, no prompting) so it stays wasm-clean and testable with no creds.
//!
//! ## The decision (fail closed)
//! - **Reversible plan** ⇒ always `Ok` (this gate only governs irreversible effects).
//! - **Irreversible plan**:
//!   - [`RunMode::Ci`] / [`RunMode::Server`] (non-interactive): **hard-fail closed** unless the
//!     operator passed an explicit `--commit-irreversible` ack ([`Ack::Granted`]). No prompt is
//!     possible with no human present, so the safe default is refusal.
//!   - [`RunMode::Cli`] (interactive): the caller must PREVIEW and then prompt the human; this
//!     gate returns [`NeedsPreview::Prompt`] so the CLI knows to confirm after the PREVIEW.
//!   - [`RunMode::CliOneShot`]: a non-interactive one-shot invocation — treated like CI: an
//!     explicit ack is required (a one-shot has no TTY to confirm on).
//!
//! The redaction control (`Secret`), the policy default-deny (t35), and the audit ledger (t36)
//! are the *other* legs of the defense-in-depth; this is the irreversible-effect leg.

use qfs_plan::Plan;
use serde::{Deserialize, Serialize};

/// The selectable **AI safety mode** (t59, decision J / roadmap §2.4): the operator-chosen preset
/// that governs HOW MUCH an autonomous agent (over MCP), and the CLI/dashboard, may auto-commit vs.
/// require an explicit human approval. The mode is *deployment configuration stored as data* (a
/// `/sys/settings` row), NOT grammar — it parameterizes the existing commit machinery without
/// changing a keyword.
///
/// ## It CONFIGURES the floor, it never lowers it
/// Two invariants are the FLOOR every mode is composed *on top of*, and which no mode can relax:
///  1. **The policy gate** ([`gate_plan`](crate)/`evaluate`, default-deny): a plan that is **out of
///     policy** is denied in **every** mode (`within_policy == false` ⇒ [`SafetyDecision::Deny`]).
///     A mode can only narrow what an *in-policy* plan may do; it can never widen the policy itself.
///  2. **Irreversibility is classified, never re-derived**: the decision routes solely on
///     [`Plan::is_irreversible`] (the planner's single source of truth) — the mode chooses whether an
///     irreversible-in-policy effect auto-applies, is held for approval, or (never) escapes the gate.
///
/// The three presets sit on a most-restrictive-to-least spectrum *for in-policy effects only*:
/// [`Self::ApproveEverything`] (every write held) is the most restrictive, [`Self::PolicyOnly`]
/// (both auto, for unattended CI) the least — but all three are equally bound by the policy floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SafetyMode {
    /// **Autonomous-in-policy** (the DEFAULT, and the safest sensible fallback for an unset /
    /// misconfigured mode — fail safe, never open). A reversible in-policy effect auto-commits; an
    /// irreversible in-policy effect ([`REMOVE`](qfs_plan::EffectKind::Remove) / a declared-
    /// irreversible [`CALL`](qfs_plan::EffectKind::Call)) is **held for a human ack** (the t52
    /// approval card / `--commit-irreversible`). This is the historical `RunMode::Server` posture.
    #[default]
    AutonomousInPolicy,
    /// **Approve-everything** (the most restrictive preset): a human ack is required for **both**
    /// reversible and irreversible in-policy effects — the agent may auto-commit nothing. Use when an
    /// operator wants to see and confirm every write an agent proposes.
    ApproveEverything,
    /// **Policy-only** (for unattended CI / batch): **both** reversible and irreversible in-policy
    /// effects auto-commit — selecting this mode IS the operator's standing, audited acknowledgement
    /// of irreversible-in-policy effects, so no per-call human approval is raised. It removes the
    /// *human* step ONLY; it does **not** touch the policy gate — an out-of-policy plan is still
    /// denied here exactly as in every other mode (the floor is intact).
    PolicyOnly,
}

/// The pure verdict of [`SafetyMode::decide`] (and [`IrreversibleGuard::decide`]): what the commit
/// boundary must do with a plan under the selected mode. Impure follow-through (applying, raising
/// the approval card / push notification) is the caller's — this type only classifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyDecision {
    /// Apply now: a reversible in-policy effect under an autonomous mode, an in-policy effect under
    /// [`SafetyMode::PolicyOnly`], or any held effect whose explicit ack was supplied.
    AutoCommit,
    /// Hold pending an explicit human ack (the t52 approval card / `--commit-irreversible`): the
    /// plan is in policy but the mode requires a human to confirm before it applies. Nothing is
    /// applied until the ack arrives.
    NeedApproval,
    /// Refuse: the plan is **out of policy**. No mode can apply it — the policy floor wins on
    /// conflict (most-restrictive-wins). Distinct from [`Self::NeedApproval`]: an ack cannot rescue
    /// a denied plan, only a policy change can.
    Deny,
}

impl SafetyMode {
    /// The stable, lowercase kebab-case label — the value persisted in `/sys/settings` and accepted
    /// by [`Self::from_label`]. Matches the serde wire form so config round-trips through either path.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            SafetyMode::AutonomousInPolicy => "autonomous-in-policy",
            SafetyMode::ApproveEverything => "approve-everything",
            SafetyMode::PolicyOnly => "policy-only",
        }
    }

    /// Parse a stored/configured label back into a mode. Accepts the canonical kebab-case label
    /// case-insensitively and treats `_` as `-` (so `autonomous_in_policy` also resolves). Returns
    /// `None` for an unknown value — callers that must NOT fail (the live resolver) use
    /// [`Self::from_label_or_default`] so an unset/garbled config fails *safe*, never open.
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|m| m.label() == norm)
    }

    /// Resolve a configured label to a mode, **failing safe** to the default
    /// ([`Self::AutonomousInPolicy`]) on any unknown/empty value (decision: an unset or misconfigured
    /// mode must fall to the safest sensible default — irreversible needs approval — never to
    /// Policy-only-auto). This is the production resolver the commit paths consult.
    #[must_use]
    pub fn from_label_or_default(s: &str) -> Self {
        Self::from_label(s).unwrap_or_default()
    }

    /// The three presets, in most-restrictive-to-least order (for in-policy effects). The single
    /// source of truth for "what modes exist" (label parsing, docs, exhaustive tests).
    pub const ALL: [SafetyMode; 3] = [
        SafetyMode::ApproveEverything,
        SafetyMode::AutonomousInPolicy,
        SafetyMode::PolicyOnly,
    ];

    /// The **pure** safety decision (step 1, roadmap §2.4): given whether the plan is irreversible
    /// and whether it is within policy, return whether it auto-commits, is held for approval, or is
    /// denied. No I/O, no ack — exhaustive over the 3×2×2 table. The POLICY FLOOR is the first arm:
    /// an out-of-policy plan is [`SafetyDecision::Deny`] in **every** mode (the gate is never relaxed
    /// by a mode). Only an *in-policy* plan reaches the per-mode routing.
    #[must_use]
    pub fn decide(self, plan_is_irreversible: bool, within_policy: bool) -> SafetyDecision {
        // FLOOR 1 — the policy gate. Out of policy ⇒ denied regardless of mode (most-restrictive
        // wins). A mode narrows in-policy behaviour; it can never widen the policy itself.
        if !within_policy {
            return SafetyDecision::Deny;
        }
        match self {
            // Reversible auto-commits; irreversible is held for the human ack (FLOOR 2 for this mode).
            SafetyMode::AutonomousInPolicy => {
                if plan_is_irreversible {
                    SafetyDecision::NeedApproval
                } else {
                    SafetyDecision::AutoCommit
                }
            }
            // The most restrictive preset: every in-policy write is held for a human ack.
            SafetyMode::ApproveEverything => SafetyDecision::NeedApproval,
            // Unattended CI: both auto-commit within policy (the mode itself is the standing ack).
            SafetyMode::PolicyOnly => SafetyDecision::AutoCommit,
        }
    }
}

impl std::fmt::Display for SafetyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for SafetyMode {
    type Err = UnknownSafetyMode;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_label(s).ok_or_else(|| UnknownSafetyMode(s.to_string()))
    }
}

/// The error a strict [`SafetyMode`] parse (`FromStr`) returns for an unknown label. The live
/// resolver does not use this (it fails safe via [`SafetyMode::from_label_or_default`]); it exists
/// for a config-validation caller that wants to reject a typo loudly. (qfs-core carries no
/// `thiserror` dep, so the trait impls are hand-written.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSafetyMode(pub String);

impl std::fmt::Display for UnknownSafetyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown safety mode `{}` (expected autonomous-in-policy / approve-everything / \
             policy-only)",
            self.0
        )
    }
}

impl std::error::Error for UnknownSafetyMode {}

/// How qfs is being driven — the context the [`IrreversibleGuard`] decides against. Whether a
/// human is present to confirm an irreversible apply is the load-bearing distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// An interactive CLI session (a TTY shell): the operator can be prompted to confirm an
    /// irreversible apply **after** the PREVIEW is shown.
    Cli,
    /// A non-interactive one-shot CLI invocation (`qfs run … --commit`, no TTY): there is no
    /// prompt to show, so an irreversible apply requires the explicit `--commit-irreversible`
    /// ack, exactly like CI.
    CliOneShot,
    /// A CI / batch context: no human, no TTY. An irreversible apply is refused unless the
    /// pipeline passed the explicit ack flag (fail closed).
    Ci,
    /// The long-lived server / daemon firing handler plans unattended. Same fail-closed posture
    /// as CI: an irreversible handler plan needs the explicit ack to be permitted.
    Server,
}

impl RunMode {
    /// Whether this mode can interactively prompt a human to confirm an irreversible apply.
    /// Only the interactive [`RunMode::Cli`] can; every other mode must rely on the explicit ack.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        matches!(self, RunMode::Cli)
    }
}

/// Whether the operator supplied the explicit `--commit-irreversible` acknowledgement. Modeled
/// as a distinct two-state type (not a bare `bool`) so a caller cannot pass `true`/`false` to
/// the wrong argument by accident, and so the ack is legible at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// The operator passed `--commit-irreversible`: an irreversible apply is explicitly allowed.
    Granted,
    /// No ack was passed: the default. An irreversible apply in a non-interactive mode is refused.
    Absent,
}

/// The gate's verdict for a plan carrying an irreversible effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedsPreview {
    /// Non-interactive mode (`Ci`/`Server`/`CliOneShot`) with no ack: the apply is **refused**.
    /// The caller MUST NOT commit; it surfaces a structured error telling the operator to re-run
    /// with `--commit-irreversible` (or in an interactive session).
    Blocked,
    /// Interactive mode: the caller must show the PREVIEW and prompt the human to confirm before
    /// committing. The apply is permitted only on an affirmative prompt.
    Prompt,
}

impl NeedsPreview {
    /// The stable, secret-free reason string for the structured error / operator message.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            NeedsPreview::Blocked => {
                "plan contains an irreversible effect (REMOVE / CALL); refusing to apply \
                 unattended without --commit-irreversible"
            }
            NeedsPreview::Prompt => {
                "plan contains an irreversible effect (REMOVE / CALL); confirm after PREVIEW"
            }
        }
    }
}

/// The irreversible-effect commit gate. Pure: it inspects the plan's `irreversible` flag and the
/// [`RunMode`] + [`Ack`], and returns whether the apply may proceed. It performs no I/O and does
/// not itself prompt — an interactive caller drives the prompt on a [`NeedsPreview::Prompt`].
#[derive(Debug, Clone, Copy, Default)]
pub struct IrreversibleGuard;

impl IrreversibleGuard {
    /// Decide whether `plan` may be committed under `mode` with acknowledgement `ack`.
    ///
    /// - A reversible plan is always `Ok(())` (this gate governs only irreversible effects).
    /// - An irreversible plan with an explicit [`Ack::Granted`] is `Ok(())` in every mode (the
    ///   operator took responsibility).
    /// - An irreversible plan without the ack in a non-interactive mode is
    ///   `Err(`[`NeedsPreview::Blocked`]`)` — fail closed.
    /// - An irreversible plan without the ack in the interactive [`RunMode::Cli`] is
    ///   `Err(`[`NeedsPreview::Prompt`]`)` — the caller must PREVIEW + confirm.
    ///
    /// # Errors
    /// Returns [`NeedsPreview`] when an irreversible plan may not silently proceed: `Blocked`
    /// (non-interactive, no ack) or `Prompt` (interactive, needs confirmation).
    pub fn require_ack(plan: &Plan, mode: RunMode, ack: Ack) -> Result<(), NeedsPreview> {
        if !plan.is_irreversible() {
            return Ok(());
        }
        if ack == Ack::Granted {
            // The operator explicitly accepted the irreversible apply in any mode.
            return Ok(());
        }
        if mode.is_interactive() {
            Err(NeedsPreview::Prompt)
        } else {
            Err(NeedsPreview::Blocked)
        }
    }

    /// The **selectable-safety-mode** commit decision (t59) — the live decision the production
    /// commit paths (`qfs_mcp::commit_plan` for MCP + the dashboard, and the CLI one-shot) consult.
    /// It composes the pure mode routing ([`SafetyMode::decide`]) with the per-call explicit `ack`:
    ///
    /// - `within_policy` MUST be the policy gate's verdict (`gate_plan(..).is_allow()`); a plan the
    ///   gate denied is [`SafetyDecision::Deny`] in every mode (the floor — the mode is consulted
    ///   *after*, and never on top of, a denied plan).
    /// - A [`SafetyDecision::NeedApproval`] held by the mode is satisfied by an explicit
    ///   [`Ack::Granted`] (the approval card's confirm / `--commit-irreversible`), collapsing it to
    ///   [`SafetyDecision::AutoCommit`]. Without the ack it stays held.
    /// - [`SafetyDecision::AutoCommit`] and [`SafetyDecision::Deny`] are unaffected by the ack — an
    ///   ack neither blocks an already-permitted apply nor rescues an out-of-policy plan.
    ///
    /// Pure: it classifies. The caller performs the apply / raises the card. Irreversibility is read
    /// ONLY from [`Plan::is_irreversible`] (never re-derived), so the safety guarantee cannot fork.
    #[must_use]
    pub fn decide(plan: &Plan, mode: SafetyMode, within_policy: bool, ack: Ack) -> SafetyDecision {
        match mode.decide(plan.is_irreversible(), within_policy) {
            // An explicit per-call ack is the human confirmation a held approval was waiting for.
            SafetyDecision::NeedApproval if ack == Ack::Granted => SafetyDecision::AutoCommit,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_plan::{EffectKind, EffectNode, NodeId, Plan, Target, VfsPath};
    use qfs_types::DriverId;

    /// A plan with one effect node; `irreversible` toggles the REMOVE (inherently irreversible)
    /// vs an INSERT (reversible) shape.
    fn plan_with(kind: EffectKind) -> Plan {
        let mut plan = Plan::pure();
        plan.nodes = vec![EffectNode::new(
            NodeId(0),
            kind,
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/outbox")),
        )];
        plan
    }

    #[test]
    fn reversible_plan_passes_in_every_mode() {
        let plan = plan_with(EffectKind::Insert);
        for mode in [
            RunMode::Cli,
            RunMode::CliOneShot,
            RunMode::Ci,
            RunMode::Server,
        ] {
            assert!(IrreversibleGuard::require_ack(&plan, mode, Ack::Absent).is_ok());
        }
    }

    #[test]
    fn irreversible_in_ci_without_ack_fails_closed() {
        let plan = plan_with(EffectKind::Remove);
        assert!(plan.is_irreversible(), "REMOVE is inherently irreversible");
        let verdict = IrreversibleGuard::require_ack(&plan, RunMode::Ci, Ack::Absent);
        assert_eq!(verdict, Err(NeedsPreview::Blocked));
        assert!(verdict
            .unwrap_err()
            .reason()
            .contains("--commit-irreversible"));
    }

    #[test]
    fn irreversible_one_shot_without_ack_is_blocked_like_ci() {
        let plan = plan_with(EffectKind::Remove);
        assert_eq!(
            IrreversibleGuard::require_ack(&plan, RunMode::CliOneShot, Ack::Absent),
            Err(NeedsPreview::Blocked)
        );
    }

    #[test]
    fn irreversible_server_without_ack_is_blocked() {
        let plan = plan_with(EffectKind::Remove);
        assert_eq!(
            IrreversibleGuard::require_ack(&plan, RunMode::Server, Ack::Absent),
            Err(NeedsPreview::Blocked)
        );
    }

    #[test]
    fn irreversible_in_interactive_cli_prompts_after_preview() {
        let plan = plan_with(EffectKind::Remove);
        assert_eq!(
            IrreversibleGuard::require_ack(&plan, RunMode::Cli, Ack::Absent),
            Err(NeedsPreview::Prompt)
        );
    }

    #[test]
    fn explicit_ack_permits_irreversible_apply_in_every_mode() {
        let plan = plan_with(EffectKind::Remove);
        for mode in [
            RunMode::Cli,
            RunMode::CliOneShot,
            RunMode::Ci,
            RunMode::Server,
        ] {
            assert!(
                IrreversibleGuard::require_ack(&plan, mode, Ack::Granted).is_ok(),
                "an explicit ack permits the apply in {mode:?}"
            );
        }
    }

    // --- t59: the three selectable safety presets ---------------------------------------------

    /// The full 3×2×2 decision table (roadmap §2.4), asserted cell-by-cell on the pure
    /// [`SafetyMode::decide`]. `within_policy` is the second axis; `irreversible` the third.
    #[test]
    fn safety_mode_decision_table_is_exhaustive() {
        use SafetyDecision::{AutoCommit, Deny, NeedApproval};
        use SafetyMode::{ApproveEverything, AutonomousInPolicy, PolicyOnly};
        // (mode, irreversible, within_policy) -> decision
        let table = [
            // Autonomous-in-policy: reversible auto, irreversible held — only within policy.
            (AutonomousInPolicy, false, true, AutoCommit),
            (AutonomousInPolicy, true, true, NeedApproval),
            (AutonomousInPolicy, false, false, Deny),
            (AutonomousInPolicy, true, false, Deny),
            // Approve-everything: both held within policy; denied out of policy.
            (ApproveEverything, false, true, NeedApproval),
            (ApproveEverything, true, true, NeedApproval),
            (ApproveEverything, false, false, Deny),
            (ApproveEverything, true, false, Deny),
            // Policy-only: both auto within policy; STILL denied out of policy (floor intact).
            (PolicyOnly, false, true, AutoCommit),
            (PolicyOnly, true, true, AutoCommit),
            (PolicyOnly, false, false, Deny),
            (PolicyOnly, true, false, Deny),
        ];
        for (mode, irreversible, within_policy, expected) in table {
            assert_eq!(
                mode.decide(irreversible, within_policy),
                expected,
                "decide({mode:?}, irreversible={irreversible}, within_policy={within_policy})"
            );
        }
    }

    /// The POLICY FLOOR: an out-of-policy plan is denied in EVERY mode — no preset can bypass the
    /// gate (most-restrictive-wins). An ack cannot rescue it either.
    #[test]
    fn out_of_policy_is_denied_in_every_mode_even_with_ack() {
        let reversible = plan_with(EffectKind::Insert);
        let irreversible = plan_with(EffectKind::Remove);
        for mode in SafetyMode::ALL {
            for plan in [&reversible, &irreversible] {
                for ack in [Ack::Absent, Ack::Granted] {
                    assert_eq!(
                        IrreversibleGuard::decide(plan, mode, false, ack),
                        SafetyDecision::Deny,
                        "{mode:?} must DENY an out-of-policy plan regardless of ack"
                    );
                }
            }
        }
    }

    /// The IRREVERSIBLE-ACK FLOOR for the approval modes: an in-policy irreversible plan is HELD
    /// (never auto-applied) in Autonomous and Approve modes until an explicit ack arrives.
    #[test]
    fn approval_modes_hold_irreversible_until_acked() {
        let plan = plan_with(EffectKind::Remove);
        for mode in [
            SafetyMode::AutonomousInPolicy,
            SafetyMode::ApproveEverything,
        ] {
            assert_eq!(
                IrreversibleGuard::decide(&plan, mode, true, Ack::Absent),
                SafetyDecision::NeedApproval,
                "{mode:?} holds an unacked irreversible in-policy plan"
            );
            assert_eq!(
                IrreversibleGuard::decide(&plan, mode, true, Ack::Granted),
                SafetyDecision::AutoCommit,
                "{mode:?} applies the irreversible plan once acked"
            );
        }
    }

    /// Approve-everything is the most restrictive preset: it HOLDS even a reversible in-policy write
    /// that Autonomous would auto-apply — the key differentiating behaviour the commit paths prove.
    #[test]
    fn approve_everything_holds_a_reversible_write_autonomous_would_auto_apply() {
        let plan = plan_with(EffectKind::Insert);
        assert_eq!(
            IrreversibleGuard::decide(&plan, SafetyMode::AutonomousInPolicy, true, Ack::Absent),
            SafetyDecision::AutoCommit
        );
        assert_eq!(
            IrreversibleGuard::decide(&plan, SafetyMode::ApproveEverything, true, Ack::Absent),
            SafetyDecision::NeedApproval
        );
    }

    /// Policy-only auto-commits an irreversible in-policy effect with no per-call ack (unattended
    /// CI) — the documented standing acknowledgement — while still respecting the policy floor.
    #[test]
    fn policy_only_auto_commits_irreversible_in_policy() {
        let plan = plan_with(EffectKind::Remove);
        assert_eq!(
            IrreversibleGuard::decide(&plan, SafetyMode::PolicyOnly, true, Ack::Absent),
            SafetyDecision::AutoCommit
        );
    }

    /// Mode config round-trips through the label (the `/sys/settings` value) and serde, and an
    /// unknown value fails SAFE to the default (never to Policy-only-auto).
    #[test]
    fn safety_mode_config_round_trips_and_fails_safe() {
        for mode in SafetyMode::ALL {
            // label <-> from_label round-trip.
            assert_eq!(SafetyMode::from_label(mode.label()), Some(mode));
            // FromStr / Display round-trip (and underscore tolerance).
            assert_eq!(mode.to_string().parse::<SafetyMode>().unwrap(), mode);
            assert_eq!(
                SafetyMode::from_label(&mode.label().replace('-', "_")),
                Some(mode)
            );
            // serde round-trip (kebab-case, matches the label).
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.label()));
            assert_eq!(serde_json::from_str::<SafetyMode>(&json).unwrap(), mode);
        }
        // Default + fail-safe.
        assert_eq!(SafetyMode::default(), SafetyMode::AutonomousInPolicy);
        assert_eq!(
            SafetyMode::from_label_or_default("nonsense"),
            SafetyMode::AutonomousInPolicy
        );
        assert_eq!(
            SafetyMode::from_label_or_default(""),
            SafetyMode::AutonomousInPolicy
        );
        assert!("nonsense".parse::<SafetyMode>().is_err());
    }
}
