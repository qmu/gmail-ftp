//! The `/sys` administration composition root (ticket t53): the System-DB-backed [`SysBackend`]
//! implementation + the async [`SysReadDriver`] read facet, both hosted in the **`qfs` binary
//! crate**.
//!
//! ## Why the backend lives in the binary (not the leaf driver crate)
//! `qfs-driver-sys` is the vendor-free administration driver (the pure introspective half + the
//! `SysApplier` over the `SysBackend` seam) and is a **`qfs-runtime` consumer**, so the
//! dep-direction guard requires it to be a **leaf** — only the terminal `qfs` binary may depend
//! onto it. The binary IS that leaf and the ONE place that opens a real DB path (decision F), so
//! the real `rusqlite` reads/writes over the System DB (and the Project DB's connection registry)
//! dead-end here, exactly like the SQL driver's `SqliteBackend`. No `rusqlite` type crosses the
//! `SysBackend` boundary (owned qfs DTOs only).
//!
//! ## Safety floor (roadmap §3.2 / §4.6)
//! - `/sys/connections` reads the connection REGISTRY (`secret_store`'s `driver`/`connection`/
//!   `created_at` columns) — names + metadata only. The `nonce`/`ciphertext` columns are NEVER
//!   selected, so no secret material can surface (the vault is read only by `qfs-secrets`).
//! - `INSERT INTO /sys/policies` lands a grant row **and** appends a t76 audit row in ONE
//!   transaction — a torn write can never leave a policy un-audited (administration observes
//!   itself). Because the `/sys` legs self-audit at the source of truth, the CLI commit path's
//!   best-effort emitter SKIPS `/sys` legs (see `commit.rs`) so the chain is not double-written.
//!
//! ## Authorization (flagged, not baked in)
//! `/sys/*` writes are high-privilege. They are gated by the SAME default-deny policy engine as
//! any other driver (the path is the authorization subject). Until the super-admin vs.
//! project-admin split is settled (roadmap §3.4), the binary wires this loopback / local-CLI
//! super-admin only — the split is recorded as an open decision rather than baked into a model.

use std::sync::{Arc, Mutex};

use qfs_core::{CfsError, RowBatch};
use qfs_driver_sys::{node_for_path, sys_node_schema, SysBackend, SysError, SysNode};
use qfs_exec::ReadDriver;
use qfs_pushdown::ScanNode;
use qfs_store::audit::{AuditEvent, ChainHead, GENESIS_PREV_HASH};
use qfs_types::{Row, Value};
use rusqlite::{Connection, OptionalExtension};

/// The bounded audit live-tail cap (mirrors `crate::audit::TAIL_CAP`) — `/sys` mutations append
/// to the same t76 chain and trim to this many rows.
const TAIL_CAP: i64 = 256;

/// The acting principal recorded on a `/sys` mutation's audit event — a label, never a credential
/// (t76 / §4.6). The local CLI invocation is the actor today; a request-derived identity replaces
/// this once the super-admin session model lands.
const ACTOR_CLI: &str = "cli";

/// The acting principal recorded when a PROVIDER WEBHOOK (t67) updates a billing plan — a label, not
/// a credential. The provider's signing secret was HMAC-verified upstream (`qfs-watchtower`) and
/// never reaches here; this names the actor as the payment provider, not a human operator.
const ACTOR_PROVIDER: &str = "provider";

/// The System-DB-backed [`SysBackend`]: the real rusqlite reads/writes over the System DB (and the
/// Project DB's connection registry). Each connection is held behind a `Mutex` (rusqlite is
/// `!Sync`; the mutex provides the `Send + Sync` the trait requires).
pub struct SystemDbBackend {
    /// The System DB (users / projects / audit_tail / sys_policies + the audit chain head).
    system: Mutex<Connection>,
    /// The Project DB connection registry (`secret_store`), if a Project DB resolved. `None`
    /// leaves `/sys/connections` empty rather than failing (best-effort, like the audit emitter).
    project: Option<Mutex<Connection>>,
}

impl SystemDbBackend {
    /// Build a backend over already-migrated connections (the test + composition seam).
    #[must_use]
    pub fn new(system: Connection, project: Option<Connection>) -> Self {
        Self {
            system: Mutex::new(system),
            project: project.map(Mutex::new),
        }
    }

    /// Open the default System DB (+ best-effort Project DB) and build the backend. Returns `None`
    /// when no config home resolves (HOME/XDG unset) — the `/sys` surface is simply not wired
    /// rather than failing the CLI (the same best-effort posture as the audit emitter).
    #[must_use]
    pub fn open_default() -> Option<Self> {
        let system = match crate::store::open_system_db() {
            Ok(Some(sys)) => sys.into_db().into_connection(),
            _ => return None,
        };
        // The connection registry lives in the Project DB; a missing/locked one leaves
        // /sys/connections empty (never a failure).
        let project = match crate::store::open_project_db() {
            Ok(Some(proj)) => Some(proj.into_db().into_connection()),
            _ => None,
        };
        Some(Self::new(system, project))
    }
}

