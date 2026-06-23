//! [`GitApplier`] — the git driver's synchronous apply leg (RFD §6), the lone impure seam the
//! introspective [`crate::GitDriver`] hands back via `applier()`, and the
//! [`cfs_runtime::SharedApplier`] the runtime's [`cfs_runtime::PlanApplierBridge`] drives under
//! `COMMIT`. It decodes a runtime [`EffectNode`] back into a [`GitEffect`] and applies it to a
//! mutable in-memory repository store:
//!
//! - **objects → refs → reflog** ordering (RFD §6): write the content-addressed objects first
//!   (idempotent — an existing oid is a no-op), then move the ref **only if** the compare-and-swap
//!   on the old oid holds (a stale old oid is rejected as a typed [`cfs_runtime::EffectError`]
//!   conflict — never clobbered), then append the recovery-oracle reflog entry. A partial failure
//!   (objects written, ref move rejected) is safe and re-runnable: the objects are GC-able and the
//!   ref is untouched.
//!
//! The store is behind a `Mutex` so the leg is usable through `&self` (the `SharedApplier`
//! statelessness contract — the mutation is the World, not an apply-hot-path accumulator). No
//! `gix` type crosses this boundary; no object bytes ever enter an error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cfs_plan::{AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use cfs_runtime::{EffectError, EffectOutput, SharedApplier};
use cfs_types::Value;

use crate::effect::GitEffect;
use crate::error::GitError;
use crate::objectdb::{frame_and_id, LooseObjectDb, ObjectDb, ObjectKind, Oid};
use crate::repo::ReflogEntry;

/// The mutable per-repo state the apply leg writes to: the loose objects, the refs, and the
/// reflog. Separate from the read-side [`crate::repo::Repo`] (which holds an `Arc<dyn ObjectDb>`
/// for reads) — the COMMIT apply path needs ownership to mutate.
#[derive(Default)]
pub struct RepoStore {
    /// The loose object database (oid → framed bytes), written by `WriteLooseObject`.
    pub db: LooseObjectDb,
    /// Ref name → current oid.
    pub refs: HashMap<String, Oid>,
    /// Ref name → append-only reflog (newest last).
    pub reflog: HashMap<String, Vec<ReflogEntry>>,
}

impl RepoStore {
    /// The current oid of a ref (the CAS comparand).
    #[must_use]
    pub fn ref_oid(&self, name: &str) -> Option<Oid> {
        self.refs.get(name).cloned()
    }

    /// The reflog for a ref, newest first (the recovery read).
    #[must_use]
    pub fn reflog(&self, name: &str) -> Vec<ReflogEntry> {
        self.reflog
            .get(name)
            .map(|v| v.iter().rev().cloned().collect())
            .unwrap_or_default()
    }
}

/// The synchronous git apply leg. Holds the per-repo mutable store behind a `Mutex` keyed by
/// repo name, so a plan over `/git/<repo>` mutates the addressed repo's objects/refs/reflog.
/// Cloneable (the store is behind an `Arc`).
#[derive(Clone, Default)]
pub struct GitApplier {
    stores: Arc<Mutex<HashMap<String, RepoStore>>>,
}

impl GitApplier {
    /// Build an applier with no repos. Repos are registered with [`GitApplier::with_store`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an initial store for a repo (the fixture seeds the starting refs/objects here).
    #[must_use]
    pub fn with_store(self, repo: impl Into<String>, store: RepoStore) -> Self {
        self.stores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(repo.into(), store);
        self
    }

    /// Read the current oid of a ref in a repo (used by tests + the recovery helper).
    #[must_use]
    pub fn ref_oid(&self, repo: &str, name: &str) -> Option<Oid> {
        self.stores
            .lock()
            .ok()?
            .get(repo)
            .and_then(|s| s.ref_oid(name))
    }

    /// Read a repo's reflog for a ref, newest first (the recovery oracle read).
    #[must_use]
    pub fn reflog(&self, repo: &str, name: &str) -> Vec<ReflogEntry> {
        self.stores
            .lock()
            .ok()
            .and_then(|s| s.get(repo).map(|s| s.reflog(name)))
            .unwrap_or_default()
    }

    /// The **recovery helper** (RFD §6): restore a ref to the prior oid recorded in its reflog —
    /// the inverse of a forced move that orphaned history. Forces the move back (the orphaned
    /// commit objects are still present, GC-able but live until then). Returns the restored oid.
    ///
    /// # Errors
    /// [`GitError`] if the repo/ref has no reflog history to recover from.
    pub fn recover_ref(&self, repo: &str, name: &str) -> Result<Oid, GitError> {
        let mut stores = lock_stores(&self.stores);
        let store = stores.get_mut(repo).ok_or_else(|| GitError::UnknownRepo {
            repo: repo.to_string(),
        })?;
        let prior = store
            .reflog(name)
            .first()
            .map(|e| e.old.clone())
            .ok_or_else(|| GitError::UnresolvedRef {
                reference: format!("{name}@reflog"),
            })?;
        let current = store.ref_oid(name).unwrap_or_else(Oid::zero);
        store.refs.insert(name.to_string(), prior.clone());
        store
            .reflog
            .entry(name.to_string())
            .or_default()
            .push(ReflogEntry {
                ref_name: name.to_string(),
                old: current,
                new: prior.clone(),
                who: "cfs-git recover".to_string(),
                message: "reset: recover prior oid from reflog".to_string(),
                time: 0,
            });
        Ok(prior)
    }

