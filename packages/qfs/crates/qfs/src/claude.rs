//! The `/claude` AI-sessions composition root (ticket t64): the on-disk [`SessionSource`]
//! implementation + the async [`ClaudeReadDriver`] read facet, both hosted in the **`qfs` binary
//! crate**.
//!
//! ## Why the source lives in the binary (not the leaf driver crate)
//! `qfs-driver-claude` is the vendor-free AI-sessions driver (the pure introspective half + the
//! `ClaudeApplier` over the `SessionSource` seam) and is a **`qfs-runtime` consumer**, so the
//! dep-direction guard requires it to be a **leaf** — only the terminal `qfs` binary may depend on
//! it. The binary IS that leaf and the ONE place that opens a real path (decision F), so the actual
//! `std::fs` reads/append over Claude Code's session state dead-end here. No filesystem path crosses
//! the `SessionSource` boundary (owned qfs DTOs only).
//!
//! ## Decision K — qfs NEVER hosts or calls an LLM (critical)
//! This composition root reads session METADATA an agent runtime already wrote and APPENDS a
//! steering instruction the agent reads. The model runs ELSEWHERE; nothing here calls or hosts an
//! LLM. There is no inference dependency in this file.
//!
//! ## Config (no credentials) — fail-closed / opt-in by default
//! The session-state location is one env var `QFS_CLAUDE_SESSIONS=<absolute-base-dir>`. **With it
//! unset there is no source — fail-closed**: the binary registers neither the live read facet nor
//! the apply driver, so a `/claude` read surfaces empty and a `/claude` commit fails closed (no
//! driver). The live cross-machine `<host>` hop (`/hosts/<host>/claude/...`) rides the t63 tunnel
//! and re-checks `POLICY` at the destination — a DOCUMENTED SEAM, not wired here.
//!
//! ## On-disk layout (the FLAGGED coupling, per the ticket)
//! The exact session-state format is an OPEN product decision that may shift across Claude Code
//! versions, so it is named here rather than baked into the driver crate. This slice models a
//! deliberately simple, stable layout under the base dir:
//! - one directory per session: `<base>/<id>/`;
//! - `<base>/<id>/meta` — `key=value` lines (`task`/`status`/`progress`/`last_message`);
//! - `<base>/<id>/instructions` — one steering instruction per line (the append-log).
//!
//! A real integration swaps this reader for Claude Code's on-disk format or a local IPC/API behind
//! the SAME `SessionSource` seam — no driver-crate change.

use std::path::PathBuf;
use std::sync::Arc;

use qfs_core::{CfsError, RowBatch};
use qfs_driver_claude::{
    claude_node_schema, instruction_session, node_for_path, ClaudeError, ClaudeNode, SessionSource,
};
use qfs_exec::ReadDriver;
use qfs_pushdown::ScanNode;
use qfs_types::{Row, Value};

/// The env var naming the Claude Code session-state base dir: `QFS_CLAUDE_SESSIONS=<base>`.
const CLAUDE_ENV: &str = "QFS_CLAUDE_SESSIONS";

/// The on-disk [`SessionSource`]: reads session metadata + the per-session instructions append-log
/// from a configured base directory, and appends a steering instruction. Owns only the base path —
/// no credential, no model handle (decision K).
pub struct DirSessionSource {
    base: PathBuf,
}

impl DirSessionSource {
    /// Build a source over `base` (the test + composition seam).
    #[must_use]
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// Open the configured source from `QFS_CLAUDE_SESSIONS`, or `None` when unset/empty — the
    /// **fail-closed default** (the `/claude` surface is simply not wired rather than binding a
    /// source that resolves nothing). Mirrors the `/fs` deny-all-by-default opt-in (t68).
    #[must_use]
    pub fn open_default() -> Option<Self> {
        match std::env::var(CLAUDE_ENV) {
            Ok(base) if !base.is_empty() => Some(Self::new(base)),
            _ => None,
        }
    }

