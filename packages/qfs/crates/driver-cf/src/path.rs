//! [`CfNode`] — the parse of a qfs [`Path`](qfs_driver::Path) into the concrete Cloudflare node
//! it names (RFD-0001 §4/§5: "the path is the type"). One `/cf` mount fans out to three
//! Cloudflare primitives, each on its own archetype:
//!
//! - **D1** (`/cf/d1/<db>/<table>`) — the relational/table archetype (SQLite-over-HTTP). The
//!   `<db>` is the D1 database name (and the `Secrets` account selector); the `<table>` is a
//!   relation reached by the t17 sqlite emitter.
//! - **KV** (`/cf/kv/<ns>/<key>`) — the blob/namespace archetype. `<ns>` is a KV namespace;
//!   `<key>` (optional) names a single entry. The namespace alone is the `ls`/key-value-table
//!   collection node.
//! - **Queues** (`/cf/queue/<name>`) — the append/log archetype. `<name>` is the queue name;
//!   `INSERT` appends, `SELECT … LIMIT n` tails.
//!
//! Pure parsing only — no I/O, no vendor type crosses.

use qfs_driver::Path;

use crate::error::CfError;

/// The mount this driver answers for. The three Cloudflare services live under `/cf/d1`,
/// `/cf/kv`, and `/cf/queue` respectively.
pub const MOUNT: &str = "/cf";

/// The `/cf/d1` service segment.
pub const D1_SEGMENT: &str = "d1";
/// The `/cf/kv` service segment.
pub const KV_SEGMENT: &str = "kv";
/// The `/cf/queue` service segment.
pub const QUEUE_SEGMENT: &str = "queue";

/// A parsed Cloudflare address — what a `/cf/...` path resolves to (RFD §4 "the path is the
/// type"). Owned, vendor-free. The introspective methods and the applier branch on this.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CfNode {
    /// `/cf` — the virtual root (lists the three services). Not itself queryable.
    Root,
    /// `/cf/d1/<db>` — a D1 database (lists its tables; not itself a relation).
    D1Db {
        /// The D1 database name (and the Secrets account selector).
        db: String,
    },
    /// `/cf/d1/<db>/<table>` — a concrete D1 table (the relational node).
    D1Table {
        /// The D1 database name.
        db: String,
        /// The table name.
        table: String,
    },
    /// `/cf/kv/<ns>` — a KV namespace (the `ls`/key-value-table collection node).
    KvNamespace {
        /// The KV namespace.
        ns: String,
    },
    /// `/cf/kv/<ns>/<key>` — a single KV entry addressed by key.
    KvKey {
        /// The KV namespace.
        ns: String,
        /// The entry key.
        key: String,
    },
    /// `/cf/queue/<name>` — a Cloudflare Queue (the append/log node).
    Queue {
        /// The queue name.
        name: String,
    },
}

impl CfNode {
    /// Parse a driver [`Path`] into a [`CfNode`].
    ///
    /// # Errors
    /// [`CfError::InvalidPath`] if the path is not under `/cf`, names an unknown service, or
    /// carries more segments than the service addressing allows.
    pub fn parse(path: &Path) -> Result<Self, CfError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into a [`CfNode`] (the core parse).
    ///
    /// # Errors
    /// [`CfError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, CfError> {
        let trimmed = raw.trim_end_matches('/');
        if trimmed == MOUNT || raw == MOUNT {
            return Ok(CfNode::Root);
        }
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(CfError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /cf mount",
            });
        };
        let segments: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            [] => Ok(CfNode::Root),
            [service]
                if *service == D1_SEGMENT
                    || *service == KV_SEGMENT
                    || *service == QUEUE_SEGMENT =>
            {
                // A bare service segment is not addressable on its own; the AI must name a
                // db/namespace/queue.
                Err(CfError::InvalidPath {
                    path: raw.to_string(),
                    reason: "a /cf service needs a target (e.g. /cf/d1/<db>/<table>, \
                             /cf/kv/<ns>, /cf/queue/<name>)",
                })
            }
            // D1: /cf/d1/<db> or /cf/d1/<db>/<table>.
            [svc, db] if *svc == D1_SEGMENT => Ok(CfNode::D1Db {
                db: (*db).to_string(),
            }),
            [svc, db, table] if *svc == D1_SEGMENT => Ok(CfNode::D1Table {
                db: (*db).to_string(),
                table: (*table).to_string(),
            }),
            // KV: /cf/kv/<ns> or /cf/kv/<ns>/<key> (the key may itself contain slashes; we keep
            // it whole by re-joining the tail).
            [svc, ns] if *svc == KV_SEGMENT => Ok(CfNode::KvNamespace {
                ns: (*ns).to_string(),
            }),
            [svc, ns, ..] if *svc == KV_SEGMENT => Ok(CfNode::KvKey {
                ns: (*ns).to_string(),
                key: segments[2..].join("/"),
            }),
            // Queues: /cf/queue/<name>.
            [svc, name] if *svc == QUEUE_SEGMENT => Ok(CfNode::Queue {
                name: (*name).to_string(),
            }),
            [svc, ..] if *svc == D1_SEGMENT => Err(CfError::InvalidPath {
                path: raw.to_string(),
                reason: "a /cf/d1 path is /cf/d1/<db>[/<table>]",
            }),
            [svc, ..] if *svc == QUEUE_SEGMENT => Err(CfError::InvalidPath {
                path: raw.to_string(),
                reason: "a /cf/queue path is /cf/queue/<name>",
            }),
            _ => Err(CfError::InvalidPath {
                path: raw.to_string(),
                reason: "unknown /cf service (expected d1, kv, or queue)",
            }),
        }
    }

    /// The Cloudflare account/db selector this address keys credential resolution on, if any.
    /// For D1 it is the `<db>`; for KV the `<ns>`; for Queues the queue `<name>`. The root has
    /// none.
    #[must_use]
    pub fn account_selector(&self) -> Option<&str> {
        match self {
            CfNode::D1Db { db } | CfNode::D1Table { db, .. } => Some(db.as_str()),
            CfNode::KvNamespace { ns } | CfNode::KvKey { ns, .. } => Some(ns.as_str()),
            CfNode::Queue { name } => Some(name.as_str()),
            CfNode::Root => None,
        }
    }
}
