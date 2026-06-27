//! `qfs-store` — the embedded SQLite persistence substrate (roadmap **M0 / t42**).
//!
//! This is the single world the dashboard, CLI, and MCP agree on (roadmap decisions **E**
//! "all-SQLite, scrap the file vault" and **F** "stateless-at-scale: a trusted reverse proxy injects
//! the tenant→DB route; clients never name a DB"). It delivers the **two databases** roadmap §4.2
//! names:
//!
//! - [`SystemDb`] — per host: projects, cross-project config, the `/sys/*` surface.
//! - [`ProjectDb`] — per project: that project's connections, config, state.
//!
//! Both wrap a sync [`Db`] over a `rusqlite::Connection`. The crate is **sync by construction**:
//! rusqlite is sync, so tokio never enters here (the same confinement that keeps `qfs-cron` and the
//! `qfs-server` policy core pure). It is a **leaf** — only the terminal `qfs` binary opens a real DB
//! path (the dep-direction guard enforces this).
//!
//! ## The decision-F seam
//!
//! [`DbSource`] yields a connection **without the caller naming a file**. The binary supplies the
//! path ([`FileSource`]) or, in tests, an in-memory DB ([`MemorySource`]); a future reverse-proxy
//! tenant→DB injection is a *different* `DbSource` impl, not a code change. The invariant locked in
//! today is: *the binary never hard-codes a DB filename in a command path; the source is injected.*
//! (We deliberately do NOT implement distributed SQLite / D1 / EFS — that is later roadmap work.)
//!
//! ## What t42 ships
//!
//! The migrations [`migrate`] runner + `schema_version` bookkeeping + **empty** schema skeletons
//! ([`SYSTEM_MIGRATIONS`] / [`PROJECT_MIGRATIONS`]). The tables are *filled* by later M0–M3 tickets
//! (t43 secrets, t45 identity, t53 `/sys/*`) in their own migrations, so each PR's schema delta
//! stays reviewable. Opening a DB and running migrations is **start-time infrastructure**, NOT a qfs
//! effect-plan: it never goes through preview/commit; it is the substrate later `/sys/*` writes
//! preview and commit *over*.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub mod audit;

mod identity_store;
pub use identity_store::SqliteIdentityStore;

mod session_store;
pub use session_store::SqliteSessionStore;

mod migrate;
pub use migrate::{applied_migrations, migrate, AppliedMigration, Migration, MigrationError};

/// A handle over a single SQLite connection. Sync; tokio must not enter here. Obtained via a
/// [`DbSource`] so nothing downstream names a file (decision F).
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open a bare `Db` from a [`DbSource`] **without** running migrations. Most callers want
    /// [`SystemDb::open`] / [`ProjectDb::open`], which open *and* migrate the right scope.
    pub fn open(source: &dyn DbSource) -> Result<Self, StoreError> {
        let conn = source.connect()?;
        // Sound durability defaults for the embedded substrate: WAL for concurrent readers, and
        // foreign-key enforcement on (off by default in SQLite). These are pragmas, not schema, so
        // they live with connection-opening, not in a migration.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(StoreError::from)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(StoreError::from)?;
        Ok(Db { conn })
    }

    /// The underlying connection (read side). Migrations and writes use the scope newtypes.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Consume this handle and yield the **owned** `rusqlite::Connection`. The binary uses this
    /// (after [`ProjectDb::into_db`]) to move a migrated connection into a backend that must OWN it
    /// — e.g. t43's `SqliteSecrets`, which holds the connection inside a `Mutex` to be `Send + Sync`.
    #[must_use]
    pub fn into_connection(self) -> Connection {
        self.conn
    }
}

/// The connection-opening seam (decision F). An impl yields a fresh connection; the binary chooses
/// *which* (a file path, an in-memory DB, or — later — a tenant→DB route injected by a reverse
/// proxy). Callers below the binary never see a filename.
pub trait DbSource {
    /// Open a fresh connection for this source.
    fn connect(&self) -> Result<Connection, StoreError>;
}

