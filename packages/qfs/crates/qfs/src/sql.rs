//! The `qfs run`/commit SQL composition root: the production, file-backed SQLite [`SqlBackend`]
//! plus the live [`ConnRegistry`] the binary plans + commits `/sql/<conn>/...` statements against.
//!
//! ## Why the backend lives in the binary (not a leaf crate)
//! `qfs-driver-sql` is the vendor-free driver (the `SqlBackend` trait + the dialect-rendered
//! compiler) and is a **`qfs-runtime` consumer** (its `sql_apply_driver` bridges to the runtime).
//! The dep_direction guard requires every runtime consumer to be a **leaf** — only the terminal
//! `qfs` binary may depend onto it — so a *separate* backend crate depending back onto
//! `qfs-driver-sql` would (structurally) un-leaf it. The binary IS that terminal leaf, so the real
//! `rusqlite` engine (and its `libsqlite3` C build) dead-ends here, exactly like tokio + the
//! reqwest transport. `qfs-cmd`/`qfs-exec` stay off both. No `rusqlite` type crosses the
//! `SqlBackend` boundary (owned qfs DTOs only).
//!
//! Guarantees: every value is a **bound** parameter (injection-safe — `'; DROP TABLE` is inert
//! data); a multi-op commit is one **ACID** transaction (`BEGIN -> ops -> COMMIT`, auto-`ROLLBACK`
//! on any error).
//!
//! ## Config (no credentials in argv/logs)
//! Each connection is one env var `QFS_SQL_<CONN>=<path-to-.sqlite>`; the `<CONN>` suffix
//! (lower-cased) is the `/sql/<conn>/...` path segment. A connection whose file cannot be opened or
//! introspected is skipped, so a `/sql/<conn>` commit for an unconfigured conn fails closed.

use std::path::Path;
use std::sync::{Arc, Mutex};

use qfs_driver_sql::{
    render_dml, Catalog, ColumnDef, ConnHandle, ConnRegistry, Dialect, DmlOp, Param, RelationKind,
    SqlBackend, SqlDriver, SqlError, TableCatalog,
};
use qfs_types::{Row, Value};

/// The env-var prefix naming a SQLite connection: `QFS_SQL_<CONN>=<path>`.
const SQL_ENV_PREFIX: &str = "QFS_SQL_";

/// A live SQLite backend wrapping a `rusqlite::Connection` behind a `Mutex` (the connection is
/// `!Sync`; the mutex provides the `Send + Sync` the trait requires). Opens a real database file,
/// so commits persist.
pub struct SqliteBackend {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteBackend {
    /// Open (creating if absent) the SQLite database at `path`.
    fn open(path: impl AsRef<Path>) -> Result<Self, SqlError> {
        let conn = rusqlite::Connection::open(path.as_ref())
            .map_err(|e| SqlError::backend("sqlite", "open", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Bind a `&[Param]` onto a rusqlite statement positionally — the proof that every value is BOUND,
/// never interpolated.
fn bind_params<'a>(params: &'a [Param]) -> Vec<Box<dyn rusqlite::ToSql + 'a>> {
    params
        .iter()
        .map(|p| -> Box<dyn rusqlite::ToSql + 'a> {
            match p {
                Param::Null => Box::new(rusqlite::types::Null),
                Param::Bool(b) => Box::new(*b),
                Param::Int(n) => Box::new(*n),
                Param::Float(f) => Box::new(*f),
                Param::Text(t) => Box::new(t.as_str()),
                Param::Bytes(b) => Box::new(b.as_slice()),
            }
        })
        .collect()
}

/// Convert a rusqlite value reference into the canonical qfs [`Value`] (the owned-DTO boundary —
/// no rusqlite type crosses past here).
fn sqlite_value_to_qfs(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => Value::Int(n),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
    }
}

impl SqlBackend for SqliteBackend {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    fn introspect(&self) -> Result<Catalog, SqlError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SqlError::backend("sqlite", "lock", "poisoned connection mutex"))?;
        let mut tables = Vec::new();
        let mut stmt = conn
            .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
        let rels: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
        drop(stmt);

        for (name, kind) in rels {
            let mut cols = Vec::new();
            let mut info = conn
                .prepare(&format!("PRAGMA table_info(\"{name}\")"))
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
            // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk.
            let rows: Vec<(String, String, i64, i64)> = info
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(|e| SqlError::backend("sqlite", "introspect", e.to_string()))?;
            drop(info);
            for (col_name, ty, notnull, pk) in rows {
                let mapped = Dialect::Sqlite.map_type(&ty);
                let is_pk = pk > 0;
                cols.push(ColumnDef::new(col_name, mapped, notnull == 0, is_pk, is_pk));
            }
            let relkind = if kind == "view" {
                RelationKind::View
            } else {
                RelationKind::Table
            };
            tables.push(TableCatalog::new(name, relkind, cols));
        }
        Ok(Catalog::new(tables))
    }

    fn execute_read(&self, sql: &str, params: &[Param]) -> Result<Vec<Row>, SqlError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SqlError::backend("sqlite", "lock", "poisoned connection mutex"))?;
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
        let col_count = stmt.column_count();
        let boxed = bind_params(params);
        let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
        let mut rows = stmt
            .query(refs.as_slice())
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
        let mut out = Vec::new();
        while let Some(r) = rows
            .next()
            .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?
        {
            let mut values = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v = r
                    .get_ref(i)
                    .map_err(|e| SqlError::backend("sqlite", "select", e.to_string()))?;
                values.push(sqlite_value_to_qfs(v));
            }
            out.push(Row::new(values));
        }
        Ok(out)
    }

    fn commit_transaction(&self, ops: &[DmlOp]) -> Result<u64, SqlError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| SqlError::backend("sqlite", "lock", "poisoned connection mutex"))?;
        let tx = conn
            .transaction()
            .map_err(|e| SqlError::backend("sqlite", "begin", e.to_string()))?;
        let mut affected = 0u64;
        for op in ops {
            let (sql, params) = render_dml(Dialect::Sqlite, op);
            let boxed = bind_params(&params);
            let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
            // On ANY error the `tx` is dropped without commit → automatic ROLLBACK (zero rows
            // changed), satisfying the all-or-nothing guarantee.
            let n = tx
                .execute(&sql, refs.as_slice())
                .map_err(|e| SqlError::backend("sqlite", "dml", e.to_string()))?;
            affected += n as u64;
        }
        tx.commit()
            .map_err(|e| SqlError::backend("sqlite", "commit", e.to_string()))?;
        Ok(affected)
    }
}