impl SysBackend for SystemDbBackend {
    fn scan(&self, node: SysNode) -> Result<RowBatch, SysError> {
        let schema = sys_node_schema(node);
        let rows = match node {
            SysNode::Users => self.scan_system(
                "SELECT id, primary_email, status, created_at FROM users ORDER BY id",
                |r| {
                    Ok(Row::new(vec![
                        int(r, 0)?,
                        text(r, 1)?,
                        text(r, 2)?,
                        nullable_text(r, 3)?,
                    ]))
                },
            )?,
            SysNode::Projects => self.scan_system(
                "SELECT id, slug, created_at FROM projects ORDER BY id",
                |r| {
                    Ok(Row::new(vec![
                        int(r, 0)?,
                        text(r, 1)?,
                        nullable_text(r, 2)?,
                    ]))
                },
            )?,
            SysNode::Audit => self.scan_system(
                "SELECT seq, actor, connection, verb, path, committed, ts \
                 FROM audit_tail ORDER BY seq",
                |r| {
                    Ok(Row::new(vec![
                        int(r, 0)?,
                        text(r, 1)?,
                        text(r, 2)?,
                        text(r, 3)?,
                        text(r, 4)?,
                        Value::Bool(r.get::<_, i64>(5)? != 0),
                        text(r, 6)?,
                    ]))
                },
            )?,
            // Names + metadata ONLY — `nonce`/`ciphertext` are NEVER selected (the redaction
            // contract, §3.2). The registry is the Project DB's `secret_store`.
            SysNode::Connections => self.scan_project(
                "SELECT driver, connection, created_at FROM secret_store \
                 ORDER BY driver, connection",
                |r| {
                    Ok(Row::new(vec![
                        text(r, 0)?,
                        text(r, 1)?,
                        nullable_text(r, 2)?,
                    ]))
                },
            )?,
            // t100020 (the CONNECT model): the DEFINED-PATH binding registry, from the Project DB.
            // Metadata only — `secret_ref` is a REFERENCE (`env:`/`vault:`), never a secret value.
            SysNode::Paths => self.scan_project(
                "SELECT path, driver_id, at_locator, secret_ref, alias_of, host, account, \
                        created_at \
                 FROM path_binding ORDER BY path",
                |r| {
                    Ok(Row::new(vec![
                        text(r, 0)?,
                        nullable_text(r, 1)?,
                        nullable_text(r, 2)?,
                        nullable_text(r, 3)?,
                        nullable_text(r, 4)?,
                        text(r, 5)?,
                        nullable_text(r, 6)?,
                        nullable_text(r, 7)?,
                    ]))
                },
            )?,
            SysNode::Policies => self.scan_system(
                "SELECT name, allow, target, created_at FROM sys_policies ORDER BY id",
                |r| {
                    Ok(Row::new(vec![
                        text(r, 0)?,
                        nullable_text(r, 1)?,
                        nullable_text(r, 2)?,
                        nullable_text(r, 3)?,
                    ]))
                },
            )?,
            // t77: the telemetry counter live view — read the in-process metrics registry (NOT the
            // System DB; qfs emits + does not store the stream, decision V). Metadata only:
            // instrument name + kind + integer counter value.
            SysNode::Metrics => crate::telemetry::metrics_snapshot()
                .into_iter()
                .map(|m| {
                    #[allow(clippy::cast_possible_truncation)]
                    Row::new(vec![
                        Value::Text(m.name),
                        Value::Text(m.kind.as_str().to_string()),
                        Value::Int(m.value as i64),
                    ])
                })
                .collect(),
            // t59: the deployment settings key/value (the safety-mode home). Metadata only.
            SysNode::Settings => self.scan_system(
                "SELECT key, value, updated_at FROM sys_settings ORDER BY key",
                |r| {
                    Ok(Row::new(vec![
                        text(r, 0)?,
                        text(r, 1)?,
                        nullable_text(r, 2)?,
                    ]))
                },
            )?,
            // t67: the per-team billing plan (the entitlement gate's authority). Metadata only —
            // team id + tier/status labels + period end. NEVER a payment secret (the schema has no
            // column for one; the provider keys live envelope-encrypted in the vault).
            SysNode::Billing => self.scan_system(
                "SELECT team_id, tier, status, current_period_end, updated_at \
                 FROM billing_subscriptions ORDER BY team_id",
                |r| {
                    Ok(Row::new(vec![
                        text(r, 0)?,
                        text(r, 1)?,
                        text(r, 2)?,
                        nullable_text(r, 3)?,
                        nullable_text(r, 4)?,
                    ]))
                },
            )?,
        };
        Ok(RowBatch::new(schema, rows))
    }

    fn insert_policy(&self, row: &RowBatch) -> Result<u64, SysError> {
        let name = required_text(row, "name")?;
        let allow = optional_text(row, "allow");
        let target = optional_text(row, "target");

        let conn = self.system.lock().map_err(poisoned)?;
        let tx = conn.unchecked_transaction().map_err(backend)?;

        // The grant row.
        tx.execute(
            "INSERT INTO sys_policies (name, allow, target) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, allow, target],
        )
        .map_err(backend)?;

        // Administration observes itself: append the t76 audit row in the SAME transaction so a
        // torn write can never leave the policy un-audited. Metadata only (verb + path), never the
        // grant's row data — the boundary `describe` enforces.
        append_audit_tx(
            &tx,
            AuditEvent {
                actor: ACTOR_CLI.to_string(),
                connection: "default".to_string(),
                verb: "INSERT".to_string(),
                path: SysNode::Policies.path(),
                committed: true,
                ts: now_rfc3339(),
            },
        )
        .map_err(backend)?;

        tx.commit().map_err(backend)?;
        Ok(1)
    }