/// Open a database at a concrete filesystem path. The **binary** owns the path resolution (e.g.
/// under `~/.config/qfs/`); this type just carries it. Parent directories are created on open.
pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    /// A file source at `path`. The file (and its parent dirs) are created on first [`connect`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileSource { path: path.into() }
    }

    /// The path this source opens (for the binary's own logging — never a secret).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl DbSource for FileSource {
    fn connect(&self) -> Result<Connection, StoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::Open {
                    detail: format!("creating DB parent dir: {e}"),
                })?;
            }
        }
        Connection::open(&self.path).map_err(StoreError::from)
    }
}

/// An ephemeral in-memory database — for tests and the migration unit tests. Never persists.
pub struct MemorySource;

impl DbSource for MemorySource {
    fn connect(&self) -> Result<Connection, StoreError> {
        Connection::open_in_memory().map_err(StoreError::from)
    }
}

/// Per-host database scope (roadmap §4.2): projects, cross-project config, the `/sys/*` surface.
/// A distinct *type* from [`ProjectDb`] so the two scopes are never confused for a string.
pub struct SystemDb(Db);

impl SystemDb {
    /// Open the System DB from `source` and apply its migrations (idempotent; safe every start).
    pub fn open(source: &dyn DbSource) -> Result<Self, StoreError> {
        let mut db = Db::open(source)?;
        migrate(&mut db, SYSTEM_MIGRATIONS)?;
        Ok(SystemDb(db))
    }

    /// The underlying handle (read side).
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.0
    }

    /// Consume this scope and yield its underlying [`Db`] (already migrated). Paired with
    /// [`Db::into_connection`] so the binary can move the migrated System-DB connection into a
    /// backend that OWNS it — t45's [`SqliteIdentityStore`], which holds the connection inside a
    /// `Mutex` to be `Send + Sync` (the same seam [`ProjectDb::into_db`] gives the secret store).
    #[must_use]
    pub fn into_db(self) -> Db {
        self.0
    }
}

/// Per-project database scope (roadmap §4.2): that project's connections, config, state. A distinct
/// *type* from [`SystemDb`].
pub struct ProjectDb(Db);

impl ProjectDb {
    /// Open a Project DB from `source` and apply its migrations (idempotent; safe every start).
    pub fn open(source: &dyn DbSource) -> Result<Self, StoreError> {
        let mut db = Db::open(source)?;
        migrate(&mut db, PROJECT_MIGRATIONS)?;
        Ok(ProjectDb(db))
    }

    /// The underlying handle (read side).
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.0
    }

    /// Consume this scope and yield its underlying [`Db`] (already migrated). Paired with
    /// [`Db::into_connection`] so the binary can move the migrated connection into a backend that
    /// owns it (t43's `SqliteSecrets`). The scope newtype is dropped once the connection is owned.
    #[must_use]
    pub fn into_db(self) -> Db {
        self.0
    }
}

/// The System DB's ordered migration set (forward-only; append, never edit a shipped entry).
pub const SYSTEM_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "system_skeleton",
        sql: include_str!("schema/system.sql"),
    },
    // t76: the hash-chained audit stream's durable state — the chain HEAD (to continue the chain
    // across restarts) + the BOUNDED live-tail buffer backing the /sys/audit live view. Appended
    // as a NEW version — migration #1's body stays frozen (the checksum guard forbids editing a
    // shipped migration in place). qfs EMITS the stream and retains only the head (decision V).
    Migration {
        version: 2,
        name: "system_audit_chain",
        sql: include_str!("schema/system_audit.sql"),
    },
    // t45 (roadmap M1): the identity tables — `users` (the human handle) + `accounts` (a linked
    // sign-in identity; 'local' rows carry an argon2id password hash). Appended as a NEW version (#3)
    // — migrations #1/#2 stay frozen (the checksum guard forbids editing a shipped migration). The
    // rusqlite `IdentityStore` impl that fills these lives in `identity_store.rs`. AUTHENTICATION
    // ONLY: a row here grants no privilege yet (decision §4.1; sessions are t46).
    Migration {
        version: 3,
        name: "system_identity",
        sql: include_str!("schema/system_identity.sql"),
    },
    // t46 (roadmap M1): server-side sessions — the `sessions` table keyed by a HASH of the opaque
    // token (never the plaintext), with `user_id` (the authenticated t45 human), `expires_at`, and
    // `rotated_from` (the prior session's hash on a rotation). Appended as a NEW version (#4) —
    // migrations #1–#3 stay frozen (the checksum guard forbids editing a shipped migration). The
    // rusqlite `SessionStore` impl that fills these lives in `session_store.rs`. AUTHENTICATION
    // STATE ONLY: a session proves WHO, not WHAT-may-you-do (authorization is M2).
    Migration {
        version: 4,
        name: "system_sessions",
        sql: include_str!("schema/system_sessions.sql"),
    },
];

