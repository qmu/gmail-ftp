//! The **EC2/Linux daemon** host primitives (behind `host-daemon`): the fsync'd
//! [`FileDurableStore`] for watcher cursors / `LAST_RUN`, and the on-disk [`AuditLedger`] sink
//! that replaces the in-memory drain for the long-lived daemon (t36, RFD §6/§8/§10).
//!
//! ## What this is NOT
//! This module does **not** rebuild the daemon's serve composition. The HTTP listener
//! (`qfs-http`), the cron interval (`qfs-cron`), and the watchtower bus + `/hooks/...` ingest
//! (`qfs-watchtower`) are ALREADY wired in `crates/qfs/src/serve.rs`; the daemon's `RuntimeHost`
//! impl (`TokioHost`, composed in the binary) formalizes that existing wiring behind the
//! [`crate::RuntimeHost`] trait — it does not reimplement the listener/interval/bus. This module
//! provides the two NEW daemon-side primitives the existing composition lacked: a durable store
//! that survives a restart, and an audit ledger that persists across the daemon's lifetime rather
//! than only flushing on shutdown.
//!
//! Both write under a caller-supplied state directory (the worktree / a tempdir in tests, the
//! systemd `StateDirectory` in production) — **never** a system path (system-safety: this is a
//! regular project, not a provisioning repo).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::dto::{StateBytes, StateKey};
use crate::host::{DurableStore, HostError, HostFuture};

/// An fsync'd, file-backed [`DurableStore`] for the daemon (RFD §8: watcher cursors / `LAST_RUN`).
/// Each key maps to one file under the state dir; `put`/`cas` write a temp file then atomically
/// rename it over the target and fsync the directory, so a crash mid-write never leaves a torn
/// cursor (the recovery property RFD §6 requires). `cas` takes a per-store lock so a concurrent
/// daemon task's compare-and-set is serialized (single-flight, the at-least-once primitive).
pub struct FileDurableStore {
    root: PathBuf,
    /// Serializes `cas` (read-modify-write) so two tasks racing the same key cannot both swap.
    lock: Mutex<()>,
}

impl FileDurableStore {
    /// Open (creating it) a file durable store rooted at `dir`. `dir` MUST be a project-local /
    /// state path (a tempdir in tests, the systemd `StateDirectory` in production) — never a
    /// system path.
    ///
    /// # Errors
    /// [`HostError::Durable`] if the directory could not be created.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, HostError> {
        let root = dir.into();
        fs::create_dir_all(&root)
            .map_err(|e| HostError::Durable(format!("create state dir: {e}")))?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    /// The on-disk path for a state key. The key is sanitized into a single flat filename (slashes
    /// → `_`) so a key like `watcher/notify/cursor` maps to one file (no directory traversal).
    fn path_for(&self, key: &StateKey) -> PathBuf {
        let flat: String = key
            .as_str()
            .chars()
            .map(|c| if c == '/' || c == '\\' { '_' } else { c })
            .collect();
        self.root.join(format!("{flat}.state"))
    }

