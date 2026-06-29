//! `qfs job run|cron` — the **external-scheduler entrypoint** (t65, roadmap §4.3 / decision M
//! revised): drive a saved `/server/jobs` plan from OS `cron` (individual) or emit the crontab line
//! the host scheduler uses. The internal scheduler daemon is RETIRED — qfs is not a scheduler.
//!
//! A `CREATE JOB <name> EVERY <interval> DO <plan>` row is now a **saved named plan plus its
//! intended cadence**: metadata an external scheduler reads and runs. This module is the binary's
//! composition root for that invocation — injected into `qfs-cmd` via the [`qfs_cmd::JobLauncher`]
//! (the `ServeLauncher` pattern), so `qfs-cmd` stays off `qfs-host`/`qfs-exec`/`qfs-runtime`.
//!
//! ## The safety floor is unchanged (the ticket's PRESERVE)
//! Whatever fires the unit, `qfs job run --commit` builds the saved plan and commits it through the
//! SAME gates the retired daemon and the CLI one-shot enforce:
//!   1. **PREVIEW by default** — without `--commit` it prints the effect summary and applies nothing.
//!   2. **Policy gate** (least privilege) — the JOB's bound `POLICY` is resolved against the config's
//!      `/server/policies` table and the built plan is gated BEFORE any apply (default-deny / atomic
//!      abort). An external trigger decides *when*, never *whether*.
//!   3. **IrreversibleGuard** — an external scheduler fires UNATTENDED ([`RunMode::Server`]), so an
//!      irreversible `REMOVE` / `CALL` needs the explicit `--commit-irreversible` ack, fail-closed.
//!   4. **The real applier** — the commit runs through [`crate::commit::apply_plan`] (the same
//!      runtime interpreter + live driver registry `qfs run --commit` uses), so it audits (t76) and
//!      emits telemetry (t77) exactly like any other commit.
//!
//! ## Idempotency is the author's responsibility now
//! External schedulers are at-least-once (a Cron Trigger can double-fire on retry). The retired
//! internal ledger no longer dedups a re-fire — keep effects idempotent (`UPSERT` / `@version`
//! preconditions) so a re-run is a no-op. This is documented in the OS-cron how-to.

use std::path::Path;

use qfs_cmd::{JobAction, JobRequest};
use qfs_core::{Ack, IrreversibleGuard, PlanSpec, RunMode};

/// The injected [`qfs_cmd::JobLauncher`] body: route a parsed `qfs job <verb>` request to `run` or
/// `cron`. Returns the process exit code; never panics.
#[must_use]
pub fn run_job_request(req: &JobRequest) -> i32 {
    match req.action {
        JobAction::Run => run_job(req),
        JobAction::Cron => emit_cron(&req.config, &req.name),
    }
}

