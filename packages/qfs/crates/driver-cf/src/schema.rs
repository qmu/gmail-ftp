//! The fixed typed [`Schema`]s for the two non-D1 Cloudflare archetypes (RFD-0001 §4/§5). D1's
//! schema is the catalog-derived `qfs_types::Schema` from the reused t17 catalog; KV and Queues
//! have a fixed shape:
//!
//! - **KV** is the degenerate two-column `(key, value)` table view (so `SELECT`/`UPSERT` work
//!   over a key-value namespace as a relation).
//! - **Queues** is the append/log tail row `(id, body, attempts)` a bounded `SELECT` yields.

use qfs_types::{Column, ColumnType, Schema};

/// The KV-as-table view schema: `(key TEXT, value TEXT)`. The degenerate two-column relation a
/// `SELECT`/`UPSERT` over `/cf/kv/<ns>` reads/writes.
#[must_use]
pub fn kv_table_schema() -> Schema {
    Schema::new(vec![
        Column::new("key", ColumnType::Text, false),
        Column::new("value", ColumnType::Text, true),
    ])
}

/// The Queues tail schema: `(id TEXT, body TEXT, attempts INT)`. The row shape a bounded-tail
/// `SELECT … LIMIT n` over `/cf/queue/<name>` yields (consumer pull / recent messages).
#[must_use]
pub fn queue_tail_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Text, false),
        Column::new("body", ColumnType::Text, true),
        Column::new("attempts", ColumnType::Int, false),
    ])
}