/// The Project DB's ordered migration set (forward-only; append, never edit a shipped entry).
pub const PROJECT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "project_skeleton",
        sql: include_str!("schema/project.sql"),
    },
    // t43: the envelope-encrypted credential store (secret_store + secret_meta) and the DB-backed
    // active-connection selection. Appended as a NEW version — migration #1's body stays frozen.
    Migration {
        version: 2,
        name: "project_secret_store",
        sql: include_str!("schema/project_secrets.sql"),
    },
];

/// Structured, secret-free persistence errors (AI-consumable; a DB path is infra, not a secret, but
/// we never render connection *contents*). Migration failures fold in via [`MigrationError`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Opening / creating the database failed (path, permissions, corruption).
    #[error("opening the database: {detail}")]
    Open { detail: String },
    /// A migration was malformed or tampered (see [`MigrationError`]).
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// An underlying SQLite error (schema, query, transaction).
    #[error("sqlite: {0}")]
    Sqlite(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn table_exists(db: &Db, name: &str) -> bool {
        db.conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .unwrap_or(false)
    }

    use rusqlite::OptionalExtension;

    #[test]
    fn migrate_is_forward_only_and_idempotent() {
        let mut db = Db::open(&MemorySource).unwrap();
        // First call applies the whole System set (v1 skeleton + v2 audit chain t76 + v3 identity
        // t45 + v4 sessions t46).
        let applied = migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        assert_eq!(
            applied,
            vec![1, 2, 3, 4],
            "first migrate applies every pending version"
        );
        // Second call on the SAME db is a verified no-op (re-verifies the checksum, re-applies none).
        let again = migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        assert!(again.is_empty(), "relaunch re-applies nothing: {again:?}");
        // The ledger records every applied migration, in order.
        let ledger = applied_migrations(&db).unwrap();
        assert_eq!(ledger.len(), SYSTEM_MIGRATIONS.len());
        assert_eq!(ledger[0].version, 1);
        assert!(!ledger[0].applied_at.is_empty(), "applied_at is stamped");
        assert_eq!(ledger[0].checksum.len(), 64, "sha256_hex is 64 hex chars");
    }

    #[test]
    fn applied_migrations_on_fresh_db_is_empty_not_an_error() {
        let db = Db::open(&MemorySource).unwrap();
        assert!(applied_migrations(&db).unwrap().is_empty());
    }

    #[test]
    fn non_contiguous_migration_list_is_rejected() {
        let mut db = Db::open(&MemorySource).unwrap();
        let bad = &[Migration {
            version: 2, // must start at 1
            name: "gap",
            sql: "CREATE TABLE t (id INTEGER);",
        }];
        let err = migrate(&mut db, bad).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Migration(MigrationError::NonContiguous { .. })
            ),
            "got {err:?}"
        );
        // Nothing was applied — the set is validated before touching the DB.
        assert!(!table_exists(&db, "t"));
    }

    #[test]
    fn editing_a_shipped_migration_in_place_is_a_checksum_mismatch() {
        let mut db = Db::open(&MemorySource).unwrap();
        let v1_a = &[Migration {
            version: 1,
            name: "x",
            sql: "CREATE TABLE x (a INTEGER);",
        }];
        migrate(&mut db, v1_a).unwrap();
        // Same version, DIFFERENT body — simulates editing a shipped migration in place.
        let v1_b = &[Migration {
            version: 1,
            name: "x",
            sql: "CREATE TABLE x (a INTEGER, b INTEGER);",
        }];
        let err = migrate(&mut db, v1_b).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Migration(MigrationError::ChecksumMismatch { version: 1, .. })
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn partial_migration_rolls_back_on_failure() {
        let mut db = Db::open(&MemorySource).unwrap();
        // Second statement is invalid SQL; the whole migration must roll back, leaving the first
        // statement's table absent and NO schema_version row.
        let bad = &[Migration {
            version: 1,
            name: "boom",
            sql: "CREATE TABLE good (id INTEGER); CREATE TABLE ;",
        }];
        assert!(migrate(&mut db, bad).is_err());
        assert!(!table_exists(&db, "good"), "failed migration rolled back");
        assert!(applied_migrations(&db).unwrap().is_empty());
    }

    #[test]
    fn system_db_open_creates_the_skeleton_tables() {
        let sys = SystemDb::open(&MemorySource).unwrap();
        assert!(table_exists(sys.db(), "projects"));
        assert!(table_exists(sys.db(), "system_config"));
        // t76 migration #2: the audit chain head + bounded live-tail buffer.
        assert!(table_exists(sys.db(), "audit_chain_head"));
        assert!(table_exists(sys.db(), "audit_tail"));
        // t45 migration #3: the identity tables.
        assert!(table_exists(sys.db(), "users"));
        assert!(table_exists(sys.db(), "accounts"));
        // t46 migration #4: the sessions table (keyed by the token hash, never the plaintext).
        assert!(table_exists(sys.db(), "sessions"));
        assert_eq!(applied_migrations(sys.db()).unwrap().len(), 4);
    }

    #[test]
    fn project_db_open_creates_the_skeleton_tables() {
        let proj = ProjectDb::open(&MemorySource).unwrap();
        assert!(table_exists(proj.db(), "connections"));
        assert!(table_exists(proj.db(), "project_config"));
        assert!(table_exists(proj.db(), "project_state"));
        // t43 migration #2: the envelope-encrypted credential store + active-account tables.
        assert!(table_exists(proj.db(), "secret_store"));
        assert!(table_exists(proj.db(), "secret_meta"));
        assert!(table_exists(proj.db(), "active_account"));
        // Both project migrations are recorded.
        assert_eq!(applied_migrations(proj.db()).unwrap().len(), 2);
    }

    #[test]
    fn project_db_into_db_into_connection_yields_a_migrated_owned_connection() {
        // The t43 seam: a migrated ProjectDb hands its OWNED rusqlite::Connection to a backend.
        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();
        // The owned connection sees the migrated schema (a write into secret_meta succeeds).
        conn.execute(
            "INSERT INTO secret_meta (id, wrapped_dek, kdf_salt) VALUES (1, x'00', x'01')",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM secret_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn embedded_migration_sets_are_contiguous_from_one() {
        for set in [SYSTEM_MIGRATIONS, PROJECT_MIGRATIONS] {
            for (i, m) in set.iter().enumerate() {
                assert_eq!(
                    m.version as usize,
                    i + 1,
                    "migration set must be 1..=N contiguous"
                );
            }
        }
    }

    #[test]
    fn file_source_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("system.db");
        // Open + migrate, then drop.
        {
            let sys = SystemDb::open(&FileSource::new(&path)).unwrap();
            sys.db()
                .conn()
                .execute("INSERT INTO projects (slug) VALUES ('p1')", [])
                .unwrap();
        }
        // Reopen: migrations are a no-op and the row is still there.
        let sys2 = SystemDb::open(&FileSource::new(&path)).unwrap();
        let n: i64 = sys2
            .db()
            .conn()
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "data persisted across reopen");
        assert_eq!(applied_migrations(sys2.db()).unwrap().len(), 4);
    }
}
