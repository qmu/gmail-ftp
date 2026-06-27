//! t77: the binary-side IMPURE half of the externalized telemetry surface — the concrete
//! `file` / `stdout` / `OTel` [`TelemetrySink`]s, the env-driven sink SELECTION, and the
//! process-local metrics registry the `/sys/metrics` live view reads.
//!
//! The PURE half (the signal records, their metadata-only JSONL line, the [`SinkKind`] selector,
//! and the [`TelemetrySink`] trait) lives in `qfs_store::telemetry`. This module owns only what
//! MUST be binary-side: opening a real path (the `file` sink), writing to the process's stdout (the
//! `stdout` sink), and the OTLP exporter SEAM (the `OTel` sink). The binary is the ONE crate that
//! resolves a real path / socket (decision F), so these dead-end here, exactly like the audit
//! chain-head I/O and the System-DB composition.
//!
//! ## Emit, don't store (decision V)
//!
//! qfs EMITS the three signals to ONE configured sink and retains nothing of the stream itself
//! (only the audit chain HEAD, t76). The sinks here are the funnel to the consumer's retention
//! (Prometheus via the OTel collector, a SIEM, qfs Cloud) — qfs never reads them back.
//!
//! ## Best-effort, never breaks the operation (§6)
//!
//! Every emit is best-effort at the call site: a missing path, a closed pipe, or an unconfigured
//! exporter is logged (secret-free, at debug) and skipped — it never fails or masks the operation
//! the telemetry observes (the same posture as the t76 audit emitter).
//!
//! ## Config (§4.6 / decision F)
//!
//! - `QFS_TELEMETRY_SINK` selects the active sink: `file` (default) / `stdout` / `otel`. An absent
//!   or unrecognised value falls back to `file`.
//! - `QFS_TELEMETRY_FILE` overrides the `file` sink path; absent, it defaults to
//!   `<config-home>/qfs/telemetry.jsonl` (alongside the System DB). A stateless Worker / Lambda
//!   (no durable disk) sets `QFS_TELEMETRY_SINK=stdout` or `=otel`.
//! - `QFS_TELEMETRY_OTLP_ENDPOINT` carries the OTLP collector endpoint for the `otel` sink seam.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use qfs_store::telemetry::{
    MetricSample, SinkKind, TelemetryError, TelemetryRecord, TelemetrySink,
};

/// The env var selecting the active sink (`file` / `stdout` / `otel`).
pub const ENV_SINK: &str = "QFS_TELEMETRY_SINK";
/// The env var overriding the `file` sink's path.
pub const ENV_FILE: &str = "QFS_TELEMETRY_FILE";
/// The env var carrying the OTLP collector endpoint for the `otel` sink.
pub const ENV_OTLP_ENDPOINT: &str = "QFS_TELEMETRY_OTLP_ENDPOINT";

/// Resolve the default `file` sink path: `QFS_TELEMETRY_FILE` if set, else
/// `<config-home>/qfs/telemetry.jsonl` (next to the System DB), else `None` when no config home
/// resolves (HOME/XDG unset) — in which case the `file` sink is a no-op rather than writing to an
/// unexpected location.
#[must_use]
pub fn default_telemetry_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ENV_FILE) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    // Mirror the System DB's config-home convention (one `qfs` config dir).
    crate::store::default_system_db_path().map(|mut db| {
        db.set_file_name("telemetry.jsonl");
        db
    })
}

/// Build the active sink from the process environment (the binary's telemetry composition root).
/// `QFS_TELEMETRY_SINK` selects `file` (default) / `stdout` / `otel`; `file` resolves its path via
/// [`default_telemetry_file_path`]. Always returns a usable sink — a `file` sink with no resolvable
/// path degrades to a no-op (best-effort), never a failure.
#[must_use]
pub fn sink_from_env() -> Box<dyn TelemetrySink> {
    let kind = SinkKind::from_config(std::env::var(ENV_SINK).ok().as_deref());
    build_sink(kind)
}

/// Build the concrete sink for `kind` (the env-independent half, so the selection is unit-testable
/// without mutating the global process env).
#[must_use]
pub fn build_sink(kind: SinkKind) -> Box<dyn TelemetrySink> {
    match kind {
        SinkKind::File => Box::new(FileSink::new(default_telemetry_file_path())),
        SinkKind::Stdout => Box::new(StdoutSink),
        SinkKind::Otel => Box::new(OtelSink::from_env()),
    }
}

/// Format one record as the exact bytes a line-structured sink writes: the canonical JSONL line +
/// a `\n` terminator. Shared by the `file` and `stdout` sinks so both frame records identically.
fn record_line(record: &TelemetryRecord) -> String {
    let mut line = record.to_jsonl();
    line.push('\n');
    line
}

