//! The Project-DB **defined-path binding** registry I/O (EPIC 20260701100000 / t100020 — the
//! `CONNECT` model). Free functions over a `rusqlite::Connection` so BOTH surfaces write the ONE
//! source of truth (the `path_binding` table, migration #8):
//!
//! - the query-language `CONNECT`/`DISCONNECT` statement (parser-desugared to a `/sys/paths`
//!   effect, applied by the runtime [`crate::sys`] backend), and
//! - the `qfs connect` / `qfs disconnect` / `qfs connect --list` CLI (direct DB I/O,
//!   writing selectors + secret REFERENCES only).
//!
//! A **defined path** is a user-chosen PATH that MOUNTS a connection: a FULL connect binds
//! `/<path>` to a `(driver, at-locator, secret-ref)`; an ALIAS binds `/<path>` to another defined
//! path (reusing its connection). The project DB is the SINGLE SOURCE OF TRUTH — there is NO
//! `connections.qfs` config file.
//!
//! ## Redaction (roadmap §3.2)
//! SELECTORS + METADATA ONLY. `secret_ref` is a REFERENCE (`env:VAR` / `vault:driver/connection`)
//! resolved at USE time ([`crate::secret_ref::resolve_secret_ref`]), NEVER a secret value: an
//! `env:` ref reads the environment at use; a `vault:` ref points at the envelope-encrypted
//! `secret_store`. An unresolvable ref leaves the path DEFINED but FAIL-CLOSED at read time.

use rusqlite::{Connection, OptionalExtension};

/// One row of the defined-path binding registry — selectors + metadata ONLY (no secret value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathBindingRow {
    /// The user-defined path (the mount point), canonicalized as `/a/b/c`.
    pub path: String,
    /// The canonical driver id for a FULL connect; `None` for an ALIAS row.
    pub driver_id: Option<String>,
    /// The non-secret connection locator (the `AT` clause); `None` when absent / for an alias.
    pub at_locator: Option<String>,
    /// The secret REFERENCE (`env:VAR` / `vault:driver/connection`); `None` when absent / for an
    /// alias. NEVER a secret value.
    pub secret_ref: Option<String>,
    /// For an ALIAS row: the target defined `path` this row reuses; `None` for a full connect.
    pub alias_of: Option<String>,
    /// Which qfs host owns the mount (ADR 0008 §1); `'local'` is the implicit embedded host. An
    /// alias inherits its target's coordinate, so an alias row keeps the default.
    pub host: String,
    /// The service-account LABEL the mount binds (ADR 0008 §4 — the mount carries the account,
    /// e.g. a Google email). `None` for a local source with no account / for an alias. NEVER a
    /// token — the credential stays in the vault, keyed by this label.
    pub account: Option<String>,
    /// When the binding was created (RFC 3339).
    pub created_at: String,
}

/// UPSERT a FULL-connect binding (`CONNECT /<path> TO <driver> [AT …] [SECRET …]`): last-writer-wins
/// per `path`. Clears any `alias_of` (a full connect is not an alias). Returns rows written (1).
///
/// # Errors
/// The underlying `rusqlite::Error` on a DB failure (the caller maps it to its own error shape).
pub fn db_upsert_binding(
    conn: &Connection,
    path: &str,
    driver_id: &str,
    at_locator: Option<&str>,
    secret_ref: Option<&str>,
    host: Option<&str>,
    account: Option<&str>,
) -> Result<u64, rusqlite::Error> {
    // The mount coordinate (ADR 0008): an absent host means the implicit embedded host.
    let host = host.unwrap_or("local");
    let n = conn.execute(
        "INSERT INTO path_binding (path, driver_id, at_locator, secret_ref, alias_of, host, account) \
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6) \
         ON CONFLICT(path) DO UPDATE SET \
             driver_id  = excluded.driver_id, \
             at_locator = excluded.at_locator, \
             secret_ref = excluded.secret_ref, \
             alias_of   = NULL, \
             host       = excluded.host, \
             account    = excluded.account, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![path, driver_id, at_locator, secret_ref, host, account],
    )?;
    Ok(n as u64)
}

/// UPSERT an ALIAS binding (`CONNECT /<path> TO /<existing-path>`): `path` reuses `target`'s
/// connection. Clears any driver/locator/secret (an alias carries none). Returns rows written (1).
///
/// # Errors
/// The underlying `rusqlite::Error` — including a foreign-key violation when `target` does not name
/// an existing defined path (the alias target must exist; fail-closed).
pub fn db_upsert_alias(
    conn: &Connection,
    path: &str,
    target: &str,
) -> Result<u64, rusqlite::Error> {
    let n = conn.execute(
        "INSERT INTO path_binding (path, driver_id, at_locator, secret_ref, alias_of, host, account) \
             VALUES (?1, NULL, NULL, NULL, ?2, 'local', NULL) \
         ON CONFLICT(path) DO UPDATE SET \
             driver_id  = NULL, \
             at_locator = NULL, \
             secret_ref = NULL, \
             alias_of   = excluded.alias_of, \
             host       = 'local', \
             account    = NULL, \
             created_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
        rusqlite::params![path, target],
    )?;
    Ok(n as u64)
}

/// Remove a defined path (`DISCONNECT /<path>`). Idempotent (removing an absent path affects 0
/// rows). Its aliases cascade (the `alias_of` FK is `ON DELETE CASCADE`; `PRAGMA foreign_keys=ON`
/// is set by the store). Returns rows removed (excluding cascaded aliases).
///
/// # Errors
/// The underlying `rusqlite::Error` on a DB failure.
pub fn db_remove_binding(conn: &Connection, path: &str) -> Result<u64, rusqlite::Error> {
    let n = conn.execute(
        "DELETE FROM path_binding WHERE path = ?1",
        rusqlite::params![path],
    )?;
    Ok(n as u64)
}