/// `qfs job run <config> <name> [--commit] [--commit-irreversible]`: boot the config, rehydrate the
/// named saved plan, build it, gate it (policy + irreversible), and commit it ONCE through the real
/// applier. PREVIEW (no apply) unless `--commit`.
fn run_job(req: &JobRequest) -> i32 {
    // Boot the config + extract its saved jobs and policy table (the qfs-server coupling lives
    // behind qfs-host's `host-daemon` feature, so the binary stays the thin entrypoint).
    let cfg = match qfs_host::jobs_from_config(&req.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("qfs job run: {e}");
            return 1;
        }
    };
    let Some(job) = cfg.find(&req.name) else {
        eprintln!(
            "qfs job run: no job named {:?} in {}",
            req.name,
            req.config.display()
        );
        return 1;
    };

    // Rehydrate the saved DO body (the canonical PlanSpec, NO re-parse) into its statement.
    let spec = match PlanSpec::from_canonical(&job.plan_canonical) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("qfs job run: saved plan not rehydratable: {e}");
            return 1;
        }
    };

    // Build the plan against the run engine (local-fs + the cred-free networked mounts so a
    // `FROM`/target resolves + plans; resolve + plan construction only, no I/O).
    let (engine, _reads, _safety) = crate::shell::run_engine_and_reads();
    let plan = match qfs_exec::build_plan(spec.statement(), &engine) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("qfs job run: plan build failed: {e}");
            return e.exit_code().code();
        }
    };

    // Policy gate (least privilege): resolve the JOB's bound POLICY against the config's
    // `/server/policies` table and gate the built plan BEFORE any apply. A job with no policy / a
    // dangling ref ⇒ fail-closed default-deny — the SAME posture the retired daemon took at fire
    // time. Atomic abort on deny: the commit is never reached, so ZERO effects apply.
    let policy = qfs_host::resolve_policy(job.policy.as_deref(), &cfg.policies);
    let gate = qfs_host::gate_plan(&policy, &plan);
    if !gate.is_allow() {
        eprintln!(
            "qfs job run: policy denied: {}",
            gate.deny_reason().unwrap_or_default()
        );
        return 1;
    }

    // PREVIEW by default (the preview→commit floor): print the secret-free effect summary and exit
    // without applying. A crontab line passes `--commit`.
    if !req.commit {
        if !req.quiet {
            println!(
                "PREVIEW job '{}' (policy {}, {} effect(s); nothing applied — pass --commit):",
                job.name,
                job.policy.as_deref().unwrap_or("<none>"),
                gate.effects.len()
            );
            for e in &gate.effects {
                println!("  {e}");
            }
        }
        return 0;
    }

    // IrreversibleGuard (RFD §6/§10): an external scheduler fires UNATTENDED — an irreversible
    // REMOVE / declared-irreversible CALL needs the explicit `--commit-irreversible` ack, fail
    // closed without it (exactly like CI and `qfs run --commit-irreversible`).
    let ack = if req.commit_irreversible {
        Ack::Granted
    } else {
        Ack::Absent
    };
    if let Err(needs) = IrreversibleGuard::require_ack(&plan, RunMode::Server, ack) {
        eprintln!("qfs job run: {}", needs.reason());
        return 1;
    }

    // Commit ONCE through the real runtime applier (audits via t76 + emits telemetry via t77, like
    // every other commit). The same WorldApply path the CLI one-shot uses — no privileged shortcut.
    match crate::commit::apply_plan(&plan) {
        Ok(()) => {
            if !req.quiet {
                println!(
                    "COMMITTED job '{}' ({} effect(s) applied)",
                    job.name,
                    gate.effects.len()
                );
            }
            0
        }
        Err(e) => {
            eprintln!("qfs job run: commit failed: {e}");
            e.exit_code().code()
        }
    }
}