/// Build the live connection registry from `QFS_SQL_*` env config. Best-effort: a connection that
/// cannot be opened/introspected is skipped (never panics). Returns an empty registry when nothing
/// is configured.
#[must_use]
pub fn conn_registry() -> ConnRegistry {
    let mut reg = ConnRegistry::new();
    // Declared `CREATE CONNECTION … DRIVER sqlite AT '<path>'` connections (the in-language source
    // of truth) come FIRST; an equally-named `QFS_SQL_*` env var (the deprecated fallback below)
    // then takes precedence on a name clash by overwriting it — keeping pre-existing setups working.
    for decl in crate::connections_config::declared_for("sqlite") {
        let Some(path) = decl.at_locator.as_deref() else {
            continue;
        };
        if let Some(handle) = open_sqlite_handle(path) {
            reg = reg.with(decl.name.to_ascii_lowercase(), handle);
        }
    }
    for (key, path) in std::env::vars() {
        let Some(conn) = key.strip_prefix(SQL_ENV_PREFIX) else {
            continue;
        };
        if conn.is_empty() || path.is_empty() {
            continue;
        }
        let conn = conn.to_ascii_lowercase();
        if let Some(handle) = open_sqlite_handle(&path) {
            reg = reg.with(conn, handle);
        }
    }
    reg
}

/// Open one SQLite file into a [`ConnHandle`] (the shared open path for an env-var or a declared
/// connection). `None` when the file cannot be opened/introspected — best-effort, never panics.
fn open_sqlite_handle(path: &str) -> Option<ConnHandle> {
    let backend: Arc<dyn SqlBackend> = Arc::new(SqliteBackend::open(path).ok()?);
    ConnHandle::new(backend).ok()
}

/// Whether any `/sql` connection is configured (a declared `DRIVER sqlite` connection OR a
/// `QFS_SQL_*` env var) — the binary only registers the sql mount + apply driver when at least one
/// resolves (so an unconfigured `/sql` commit fails closed).
#[must_use]
pub fn has_connections() -> bool {
    std::env::vars().any(|(k, v)| k.starts_with(SQL_ENV_PREFIX) && !v.is_empty())
        || crate::connections_config::declared_for("sqlite")
            .iter()
            .any(|c| c.at_locator.is_some())
}

/// A fresh [`SqlDriver`] over the live registry — the planning mount AND the source the apply
/// driver is built from.
#[must_use]
pub fn sql_driver() -> SqlDriver {
    SqlDriver::new(conn_registry())
}

/// Build a [`SqlDriver`] over a freshly-seeded temp SQLite database under connection `conn` (the
/// `ddl` is executed before catalog introspection). Returns the temp path (so the caller deletes it)
/// and the introspected driver — a test-only helper for the read-facet adapter tests, which need a
/// live catalog without `QFS_SQL_*` env config.
#[cfg(test)]
pub(crate) fn seeded_test_driver(conn: &str, ddl: &str) -> (std::path::PathBuf, SqlDriver) {
    let mut path = std::env::temp_dir();
    path.push(format!("qfs-sqlread-{conn}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let backend = SqliteBackend::open(&path).expect("open temp db");
    {
        let c = backend.conn.lock().expect("lock");
        c.execute_batch(ddl).expect("seed ddl");
    }
    let backend: Arc<dyn SqlBackend> = Arc::new(backend);
    let handle = ConnHandle::new(backend).expect("introspect catalog");
    let registry = ConnRegistry::new().with(conn.to_string(), handle);
    (path, SqlDriver::new(registry))
}

#[cfg(test)]
mod tests {
    //! Real-engine tests against a temp-file SQLite database (not in-memory), proving the
    //! production file-backed path persists, introspects, commits atomically, and reads back.
    use super::*;

    fn temp_db_path() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("qfs-sql-binmod-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn file_backed_introspect_and_read_roundtrip() {
        let path = temp_db_path();
        let backend = SqliteBackend::open(&path).expect("open temp db");
        {
            let conn = backend.conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER);",
            )
            .unwrap();
        }
        let catalog = backend.introspect().expect("introspect");
        let users = catalog
            .tables
            .iter()
            .find(|t| t.name == "users")
            .expect("users catalogued");
        assert_eq!(users.kind, RelationKind::Table);
        assert!(users.columns.iter().any(|c| c.name == "id" && c.pk));
        assert!(users
            .columns
            .iter()
            .any(|c| c.name == "name" && !c.nullable));
        let rows = backend
            .execute_read("SELECT id, name FROM users", &[])
            .expect("read");
        assert!(rows.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
