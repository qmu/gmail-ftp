//! The effect-plan **interpreter** (RFD-0001 §3 `COMMIT : Plan -> World`, §6 runtime) — the
//! sole impure stage of cfs. It walks a [`Plan`] in topological frontiers, coalesces each
//! frontier into per-`(driver, kind)` batches, dispatches independent batches concurrently
//! under two-level concurrency caps, threads per-leg timeouts + bounded retries (never on
//! irreversible legs), re-checks capability gating at apply time, and produces the
//! [`Outcome`] ledger. PREVIEW performs **no** execution.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cfs_plan::{NodeId, Plan};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;

use crate::batch::{coalesce, BatchGroup};
use crate::caps::{CapabilitySet, ConcurrencyLimits, RetryPolicy};
use crate::driver::{ApplyCx, DriverRegistry};
use crate::error::{ApplyError, EffectError};
use crate::outcome::{EffectOutput, LedgerEntry, LegStatus, Outcome};
use crate::schedule::{Frontier, Ready};

/// The effect interpreter (RFD §3/§6). Holds the apply-time [`DriverRegistry`], the
/// concurrency caps, and the retry policy; [`Interpreter::commit`] runs a plan.
#[derive(Clone)]
pub struct Interpreter {
    drivers: Arc<DriverRegistry>,
    limits: ConcurrencyLimits,
    retry: RetryPolicy,
}

impl Interpreter {
    /// Construct an interpreter over a driver registry with the given concurrency caps and
    /// retry policy.
    #[must_use]
    pub fn new(drivers: DriverRegistry, limits: ConcurrencyLimits, retry: RetryPolicy) -> Self {
        Self {
            drivers: Arc::new(drivers),
            limits,
            retry,
        }
    }

    /// Borrow the underlying driver registry — the seam the t11 transactional commit
    /// (`crate::txn`) resolves async drivers through, kept crate-private so the field stays
    /// encapsulated.
    pub(crate) fn drivers_arc(&self) -> &DriverRegistry {
        &self.drivers
    }

    /// Construct an interpreter with default limits/retry.
    #[must_use]
    pub fn with_defaults(drivers: DriverRegistry) -> Self {
        Self::new(
            drivers,
            ConcurrencyLimits::default(),
            RetryPolicy::default(),
        )
    }