/// `qfs job cron <config> <name>`: emit the OS-cron crontab line that invokes the saved JOB on its
/// `EVERY` cadence — the individual counterpart of the managed `[triggers] crons` entry the
/// wrangler generator emits. The operator drops the printed line into a host crontab; qfs runs no
/// scheduler of its own.
fn emit_cron(config: &Path, name: &str) -> i32 {
    let cfg = match qfs_host::jobs_from_config(config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("qfs job cron: {e}");
            return 1;
        }
    };
    let Some(job) = cfg.find(name) else {
        eprintln!(
            "qfs job cron: no job named {name:?} in {}",
            config.display()
        );
        return 1;
    };

    // The crontab line: the derived schedule + the non-interactive invocation that runs the saved
    // plan. We print the config path as given (the operator anchors it to an absolute path in the
    // crontab). `--commit` is included because a scheduled run is meant to apply.
    println!(
        "# qfs JOB '{}' — EVERY {} (decision M: OS cron owns the *when*; qfs is not a scheduler).",
        job.name, job.every
    );
    println!(
        "# Ensure cron's environment carries QFS_PASSPHRASE (+ any connection creds) for the commit."
    );
    println!(
        "# An irreversible plan (REMOVE / CALL) additionally needs --commit-irreversible (fail-closed)."
    );
    println!(
        "{cron}\tqfs job run {config} {name} --commit",
        cron = job.cron,
        config = config.display(),
        name = job.name,
    );
    // The managed-tier equivalent, so an operator sees both paths emit the SAME cadence.
    println!(
        "# Managed tier (Cloudflare Cron Triggers): [triggers] crons = [\"{}\"]",
        job.cron
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Write a `.qfs` config to a NEW file inside `dir` and return its path.
    fn write_config(dir: &std::path::Path, src: &str) -> PathBuf {
        let path = dir.join("app.qfs");
        let mut f = std::fs::File::create(&path).expect("create config");
        f.write_all(src.as_bytes()).expect("write config");
        path
    }

    fn req(action: JobAction, config: PathBuf, name: &str, commit: bool, ack: bool) -> JobRequest {
        JobRequest {
            action,
            config,
            name: name.to_string(),
            commit,
            commit_irreversible: ack,
            json: false,
            format: None,
            quiet: true,
        }
    }

    /// `qfs job run --commit` builds + commits a saved REMOVE plan ONCE through the policy gate +
    /// the real applier, deleting the file. (REMOVE is irreversible, so the run also exercises the
    /// IrreversibleGuard ack path.) The local mount is rooted at `/`, so `/local/<abs>` → host
    /// `<abs>` — the tempdir is on the big disk via TMPDIR.
    #[test]
    fn job_run_commits_a_defined_plan_once() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"bye").unwrap();
        let cfg = write_config(
            dir.path(),
            &format!(
                "CREATE POLICY p ALLOW REMOVE;\n\
                 CREATE JOB cleanup EVERY '1h' DO REMOVE /local{} POLICY p;\n",
                victim.display()
            ),
        );
        // With the irreversible ack, the commit applies and the file is gone.
        let code = run_job(&req(JobAction::Run, cfg, "cleanup", true, true));
        assert_eq!(code, 0, "job run --commit --commit-irreversible succeeds");
        assert!(!victim.exists(), "the saved REMOVE plan committed once");
    }

    /// The safety floor is PRESERVED: an irreversible saved plan committed WITHOUT the ack is
    /// refused (fail-closed, RunMode::Server), and nothing is applied.
    #[test]
    fn job_run_irreversible_without_ack_is_blocked() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("keep.txt");
        std::fs::write(&victim, b"stay").unwrap();
        let cfg = write_config(
            dir.path(),
            &format!(
                "CREATE POLICY p ALLOW REMOVE;\n\
                 CREATE JOB cleanup EVERY '1h' DO REMOVE /local{} POLICY p;\n",
                victim.display()
            ),
        );
        let code = run_job(&req(JobAction::Run, cfg, "cleanup", true, false));
        assert_eq!(
            code, 1,
            "irreversible commit without --commit-irreversible is refused"
        );
        assert!(victim.exists(), "fail-closed: nothing was applied");
    }

    /// PREVIEW by default: no `--commit` applies nothing, even with a permissive policy.
    #[test]
    fn job_run_previews_without_commit() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("preview.txt");
        std::fs::write(&victim, b"intact").unwrap();
        let cfg = write_config(
            dir.path(),
            &format!(
                "CREATE POLICY p ALLOW REMOVE;\n\
                 CREATE JOB cleanup EVERY '1h' DO REMOVE /local{} POLICY p;\n",
                victim.display()
            ),
        );
        let code = run_job(&req(JobAction::Run, cfg, "cleanup", false, false));
        assert_eq!(code, 0, "preview exits 0");
        assert!(victim.exists(), "PREVIEW applies nothing");
    }

    /// A saved plan whose bound policy does NOT allow its verb is DENIED (default-deny floor),
    /// atomic abort — nothing applied.
    #[test]
    fn job_run_policy_denied_aborts() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("denied.txt");
        std::fs::write(&victim, b"safe").unwrap();
        let cfg = write_config(
            dir.path(),
            &format!(
                // Policy allows INSERT only; the JOB's REMOVE is denied.
                "CREATE POLICY p ALLOW INSERT;\n\
                 CREATE JOB cleanup EVERY '1h' DO REMOVE /local{} POLICY p;\n",
                victim.display()
            ),
        );
        let code = run_job(&req(JobAction::Run, cfg, "cleanup", true, true));
        assert_eq!(code, 1, "policy-denied plan aborts");
        assert!(victim.exists(), "atomic abort: nothing applied");
    }

    /// A missing job name is a clean error, not a panic.
    #[test]
    fn job_run_unknown_name_errors() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cfg = write_config(dir.path(), "CREATE POLICY p ALLOW REMOVE;\n");
        let code = run_job(&req(JobAction::Run, cfg, "absent", true, true));
        assert_eq!(code, 1);
    }

    /// `qfs job cron` emits the crontab line for the saved cadence (the individual counterpart of
    /// the managed `[triggers] crons` entry) — proves qfs EMITS a correct crontab line for a job.
    #[test]
    fn job_cron_emits_the_crontab_cadence() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cfg = write_config(
            dir.path(),
            "CREATE POLICY p ALLOW INSERT;\n\
             CREATE JOB nightly EVERY '6h' DO INSERT INTO /cf/r2/backups VALUES ('s') POLICY p;\n",
        );
        // emit_cron returns 0 and (covered by qfs-host's cron derivation) maps 6h → `0 */6 * * *`.
        assert_eq!(emit_cron(&cfg, "nightly"), 0);
        assert_eq!(emit_cron(&cfg, "absent"), 1, "unknown job is a clean error");
    }
}
