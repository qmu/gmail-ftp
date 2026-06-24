//! **Source watchers (pollers)** (t34, RFD §8): a [`Watcher`] periodically re-runs a source query
//! through the READ path (pure read), diffs the result against its [`WatcherCursor`], emits an
//! [`Event`] per new/changed row, and persists the cursor — **only AFTER publish** (so a restart
//! re-emits at most a bounded window, never silently skips; at-least-once).
//!
//! The [`WatcherStore`] trait is the DO-backed cursor seam (the pure core names it); the in-process
//! [`MemWatcherStore`] is the EC2 impl. [`Watcher::poll_once`] is `native`-gated (it drives the
//! qfs-exec read path, which pulls tokio).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A watcher's persisted high-water cursor: the set of native ids it has already emitted for a
/// source. Diffing a fresh poll against this yields the NEW ids (the at-least-once window). Owned,
/// serializable (a DO value in the deferred CF impl).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherCursor {
    /// The native ids already emitted for this source (the "seen" set).
    pub seen: BTreeSet<String>,
}

impl WatcherCursor {
    /// An empty cursor (the first-poll starting point).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `native_id` has already been emitted.
    #[must_use]
    pub fn contains(&self, native_id: &str) -> bool {
        self.seen.contains(native_id)
    }

    /// Mark `native_id` emitted.
    pub fn mark(&mut self, native_id: impl Into<String>) {
        self.seen.insert(native_id.into());
    }
}

/// The cursor persistence seam (the DO-backed store, deferred). `load` returns the persisted cursor
/// for a source key (`None` if never polled); `save` durably writes it. The watcher persists the
/// cursor ONLY after the corresponding events are durably published, so a crash before `save`
/// re-emits the window (at-least-once), never skips it.
pub trait WatcherStore: Send + Sync {
    /// Load the persisted cursor for `key` (`None` if never saved).
    fn load(&self, key: &str) -> Option<WatcherCursor>;

    /// Durably persist `cursor` for `key`.
    ///
    /// # Errors
    /// A secret-free message on a persistence failure.
    fn save(&self, key: &str, cursor: &WatcherCursor) -> Result<(), String>;
}

/// The in-process cursor store (the EC2 impl). A `Mutex<BTreeMap>` — sufficient for a single-node
/// deployment; the DO-backed store is the E7 carry-over behind the same trait.
#[derive(Debug, Default)]
pub struct MemWatcherStore {
    cursors: std::sync::Mutex<std::collections::BTreeMap<String, WatcherCursor>>,
}

impl MemWatcherStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WatcherStore for MemWatcherStore {
    fn load(&self, key: &str) -> Option<WatcherCursor> {
        self.cursors.lock().ok().and_then(|c| c.get(key).cloned())
    }

    fn save(&self, key: &str, cursor: &WatcherCursor) -> Result<(), String> {
        let mut guard = self
            .cursors
            .lock()
            .map_err(|_| "watcher store lock poisoned".to_string())?;
        guard.insert(key.to_string(), cursor.clone());
        Ok(())
    }
}

/// A polling source watcher (RFD §8). Holds the source path, the poll interval (epoch seconds),
/// the native-id column to diff on, and a cursor key. Pure data; the polling drive is
/// [`Watcher::poll_once`] (native).
#[derive(Debug, Clone)]
pub struct Watcher {
    /// The source path to re-query, e.g. `/mail/inbox`.
    pub source: String,
    /// The poll interval in seconds (the cadence the daemon ticks at).
    pub interval_secs: u64,
    /// The native-id column the diff keys on (e.g. `id`, `etag`, `@version`).
    pub id_column: String,
    /// The cursor store key (defaults to the source path).
    pub cursor_key: String,
}

impl Watcher {
    /// Construct a watcher over a source, interval, and the native-id column to diff on.
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        interval_secs: u64,
        id_column: impl Into<String>,
    ) -> Self {
        let source = source.into();
        Self {
            cursor_key: source.clone(),
            source,
            interval_secs,
            id_column: id_column.into(),
        }
    }
}

#[cfg(feature = "native")]
mod native {
    use super::{Watcher, WatcherStore};
    use crate::bus::EventBus;
    use crate::event::{Event, EventKind, SourcePath};
    use qfs_core::{Engine, Value};
    use qfs_exec::ReadRegistry;

    impl Watcher {
        /// Poll the source ONCE: run the source query through the READ path (pure read), diff the
        /// result rows against the loaded cursor (by the native-id column), publish a `RowAppended`
        /// [`Event`] per NEW row, then persist the cursor — **only after** every event was
        /// published (so a crash before `save` re-emits the window, never skips it). Returns how
        /// many events were emitted.
        ///
        /// PURITY: the read is a pure read (no mutation of the source); the only writes are the bus
        /// publish (durable enqueue) + the cursor save. No COMMIT happens here — that is the
        /// dispatcher's job when a trigger matches the emitted event.
        ///
        /// # Errors
        /// A secret-free message if the read, the publish, or the cursor save fails. On a publish
        /// failure the cursor is NOT advanced (the un-published rows re-emit next poll).
        pub async fn poll_once(
            &self,
            engine: &Engine,
            reads: &ReadRegistry,
            bus: &dyn EventBus,
            store: &dyn WatcherStore,
            now: i64,
        ) -> Result<usize, String> {
            let stmt = qfs_exec::parse(&format!("FROM {}", self.source))
                .map_err(|e| format!("watcher source parse failed: {e}"))?;
            let rows = qfs_exec::execute_read(&stmt, &engine.mounts, reads)
                .await
                .map_err(|e| format!("watcher read failed: {e}"))?;

            let columns: Vec<String> = rows.columns().iter().map(|c| c.to_string()).collect();
            let id_idx = columns.iter().position(|c| c == &self.id_column);

            let mut cursor = store.load(&self.cursor_key).unwrap_or_default();
            let mut emitted = 0usize;
            // A scratch cursor we only COMMIT to the store after publishing every new event.
            let mut advanced = cursor.clone();

            for (seq, row) in rows.rows.iter().enumerate() {
                let native_id = match id_idx.and_then(|i| row.values.get(i)) {
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::Int(n)) | Some(Value::Timestamp(n)) => n.to_string(),
                    // No id column / unstringable id: fall back to the positional seq (still
                    // bounded + deterministic within a poll, so a re-poll re-diffs correctly).
                    _ => format!("seq{seq}"),
                };
                if cursor.contains(&native_id) {
                    continue; // already emitted
                }
                let event = Event::new(
                    format!("{}#{native_id}", self.source),
                    SourcePath::new(self.source.clone()),
                    EventKind::RowAppended,
                    &native_id,
                    columns.clone(),
                    row.clone(),
                    now,
                );
                // Publish (durable enqueue) BEFORE advancing the cursor (at-least-once).
                bus.publish(event)
                    .map_err(|e| format!("watcher publish failed: {e}"))?;
                advanced.mark(&native_id);
                emitted += 1;
            }

            // Persist the cursor ONLY after every event was published (so a crash before this line
            // re-emits the bounded window, never silently skips it — the recovery invariant).
            cursor = advanced;
            store
                .save(&self.cursor_key, &cursor)
                .map_err(|e| format!("watcher cursor save failed: {e}"))?;
            Ok(emitted)
        }
    }
}
