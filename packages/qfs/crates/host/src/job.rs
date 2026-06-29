//! Saved-JOB extraction for the **external scheduler** (t65, roadmap §4.3 / decision M revised):
//! project a booted `.qfs` config's `/server/jobs` rows into the owned [`JobSpec`]s an external
//! scheduler invokes.
//!
//! qfs is **not a scheduler** (decision M, revised). A `CREATE JOB <name> EVERY <interval> DO
//! <plan>` row is no longer fired by an internal daemon — it is a **saved named plan plus its
//! intended cadence**, metadata the *external* scheduler reads and runs:
//!
//! - **Individual / local** — OS `cron` runs `qfs job run <config> <name>` on the row's cadence.
//!   [`JobSpec::cron`] is the crontab line to drop into the host crontab (`qfs job cron` emits it).
//! - **Managed tier** — Cloudflare Cron Triggers fire the qfs Worker; the same cadence becomes the
//!   `[triggers] crons` entry the [`crate::wrangler`] generator emits.
//!
//! This module boots the config through the t30 `qfs_server::Runtime` (gated behind `host-daemon`,
//! like [`crate::from_server`] — `qfs-server` pulls tokio `signal`, no-wasm) and reads each
//! [`qfs_server::JobDef`]'s saved DO body (the canonical [`qfs_core::PlanSpec`] source the binary
//! rehydrates and commits through the SAME policy gate + IrreversibleGuard the CLI one-shot uses).
//! It NEVER fires anything itself — there is no daemon, no tick, no lease.

use std::path::Path;

use qfs_server::{JobDef, PolicyTable};

use crate::derive::cron_from_every;

/// One saved `/server/jobs` row, projected into owned, vendor-free strings — the **invokable
/// unit** an external scheduler drives. Carries the saved DO plan body (`plan_canonical`, the
/// canonical [`qfs_core::PlanSpec`] source the binary rehydrates + builds), the row's intended
/// cadence (`every` + the derived crontab `cron`), and the bound least-privilege `policy` handle
/// the externally-invoked commit is gated under (preserving the safety floor; an external trigger
/// decides *when*, never *whether*).
#[derive(Debug, Clone)]
pub struct JobSpec {
    /// The job name (the `/server/jobs` row key; the handle `qfs job run <config> <name>` uses).
    pub name: String,
    /// The raw `EVERY <interval>` cadence text (e.g. `6h`), as authored.
    pub every: String,
    /// The crontab line the cadence maps to (`cron_from_every`) — what OS `cron` and Cloudflare
    /// Cron Triggers schedule. (Both individual + managed read the SAME derived expression.)
    pub cron: String,
    /// The bound `POLICY <name>` handle the externally-invoked commit is gated under. `None` ⇒ no
    /// policy attached ⇒ fail-closed default-deny at run time (the same posture the retired daemon
    /// took — the safety floor is unchanged; only the *when* moved out of qfs).
    pub policy: Option<String>,
    /// The saved DO plan body as canonical [`qfs_core::PlanSpec`] source. The binary rehydrates it
    /// (`PlanSpec::from_canonical`, no re-parse), builds the plan, gates it, and commits once.
    pub plan_canonical: String,
}

/// The saved jobs of a booted config plus its live `/server/policies` table — everything an
/// external invocation needs to resolve a JOB's bound policy and gate its plan.
pub struct ConfigJobs {
    /// The `/server/jobs` rows, projected into owned [`JobSpec`]s.
    pub jobs: Vec<JobSpec>,
    /// The `/server/policies` table (`name → PolicyDef`) the bound `policy` handle resolves
    /// against (least privilege; the same gate the retired daemon enforced at fire time).
    pub policies: PolicyTable,
}

impl ConfigJobs {
    /// Find a saved job by name (the handle an external scheduler invokes it by).
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&JobSpec> {
        self.jobs.iter().find(|j| j.name == name)
    }
}

/// Boot a `.qfs` config (the in-memory parse→lower→COMMIT path, the same one
/// [`crate::from_server::bindings_from_config`] uses) and extract its saved `/server/jobs` rows +
/// policy table. The terminal binary calls this so it never names `qfs-server` directly (its
/// dep-allowlist stays the thin-entrypoint set the dep-direction guard pins); the `qfs-server`
/// coupling lives behind `host-daemon`.
///
/// # Errors
/// A secret-free, line-located error string on any read / parse / lower / commit failure.
pub fn jobs_from_config(config: &Path) -> Result<ConfigJobs, String> {
    let mut rt = qfs_server::Runtime::new();
    rt.boot(config).map_err(|e| format!("boot: {e}"))?;
    let state = rt.snapshot();
    let jobs = state.jobs.values().map(job_spec).collect();
    Ok(ConfigJobs {
        jobs,
        policies: state.policies,
    })
}

/// Project one [`JobDef`] into the owned [`JobSpec`] (cadence → crontab; saved DO body → canonical
/// source). No firing, no rehydration here — the binary owns the rehydrate + build + commit.
fn job_spec(def: &JobDef) -> JobSpec {
    JobSpec {
        name: def.name.clone(),
        every: def.every.clone(),
        cron: cron_from_every(&def.every),
        policy: def.policy.clone(),
        plan_canonical: def.plan.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a `.qfs` config to a tempfile and boot it.
    fn boot(src: &str) -> ConfigJobs {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(src.as_bytes()).expect("write config");
        jobs_from_config(f.path()).expect("boot config")
    }

    #[test]
    fn extracts_a_saved_job_with_its_crontab_and_policy() {
        // The JOB DEFINITION (/server/jobs) still works under externalization: it is a saved named
        // plan + cadence, NOT something qfs fires. We read it back and derive its crontab line.
        let cfg = boot(
            "CREATE POLICY p ALLOW INSERT;\n\
             CREATE JOB nightly EVERY '6h' DO INSERT INTO /cf/r2/backups VALUES ('snap') POLICY p;\n",
        );
        assert_eq!(cfg.jobs.len(), 1, "one saved JOB row");
        let job = cfg.find("nightly").expect("job by name");
        assert_eq!(job.every, "6h");
        // EVERY 6h → the crontab line OS cron / Cloudflare Cron Triggers schedule.
        assert_eq!(job.cron, "0 */6 * * *");
        assert_eq!(job.policy.as_deref(), Some("p"));
        assert!(
            !job.plan_canonical.is_empty(),
            "the saved DO body is carried as canonical PlanSpec source"
        );
        // The bound policy is resolvable against the extracted table (the gate the external
        // invocation runs under).
        assert!(cfg.policies.contains_key("p"), "policy table carried");
    }

    #[test]
    fn missing_job_is_none_not_a_panic() {
        let cfg = boot("CREATE POLICY p ALLOW INSERT;\n");
        assert!(cfg.find("absent").is_none());
        assert!(cfg.jobs.is_empty());
    }
}