/// List every defined-path binding (metadata only), ordered by `path`. The passphrase-free read the
/// registration path (t100040), `qfs connect --list`, and `/sys/paths` all consult.
///
/// # Errors
/// The underlying `rusqlite::Error` on a DB failure.
pub fn db_list_bindings(conn: &Connection) -> Result<Vec<PathBindingRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT path, driver_id, at_locator, secret_ref, alias_of, host, account, created_at \
         FROM path_binding ORDER BY path",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PathBindingRow {
                path: r.get(0)?,
                driver_id: r.get(1)?,
                at_locator: r.get(2)?,
                secret_ref: r.get(3)?,
                alias_of: r.get(4)?,
                host: r.get(5)?,
                account: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Read a single defined-path binding by `path`, or `None` if unbound.
///
/// # Errors
/// The underlying `rusqlite::Error` on a DB failure (a missing row is `Ok(None)`, not an error).
pub fn db_get_binding(
    conn: &Connection,
    path: &str,
) -> Result<Option<PathBindingRow>, rusqlite::Error> {
    conn.query_row(
        "SELECT path, driver_id, at_locator, secret_ref, alias_of, host, account, created_at \
         FROM path_binding WHERE path = ?1",
        rusqlite::params![path],
        |r| {
            Ok(PathBindingRow {
                path: r.get(0)?,
                driver_id: r.get(1)?,
                at_locator: r.get(2)?,
                secret_ref: r.get(3)?,
                alias_of: r.get(4)?,
                host: r.get(5)?,
                account: r.get(6)?,
                created_at: r.get(7)?,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_store::{migrate, Db, MemorySource, PROJECT_MIGRATIONS};

    fn migrated() -> Connection {
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, PROJECT_MIGRATIONS).unwrap();
        db.into_connection()
    }

    #[test]
    fn full_connect_upserts_and_reads_back() {
        let conn = migrated();
        db_upsert_binding(
            &conn,
            "/work/orders",
            "postgres",
            Some("postgres://db/orders"),
            Some("env:PG_PASS"),
            None,
            None,
        )
        .unwrap();
        let row = db_get_binding(&conn, "/work/orders").unwrap().unwrap();
        assert_eq!(row.driver_id.as_deref(), Some("postgres"));
        assert_eq!(row.at_locator.as_deref(), Some("postgres://db/orders"));
        assert_eq!(row.secret_ref.as_deref(), Some("env:PG_PASS"));
        assert_eq!(row.alias_of, None);
        // ADR 0008: an omitted host is the implicit embedded host; a local source has no account.
        assert_eq!(row.host, "local");
        assert_eq!(row.account, None);
    }

    /// ADR 0008 §4 — the mount carries the (host, driver, account) coordinate, and two accounts of
    /// one driver coexist as two paths (what the abolished `active_account` selection can't do).
    #[test]
    fn mount_coordinate_round_trips_and_two_accounts_coexist() {
        let conn = migrated();
        db_upsert_binding(
            &conn,
            "/mail",
            "gmail",
            None,
            None,
            None,
            Some("you@work.example"),
        )
        .unwrap();
        db_upsert_binding(
            &conn,
            "/mail-priv",
            "gmail",
            None,
            None,
            Some("local"),
            Some("me@personal.example"),
        )
        .unwrap();
        let work = db_get_binding(&conn, "/mail").unwrap().unwrap();
        let priv_ = db_get_binding(&conn, "/mail-priv").unwrap().unwrap();
        assert_eq!(work.account.as_deref(), Some("you@work.example"));
        assert_eq!(priv_.account.as_deref(), Some("me@personal.example"));
        assert_eq!(work.host, "local");
        assert_eq!(priv_.host, "local");
        // Re-connecting a path replaces its coordinate (last-writer-wins on the mount).
        db_upsert_binding(&conn, "/mail", "gmail", None, None, None, None).unwrap();
        assert_eq!(
            db_get_binding(&conn, "/mail").unwrap().unwrap().account,
            None
        );
    }

    #[test]
    fn alias_reuses_a_connection_and_cascades_on_remove() {
        let conn = migrated();
        db_upsert_binding(&conn, "/work/orders", "postgres", None, None, None, None).unwrap();
        db_upsert_alias(&conn, "/db", "/work/orders").unwrap();
        assert_eq!(
            db_get_binding(&conn, "/db")
                .unwrap()
                .unwrap()
                .alias_of
                .as_deref(),
            Some("/work/orders")
        );
        // Removing the target cascades its aliases (FK ON DELETE CASCADE).
        db_remove_binding(&conn, "/work/orders").unwrap();
        assert!(db_get_binding(&conn, "/db").unwrap().is_none());
        assert!(db_list_bindings(&conn).unwrap().is_empty());
    }

    #[test]
    fn alias_to_a_missing_target_is_rejected_fail_closed() {
        let conn = migrated();
        // The alias target must be an existing defined path (FK) — a dangling alias is refused.
        assert!(db_upsert_alias(&conn, "/db", "/nope").is_err());
    }

    #[test]
    fn disconnect_is_idempotent() {
        let conn = migrated();
        assert_eq!(db_remove_binding(&conn, "/gone").unwrap(), 0);
    }
}