    fn set_setting(&self, row: &RowBatch) -> Result<u64, SysError> {
        let key = required_text(row, "key")?;
        let value = required_text(row, "value")?;

        let conn = self.system.lock().map_err(poisoned)?;
        let tx = conn.unchecked_transaction().map_err(backend)?;

        // Upsert on `key`: a setting is single-valued, so re-setting it replaces the prior value
        // (and bumps `updated_at`). The safety mode is one such row (`key = 'safety_mode'`).
        tx.execute(
            "INSERT INTO sys_settings (key, value, updated_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ','now')) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, value],
        )
        .map_err(backend)?;

        // Administration observes itself: append the t76 audit row in the SAME transaction (a torn
        // write can never leave the setting un-audited). Metadata only (verb + path), never the
        // value — the boundary `describe` enforces.
        append_audit_tx(
            &tx,
            AuditEvent {
                actor: ACTOR_CLI.to_string(),
                connection: "default".to_string(),
                verb: "INSERT".to_string(),
                path: SysNode::Settings.path(),
                committed: true,
                ts: now_rfc3339(),
            },
        )
        .map_err(backend)?;

        tx.commit().map_err(backend)?;
        Ok(1)
    }

    fn set_billing(&self, row: &RowBatch) -> Result<u64, SysError> {
        let team_id = required_text(row, "team_id")?;
        let tier = required_text(row, "tier")?;
        let status = required_text(row, "status")?;
        let current_period_end = optional_text(row, "current_period_end");

        let conn = self.system.lock().map_err(poisoned)?;
        let tx = conn.unchecked_transaction().map_err(backend)?;
        upsert_billing_tx(&tx, &team_id, &tier, &status, current_period_end.as_deref())
            .map_err(backend)?;
        // Administration observes itself: append the t76 audit row in the SAME transaction. Metadata
        // only (verb + path), never the plan row — the boundary `describe` enforces.
        append_audit_tx(
            &tx,
            AuditEvent {
                actor: ACTOR_CLI.to_string(),
                connection: "default".to_string(),
                verb: "INSERT".to_string(),
                path: SysNode::Billing.path(),
                committed: true,
                ts: now_rfc3339(),
            },
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(1)
    }

    fn upsert_binding(&self, row: &RowBatch) -> Result<u64, SysError> {
        // t100020 (the CONNECT model): bind / re-bind a defined path in the PROJECT DB `path_binding`
        // table (upsert on `path`). A row carrying `alias_of` is an ALIAS (reuse another defined
        // path's connection); otherwise it is a FULL connect and MUST name a driver.
        let path = required_text(row, "path")?;
        let alias_of = optional_text(row, "alias_of");
        let Some(project) = &self.project else {
            return Err(SysError::Backend(
                "no project database (set HOME or XDG_CONFIG_HOME) to persist a connection".into(),
            ));
        };
        let conn = project.lock().map_err(poisoned)?;
        let affected = if let Some(target) = alias_of {
            crate::path_binding::db_upsert_alias(&conn, &path, &target).map_err(binding_err)?
        } else {
            let driver = required_text(row, "driver").map_err(|_| SysError::MalformedEffect {
                reason: "a full CONNECT needs a driver (or a `/path` alias target)".into(),
            })?;
            let at = optional_text(row, "at");
            let secret_ref = optional_text(row, "secret_ref");
            // ADR 0008: the mount coordinate — an absent HOST clause means the implicit embedded
            // `local` host (defaulted in the binding I/O); `account` is a label, never a token.
            let host = optional_text(row, "host");
            let account = optional_text(row, "account");
            crate::path_binding::db_upsert_binding(
                &conn,
                &path,
                &driver,
                at.as_deref(),
                secret_ref.as_deref(),
                host.as_deref(),
                account.as_deref(),
            )
            .map_err(binding_err)?
        };
        drop(conn);
        // Administration observes itself (t76). The binding lives in the Project DB and the audit
        // chain in the System DB, so — unlike the single-DB /sys writes — this appends best-effort
        // AFTER the binding commits (a cross-DB write cannot share one transaction).
        self.audit_paths_write("INSERT", &path);
        Ok(affected)
    }

    fn remove_binding(&self, path: &str) -> Result<u64, SysError> {
        // t100020: `DISCONNECT` — remove the defined path from the PROJECT DB (aliases cascade).
        let Some(project) = &self.project else {
            return Err(SysError::Backend(
                "no project database (set HOME or XDG_CONFIG_HOME) to remove a connection".into(),
            ));
        };
        let conn = project.lock().map_err(poisoned)?;
        let affected = crate::path_binding::db_remove_binding(&conn, path).map_err(binding_err)?;
        drop(conn);
        self.audit_paths_write("REMOVE", path);
        Ok(affected)
    }
}

impl SystemDbBackend {
    /// Append a best-effort t76 audit row (into the System DB) for a `/sys/paths` mutation. The
    /// binding lives in the Project DB, so this cannot share the write's transaction; it runs after
    /// the binding commits and NEVER fails the mutation (administration still observes itself, but a
    /// torn audit does not roll back a committed connect). Metadata only — verb + node path, never
    /// the binding row.
    fn audit_paths_write(&self, verb: &str, user_path: &str) {
        let Ok(conn) = self.system.lock() else {
            return;
        };
        let Ok(tx) = conn.unchecked_transaction() else {
            return;
        };
        let event = AuditEvent {
            actor: ACTOR_CLI.to_string(),
            connection: "default".to_string(),
            verb: verb.to_string(),
            // The defined path is the subject; record it under the /sys/paths node it was written to.
            path: format!("{}{}", SysNode::Paths.path(), user_path),
            committed: true,
            ts: now_rfc3339(),
        };
        if append_audit_tx(&tx, event).is_ok() {
            let _ = tx.commit();
        }
    }
    /// Resolve a team's recorded **billing plan** (t67) from `/sys/billing` — the authority the
    /// ENTITLEMENT GATE reads. **Fail-closed (default-deny toward the free floor):** a missing row, a
    /// read error, or a garbled tier/status all resolve to the FREE plan
    /// ([`qfs_identity::BillingPlan::free`]) — an unpaid/unknown team never gains paid entitlements.
    /// The labels are decoded through the pure model, so a corrupted value can only LOSE entitlements.
    #[must_use]
    pub fn get_billing_plan(&self, team_id: &str) -> qfs_identity::BillingPlan {
        let Ok(conn) = self.system.lock() else {
            return qfs_identity::BillingPlan::free();
        };
        conn.query_row(
            "SELECT tier, status FROM billing_subscriptions WHERE team_id = ?1",
            rusqlite::params![team_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
        .map_or_else(qfs_identity::BillingPlan::free, |(tier, status)| {
            qfs_identity::BillingPlan::decode(&tier, &status)
        })
    }

    /// Apply a provider subscription event to a team's plan, **idempotently** (t67, the at-least-once
    /// webhook update path). The provider's `event_id` is inserted into the `billing_events` dedup
    /// ledger inside the SAME transaction as the upsert: a REPLAYED event (same id) is a no-op (the
    /// plan is not double-applied), so a re-delivered "subscription cancelled" cannot fight a later
    /// "renewed". Returns `true` when this call applied the event, `false` when it was a deduped
    /// replay. The plan row + the audit row + the ledger row commit atomically.
    ///
    /// This is the SEAM the binary's webhook handler (`crate::billing`) calls after
    /// `qfs-watchtower` has HMAC-verified the request — no payment secret crosses into this method
    /// (only the already-verified, decoded plan labels).
    ///
    /// # Errors
    /// [`SysError::Backend`] on an I/O failure.
    pub fn apply_provider_event(
        &self,
        event_id: &str,
        team_id: &str,
        tier: &str,
        status: &str,
        current_period_end: Option<&str>,
    ) -> Result<bool, SysError> {
        let conn = self.system.lock().map_err(poisoned)?;
        let tx = conn.unchecked_transaction().map_err(backend)?;
        // Dedup FIRST (the ledger PK is the provider event id): INSERT OR IGNORE — a 0-row result
        // means this exact event was already applied, so we apply NOTHING and report a deduped replay.
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO billing_events (event_id, team_id) VALUES (?1, ?2)",
                rusqlite::params![event_id, team_id],
            )
            .map_err(backend)?;
        if inserted == 0 {
            // A replay: roll back (nothing changed) and report "not applied".
            tx.rollback().map_err(backend)?;
            return Ok(false);
        }
        upsert_billing_tx(&tx, team_id, tier, status, current_period_end).map_err(backend)?;
        append_audit_tx(
            &tx,
            AuditEvent {
                actor: ACTOR_PROVIDER.to_string(),
                connection: "default".to_string(),
                verb: "INSERT".to_string(),
                path: SysNode::Billing.path(),
                committed: true,
                ts: now_rfc3339(),
            },
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    /// Read a single setting `value` by `key` from the System DB (best-effort: a missing row or a
    /// read error yields `None`). The READ side of the `/sys/settings` round-trip.
    #[must_use]
    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.system.lock().ok()?;
        conn.query_row(
            "SELECT value FROM sys_settings WHERE key = ?1",
            rusqlite::params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// Resolve the active selectable **safety mode** (t59) from the persisted `/sys/settings` row,
    /// **failing safe** to the default ([`SafetyMode::AutonomousInPolicy`](qfs_core::SafetyMode)) on
    /// a missing/garbled value (decision: an unset/misconfigured mode falls to the safest sensible
    /// default — irreversible needs approval — never to Policy-only-auto).
    #[must_use]
    pub fn resolve_safety_mode(&self) -> qfs_core::SafetyMode {
        self.get_setting(SAFETY_MODE_KEY)
            .map_or_else(qfs_core::SafetyMode::default, |v| {
                qfs_core::SafetyMode::from_label_or_default(&v)
            })
    }
}

/// The `/sys/settings` key under which the selectable safety mode (t59) is stored.
pub const SAFETY_MODE_KEY: &str = "safety_mode";

/// Resolve the deployment's active selectable **safety mode** (t59) for the binary's commit faces
/// (the CLI one-shot run context + the serve MCP/dashboard engine). Precedence, most-authoritative
/// first, **failing safe** at every step (an unset/garbled source never opens the floor):
///   1. the persisted `/sys/settings` `safety_mode` row (the operator's stored choice, surfaced as
///      data and set via `INSERT INTO /sys/settings`);
///   2. the `QFS_SAFETY_MODE` env (the unattended / no-System-DB config path — CI, agents);
///   3. the safe default [`SafetyMode::AutonomousInPolicy`](qfs_core::SafetyMode).
#[must_use]
pub fn resolve_active_safety_mode() -> qfs_core::SafetyMode {
    if let Some(backend) = SystemDbBackend::open_default() {
        if let Some(value) = backend.get_setting(SAFETY_MODE_KEY) {
            return qfs_core::SafetyMode::from_label_or_default(&value);
        }
    }
    match std::env::var("QFS_SAFETY_MODE") {
        Ok(value) => qfs_core::SafetyMode::from_label_or_default(&value),
        Err(_) => qfs_core::SafetyMode::default(),
    }
}

impl SystemDbBackend {
    /// Run a read over the System DB connection, mapping each row with `f`.
    fn scan_system<F>(&self, sql: &str, f: F) -> Result<Vec<Row>, SysError>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Row>,
    {
        let conn = self.system.lock().map_err(poisoned)?;
        query_rows(&conn, sql, f)
    }

    /// Run a read over the Project DB connection registry, mapping each row with `f`. Returns an
    /// empty result when no Project DB is wired (best-effort).
    fn scan_project<F>(&self, sql: &str, f: F) -> Result<Vec<Row>, SysError>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Row>,
    {
        let Some(project) = &self.project else {
            return Ok(Vec::new());
        };
        let conn = project.lock().map_err(poisoned)?;
        query_rows(&conn, sql, f)
    }
}

/// Run `sql` and collect each row through `f` into owned [`Row`]s (no vendor type escapes).
fn query_rows<F>(conn: &Connection, sql: &str, f: F) -> Result<Vec<Row>, SysError>
where
    F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Row>,
{
    let mut stmt = conn.prepare(sql).map_err(backend)?;
    let mapped = stmt.query_map([], |r| f(r)).map_err(backend)?;
    let mut out = Vec::new();
    for r in mapped {
        out.push(r.map_err(backend)?);
    }
    Ok(out)
}

/// Append one event to the t76 hash chain INSIDE an existing transaction (the policy write's
/// transaction): read the head, compute the next chained event, persist the head, append the
/// bounded tail row, and trim. Mirrors `crate::audit::append_event`, but composed into the caller's
/// transaction so the policy row + its audit row commit atomically.
fn append_audit_tx(tx: &rusqlite::Transaction<'_>, event: AuditEvent) -> rusqlite::Result<()> {
    let head: Option<ChainHead> = tx
        .query_row(
            "SELECT seq, content_hash, prev_hash FROM audit_chain_head WHERE id = 1",
            [],
            |r| {
                Ok(ChainHead {
                    seq: r.get::<_, i64>(0)? as u64,
                    content_hash: r.get(1)?,
                    prev_hash: r.get(2)?,
                })
            },
        )
        .optional()?;

    let (seq, prev_hash) = match head {
        Some(h) => (h.seq + 1, h.hash()),
        None => (1, GENESIS_PREV_HASH.to_string()),
    };
    let chained = event.chain(seq, prev_hash);
    let new_head = chained.head();

    tx.execute(
        "INSERT INTO audit_chain_head (id, seq, content_hash, prev_hash) VALUES (1, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET seq = excluded.seq, content_hash = excluded.content_hash, \
         prev_hash = excluded.prev_hash",
        rusqlite::params![
            new_head.seq as i64,
            new_head.content_hash,
            new_head.prev_hash
        ],
    )?;
    tx.execute(
        "INSERT INTO audit_tail \
         (seq, actor, connection, verb, path, committed, ts, content_hash, prev_hash, hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            chained.seq as i64,
            chained.event.actor,
            chained.event.connection,
            chained.event.verb,
            chained.event.path,
            i64::from(chained.event.committed),
            chained.event.ts,
            chained.content_hash,
            chained.prev_hash,
            chained.hash,
        ],
    )?;
    tx.execute(
        "DELETE FROM audit_tail WHERE seq <= ?1 - ?2",
        rusqlite::params![chained.seq as i64, TAIL_CAP],
    )?;
    Ok(())
}

/// Upsert one team's billing plan row (t67) INSIDE an existing transaction — an **upsert on
/// `team_id`** (a team has one current plan, so re-recording it replaces the row and bumps
/// `updated_at`). Shared by the gated `/sys/billing` write (`set_billing`) and the provider-webhook
/// apply path (`apply_provider_event`) so both land identical plan state. Metadata only — never a
/// payment secret.
fn upsert_billing_tx(
    tx: &rusqlite::Transaction<'_>,
    team_id: &str,
    tier: &str,
    status: &str,
    current_period_end: Option<&str>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO billing_subscriptions (team_id, tier, status, current_period_end, updated_at) \
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ','now')) \
         ON CONFLICT(team_id) DO UPDATE SET tier = excluded.tier, status = excluded.status, \
         current_period_end = excluded.current_period_end, updated_at = excluded.updated_at",
        rusqlite::params![team_id, tier, status, current_period_end],
    )?;
    Ok(())
}

/// The current UTC time as an RFC3339 string for an audit event's `ts`. A format failure on an
/// impossible date falls back to the Unix epoch rather than panicking (the audit never breaks the
/// operation).
fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

// --- small typed column getters (keep the row builders legible) ----------------------------------

fn int(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Value> {
    Ok(Value::Int(r.get::<_, i64>(idx)?))
}
fn text(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Value> {
    Ok(Value::Text(r.get::<_, String>(idx)?))
}
fn nullable_text(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Value> {
    Ok(match r.get::<_, Option<String>>(idx)? {
        Some(s) => Value::Text(s),
        None => Value::Null,
    })
}

/// Read a required text column from the single-row write payload by name (the policy `name`).
fn required_text(row: &RowBatch, col: &str) -> Result<String, SysError> {
    match cell(row, col) {
        Some(Value::Text(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(SysError::MalformedEffect {
            reason: format!("INSERT INTO /sys/policies requires a non-empty `{col}`"),
        }),
    }
}

/// Read an optional text column (absent/null/empty → `None`).
fn optional_text(row: &RowBatch, col: &str) -> Option<String> {
    match cell(row, col) {
        Some(Value::Text(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The single write payload's value for `col`, if the batch carries that column.
fn cell<'a>(row: &'a RowBatch, col: &str) -> Option<&'a Value> {
    let idx = row
        .schema
        .columns
        .iter()
        .position(|c| c.name.as_str() == col)?;
    row.rows.first().and_then(|r| r.values.get(idx))
}

fn backend(e: rusqlite::Error) -> SysError {
    SysError::Backend(e.to_string())
}

/// Map a `path_binding` I/O error (t100020). A foreign-key violation means an ALIAS named a target
/// defined path that does not exist — a fail-closed rejection with a clear, secret-free reason
/// (never a fake mount); everything else is a plain backend error.
fn binding_err(e: rusqlite::Error) -> SysError {
    if let rusqlite::Error::SqliteFailure(err, _) = &e {
        if err.code == rusqlite::ErrorCode::ConstraintViolation {
            return SysError::MalformedEffect {
                reason: "the alias target is not a defined path — CONNECT it first".to_string(),
            };
        }
    }
    SysError::Backend(e.to_string())
}
fn poisoned<T>(_: std::sync::PoisonError<T>) -> SysError {
    SysError::Backend("poisoned system db connection mutex".to_string())
}

/// The async read facet (the `/sys` counterpart of `shell.rs`'s `LocalReadDriver`): adapts the
/// synchronous [`SysBackend::scan`] to qfs-exec's [`ReadDriver`] seam. Lives in the binary because
/// `ReadDriver` is a qfs-exec type and the driver crate must stay off qfs-exec (dep direction).
pub struct SysReadDriver {
    backend: Arc<dyn SysBackend>,
}

impl SysReadDriver {
    /// Build the read adapter over an injected backend.
    #[must_use]
    pub fn new(backend: Arc<dyn SysBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait::async_trait]
impl ReadDriver for SysReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let node = node_for_path(&scan.path).ok_or_else(|| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: "not a /sys admin path",
        })?;
        self.backend.scan(node).map_err(|e| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: sys_error_reason(&e),
        })
    }
}

/// A stable, secret-free reason code for a `/sys` read failure (the executor maps it to its kind).
fn sys_error_reason(e: &SysError) -> &'static str {
    match e {
        SysError::UnknownNode { .. } => "unknown_sys_node",
        SysError::AppendOnly { .. } => "append_only",
        SysError::MalformedEffect { .. } => "malformed_effect",
        SysError::Backend(_) => "read_failed",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_pushdown::{PushedQuery, ScanNode};
    use qfs_store::audit::{verify_chain, GENESIS_PREV_HASH};
    use qfs_store::{FileSource, ProjectDb, SystemDb};
    use qfs_types::{Column, ColumnType, Schema};
    use tempfile::TempDir;

    /// A backend over fresh FILE-backed System + Project DBs (migrated), pre-seeded with fixture
    /// rows. File-backed (not in-memory) so the audit chain can be re-read through a second
    /// `SystemDb` view on the same path (the backend OWNS its connection). The `TempDir` is
    /// returned so the files outlive the test.
    fn fixture_backend() -> (TempDir, SystemDbBackend) {
        let dir = TempDir::new().unwrap();
        let sys_path = dir.path().join("system.db");
        let proj_path = dir.path().join("project.db");

        let sys = SystemDb::open(&FileSource::new(&sys_path))
            .unwrap()
            .into_db()
            .into_connection();
        sys.execute("INSERT INTO projects (slug) VALUES ('alpha')", [])
            .unwrap();
        sys.execute(
            "INSERT INTO users (primary_email, status) VALUES ('a@qmu.jp', 'active')",
            [],
        )
        .unwrap();

        let proj = ProjectDb::open(&FileSource::new(&proj_path))
            .unwrap()
            .into_db()
            .into_connection();
        // A connection-registry row WITH secret material — the scan must never surface it.
        proj.execute(
            "INSERT INTO secret_store (driver, connection, nonce, ciphertext) \
             VALUES ('github', 'work', x'00', ?1)",
            rusqlite::params![b"SUPER-SECRET-TOKEN".to_vec()],
        )
        .unwrap();

        (dir, SystemDbBackend::new(sys, Some(proj)))
    }

    fn texts(batch: &RowBatch, col: &str) -> Vec<String> {
        let idx = batch
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == col)
            .expect("column present");
        batch
            .rows
            .iter()
            .filter_map(|r| match &r.values[idx] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn policy_payload() -> RowBatch {
        let schema = Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("allow", ColumnType::Text, true),
            Column::new("target", ColumnType::Text, true),
        ]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text("analysts".into()),
                Value::Text("SELECT".into()),
                Value::Text("/sql/*".into()),
            ])],
        )
    }

    #[test]
    fn scans_users_projects_audit_rows() {
        let (_d, backend) = fixture_backend();
        let users = backend.scan(SysNode::Users).unwrap();
        assert_eq!(texts(&users, "primary_email"), vec!["a@qmu.jp"]);
        let projects = backend.scan(SysNode::Projects).unwrap();
        assert_eq!(texts(&projects, "slug"), vec!["alpha"]);
        // /sys/audit scans (empty before any mutation, but the relation resolves + types).
        let audit = backend.scan(SysNode::Audit).unwrap();
        assert!(audit.rows.is_empty());
        assert!(audit.schema.column("committed").is_some());
    }

    #[test]
    fn connections_project_names_only_never_secrets() {
        let (_d, backend) = fixture_backend();
        let conns = backend.scan(SysNode::Connections).unwrap();
        // The registry row is present (driver + label + metadata).
        assert_eq!(texts(&conns, "driver"), vec!["github"]);
        assert_eq!(texts(&conns, "connection"), vec!["work"]);
        // The schema has NO secret column, and the secret value never appears anywhere in the batch.
        for forbidden in ["nonce", "ciphertext", "secret"] {
            assert!(
                conns.schema.column(forbidden).is_none(),
                "/sys/connections must not expose `{forbidden}`"
            );
        }
        let dump = format!("{conns:?}");
        assert!(
            !dump.contains("SUPER-SECRET-TOKEN") && !dump.contains("SUPER"),
            "no secret material may surface through /sys/connections"
        );
    }

    #[test]
    fn insert_policy_applies_and_appends_a_verifiable_audit_row() {
        let dir = TempDir::new().unwrap();
        let sys_path = dir.path().join("system.db");
        // Build a backend whose System DB is the file at `sys_path` (so we can re-read its audit
        // chain through a second SystemDb view below).
        let sys = SystemDb::open(&FileSource::new(&sys_path))
            .unwrap()
            .into_db()
            .into_connection();
        let backend = SystemDbBackend::new(sys, None);

        let n = backend.insert_policy(&policy_payload()).unwrap();
        assert_eq!(n, 1);

        // The grant row is readable back through /sys/policies.
        let policies = backend.scan(SysNode::Policies).unwrap();
        assert_eq!(texts(&policies, "name"), vec!["analysts"]);
        assert_eq!(texts(&policies, "allow"), vec!["SELECT"]);

        // The mutation appended exactly one audit row (administration observes itself) — visible
        // both through the /sys/audit relation and the durable t76 chain.
        let audit = backend.scan(SysNode::Audit).unwrap();
        assert_eq!(texts(&audit, "verb"), vec!["INSERT"]);
        assert_eq!(texts(&audit, "path"), vec!["/sys/policies"]);

        // Re-open the SAME System DB file to verify the chain via the pure verifier (the head +
        // tail were written transactionally with the policy row).
        let view = SystemDb::open(&FileSource::new(&sys_path)).unwrap();
        let tail = crate::audit::recent_tail(&view).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(verify_chain(&tail, GENESIS_PREV_HASH), None);
    }

    fn settings_payload(key: &str, value: &str) -> RowBatch {
        let schema = Schema::new(vec![
            Column::new("key", ColumnType::Text, false),
            Column::new("value", ColumnType::Text, false),
        ]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text(key.into()),
                Value::Text(value.into()),
            ])],
        )
    }