    /// Read one session's `meta` file into `(task, status, progress, last_message)`. A missing meta
    /// file reads as all-empty with `status = "unknown"` (robust, never a panic). Secret-free: the
    /// reader only ever surfaces these four metadata keys.
    fn read_meta(&self, id: &str) -> (Value, Value, Value, Value) {
        let path = self.base.join(id).join("meta");
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut task = Value::Null;
        let mut status = Value::Text("unknown".to_string());
        let mut progress = Value::Null;
        let mut last_message = Value::Null;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "task" => task = Value::Text(value),
                "status" => status = Value::Text(value),
                "progress" => progress = Value::Text(value),
                "last_message" => last_message = Value::Text(value),
                // Any other key is ignored — only the four metadata fields ever surface (no secret).
                _ => {}
            }
        }
        (task, status, progress, last_message)
    }
}

impl SessionSource for DirSessionSource {
    fn scan_sessions(&self) -> Result<RowBatch, ClaudeError> {
        let schema = claude_node_schema(ClaudeNode::Sessions);
        let mut ids: Vec<String> = Vec::new();
        // A missing base dir yields zero sessions (fail-closed read, never an error).
        if let Ok(entries) = std::fs::read_dir(&self.base) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        ids.push(name.to_string());
                    }
                }
            }
        }
        ids.sort();
        let rows = ids
            .into_iter()
            .map(|id| {
                let (task, status, progress, last_message) = self.read_meta(&id);
                Row::new(vec![Value::Text(id), task, status, progress, last_message])
            })
            .collect();
        Ok(RowBatch::new(schema, rows))
    }

    fn scan_instructions(&self, session: &str) -> Result<RowBatch, ClaudeError> {
        let schema = claude_node_schema(ClaudeNode::Instructions);
        let path = self.base.join(session).join("instructions");
        // A session with no instructions log yields an empty batch (robust, not an error).
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let rows = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| Row::new(vec![Value::Null, Value::Text(line.to_string())]))
            .collect();
        Ok(RowBatch::new(schema, rows))
    }

    fn append_instruction(&self, session: &str, row: &RowBatch) -> Result<u64, ClaudeError> {
        use std::io::Write;
        // The session directory must already exist — steering a non-existent agent is a structured
        // error, never an implicit create (fail-closed).
        let dir = self.base.join(session);
        if !dir.is_dir() {
            return Err(ClaudeError::UnknownSession {
                session: session.to_string(),
            });
        }
        // Extract the instruction text from the first row's `instruction` column.
        let idx = row
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == "instruction")
            .ok_or_else(|| ClaudeError::MalformedEffect {
                reason: "INSERT carries no `instruction` column".to_string(),
            })?;
        let text = match row.rows.first().and_then(|r| r.values.get(idx)) {
            Some(Value::Text(t)) => t.clone(),
            _ => {
                return Err(ClaudeError::MalformedEffect {
                    reason: "`instruction` value is missing or not text".to_string(),
                })
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("instructions"))
            .map_err(|e| ClaudeError::Source(e.to_string()))?;
        writeln!(file, "{text}").map_err(|e| ClaudeError::Source(e.to_string()))?;
        Ok(1)
    }
}

/// The async read facet (the `/claude` counterpart of `shell.rs`'s `LocalReadDriver` and `sys.rs`'s
/// `SysReadDriver`): adapts the synchronous [`SessionSource`] reads to qfs-exec's [`ReadDriver`]
/// seam. Lives in the binary because `ReadDriver` is a qfs-exec type and the driver crate must stay
/// off qfs-exec (dep direction).
pub struct ClaudeReadDriver {
    source: Arc<dyn SessionSource>,
}

impl ClaudeReadDriver {
    /// Build the read adapter over an injected source.
    #[must_use]
    pub fn new(source: Arc<dyn SessionSource>) -> Self {
        Self { source }
    }
}

#[async_trait::async_trait]
impl ReadDriver for ClaudeReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let node = node_for_path(&scan.path).ok_or_else(|| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: "not a /claude session path",
        })?;
        let result = match node {
            ClaudeNode::Sessions => self.source.scan_sessions(),
            ClaudeNode::Instructions => {
                let session =
                    instruction_session(&scan.path).ok_or_else(|| CfsError::InvalidPath {
                        path: scan.path.clone(),
                        reason: "no session id in /claude instructions path",
                    })?;
                self.source.scan_instructions(&session)
            }
        };
        result.map_err(|e| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: claude_error_reason(&e),
        })
    }
}

