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
}
