//! [`SqlPath`] — the parse of a qfs [`Path`](qfs_driver::Path) into the concrete SQL node it
//! names (RFD-0001 §5). A SQL database maps onto the **relational archetype**: a table is a
//! queryable/mutable relation, addressed under the connection it lives in.
//!
//! ## Addressing
//! - `/sql` — the virtual root (no connection selected; lists nothing queryable on its own).
//! - `/sql/<conn>` — a registered connection (lists its tables; not itself a relation).
//! - `/sql/<conn>/<schema>/<table>` — a concrete table/view relation.
//! - `/sql/<conn>/<table>` — a table in the connection's **default** schema (the schema segment
//!   omitted; common for SQLite, which has no schema namespace).
//!
//! The `<conn>` segment is both the connection-registry key and the `Secrets` account selector
//! for credential resolution. Pure parsing only — no I/O, no vendor type crosses.

use qfs_driver::Path;

use crate::error::SqlError;

/// The mount this driver answers for. The virtual root carries no connection; child segments
/// select the connection, schema, and table.
pub const MOUNT: &str = "/sql";

/// A parsed SQL address — what a `/sql/...` path resolves to. Owned, vendor-free. The
/// introspective methods and the compiler/applier branch on this.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqlPath {
    /// `/sql` — the virtual root (no connection selected).
    Root,
    /// `/sql/<conn>` — a registered connection (not itself a relation).
    Connection {
        /// The connection-registry key (and Secrets account selector).
        conn: String,
    },
    /// `/sql/<conn>/<schema>/<table>` or `/sql/<conn>/<table>` — a concrete relation.
    Table {
        /// The connection key.
        conn: String,
        /// The schema name (empty for the connection's default schema, e.g. SQLite).
        schema: String,
        /// The table (or view) name.
        table: String,
    },
}

impl SqlPath {
    /// Parse a driver [`Path`] into a [`SqlPath`].
    ///
    /// # Errors
    /// [`SqlError::InvalidPath`] if the path is not under `/sql`, names an empty segment, or
    /// carries more segments than `<conn>/<schema>/<table>`.
    pub fn parse(path: &Path) -> Result<Self, SqlError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into a [`SqlPath`] (the core parse).
    ///
    /// # Errors
    /// [`SqlError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, SqlError> {
        let trimmed = raw.trim_end_matches('/');
        if trimmed == MOUNT || raw == MOUNT {
            return Ok(SqlPath::Root);
        }
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(SqlError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /sql mount",
            });
        };

        let segments: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            [] => Ok(SqlPath::Root),
            [conn] => Ok(SqlPath::Connection {
                conn: (*conn).to_string(),
            }),
            [conn, table] => Ok(SqlPath::Table {
                conn: (*conn).to_string(),
                schema: String::new(),
                table: (*table).to_string(),
            }),
            [conn, schema, table] => Ok(SqlPath::Table {
                conn: (*conn).to_string(),
                schema: (*schema).to_string(),
                table: (*table).to_string(),
            }),
            _ => Err(SqlError::InvalidPath {
                path: raw.to_string(),
                reason: "a /sql path is /sql/<conn>[/<schema>]/<table>",
            }),
        }
    }

    /// The connection key this address selects, if any. `None` for the virtual root.
    #[must_use]
    pub fn conn(&self) -> Option<&str> {
        match self {
            SqlPath::Connection { conn } | SqlPath::Table { conn, .. } => Some(conn.as_str()),
            SqlPath::Root => None,
        }
    }

    /// Whether this address selects a concrete table/view (the queryable/mutable node). The root
    /// and a bare connection are not relations on their own.
    #[must_use]
    pub const fn is_table(&self) -> bool {
        matches!(self, SqlPath::Table { .. })
    }
}