    /// t59: the selectable safety mode round-trips through `/sys/settings` — `set_setting` (the
    /// `INSERT INTO /sys/settings` backend) persists it, `get_setting` / `resolve_safety_mode` read
    /// it back, the upsert REPLACES on a re-set, and an unset/garbled value resolves SAFE.
    #[test]
    fn safety_mode_round_trips_through_sys_settings() {
        let (_d, backend) = fixture_backend();

        // Unset ⇒ the safe default (autonomous-in-policy).
        assert_eq!(backend.get_setting(SAFETY_MODE_KEY), None);
        assert_eq!(
            backend.resolve_safety_mode(),
            qfs_core::SafetyMode::AutonomousInPolicy
        );

        // Set policy-only, read it back as both the raw value and the resolved mode.
        let n = backend
            .set_setting(&settings_payload(SAFETY_MODE_KEY, "policy-only"))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            backend.get_setting(SAFETY_MODE_KEY).as_deref(),
            Some("policy-only")
        );
        assert_eq!(
            backend.resolve_safety_mode(),
            qfs_core::SafetyMode::PolicyOnly
        );
        // The mutation is visible as DATA through the /sys/settings relation (one-engine-three-faces).
        let settings = backend.scan(SysNode::Settings).unwrap();
        assert_eq!(texts(&settings, "key"), vec!["safety_mode"]);
        assert_eq!(texts(&settings, "value"), vec!["policy-only"]);

