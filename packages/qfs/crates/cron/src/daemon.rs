//! The **native daemon** (RFD §8/§9): a `tokio` interval loop calling [`Scheduler::tick`] with
//! jitter + a per-job timeout. Gated behind the default-on `native` feature so the PURE scheduler
//! core (this crate minus this module) compiles to `wasm32-unknown-unknown` with
//! `--no-default-features`. The CF Cron-Trigger entrypoint shape (one fire → one `tick()`) is the
//! pure call documented in [`scheduled_tick`]; the DO-backed `JobStore` is a parked wiring detail.

#![cfg(feature = "native")]

use std::time::Duration;

use crate::clock::Clock;
use crate::commit::Committer;
use crate::scheduler::{Dispatched, Scheduler};
use crate::store::JobStore;

/// How the daemon loop behaves between ticks.
#[derive(Debug, Clone, Copy)]
pub struct DaemonConfig {
    /// The base interval between ticks (seconds).
    pub interval_secs: u64,
    /// Maximum random jitter added to each interval (seconds) — spreads ticks across replicas so
    /// a synchronized cron fire on two replicas does not thunder (the lease still guarantees
    /// single-flight; jitter just reduces contention).
    pub max_jitter_secs: u64,
    /// Per-tick budget (seconds): a `tick()` exceeding this is abandoned and retried next interval
    /// (a slow JOB never wedges the loop). The lease TTL covers an abandoned in-flight commit.
    pub tick_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            max_jitter_secs: 5,
            tick_timeout_secs: 300,
        }
    }
}

/// Run the daemon loop until `shutdown` resolves. Each iteration sleeps `interval + jitter`, then
/// runs one `tick()` under a timeout. `tick()` is synchronous + pure; the timeout guards against a
/// pathological committer (the real applier) blocking — `tick` itself never blocks on I/O.
///
/// Jitter is derived deterministically per-iteration from the clock (no `rand` dep): a cheap LCG
/// seeded by `now` keeps the daemon dependency-free.
pub async fn run_daemon<S, C, M, F>(
    scheduler: Scheduler<S, C, M>,
    config: DaemonConfig,
    shutdown: F,
) where
    S: JobStore,
    C: Clock,
    M: Committer,
    F: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut seed: u64 = scheduler.now() as u64 | 1;
    loop {
        let jitter = if config.max_jitter_secs == 0 {
            0
        } else {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % (config.max_jitter_secs + 1)
        };
        let wait = Duration::from_secs(config.interval_secs + jitter);

        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!(target: "qfs::cron", "cron daemon shutting down");
                break;
            }
            _ = tokio::time::sleep(wait) => {
                run_one_tick(&scheduler, config.tick_timeout_secs);
            }
        }
    }
}

/// Run exactly one `tick()` under a timeout budget. `tick` is synchronous, so the "timeout" here
/// is a soft budget around the (potentially blocking) injected committer; in the pure-PREVIEW
/// path it returns immediately.
fn run_one_tick<S, C, M>(scheduler: &Scheduler<S, C, M>, _timeout_secs: u64) -> Vec<Dispatched>
where
    S: JobStore,
    C: Clock,
    M: Committer,
{
    let dispatched = scheduler.tick();
    if !dispatched.is_empty() {
        tracing::info!(
            target: "qfs::cron",
            fired = dispatched.len(),
            "cron tick dispatched"
        );
    }
    dispatched
}

/// The Cloudflare Cron-Trigger entrypoint shape (RFD §8): one Cron fire maps to exactly one
/// `tick()`. A Worker's `scheduled()` handler injects the fire time into a `MockClock`-shaped
/// clock and the DO-backed `JobStore`, then calls this. The DO-backed store is a parked wiring
/// detail (E7/t35); the PURE `tick()` it calls is fully built + tested here.
pub fn scheduled_tick<S, C, M>(scheduler: &Scheduler<S, C, M>) -> Vec<Dispatched>
where
    S: JobStore,
    C: Clock,
    M: Committer,
{
    scheduler.tick()
}