    /// **PREVIEW**: walk the plan in topological order and produce a ledger that records the
    /// disposition each node *would* have, without calling any driver. Skips are computed
    /// exactly as commit would; every other node is reported as a zero-duration "applied"
    /// only in the sense of plan order — no I/O occurs. Returns [`ApplyError::InvalidPlan`]
    /// for a cyclic plan.
    ///
    /// # Errors
    /// [`ApplyError::InvalidPlan`] if the plan is not a DAG.
    pub fn preview(&self, plan: &Plan, caps: &CapabilitySet) -> Result<Outcome, ApplyError> {
        // Preview is synchronous and pure (no driver calls), but it drives the SAME `Frontier`
        // that `commit` uses so the t09 skip/topo semantics have a single source of truth and
        // cannot drift between the dry run and the real apply. The only difference from commit
        // is that a would-run node is recorded as a zero-duration "applied" and immediately
        // marked `complete` (no driver dispatch), while a capability denial is recorded as
        // failed and marked `fail` (so its dependents surface as skips, exactly as in commit).
        let order = cfs_plan::topo_order(plan).ok_or(ApplyError::InvalidPlan)?;
        let mut frontier = Frontier::new(plan).ok_or(ApplyError::InvalidPlan)?;
        let mut ledger_by_id: HashMap<NodeId, LedgerEntry> = HashMap::new();

        loop {
            let ready = frontier.ready();
            if ready.is_empty() {
                // Either every node has settled, or the frontier is genuinely stuck. With no
                // in-flight work in preview, an empty ready-set means the walk is finished.
                break;
            }
            for r in ready {
                match r {
                    Ready::Skip { id, cause } => {
                        // Same skip accounting as commit (Frontier already tainted + relaxed).
                        ledger_by_id.insert(id, skip_entry(plan, id, cause));
                    }
                    Ready::Run(id) => {
                        let Some(node) = plan.node(id) else {
                            return Err(ApplyError::UnknownNode(id));
                        };
                        if caps.allows(&node.target, &node.kind) {
                            // Would-apply leg: zero-duration "applied" estimate, no driver call.
                            ledger_by_id.insert(
                                id,
                                LedgerEntry {
                                    id,
                                    driver: node.target.driver.clone(),
                                    kind: node.kind.clone(),
                                    irreversible: node.irreversible,
                                    status: LegStatus::Applied {
                                        affected: 0,
                                        attempts: 0,
                                    },
                                    duration: Duration::ZERO,
                                },
                            );
                            frontier.complete(id);
                        } else {
                            // A preview still surfaces a capability denial (so the dry run
                            // warns), and its dependents are then shown as skipped — but no
                            // driver is touched. `fail` taints it so the Frontier propagates
                            // the skip identically to commit.
                            ledger_by_id.insert(
                                id,
                                LedgerEntry {
                                    id,
                                    driver: node.target.driver.clone(),
                                    kind: node.kind.clone(),
                                    irreversible: node.irreversible,
                                    status: LegStatus::Failed {
                                        error: EffectError::CapabilityDenied {
                                            driver: node.target.driver.clone(),
                                            verb: node.kind.label().to_string(),
                                        },
                                        attempts: 0,
                                    },
                                    duration: Duration::ZERO,
                                },
                            );
                            frontier.fail(id);
                        }
                    }
                }
            }
        }
        Ok(self.assemble(&order, ledger_by_id))
    }