    /// Read the current bytes at `key`, synchronously (the daemon's fs read).
    fn read_sync(&self, key: &StateKey) -> Result<Option<StateBytes>, HostError> {
        match fs::read(self.path_for(key)) {
            Ok(bytes) => Ok(Some(StateBytes::new(bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HostError::Durable(format!("read state: {e}"))),
        }
    }

    /// Atomically write `val` at `key`: write a sibling temp file, fsync it, rename over the
    /// target, then fsync the directory so the rename is durable.
    fn write_sync(&self, key: &StateKey, val: &StateBytes) -> Result<(), HostError> {
        let target = self.path_for(key);
        let tmp = target.with_extension("state.tmp");
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| HostError::Durable(format!("create temp: {e}")))?;
            f.write_all(val.as_slice())
                .map_err(|e| HostError::Durable(format!("write temp: {e}")))?;
            f.sync_all()
                .map_err(|e| HostError::Durable(format!("fsync temp: {e}")))?;
        }
        fs::rename(&tmp, &target).map_err(|e| HostError::Durable(format!("rename: {e}")))?;
        // fsync the directory so the rename itself is durable (best-effort: a fs that rejects a
        // dir fsync — some platforms — degrades gracefully, the rename is already ordered).
        if let Ok(dir) = fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

impl DurableStore for FileDurableStore {
    fn get<'a>(&'a self, key: &'a StateKey) -> HostFuture<'a, Option<StateBytes>> {
        Box::pin(async move { self.read_sync(key) })
    }

    fn put<'a>(&'a self, key: &'a StateKey, val: StateBytes) -> HostFuture<'a, ()> {
        Box::pin(async move { self.write_sync(key, &val) })
    }

    fn cas<'a>(
        &'a self,
        key: &'a StateKey,
        expect: Option<StateBytes>,
        val: StateBytes,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let _guard = self
                .lock
                .lock()
                .map_err(|_| HostError::Durable("cas lock poisoned".to_string()))?;
            let current = self.read_sync(key)?;
            if current == expect {
                self.write_sync(key, &val)?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// An append-only, on-disk audit ledger for the daemon (RFD §6/§10). Replaces the in-memory drain
/// (`qfs_server::AuditSink::drain`) for the long-lived daemon: every fired plan / config mutation
/// is appended as one secret-free line and fsync'd, so the operator (and a future recovery pass)
/// can read the full history even across restarts — not only what was buffered at the last
/// shutdown. Lines are NAMES + OPS only, never a token or a row's contents.
pub struct AuditLedger {
    path: PathBuf,
    file: Mutex<fs::File>,
}

impl AuditLedger {
    /// Open (creating + appending to) the ledger file under the state dir.
    ///
    /// # Errors
    /// [`HostError::Durable`] if the directory or file could not be opened.
    pub fn open(dir: impl AsRef<Path>, filename: &str) -> Result<Self, HostError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)
            .map_err(|e| HostError::Durable(format!("create ledger dir: {e}")))?;
        let path = dir.join(filename);
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| HostError::Durable(format!("open ledger: {e}")))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Append one secret-free audit line (it is fsync'd before returning, so a crash never loses a
    /// recorded fire). The caller passes the already-secret-free summary
    /// (`qfs_server::AuditEntry::summary()` / a fired-plan record summary).
    ///
    /// # Errors
    /// [`HostError::Durable`] if the append/fsync failed.
    pub fn append(&self, line: &str) -> Result<(), HostError> {
        let mut f = self
            .file
            .lock()
            .map_err(|_| HostError::Durable("ledger lock poisoned".to_string()))?;
        writeln!(f, "{line}").map_err(|e| HostError::Durable(format!("append ledger: {e}")))?;
        f.sync_all()
            .map_err(|e| HostError::Durable(format!("fsync ledger: {e}")))?;
        Ok(())
    }

    /// The ledger file path (for the operator / tests).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Count the lines currently persisted (test/observability aid).
    ///
    /// # Errors
    /// [`HostError::Durable`] on a read failure.
    pub fn line_count(&self) -> Result<usize, HostError> {
        let text = fs::read_to_string(&self.path)
            .map_err(|e| HostError::Durable(format!("read ledger: {e}")))?;
        Ok(text.lines().filter(|l| !l.is_empty()).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::block_on;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("qfs-host-test-{}", std::process::id()));
        p.push(format!("{:?}", std::time::SystemTime::now()));
        p
    }

    #[test]
    fn file_store_put_get_roundtrip_and_cas() {
        let dir = tmp();
        let store = FileDurableStore::open(&dir).unwrap();
        let key = StateKey::new("watcher/notify/cursor");
        assert!(block_on(store.get(&key)).unwrap().is_none());
        block_on(store.put(&key, StateBytes::new(b"v1".to_vec()))).unwrap();
        assert_eq!(
            block_on(store.get(&key)).unwrap().unwrap().as_slice(),
            b"v1"
        );
        // cas with the right expectation swaps; with the wrong one is a no-op.
        assert!(block_on(store.cas(
            &key,
            Some(StateBytes::new(b"v1".to_vec())),
            StateBytes::new(b"v2".to_vec())
        ))
        .unwrap());
        assert!(!block_on(store.cas(
            &key,
            Some(StateBytes::new(b"v1".to_vec())), // stale expectation
            StateBytes::new(b"v3".to_vec())
        ))
        .unwrap());
        assert_eq!(
            block_on(store.get(&key)).unwrap().unwrap().as_slice(),
            b"v2"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ledger_appends_and_persists() {
        let dir = tmp();
        let ledger = AuditLedger::open(&dir, "audit.log").unwrap();
        ledger
            .append("boot INSERT /server/jobs name=nightly")
            .unwrap();
        ledger.append("fired job:nightly").unwrap();
        assert_eq!(ledger.line_count().unwrap(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
