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
pub mod telemetry;
pub mod worm;

mod identity_store;
pub use identity_store::SqliteIdentityStore;

mod invite_store;
pub use invite_store::SqliteInviteStore;

mod session_store;
pub use session_store::SqliteSessionStore;

mod oauth_key_store;
pub use oauth_key_store::{OauthKeyStore, StoredSigningKey};

mod oauth_store;
pub use oauth_store::{RedeemedCode, RedeemedRefresh, RegisteredClient, SqliteOauthFlowStore};

mod oidc_provider_store;
pub use oidc_provider_store::{NewOidcProvider, OidcProviderRecord, SqliteOidcProviderStore};

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
    // t48 (roadmap M2): the OAuth authorization-server signing keys — `oauth_keys` (one row per AS
    // signing keypair, keyed by its RFC 7638 `kid`; the PRIVATE key envelope-encrypted at rest under
    // a System-DB data-key, the PUBLIC JWK stored in the clear for `/jwks.json`) + the single-row
    // `oauth_key_meta` wrapped-DEK envelope (mirroring the Project DB's `secret_meta`, t43). Appended
    // as a NEW version (#5) — migrations #1–#4 stay frozen (the checksum guard forbids editing a
    // shipped migration). The rusqlite `OauthKeyStore` impl that fills these lives in
    // `oauth_key_store.rs`. KEY PUBLICATION ONLY: t48 publishes the discovery docs + JWKS and issues
    // NO tokens yet (token minting is t49/t50).
    Migration {
        version: 5,
        name: "system_oauth_keys",
        sql: include_str!("schema/system_oauth_keys.sql"),
    },
    // t49 (roadmap M2): the OAuth authorization-server FLOW state — `oauth_clients` (RFC 7591
    // dynamically-registered clients with their exact redirect-URI allowlist), `oauth_codes`
    // (short-lived, single-use authorization codes keyed by a HASH, bound to client + redirect +
    // PKCE challenge + user), and the `oauth_refresh_tokens` handle skeleton (issued here, enforced
    // in t50). Appended as a NEW version (#6) — migrations #1–#5 stay frozen (the checksum guard
    // forbids editing a shipped migration). The rusqlite `SqliteOauthFlowStore` impl that fills these
    // lives in `oauth_store.rs`. Codes/handles/secrets are stored ONLY as hashes (RFD §10).
    Migration {
        version: 6,
        name: "system_oauth_clients",
        sql: include_str!("schema/system_oauth_clients.sql"),
    },
    // t53 (roadmap §3.4 / M3): the `/sys/policies` grant rows the administration driver reads and
    // (gated) writes. Appended as a NEW version (#7) — migrations #1–#6 stay frozen (the checksum
    // guard forbids editing a shipped migration). The rusqlite read/write that fills these columns
    // lives in the binary-injected `SysBackend` (`crates/qfs/src/sys.rs`); this declares the shape.
    Migration {
        version: 7,
        name: "system_policies",
        sql: include_str!("schema/system_policies.sql"),
    },
    // t55 (roadmap M5): invites + memberships — the JOINING half of decision B. `invites` holds
    // one-time, expiring invitations (the token stored ONLY as a `sha256` digest, like sessions /
    // password hashes) and `memberships` links a redeemed user to the host (or a project). Appended
    // as a NEW version (#8) — migrations #1–#7 stay frozen (the checksum guard forbids editing a
    // shipped migration). The rusqlite `SqliteInviteStore` impl that fills these lives in
    // `invite_store.rs`. MEMBERSHIP ONLY: a row here says "belongs", never "may do X" (§4.1; the ACL
    // is t57).
    Migration {
        version: 8,
        name: "system_invites",
        sql: include_str!("schema/system_invites.sql"),
    },
    // t56 (roadmap M5): upstream OIDC federation providers — the "hub model" RP registration store.
    // `oidc_providers` records each UPSTREAM IdP a host trusts for human login (issuer, RP client id,
    // the upstream client secret ENVELOPE-ENCRYPTED at rest, and the cached discovery/JWKS) +
    // `oidc_provider_meta` is the single-row wrapped-DEK envelope (mirroring `oauth_key_meta`, t48).
    // Appended as a NEW version (#9) — migrations #1–#8 stay frozen (the checksum guard forbids
    // editing a shipped migration). The rusqlite `SqliteOidcProviderStore` impl that fills these lives
    // in `oidc_provider_store.rs`. AUTHENTICATION ONLY: a trusted issuer grants identity, not
    // privilege (§4.1; the ACL is t57).
    Migration {
        version: 9,
        name: "system_oidc_providers",
        sql: include_str!("schema/system_oidc_providers.sql"),
    },
    // t59 (roadmap §2.4 / M5): the deployment SETTINGS key/value the `/sys/settings` admin path
    // reads and (gated) writes — the home of the selectable AI safety mode (decision J), stored as
    // data so it is describable / committable through one-engine-three-faces. Appended as a NEW
    // version (#10) — migrations #1–#9 stay frozen (the checksum guard forbids editing a shipped
    // migration). The rusqlite read/write that fills these columns lives in the binary-injected
    // `SysBackend` (`crates/qfs/src/sys.rs`); this declares the shape. The setting CONFIGURES the
    // safety floor, it never lowers it (an unset/garbled value resolves to the safe default).
    Migration {
        version: 10,
        name: "system_settings",
        sql: include_str!("schema/system_settings.sql"),
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
    // t54 (roadmap M4): the cloud-connection consent ledger (`connection_consent`) — selectors +
    // metadata only, recording that a signed-in operator granted a cloud connection explicit
    // consent. Appended as a NEW version (#3) — migrations #1–#2 stay frozen (the checksum guard
    // forbids editing a shipped migration). The passphrase-free read/write that fills these columns
    // lives in the binary (`crates/qfs/src/secret_store.rs`); this declares the shape.
    Migration {
        version: 3,
        name: "project_connection_consent",
        sql: include_str!("schema/project_consent.sql"),
    },
    // t81 (roadmap M5, decision U / §3.3): the project/team-owned (shared) connection registry
    // (`shared_connection`) — a row marks a connection PROJECT-owned and records the realm `scope`
    // the acting member's actor-policy must grant to USE it. Selectors + metadata only; the
    // credential stays encrypted in `secret_store` (migration #2). Appended as a NEW version (#4) —
    // migrations #1–#3 stay frozen (the checksum guard forbids editing a shipped migration). The
    // passphrase-free read/write that fills these columns lives in the binary
    // (`crates/qfs/src/secret_store.rs`); this declares the shape.
    Migration {
        version: 4,
        name: "project_shared_connection",
        sql: include_str!("schema/project_shared_connections.sql"),
    },
    // t79 (roadmap M5, decision U / §4.5): credential ROTATION & REVOCATION columns on
    // `secret_store` (`last_rotated`, `revoked_at`). A rotation re-mints the secret + re-wraps the
    // DEK; a revocation marks the connection unresolvable (the bind refuses to decrypt it). Appended
    // as a NEW version (#5) that ALTERs the table forward — migrations #1–#4 stay frozen (the
    // checksum guard forbids editing a shipped migration). Plaintext metadata columns only; the
    // at-rest envelope crypto is unchanged. The rotation/revocation I/O that fills these columns
    // lives in the binary (`crates/qfs/src/secret_store.rs`); this declares the shape.
    Migration {
        version: 5,
        name: "project_rotation_revocation",
        sql: include_str!("schema/project_rotation.sql"),
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
    /// An envelope-encrypted store (t48 OAuth keys) could not be unlocked — the passphrase is wrong
    /// or the wrapped-DEK metadata was tampered. Value-free: it names no key material (RFD §10).
    #[error("the encrypted store could not be unlocked (wrong passphrase or tampered metadata)")]
    Locked,
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

    /// Whether `table` has a column named `column` (via `pragma_table_info`). Used to assert an
    /// `ALTER TABLE ADD COLUMN` migration landed (t79).
    fn column_exists(db: &Db, table: &str, column: &str) -> bool {
        db.conn()
            .query_row(
                "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
                [table, column],
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
        // t45 + v4 sessions t46 + v5 oauth keys t48 + v6 oauth flow clients/codes t49 + v7
        // /sys/policies t53).
        let applied = migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        assert_eq!(
            applied,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
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
        // t48 migration #5: the OAuth signing keys + the wrapped-DEK envelope meta.
        assert!(table_exists(sys.db(), "oauth_keys"));
        assert!(table_exists(sys.db(), "oauth_key_meta"));
        // t49 migration #6: the OAuth flow tables (clients + codes + refresh handles).
        assert!(table_exists(sys.db(), "oauth_clients"));
        assert!(table_exists(sys.db(), "oauth_codes"));
        assert!(table_exists(sys.db(), "oauth_refresh_tokens"));
        // t53 migration #7: the /sys/policies grant rows.
        assert!(table_exists(sys.db(), "sys_policies"));
        // t55 migration #8: the invites + memberships tables.
        assert!(table_exists(sys.db(), "invites"));
        assert!(table_exists(sys.db(), "memberships"));
        // t56 migration #9: the upstream OIDC federation provider registry + its envelope meta.
        assert!(table_exists(sys.db(), "oidc_providers"));
        assert!(table_exists(sys.db(), "oidc_provider_meta"));
        // t59 migration #10: the /sys/settings deployment key/value (the safety-mode home).
        assert!(table_exists(sys.db(), "sys_settings"));
        assert_eq!(applied_migrations(sys.db()).unwrap().len(), 10);
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
        // t54 migration #3: the cloud-connection consent ledger.
        assert!(table_exists(proj.db(), "connection_consent"));
        // t81 migration #4: the project/team-owned (shared) connection registry.
        assert!(table_exists(proj.db(), "shared_connection"));
        // t79 migration #5: rotation/revocation columns on secret_store (last_rotated, revoked_at).
        assert!(column_exists(proj.db(), "secret_store", "last_rotated"));
        assert!(column_exists(proj.db(), "secret_store", "revoked_at"));
        // All five project migrations are recorded.
        assert_eq!(applied_migrations(proj.db()).unwrap().len(), 5);
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
        assert_eq!(applied_migrations(sys2.db()).unwrap().len(), 10);
    }

    #[test]
    fn system_invites_migration_v8_applies_idempotently() {
        // t55: migration #8 is idempotent — opening the System DB twice applies it once, re-verifies
        // it the second time (checksum), and the invites/memberships tables + their indexes exist.
        let mut db = Db::open(&MemorySource).unwrap();
        let applied = migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        assert!(applied.contains(&8), "v8 applied on the first migrate");
        // A relaunch re-applies nothing (the v8 body is re-verified by checksum, not re-run).
        assert!(migrate(&mut db, SYSTEM_MIGRATIONS).unwrap().is_empty());
        assert!(table_exists(&db, "invites"));
        assert!(table_exists(&db, "memberships"));
        // The unique index that makes "is a member of (scope, project)" singular is present.
        let idx_exists: bool = db
            .conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='memberships_user_scope_project'",
                [],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .unwrap_or(false);
        assert!(idx_exists, "the membership uniqueness index was created");
    }
}