    /// **COMMIT** (RFD §3 the only impure op): apply the plan against the World. Walks the
    /// DAG in frontiers, auto-batches each frontier per `(driver, kind)`, runs independent
    /// batches concurrently under the global + per-driver caps, retries retryable
    /// non-irreversible legs up to the bound, and records every leg in the [`Outcome`]
    /// ledger. A failed leg taints its transitive dependents (they are skipped), while
    /// in-flight batches are drained.
    ///
    /// # Errors
    /// [`ApplyError::InvalidPlan`] if the plan is not a DAG (nothing is applied).
    pub async fn commit(&self, plan: Plan, caps: &CapabilitySet) -> Result<Outcome, ApplyError> {
        let order = cfs_plan::topo_order(&plan).ok_or(ApplyError::InvalidPlan)?;
        let mut frontier = Frontier::new(&plan).ok_or(ApplyError::InvalidPlan)?;
        let mut ledger_by_id: HashMap<NodeId, LedgerEntry> = HashMap::new();

        let global = Arc::new(Semaphore::new(self.limits.global));
        let mut per_driver: HashMap<cfs_types::DriverId, Arc<Semaphore>> = HashMap::new();

        // In-flight batch dispatches; each resolves to the per-effect results of one group.
        // A group is pushed here ONLY after its global permit has been acquired, so the number
        // of resident `run_group` futures (each owning a cloned `RowBatch`) is bounded by
        // `limits.global` — a wide frontier cannot materialise unboundedly many pending
        // futures (ticket: "must not spawn unbounded tasks / cannot exhaust memory").
        let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
        // Coalesced groups that are ready to run but have not yet acquired a global permit.
        // They hold no future and no extra clones beyond the `BatchGroup` itself; admission is
        // gated below so resident futures never exceed `limits.global`.
        let mut admit_queue: std::collections::VecDeque<BatchGroup> =
            std::collections::VecDeque::new();

        loop {
            // 1) Materialise the WHOLE current ready-set, then group it across the whole
            //    frontier (so N independent same-key effects coalesce into one batch).
            let ready = frontier.ready();
            // Track whether this iteration made any progress (surfaced a node, spawned a
            // group, or settled a denial) so we know when an empty in-flight set means the
            // walk is genuinely stuck vs. just needs another pass to surface skips.
            let mut progressed = !ready.is_empty();
            let mut run_nodes: Vec<&cfs_plan::EffectNode> = Vec::new();
            for r in ready {
                match r {
                    Ready::Skip { id, cause } => {
                        let entry = skip_entry(&plan, id, cause);
                        ledger_by_id.insert(id, entry);
                    }
                    Ready::Run(id) => {
                        if let Some(node) = plan.node(id) {
                            run_nodes.push(node);
                        }
                    }
                }
            }

            // 2) Capability re-check + coalesce. A denied effect fails immediately (no
            //    dispatch); its dependents will be skipped on the next ready() pass.
            let mut runnable: Vec<&cfs_plan::EffectNode> = Vec::new();
            for node in run_nodes {
                if caps.allows(&node.target, &node.kind) {
                    runnable.push(node);
                } else {
                    let entry = LedgerEntry {
                        id: node.id,
                        driver: node.target.driver.clone(),
                        kind: node.kind.clone(),
                        irreversible: node.irreversible,
                        status: LegStatus::Failed {
                            error: EffectError::CapabilityDenied {
                                driver: node.target.driver.clone(),
                                verb: node.kind.label().to_string(),
                            },
                            attempts: 0,
                        },
                        duration: Duration::ZERO,
                    };
                    ledger_by_id.insert(node.id, entry);
                    frontier.fail(node.id);
                    progressed = true;
                }
            }

            // 3) Enqueue this frontier's coalesced groups for *admission*. Coalescing still
            //    runs over the whole ready-set (N+1 → 1 preserved); the groups simply wait in
            //    `admit_queue` until a global permit frees up rather than each spawning a
            //    future eagerly.
            for group in coalesce(&runnable) {
                progressed = true;
                admit_queue.push_back(group);
            }

            // 4) Admit as many queued groups as the global cap allows *right now*. Each
            //    admitted group carries its already-acquired owned global permit into
            //    `run_group`, so resident futures are bounded by `limits.global`. We never
            //    block here: `try_acquire_owned` only takes free permits, and the loop's
            //    await below frees one as each in-flight group completes, re-admitting the
            //    next queued group on the following pass.
            while !admit_queue.is_empty() {
                let Ok(global_permit) = global.clone().try_acquire_owned() else {
                    break;
                };
                // Non-empty checked above; a permit is in hand.
                let Some(group) = admit_queue.pop_front() else {
                    break;
                };
                let driver_id = group.key.driver.clone();
                let driver = self.drivers.get(&driver_id);
                let per = per_driver
                    .entry(driver_id.clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(self.limits.per_driver)))
                    .clone();
                let retry = self.retry;
                in_flight.push(run_group(group, driver, global_permit, per, retry));
            }

            // 5) Termination: the walk is over once every node has settled and nothing is
            //    queued or in flight.
            if frontier.is_done() && in_flight.is_empty() && admit_queue.is_empty() {
                break;
            }

            // If nothing is in flight, decide whether to loop again (more nodes became ready
            // this pass — e.g. skips to surface, or queued groups awaiting admission) or
            // break. Breaking only when the iteration made no progress AND nothing is in
            // flight AND nothing is queued guards against an infinite spin on a malformed
            // plan while still draining newly-ready skip frontiers and admission backlog.
            if in_flight.is_empty() {
                if progressed || !admit_queue.is_empty() {
                    continue;
                }
                break;
            }

            // 6) Await the next completed group, fold its results into the ledger + frontier,
            //    then loop to surface newly-unblocked nodes (auto-advancing the frontier) and
            //    re-admit queued groups against the just-freed global permit.
            if let Some(results) = in_flight.next().await {
                for (entry, ok) in results {
                    let id = entry.id;
                    ledger_by_id.insert(id, entry);
                    if ok {
                        frontier.complete(id);
                    } else {
                        frontier.fail(id);
                    }
                }
            }
        }