    /// Apply one decoded [`GitEffect`] to the addressed repo's store. The single place the World
    /// mutates.
    fn apply_effect(&self, repo: &str, effect: &GitEffect) -> Result<u64, GitError> {
        let mut stores = lock_stores(&self.stores);
        let store = stores.entry(repo.to_string()).or_default();
        match effect {
            GitEffect::WriteLooseObject { oid, kind, payload } => {
                // Content-addressed idempotency: writing an existing oid is a no-op.
                if store.db.contains(oid) {
                    return Ok(0);
                }
                let written = store.db.insert_object(*kind, payload);
                // Defensive: the planner-computed oid must equal the content address.
                if &written != oid {
                    return Err(GitError::Corrupt {
                        reason: "planned oid does not match content address".to_string(),
                    });
                }
                Ok(1)
            }
            GitEffect::UpdateRef {
                name,
                old,
                new,
                force,
            } => {
                let current = store.ref_oid(name);
                // Compare-and-swap on the old oid (optimistic concurrency). A `force` move skips
                // the equality check but still records the prior oid for reflog recovery.
                if !force {
                    match (old, &current) {
                        (Some(expected), Some(actual)) if expected != actual => {
                            return Err(GitError::RefCasConflict {
                                name: name.clone(),
                                expected: expected.as_str().to_string(),
                                actual: actual.as_str().to_string(),
                            });
                        }
                        // A creation (`old = None`) must find no existing ref.
                        (None, Some(actual)) => {
                            return Err(GitError::RefCasConflict {
                                name: name.clone(),
                                expected: "(none — creation)".to_string(),
                                actual: actual.as_str().to_string(),
                            });
                        }
                        // `old = Some` but the ref is absent: a stale expectation.
                        (Some(expected), None) => {
                            return Err(GitError::RefCasConflict {
                                name: name.clone(),
                                expected: expected.as_str().to_string(),
                                actual: "(none — ref absent)".to_string(),
                            });
                        }
                        _ => {}
                    }
                }
                store.refs.insert(name.clone(), new.clone());
                Ok(1)
            }
            GitEffect::WriteReflogEntry {
                name,
                old,
                new,
                who,
                message,
                time,
            } => {
                store
                    .reflog
                    .entry(name.clone())
                    .or_default()
                    .push(ReflogEntry {
                        ref_name: name.clone(),
                        old: old.clone(),
                        new: new.clone(),
                        who: who.clone(),
                        message: message.clone(),
                        time: *time,
                    });
                Ok(1)
            }
        }
    }

    /// Decode a runtime [`EffectNode`] into a [`GitEffect`] + the addressed repo, then apply it.
    fn apply_node(&self, node: &EffectNode) -> Result<u64, GitError> {
        let (repo, effect) = decode_node(node)?;
        self.apply_effect(&repo, &effect)
    }
}

/// Lock the per-repo store map, recovering the guard on poison (a poisoned mutex means a prior
/// apply panicked; the store is still a valid `HashMap`, so we recover rather than `expect`).
fn lock_stores(
    stores: &Mutex<HashMap<String, RepoStore>>,
) -> std::sync::MutexGuard<'_, HashMap<String, RepoStore>> {
    stores
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Decode a runtime [`EffectNode`] into the addressed `(repo, GitEffect)`. The effect rides in
/// the node's row args (a single row whose typed columns carry the effect fields) — the planner
/// encodes it this way so the pure plan crate stays git-agnostic.
fn decode_node(node: &EffectNode) -> Result<(String, GitEffect), GitError> {
    let path = node.target.path.as_str();
    let gp = crate::path::GitPath::parse(path)?;
    // Only INSERT (commit creation / object writes) and UPDATE (ref moves) reach the apply leg;
    // capability gating at parse time rejects the rest, but we backstop here (RFD defense in depth).
    match &node.kind {
        EffectKind::Insert | EffectKind::Update | EffectKind::Call(_) => {}
        other => {
            return Err(GitError::CapabilityDenied {
                path: path.to_string(),
                verb: static_kind_label(other),
            });
        }
    }
    let effect = effect_from_row(node)?;
    Ok((gp.repo, effect))
}

