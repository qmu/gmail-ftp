//! The versioned, checksum-verified, forward-only embedded-migrations runner (roadmap §4.2).
//!
//! A scope (System / Project) declares an **ordered** list of [`Migration`]s. [`migrate`] applies
//! the pending ones idempotently, each inside its own transaction (so a crash mid-migration rolls
//! back — decision E's "built fresh" must not mean "corruptible"), and is safe to call on **every**
//! start/relaunch: an already-applied migration is RE-VERIFIED by checksum rather than re-run.
//!
//! Invariants the runner enforces (fail-closed):
//! - **Forward-only.** Versions are a strictly-increasing sequence starting at 1, with no gaps; a
//!   malformed migration set is a programming error caught at startup, not silent drift.
//! - **Tamper-evident.** Each applied migration's `sha256_hex(sql)` is stored; a relaunch whose
//!   embedded SQL no longer matches the recorded checksum is a [`MigrationError::ChecksumMismatch`]
//!   (someone edited a shipped migration in place — forbidden; add a new version instead).
//!
//! ## Healing a botched in-place edit that already SHIPPED
//!
//! The tamper-evidence above assumes an in-place edit is caught *before* it reaches a real DB. Once
//! a botched edit ships in a release, two DB lineages exist in the wild — old-body and new-body —
//! and neither embedded checksum can satisfy both (reverting breaks the new-body release; keeping
//! the new body fail-closes every old-body DB). The narrow, audited escape hatch for *that already-
//! happened* case is [`SUPERSEDED_BODIES`]: a registry, keyed by the OLD body's checksum, of the
//! idempotent SQL that heals an old-body DB FORWARD to the current body. When the runner meets a
//! recorded checksum that is a registered superseded body, it runs the heal + re-stamps the
//! recorded checksum (in one transaction) instead of erroring. An UNLISTED mismatch still fails
//! closed, so tamper-evidence is intact for genuinely-unknown edits. See ticket `20260630203120`.

use qfs_crypto_core::sha256_hex;
use rusqlite::OptionalExtension;

use crate::{Db, StoreError};

/// One ordered, immutable schema change. `version` is 1-based and contiguous within a scope; `sql`
/// is applied verbatim via `execute_batch` (it may contain several statements). Once shipped, a
/// migration's `sql` is FROZEN — fixing it means appending a new `version`, never editing in place
/// (the checksum guard enforces this).
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// 1-based, contiguous version within the scope's migration list.
    pub version: u32,
    /// A short human label for logs / the `schema_version` row (not load-bearing).
    pub name: &'static str,
    /// The migration body. Frozen once shipped; checksummed for tamper-evidence.
    pub sql: &'static str,
}

/// A prior, *released* body of a migration that was later edited in place — leaving DBs of BOTH
/// bodies in the wild (a botched-but-shipped edit; see the module docs). Listing the OLD body's
/// checksum here lets the runner heal an old-body DB FORWARD to the current body instead of
/// fail-closing. Keyed by checksum, which is content-addressed and therefore globally unique across
/// scopes (System / Project) — only a DB that applied EXACTLY that body records that checksum, so
/// `reconcile` is guaranteed to run against the schema that body created.
#[derive(Debug, Clone, Copy)]
pub struct SupersededBody {
    /// `sha256_hex` of the OLD shipped body (64 hex chars).
    pub old_checksum: &'static str,
    /// Idempotent forward-heal SQL bringing the OLD body's schema up to the CURRENT body's. Runs in
    /// the SAME transaction that re-stamps the recorded checksum, so a crash rolls back both.
    pub reconcile: &'static str,
}

/// The registry of botched-but-released in-place edits we heal forward (keyed by the OLD checksum).
///
/// Project migration v2 (`project_secret_store`): v0.0.9's commit `f95d20c` renamed the credential
/// column `account` → `connection` by EDITING v2's body in place (checksum `1be5979f…` → `97466be6…`)
/// rather than appending a new version. It shipped in the v0.0.9 release, so pre-v0.0.9 Project DBs
/// (the old `account` column) cannot open against v0.0.9+ code. Reverting v2 is not an option — that
/// would fail-close every v0.0.9 DB (the new `connection` column). So we heal the OLD lineage forward:
/// rename its columns to match, then re-stamp the recorded checksum to the current body's.
pub const SUPERSEDED_BODIES: &[SupersededBody] = &[SupersededBody {
    old_checksum: "1be5979f79fed42dc5e47bf5ea9dd8e086721f8ab9ffc784093bf6699c7f16bb",
    reconcile: "ALTER TABLE secret_store RENAME COLUMN account TO connection;\n\
                ALTER TABLE active_account RENAME COLUMN account TO connection;",
}];

/// A row of the `schema_version` bookkeeping table — one per applied migration.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: u32,
    pub name: String,
    pub checksum: String,
    pub applied_at: String,
}