        Ok(self.assemble(&order, ledger_by_id))
    }

    /// Assemble the final ledger in stable topological order (regardless of the wall-clock
    /// completion interleaving), so the [`Outcome`] is deterministic and golden-testable.
    fn assemble(&self, order: &[NodeId], mut by_id: HashMap<NodeId, LedgerEntry>) -> Outcome {
        let mut ledger = Vec::with_capacity(order.len());
        for id in order {
            if let Some(entry) = by_id.remove(id) {
                ledger.push(entry);
            }
        }
        Outcome { ledger }
    }
}

/// Build a "skipped" ledger entry for `id` whose dependency `cause` failed.
fn skip_entry(plan: &Plan, id: NodeId, cause: NodeId) -> LedgerEntry {
    let (driver, kind, irreversible) = plan
        .node(id)
        .map(|n| (n.target.driver.clone(), n.kind.clone(), n.irreversible))
        .unwrap_or_else(|| {
            (
                cfs_types::DriverId::new(""),
                cfs_plan::EffectKind::Read,
                false,
            )
        });
    LedgerEntry {
        id,
        driver,
        kind,
        irreversible,
        status: LegStatus::Skipped { cause },
        duration: Duration::ZERO,
    }
}

/// Dispatch one coalesced [`BatchGroup`] to its driver under the two-level concurrency caps,
/// applying per-leg timeout + bounded retry, and return one `(LedgerEntry, applied_ok)` per
/// effect in the group. The global permit is acquired by the driver loop *before* this future
/// is constructed (bounding the count of resident futures to `limits.global`) and moved in
/// here as `global_permit`; the per-driver permit is acquired here so blast radius (global) is
/// bounded while upstream rate limits (per-driver) are respected.
async fn run_group(
    group: BatchGroup,
    driver: Option<Arc<dyn crate::driver::ApplyDriver>>,
    global_permit: tokio::sync::OwnedSemaphorePermit,
    per_driver: Arc<Semaphore>,
    retry: RetryPolicy,
) -> Vec<(LedgerEntry, bool)> {
    // Backpressure: hold both permits for the lifetime of the driver call. The global permit
    // arrives already acquired (admission gate); the per-driver permit `acquire_owned` cannot
    // error unless the semaphore is closed (we never close it), so a failure degrades to "no
    // extra per-driver limit" rather than a panic. The global permit is released on drop.
    let _g = global_permit;
    let _p = per_driver.acquire_owned().await.ok();

    let span = tracing::info_span!(
        "apply_batch",
        driver = %group.key.driver.as_str(),
        kind = %group.key.kind_label,
        size = group.inputs.len()
    );
    let _enter = span.enter();

    let started = Instant::now();
    let Some(driver) = driver else {
        // No driver registered for this id — every leg in the group fails terminally.
        let elapsed = started.elapsed();
        return group
            .inputs
            .iter()
            .map(|input| {
                let entry = failed_entry(
                    &group,
                    input,
                    EffectError::Terminal {
                        reason: format!("no driver registered for `{}`", group.key.driver.as_str()),
                    },
                    1,
                    elapsed,
                );
                (entry, false)
            })
            .collect();
    };

    // Attempt loop with bounded retry. Retries re-dispatch the WHOLE group, but only legs
    // that failed-retryable-and-reversible are re-attempted; succeeded/terminal/irreversible
    // legs are pinned to their first outcome. This keeps batching while honouring per-leg
    // retry semantics.
    let mut pending: Vec<usize> = (0..group.inputs.len()).collect();
    let mut outcomes: Vec<Option<Result<EffectOutput, EffectError>>> =
        (0..group.inputs.len()).map(|_| None).collect();
    let mut attempts: Vec<u32> = vec![0; group.inputs.len()];

    let mut attempt = 0u32;
    while !pending.is_empty() {
        attempt += 1;
        let last = attempt >= retry.max_attempts;
        let cx = ApplyCx { last_attempt: last };
        let subset: Vec<crate::driver::EffectInput> =
            pending.iter().map(|&i| group.inputs[i].clone()).collect();

        let results = dispatch_with_timeout(&*driver, &group, &subset, &cx, retry).await;

        let mut next_pending: Vec<usize> = Vec::new();
        for (slot, res) in pending.iter().zip(results) {
            attempts[*slot] = attempt;
            let irreversible = group.inputs[*slot].irreversible;
            match &res {
                Err(e) if e.is_retryable() && !irreversible && !last => {
                    // Retry this leg on the next attempt; do not pin its outcome yet.
                    next_pending.push(*slot);
                }
                _ => {
                    outcomes[*slot] = Some(res);
                }
            }
        }
        pending = next_pending;
    }

    let elapsed = started.elapsed();
    group
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            let n = attempts[i].max(1);
            match outcomes[i].take() {
                Some(Ok(out)) => (applied_entry(&group, input, out.affected, n, elapsed), true),
                Some(Err(err)) => (failed_entry(&group, input, err, n, elapsed), false),
                None => (
                    failed_entry(
                        &group,
                        input,
                        EffectError::terminal("driver returned no result for effect"),
                        n,
                        elapsed,
                    ),
                    false,
                ),
            }
        })
        .collect()
}