        // Upsert REPLACES (a setting is single-valued) — re-set to approve-everything.
        backend
            .set_setting(&settings_payload(SAFETY_MODE_KEY, "approve-everything"))
            .unwrap();
        assert_eq!(
            backend.resolve_safety_mode(),
            qfs_core::SafetyMode::ApproveEverything
        );
        let settings = backend.scan(SysNode::Settings).unwrap();
        assert_eq!(settings.rows.len(), 1, "upsert replaces, never duplicates");

        // A garbled persisted value fails SAFE to the default (never Policy-only-auto).
        backend
            .set_setting(&settings_payload(SAFETY_MODE_KEY, "nonsense"))
            .unwrap();
        assert_eq!(
            backend.resolve_safety_mode(),
            qfs_core::SafetyMode::AutonomousInPolicy
        );

        // Each set self-audited (administration observes itself): 3 INSERT audit rows on /sys/settings.
        let audit = backend.scan(SysNode::Audit).unwrap();
        assert_eq!(texts(&audit, "path"), vec!["/sys/settings"; 3]);
    }

    fn billing_payload(team: &str, tier: &str, status: &str) -> RowBatch {
        let schema = Schema::new(vec![
            Column::new("team_id", ColumnType::Text, false),
            Column::new("tier", ColumnType::Text, false),
            Column::new("status", ColumnType::Text, false),
        ]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text(team.into()),
                Value::Text(tier.into()),
                Value::Text(status.into()),
            ])],
        )
    }

    /// t67: recording a tier through `INSERT INTO /sys/billing` round-trips as DATA and drives the
    /// ENTITLEMENT GATE — a paid-team active plan permits team connections; a free plan and an
    /// UNRECORDED team both fail closed to free (deny). The upsert replaces on a re-record.
    #[test]
    fn billing_tier_round_trips_and_gates_team_connections() {
        use qfs_identity::Capability;
        let (_d, backend) = fixture_backend();

        // An unrecorded team is the free floor (default-deny toward the lower tier).
        let unknown = backend.get_billing_plan("team-ghost");
        assert!(!unknown.permits(Capability::TeamConnections));

        // Record a PAID team plan; it round-trips through /sys/billing and unlocks the paid capability.
        assert_eq!(
            backend
                .set_billing(&billing_payload("team-acme", "paid-team", "active"))
                .unwrap(),
            1
        );
        let plan = backend.get_billing_plan("team-acme");
        assert_eq!(plan.tier, qfs_identity::Tier::PaidTeam);
        assert!(
            plan.permits(Capability::TeamConnections),
            "an actively-paid team unlocks team connections"
        );
        // Visible as DATA through the /sys/billing relation (one-engine-three-faces).
        let billing = backend.scan(SysNode::Billing).unwrap();
        assert_eq!(texts(&billing, "team_id"), vec!["team-acme"]);
        assert_eq!(texts(&billing, "tier"), vec!["paid-team"]);

        // Upsert REPLACES (one plan per team): downgrade to free ⇒ the gate now DENIES (fail closed).
        backend
            .set_billing(&billing_payload("team-acme", "free-individual", "inactive"))
            .unwrap();
        assert_eq!(backend.scan(SysNode::Billing).unwrap().rows.len(), 1);
        assert!(
            !backend
                .get_billing_plan("team-acme")
                .permits(Capability::TeamConnections),
            "a downgraded plan must lose the paid capability"
        );

        // A garbled stored tier resolves to the free floor (never paid).
        backend
            .set_billing(&billing_payload("team-x", "enterprise-unlimited", "active"))
            .unwrap();
        assert!(!backend
            .get_billing_plan("team-x")
            .permits(Capability::TeamConnections));

        // Each set self-audited (administration observes itself): 3 INSERT rows on /sys/billing.
        let audit = backend.scan(SysNode::Audit).unwrap();
        assert_eq!(texts(&audit, "path"), vec!["/sys/billing"; 3]);
    }

    /// t67: the at-least-once provider webhook apply is IDEMPOTENT — a replayed event id (the dedup
    /// ledger PK) is a no-op, so a re-delivered "subscription cancelled" cannot double-apply or fight
    /// a later state. A NEW event id does apply (flips the plan).
    #[test]
    fn provider_webhook_apply_is_idempotent() {
        use qfs_identity::Capability;
        let (_d, backend) = fixture_backend();

        // First delivery of evt-1 (upgrade to paid) APPLIES.
        assert!(backend
            .apply_provider_event(
                "evt-1",
                "team-acme",
                "paid-team",
                "active",
                Some("2026-12-31")
            )
            .unwrap());
        assert!(backend
            .get_billing_plan("team-acme")
            .permits(Capability::TeamConnections));

        // A REPLAY of evt-1 (same id) is a deduped no-op (false), state unchanged.
        assert!(!backend
            .apply_provider_event("evt-1", "team-acme", "free-individual", "canceled", None)
            .unwrap());
        assert!(
            backend
                .get_billing_plan("team-acme")
                .permits(Capability::TeamConnections),
            "a replayed event must NOT double-apply / overwrite the plan"
        );

        // A NEW event (evt-2, cancellation) DOES apply ⇒ the gate fails closed to free.
        assert!(backend
            .apply_provider_event("evt-2", "team-acme", "free-individual", "canceled", None)
            .unwrap());
        assert!(!backend
            .get_billing_plan("team-acme")
            .permits(Capability::TeamConnections));
    }

    #[test]
    fn insert_policy_requires_a_name() {
        let (_d, backend) = fixture_backend();
        let schema = Schema::new(vec![Column::new("allow", ColumnType::Text, true)]);
        let row = RowBatch::new(schema, vec![Row::new(vec![Value::Text("SELECT".into())])]);
        assert!(backend.insert_policy(&row).is_err());
    }

    #[tokio::test]
    async fn read_driver_scans_through_the_seam() {
        let (_d, backend) = fixture_backend();
        let reader = SysReadDriver::new(Arc::new(backend));
        let scan = ScanNode {
            source: qfs_pushdown::SourceId::new("sys"),
            path: "/sys/users".to_string(),
            pushed: PushedQuery::default(),
            schema: sys_node_schema(SysNode::Users),
        };
        let batch = reader.scan(&scan).await.unwrap();
        assert_eq!(texts(&batch, "primary_email"), vec!["a@qmu.jp"]);
        // An unknown /sys segment is a structured invalid-path error (no panic).
        let bad = ScanNode {
            source: qfs_pushdown::SourceId::new("sys"),
            path: "/sys/nope".to_string(),
            pushed: PushedQuery::default(),
            schema: Schema::new(vec![]),
        };
        assert!(reader.scan(&bad).await.is_err());
    }
}