/// Apply every pending migration in `migrations` to `db`, in order. Returns the versions applied
/// **this call** (empty on a no-op relaunch). Idempotent and transactional per migration.
///
/// Errors:
/// - [`MigrationError::NonContiguous`] if the embedded list is not `1,2,3,…` (a programming error).
/// - [`MigrationError::ChecksumMismatch`] if an already-applied migration's body changed.
pub fn migrate(db: &mut Db, migrations: &[Migration]) -> Result<Vec<u32>, StoreError> {
    // Validate the embedded set is forward-only & contiguous BEFORE touching the DB — a malformed
    // set is a build-time mistake, surfaced loudly rather than half-applied.
    for (i, m) in migrations.iter().enumerate() {
        let expected = u32::try_from(i + 1).map_err(|_| MigrationError::NonContiguous {
            position: i,
            version: m.version,
        })?;
        if m.version != expected {
            return Err(MigrationError::NonContiguous {
                position: i,
                version: m.version,
            }
            .into());
        }
    }

    let conn = &mut db.conn;
    // The bookkeeping table. `applied_at` defaults to UTC so a relaunch records a real wall-clock
    // stamp without the caller threading a clock through (decision E: start-time infrastructure).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            checksum   TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
        );",
    )
    .map_err(StoreError::from)?;

    let mut applied_now = Vec::new();
    for m in migrations {
        let checksum = sha256_hex(m.sql.as_bytes());
        let existing: Option<String> = conn
            .query_row(
                "SELECT checksum FROM schema_version WHERE version = ?1",
                [m.version],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        match existing {
            // Already applied AND the body still matches: verified no-op.
            Some(prev) if prev == checksum => continue,
            // Already applied but the embedded body changed. Two cases:
            //   1. A KNOWN botched-but-released in-place edit (the recorded body is a registered
            //      `SUPERSEDED_BODIES` entry): heal the old-body DB forward to the current body and
            //      re-stamp its recorded checksum, in ONE transaction (a crash rolls back both).
            //   2. Anything else: a genuinely-unknown in-place edit. Fail closed — do not silently
            //      diverge from on-disk state.
            Some(prev) => {
                if let Some(s) = SUPERSEDED_BODIES.iter().find(|s| s.old_checksum == prev) {
                    let tx = conn.transaction().map_err(StoreError::from)?;
                    tx.execute_batch(s.reconcile).map_err(StoreError::from)?;
                    tx.execute(
                        "UPDATE schema_version SET checksum = ?1, name = ?2 WHERE version = ?3",
                        rusqlite::params![checksum, m.name, m.version],
                    )
                    .map_err(StoreError::from)?;
                    tx.commit().map_err(StoreError::from)?;
                    continue;
                }
                return Err(MigrationError::ChecksumMismatch {
                    version: m.version,
                    recorded: prev,
                    embedded: checksum,
                }
                .into());
            }
            // Pending: apply the body + record it in ONE transaction so a crash rolls back both.
            None => {
                let tx = conn.transaction().map_err(StoreError::from)?;
                tx.execute_batch(m.sql).map_err(StoreError::from)?;
                tx.execute(
                    "INSERT INTO schema_version (version, name, checksum) VALUES (?1, ?2, ?3)",
                    rusqlite::params![m.version, m.name, checksum],
                )
                .map_err(StoreError::from)?;
                tx.commit().map_err(StoreError::from)?;
                applied_now.push(m.version);
            }
        }
    }
    Ok(applied_now)
}

/// Read the `schema_version` ledger (empty before the first [`migrate`]). Ordered by version.
pub fn applied_migrations(db: &Db) -> Result<Vec<AppliedMigration>, StoreError> {
    // No bookkeeping table yet => no migrations applied (a fresh DB), not an error.
    let exists: bool = db
        .conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .optional()
        .map_err(StoreError::from)?
        .unwrap_or(false);
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = db
        .conn
        .prepare("SELECT version, name, checksum, applied_at FROM schema_version ORDER BY version")
        .map_err(StoreError::from)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(AppliedMigration {
                version: r.get(0)?,
                name: r.get(1)?,
                checksum: r.get(2)?,
                applied_at: r.get(3)?,
            })
        })
        .map_err(StoreError::from)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StoreError::from)?);
    }
    Ok(out)
}

/// Structured, secret-free migration failures (folded into [`StoreError`]).
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The embedded migration list is not a contiguous `1,2,3,…` sequence — a programming error.
    #[error("migration list is not forward-only/contiguous: entry #{position} declares version {version}")]
    NonContiguous { position: usize, version: u32 },
    /// A shipped migration's body was edited in place (recorded checksum ≠ embedded checksum).
    #[error("migration v{version} was edited in place after being applied (recorded {recorded}, embedded {embedded}); ship a NEW version instead")]
    ChecksumMismatch {
        version: u32,
        recorded: String,
        embedded: String,
    },
}