/// Invoke the driver's batch entrypoint, wrapping it in the per-leg timeout if configured.
/// A timeout maps **every** leg of the subset to [`EffectError::TimedOut`] (the batch call
/// did not return in time; the runtime cannot tell which legs landed, so all are reported as
/// timed out and only reversible ones are retried).
async fn dispatch_with_timeout(
    driver: &dyn crate::driver::ApplyDriver,
    group: &BatchGroup,
    subset: &[crate::driver::EffectInput],
    cx: &ApplyCx,
    retry: RetryPolicy,
) -> Vec<Result<EffectOutput, EffectError>> {
    let call = driver.apply_batch(group.kind.clone(), subset, cx);
    match retry.timeout_millis {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), call).await {
            Ok(results) => normalise_len(results, subset),
            Err(_) => subset
                .iter()
                .map(|_| Err(EffectError::TimedOut { millis: ms }))
                .collect(),
        },
        None => normalise_len(call.await, subset),
    }
}

/// Defensively align a driver's result vector to the subset length: a well-behaved driver
/// returns exactly one result per input, but a buggy one is coerced (extra results dropped,
/// missing ones filled with a terminal error) rather than panicking — the lib stays
/// panic-free.
fn normalise_len(
    mut results: Vec<Result<EffectOutput, EffectError>>,
    subset: &[crate::driver::EffectInput],
) -> Vec<Result<EffectOutput, EffectError>> {
    if results.len() == subset.len() {
        return results;
    }
    results.truncate(subset.len());
    while results.len() < subset.len() {
        results.push(Err(EffectError::terminal(
            "driver returned fewer results than inputs",
        )));
    }
    results
}

fn applied_entry(
    group: &BatchGroup,
    input: &crate::driver::EffectInput,
    affected: u64,
    attempts: u32,
    duration: Duration,
) -> LedgerEntry {
    LedgerEntry {
        id: input.id,
        driver: group.key.driver.clone(),
        kind: input.kind.clone(),
        irreversible: input.irreversible,
        status: LegStatus::Applied { affected, attempts },
        duration,
    }
}

fn failed_entry(
    group: &BatchGroup,
    input: &crate::driver::EffectInput,
    error: EffectError,
    attempts: u32,
    duration: Duration,
) -> LedgerEntry {
    LedgerEntry {
        id: input.id,
        driver: group.key.driver.clone(),
        kind: input.kind.clone(),
        irreversible: input.irreversible,
        status: LegStatus::Failed { error, attempts },
        duration,
    }
}