/// The `file` sink (the default): APPEND each record's JSONL line to a path (point it at a rotating
/// local file; rotation/retention is the consumer's concern — qfs only appends). A `None` path
/// (no resolvable config home) makes every emit a silent no-op (best-effort), so a host without a
/// config home runs un-sunk rather than failing.
pub struct FileSink {
    /// The append target, or `None` for the no-op sink.
    path: Option<PathBuf>,
}

impl FileSink {
    /// Build a `file` sink over `path` (`None` = the no-op sink).
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

impl TelemetrySink for FileSink {
    fn emit(&self, record: &TelemetryRecord) -> Result<(), TelemetryError> {
        let Some(path) = &self.path else {
            return Ok(()); // no-op sink (no config home)
        };
        // Best-effort: create the parent dir if missing, open for append, write one line.
        if let Some(parent) = path.parent() {
            // A failure to create the dir is surfaced as an Emit error (the caller logs + continues).
            std::fs::create_dir_all(parent)
                .map_err(|e| TelemetryError::Emit(format!("create dir: {e}")))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| TelemetryError::Emit(format!("open: {e}")))?;
        f.write_all(record_line(record).as_bytes())
            .map_err(|e| TelemetryError::Emit(format!("write: {e}")))
    }

    fn kind(&self) -> SinkKind {
        SinkKind::File
    }
}

/// The `stdout` sink (12-factor): write each record's JSONL line to the process's stdout, where the
/// host platform (a container runtime, a Worker / Lambda log pipe) captures and forwards it. The
/// recommended override for a stateless host with no durable disk (decision F).
pub struct StdoutSink;

impl TelemetrySink for StdoutSink {
    fn emit(&self, record: &TelemetryRecord) -> Result<(), TelemetryError> {
        let mut out = io::stdout().lock();
        out.write_all(record_line(record).as_bytes())
            .map_err(|e| TelemetryError::Emit(format!("stdout: {e}")))
    }

    fn kind(&self) -> SinkKind {
        SinkKind::Stdout
    }
}

/// The `OTel` / OTLP sink — **the exporter SEAM** (the recommended production transport; every
/// downstream tool reads from the collector, qfs exposes no native scrape endpoint).
///
/// **Offline-honest status (t77):** the seam is present and selectable, but the OTLP exporter is
/// **NOT wired** — a vetted OpenTelemetry / OTLP exporter crate is not resolvable in the offline
/// build cache, and t77 deliberately does NOT hand-roll the OTLP wire protocol. So `emit` renders
/// the record (proving the metadata-only line the exporter would carry) and records it through
/// `tracing` at debug under the `qfs::telemetry::otel` target, then returns `Ok` (best-effort). When
/// a vetted exporter lands, replace the `emit` body with a real `OTLPExporter::export(...)` over
/// [`OtelSink::endpoint`] — the call sites, the signal model, and the `/sys/metrics` view are
/// unchanged (this is the only place that needs the exporter dependency).
pub struct OtelSink {
    /// The configured OTLP collector endpoint (`QFS_TELEMETRY_OTLP_ENDPOINT`), or `None`.
    endpoint: Option<String>,
}

impl OtelSink {
    /// Build the seam over an explicit endpoint (`None` = unconfigured).
    #[must_use]
    pub fn new(endpoint: Option<String>) -> Self {
        Self { endpoint }
    }

    /// Build the seam reading the OTLP endpoint from `QFS_TELEMETRY_OTLP_ENDPOINT`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(
            std::env::var(ENV_OTLP_ENDPOINT)
                .ok()
                .filter(|e| !e.is_empty()),
        )
    }

    /// The configured OTLP endpoint, if any (the real exporter target once wired).
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }
}

impl TelemetrySink for OtelSink {
    fn emit(&self, record: &TelemetryRecord) -> Result<(), TelemetryError> {
        // SEAM: the exporter is not wired offline (no OTLP crate in the cache). We render the line
        // the exporter WOULD carry (metadata-only, proving the boundary) and trace it; never fail.
        tracing::debug!(
            target: "qfs::telemetry::otel",
            endpoint = self.endpoint.as_deref().unwrap_or("<unset>"),
            "otel exporter not wired (dep unavailable offline); record: {}",
            record.to_jsonl()
        );
        Ok(())
    }

    fn kind(&self) -> SinkKind {
        SinkKind::Otel
    }
}

// --- The process-local metrics registry (the `/sys/metrics` live view source) ---------------------