/// Read the `effect_kind` discriminator column + the per-kind columns from the node's first row.
fn effect_from_row(node: &EffectNode) -> Result<GitEffect, GitError> {
    let kind = text_col(node, "effect_kind").ok_or_else(|| GitError::MalformedEffect {
        verb: static_kind_label(&node.kind),
        path: node.target.path.as_str().to_string(),
        reason: "missing `effect_kind` column".to_string(),
    })?;
    match kind.as_str() {
        "write_object" => {
            let oid = Oid::parse(&req_text(node, "oid")?)?;
            let okind = parse_object_kind(&req_text(node, "object_kind")?)?;
            let payload = bytes_col(node, "payload").unwrap_or_default();
            Ok(GitEffect::WriteLooseObject {
                oid,
                kind: okind,
                payload,
            })
        }
        "update_ref" => {
            let name = req_text(node, "ref_name")?;
            let old = match text_col(node, "old_oid") {
                Some(s) if !s.is_empty() => Some(Oid::parse(&s)?),
                _ => None,
            };
            let new = Oid::parse(&req_text(node, "new_oid")?)?;
            let force = text_col(node, "force").as_deref() == Some("true");
            Ok(GitEffect::UpdateRef {
                name,
                old,
                new,
                force,
            })
        }
        "write_reflog" => Ok(GitEffect::WriteReflogEntry {
            name: req_text(node, "ref_name")?,
            old: Oid::parse(&req_text(node, "old_oid")?)?,
            new: Oid::parse(&req_text(node, "new_oid")?)?,
            who: text_col(node, "who").unwrap_or_default(),
            message: text_col(node, "message").unwrap_or_default(),
            time: int_col(node, "time").unwrap_or(0),
        }),
        other => Err(GitError::MalformedEffect {
            verb: static_kind_label(&node.kind),
            path: node.target.path.as_str().to_string(),
            reason: format!("unknown effect_kind `{other}`"),
        }),
    }
}

fn parse_object_kind(s: &str) -> Result<ObjectKind, GitError> {
    match s {
        "blob" => Ok(ObjectKind::Blob),
        "tree" => Ok(ObjectKind::Tree),
        "commit" => Ok(ObjectKind::Commit),
        "tag" => Ok(ObjectKind::Tag),
        other => Err(GitError::MalformedEffect {
            verb: "WRITE_OBJECT",
            path: String::new(),
            reason: format!("unknown object kind `{other}`"),
        }),
    }
}

/// Compute the content-addressed oid for an object payload (re-exported helper for plan building).
#[must_use]
pub fn object_oid(kind: ObjectKind, payload: &[u8]) -> Oid {
    frame_and_id(kind, payload).0
}

fn col_index(node: &EffectNode, name: &str) -> Option<usize> {
    node.args
        .schema
        .columns
        .iter()
        .position(|c| c.name.as_str() == name)
}

fn text_col(node: &EffectNode, name: &str) -> Option<String> {
    let idx = col_index(node, name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) => Some(t.clone()),
        _ => None,
    }
}

fn req_text(node: &EffectNode, name: &str) -> Result<String, GitError> {
    text_col(node, name)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GitError::MalformedEffect {
            verb: static_kind_label(&node.kind),
            path: node.target.path.as_str().to_string(),
            reason: format!("missing `{name}` column"),
        })
}

fn int_col(node: &EffectNode, name: &str) -> Option<i64> {
    let idx = col_index(node, name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Int(i) | Value::Timestamp(i)) => Some(*i),
        _ => None,
    }
}

fn bytes_col(node: &EffectNode, name: &str) -> Option<Vec<u8>> {
    let idx = col_index(node, name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        Some(Value::Text(t)) => Some(t.clone().into_bytes()),
        _ => None,
    }
}

fn static_kind_label(kind: &EffectKind) -> &'static str {
    match kind {
        EffectKind::Read => "READ",
        EffectKind::List => "LIST",
        EffectKind::Insert => "INSERT",
        EffectKind::Upsert => "UPSERT",
        EffectKind::Update => "UPDATE",
        EffectKind::Remove => "REMOVE",
        EffectKind::Call(_) => "CALL",
        _ => "WRITE",
    }
}

/// Map a [`GitError`] to a runtime [`EffectError`]: a CAS conflict becomes a typed `Conflict`
/// (carrying the version the world actually holds, so the txn bridge surfaces the real
/// coordinate); a merge conflict / malformed effect is terminal; corruption is terminal. Nothing
/// here is retryable (a git apply is deterministic).
fn to_effect_error(e: GitError) -> EffectError {
    match e {
        GitError::RefCasConflict { actual, .. } => EffectError::conflict(actual),
        other => EffectError::terminal(other.to_string()),
    }
}

impl SharedApplier for GitApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self.apply_node(node).map_err(to_effect_error)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for GitApplier {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