/// A stable, secret-free reason code for a `/claude` read failure (the executor maps it to its kind).
fn claude_error_reason(e: &ClaudeError) -> &'static str {
    match e {
        ClaudeError::UnknownNode { .. } => "unknown_claude_node",
        ClaudeError::UnknownSession { .. } => "unknown_session",
        ClaudeError::Unsupported { .. } => "unsupported_verb",
        ClaudeError::MalformedEffect { .. } => "malformed_effect",
        ClaudeError::Source(_) => "read_failed",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_types::{Column, ColumnType, Schema};
    use tempfile::TempDir;

    /// A base dir with two seeded sessions (one running, one done) + an instruction log on the
    /// running one. Hermetic: a tempdir, no agent runtime, no model (decision K).
    fn fixture() -> (TempDir, DirSessionSource) {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        let s1 = base.join("s-1");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::write(
            s1.join("meta"),
            "task=write the t64 driver\nstatus=running\nprogress=3/5\nlast_message=scanning\n",
        )
        .unwrap();
        std::fs::write(s1.join("instructions"), "focus on the failing test\n").unwrap();
        let s2 = base.join("s-2");
        std::fs::create_dir_all(&s2).unwrap();
        std::fs::write(s2.join("meta"), "task=review\nstatus=done\n").unwrap();
        let source = DirSessionSource::new(base.to_path_buf());
        (dir, source)
    }

    fn texts(batch: &RowBatch, col: &str) -> Vec<String> {
        let idx = batch
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == col)
            .expect("column present");
        batch
            .rows
            .iter()
            .filter_map(|r| match &r.values[idx] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Reading session metadata from the on-disk fixture works — the live local read smoke.
    #[test]
    fn scan_sessions_reads_metadata() {
        let (_d, source) = fixture();
        let batch = source.scan_sessions().unwrap();
        assert_eq!(batch.rows.len(), 2);
        assert_eq!(texts(&batch, "id"), vec!["s-1", "s-2"]);
        assert_eq!(texts(&batch, "status"), vec!["running", "done"]);
        // The read schema is exactly the one the driver describes (no drift).
        assert_eq!(batch.schema, claude_node_schema(ClaudeNode::Sessions));
    }

    /// The per-session instructions append-log reads back.
    #[test]
    fn scan_instructions_reads_the_log() {
        let (_d, source) = fixture();
        let batch = source.scan_instructions("s-1").unwrap();
        assert_eq!(
            texts(&batch, "instruction"),
            vec!["focus on the failing test"]
        );
        // An unknown session yields an empty log (robust, not an error).
        assert!(source.scan_instructions("nope").unwrap().rows.is_empty());
    }

    /// Appending a steering instruction (the one gated WRITE) round-trips through the log.
    #[test]
    fn append_instruction_round_trips() {
        let (_d, source) = fixture();
        let schema = Schema::new(vec![Column::new("instruction", ColumnType::Text, false)]);
        let row = RowBatch::new(
            schema,
            vec![Row::new(vec![Value::Text("now run the tests".into())])],
        );
        assert_eq!(source.append_instruction("s-1", &row).unwrap(), 1);
        let back = texts(&source.scan_instructions("s-1").unwrap(), "instruction");
        assert_eq!(back, vec!["focus on the failing test", "now run the tests"]);
    }

    /// Steering a non-existent session is a structured error (fail-closed) — never an implicit create.
    #[test]
    fn append_to_unknown_session_fails_closed() {
        let (_d, source) = fixture();
        let schema = Schema::new(vec![Column::new("instruction", ColumnType::Text, false)]);
        let row = RowBatch::new(schema, vec![Row::new(vec![Value::Text("hi".into())])]);
        let err = source.append_instruction("ghost", &row).unwrap_err();
        assert!(matches!(err, ClaudeError::UnknownSession { .. }));
    }

    /// `open_default` is fail-closed: with `QFS_CLAUDE_SESSIONS` unset there is no source (the
    /// process under test sets no such var), so `/claude` is left unwired.
    #[test]
    fn open_default_is_fail_closed_without_env() {
        // The test process does not set QFS_CLAUDE_SESSIONS.
        assert!(std::env::var(CLAUDE_ENV).is_err());
        assert!(DirSessionSource::open_default().is_none());
    }
}