/// The process-global counter registry. qfs EMITS metrics to the configured sink and does NOT
/// persist the stream (decision V); this in-memory registry is the BOUNDED live view a long-running
/// server surfaces at `/sys/metrics` — current-process counter totals, never a durable time series
/// (retention is the consumer's via the sink). Keyed by instrument name → counter total.
fn registry() -> &'static Mutex<BTreeMap<String, i64>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, i64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Increment the named counter by `by` in the process-local registry (best-effort: a poisoned lock
/// is ignored rather than panicking — a metric must never break the operation it counts).
pub fn incr_counter(name: &str, by: i64) {
    if let Ok(mut map) = registry().lock() {
        *map.entry(name.to_string()).or_insert(0) += by;
    }
}

/// Snapshot the process-local counters as [`MetricSample`]s (ascending by name; `BTreeMap` keeps
/// the order stable). This is the row source the `/sys/metrics` backend scan reads.
#[must_use]
pub fn metrics_snapshot() -> Vec<MetricSample> {
    match registry().lock() {
        #[allow(clippy::cast_precision_loss)]
        Ok(map) => map
            .iter()
            .map(|(name, &v)| MetricSample::counter(name.clone(), v as f64))
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_store::audit::AuditEvent;
    use qfs_store::telemetry::TraceSpan;

    fn audit_record() -> TelemetryRecord {
        TelemetryRecord::Audit(AuditEvent {
            actor: "cli".to_string(),
            connection: "default".to_string(),
            verb: "UPSERT".to_string(),
            path: "/local/notes.txt".to_string(),
            committed: true,
            ts: "2026-06-28T00:00:00Z".to_string(),
        })
    }

    #[test]
    fn file_sink_writes_the_audit_record_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("telemetry.jsonl");
        let sink = FileSink::new(Some(path.clone()));

        // Emit two records: an audit event + a metric. Each lands as ONE JSONL line.
        sink.emit(&audit_record()).unwrap();
        sink.emit(&TelemetryRecord::Metric(MetricSample::counter(
            "qfs_commit_total",
            1.0,
        )))
        .unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per emitted record (append)");
        // Round-trip the audit record's metadata fields back out of the written line.
        assert!(lines[0].contains("\"signal\":\"audit\""));
        assert!(lines[0].contains("\"verb\":\"UPSERT\""));
        assert!(lines[0].contains("\"path\":\"/local/notes.txt\""));
        assert!(lines[1].contains("\"name\":\"qfs_commit_total\""));
        assert_eq!(sink.kind(), SinkKind::File);
    }

    #[test]
    fn file_sink_with_no_path_is_a_silent_noop() {
        // No config home / no path => the sink never fails and writes nothing.
        let sink = FileSink::new(None);
        assert!(sink.emit(&audit_record()).is_ok());
    }

    #[test]
    fn line_sinks_format_exactly_one_terminated_line() {
        // The bytes a `file`/`stdout` sink writes: the canonical JSONL line + a single newline.
        // (This is what the `stdout` sink writes to the captured stdout pipe — asserting it here
        // avoids depending on the global stdout in a parallel test run.)
        let line = record_line(&TelemetryRecord::Trace(TraceSpan::new(
            "qfs.commit",
            "commit",
            4.0,
        )));
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "exactly one line");
        assert!(line.contains("\"signal\":\"trace\""));
        assert!(line.contains("\"stage\":\"commit\""));
    }

    #[test]
    fn sink_selection_builds_the_configured_sink() {
        assert_eq!(build_sink(SinkKind::File).kind(), SinkKind::File);
        assert_eq!(build_sink(SinkKind::Stdout).kind(), SinkKind::Stdout);
        assert_eq!(build_sink(SinkKind::Otel).kind(), SinkKind::Otel);
    }

    #[test]
    fn otel_seam_compiles_and_never_fails() {
        // The OTel sink is present + selectable; its exporter is parked (offline), so emit is a
        // best-effort no-fail render. This pins that the seam COMPILES and is wired into selection.
        let sink = OtelSink::new(Some("http://collector:4317".to_string()));
        assert_eq!(sink.endpoint(), Some("http://collector:4317"));
        assert!(sink.emit(&audit_record()).is_ok());
        assert!(OtelSink::from_env().emit(&audit_record()).is_ok());
    }

    #[test]
    fn no_secret_leaks_through_a_sink_line() {
        // A sink can only ever write the record's metadata-only line — there is no field for a
        // secret, so a would-be token never reaches the transport.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        FileSink::new(Some(path.clone()))
            .emit(&audit_record())
            .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("super-secret-token"));
    }

    #[test]
    fn metrics_registry_counts_and_snapshots() {
        // Use a uniquely-named counter so the process-global registry can't collide with other
        // tests running in the same binary.
        let name = "qfs_test_unique_counter_xyz";
        incr_counter(name, 2);
        incr_counter(name, 3);
        let snap = metrics_snapshot();
        let found = snap
            .iter()
            .find(|m| m.name == name)
            .expect("counter present");
        assert!((found.value - 5.0).abs() < f64::EPSILON);
    }
}
