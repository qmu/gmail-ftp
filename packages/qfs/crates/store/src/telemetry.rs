//! The **pure** telemetry signal model + sink contract (roadmap **decision V** / §4.6, ticket
//! **t77**). qfs EMITS three signals — **audit** (the hash-chained stream from t76), **metrics**
//! (counters / histograms), and **traces** (per-plan `describe`→pushdown→combine→`commit` spans) —
//! to ONE of three prepared output sinks (`file` default, `stdout`, or `OTel`) and nothing else.
//! Everything downstream (Prometheus via the collector, Grafana, Datadog, a SIEM, qfs Cloud's
//! dashboards) consumes one of those. qfs emits; it does NOT store the stream (retention is the
//! consumer's concern).
//!
//! This module is the PURE half — the signal records, their **metadata-only** canonical JSONL
//! line, the [`SinkKind`] config selector, and the [`TelemetrySink`] trait. The IMPURE half — the
//! concrete `file` / `stdout` / `OTel` sinks that actually touch a path / stdout / the network —
//! lives binary-side (`crates/qfs/src/telemetry.rs`), because only the terminal binary opens a
//! real path or socket (decision F), exactly as the audit chain-head I/O does.
//!
//! ## Metadata only — the same boundary `describe` enforces (§3.2 / §4.6)
//!
//! Every telemetry record carries **shape** (verbs / paths / counts / latencies / labels), never a
//! secret, never a credential, never a row's payload. The records are built ONLY from labelled
//! metadata fields — there is structurally NOWHERE to put a secret — so a formatted line is safe to
//! append to a log, print to stdout, or ship to a collector. This is the SAME guarantee
//! [`crate::audit::AuditEvent`] gives the audit signal; the metric / trace records extend it.

use std::fmt::Write as _;

use crate::audit::AuditEvent;

/// Which of the three prepared output sinks the deployment emits to (§4.6 / decision V). A **closed
/// set**: a new transport is a new variant here, never a side-channel. `file` is the default (an
/// operator with a persistent disk); a stateless Worker / Lambda (no durable disk — decision F)
/// overrides to `stdout` (12-factor; the platform captures) or `OTel` (the recommended collector
/// path — Prometheus reads FROM the collector, qfs exposes no native scrape endpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    /// Append the JSONL stream to a path (the default — point it at a rotating local file).
    File,
    /// Write the JSONL stream to stdout (12-factor; the host platform captures + forwards).
    Stdout,
    /// Export to an OpenTelemetry / OTLP collector (traces + metrics + logs). The RECOMMENDED
    /// production path: every downstream tool reads from the collector, never from qfs directly.
    Otel,
}

impl SinkKind {
    /// The default sink when nothing is configured: [`SinkKind::File`] (§4.6 — file is the default;
    /// servers explicitly override to `stdout`/`OTel`).
    #[must_use]
    pub const fn default_kind() -> Self {
        Self::File
    }

    /// Parse a sink selector (`"file"` / `"stdout"` / `"otel"`, case-insensitive), or `None` if the
    /// token names no known sink. The caller decides whether an unknown token is a hard error or a
    /// fall-back to [`SinkKind::default_kind`].
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "file" => Some(Self::File),
            "stdout" => Some(Self::Stdout),
            "otel" | "otlp" => Some(Self::Otel),
            _ => None,
        }
    }

    /// Resolve the configured sink from an optional selector string (e.g. the `QFS_TELEMETRY_SINK`
    /// env value): an absent or unrecognised value falls back to [`SinkKind::default_kind`], so a
    /// typo degrades to the safe default rather than dropping the stream.
    #[must_use]
    pub fn from_config(value: Option<&str>) -> Self {
        value
            .and_then(Self::parse)
            .unwrap_or_else(Self::default_kind)
    }

    /// The canonical lower-case token naming this sink (the inverse of [`SinkKind::parse`]).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Stdout => "stdout",
            Self::Otel => "otel",
        }
    }
}

/// A metric's shape (RFD §6 / §4.6). A **counter** monotonically increases (preview / commit
/// counts, rate-limit hits, errors); a **histogram** records a distribution sample (per-driver call
/// latency, pushdown ratio). The kind rides the line so the downstream collector maps it onto the
/// right instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A monotonically increasing total (e.g. `qfs_commit_total`).
    Counter,
    /// A single observation of a distribution (e.g. a per-driver latency in milliseconds).
    Histogram,
}

impl MetricKind {
    /// The canonical lower-case token for this kind (rendered into the JSONL line + `/sys/metrics`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Histogram => "histogram",
        }
    }
}

/// One metric sample — METADATA ONLY: a stable instrument `name`, its [`MetricKind`], a numeric
/// `value`, and zero or more secret-free `labels` (e.g. `driver=github`, `verb=INSERT`). NEVER a
/// secret or a row payload (there is no field for one). Counters carry an integer-valued `f64`;
/// histograms carry the observed sample.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSample {
    /// The instrument name, e.g. `qfs_commit_total` / `qfs_driver_call_latency_ms`.
    pub name: String,
    /// Whether this is a counter total or a histogram observation.
    pub kind: MetricKind,
    /// The sample value (a counter total, or a single histogram observation).
    pub value: f64,
    /// Secret-free dimensions (e.g. `[("driver","github"),("verb","INSERT")]`).
    pub labels: Vec<(String, String)>,
}

impl MetricSample {
    /// A bare counter sample with no labels.
    #[must_use]
    pub fn counter(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            kind: MetricKind::Counter,
            value,
            labels: Vec::new(),
        }
    }

    /// A bare histogram observation with no labels.
    #[must_use]
    pub fn histogram(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            kind: MetricKind::Histogram,
            value,
            labels: Vec::new(),
        }
    }

    /// Attach a secret-free label dimension (builder style).
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.labels.push((key.into(), val.into()));
        self
    }
}

/// One trace span — METADATA ONLY: a span `name`, the pipeline `stage` it covers
/// (`describe`/`pushdown`/`combine`/`commit`), its wall-clock `duration_ms`, and secret-free
/// `attrs`. The engine wraps a plan's stages in these so a slow `commit` is attributable to a
/// stage / driver without ever recording a row or a credential.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceSpan {
    /// The span name (e.g. `qfs.commit`).
    pub name: String,
    /// The pipeline stage this span covers: `describe` / `pushdown` / `combine` / `commit`.
    pub stage: String,
    /// The span's wall-clock duration in milliseconds.
    pub duration_ms: f64,
    /// Secret-free span attributes (e.g. `[("driver","github"),("effects","3")]`).
    pub attrs: Vec<(String, String)>,
}

impl TraceSpan {
    /// A span over `stage` with the given `name` and `duration_ms` and no attributes.
    #[must_use]
    pub fn new(name: impl Into<String>, stage: impl Into<String>, duration_ms: f64) -> Self {
        Self {
            name: name.into(),
            stage: stage.into(),
            duration_ms,
            attrs: Vec::new(),
        }
    }

    /// Attach a secret-free attribute (builder style).
    #[must_use]
    pub fn with_attr(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.attrs.push((key.into(), val.into()));
        self
    }
}

/// One emitted telemetry record — one of the three signals (§4.6). The sink fans it out to the
/// configured transport. Each variant is metadata-only by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryRecord {
    /// An audit event (the t76 hash-chained stream; here as the audit SIGNAL routed to a sink).
    Audit(AuditEvent),
    /// A metric sample (counter / histogram).
    Metric(MetricSample),
    /// A trace span.
    Trace(TraceSpan),
}

impl TelemetryRecord {
    /// The signal discriminator written as the line's `signal` field (`audit`/`metric`/`trace`).
    #[must_use]
    pub const fn signal(&self) -> &'static str {
        match self {
            Self::Audit(_) => "audit",
            Self::Metric(_) => "metric",
            Self::Trace(_) => "trace",
        }
    }

    /// Render this record as ONE canonical JSON Lines (JSONL) line — a single self-describing
    /// object, NO trailing newline (the sink appends the line terminator). The encoding is
    /// hand-rolled (qfs-store carries no `serde_json`) but RFC-8259-correct for the field set:
    /// strings are escaped, numbers are finite, and the field order is fixed so the output is
    /// deterministic. The line carries ONLY the record's labelled metadata — there is nowhere to
    /// smuggle a secret.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let mut s = String::new();
        s.push('{');
        write_kv_str(&mut s, "signal", self.signal());
        match self {
            Self::Audit(e) => {
                s.push(',');
                write_kv_str(&mut s, "actor", &e.actor);
                s.push(',');
                write_kv_str(&mut s, "connection", &e.connection);
                s.push(',');
                write_kv_str(&mut s, "verb", &e.verb);
                s.push(',');
                write_kv_str(&mut s, "path", &e.path);
                s.push(',');
                write_kv_bool(&mut s, "committed", e.committed);
                s.push(',');
                write_kv_str(&mut s, "ts", &e.ts);
            }
            Self::Metric(m) => {
                s.push(',');
                write_kv_str(&mut s, "name", &m.name);
                s.push(',');
                write_kv_str(&mut s, "kind", m.kind.as_str());
                s.push(',');
                write_kv_num(&mut s, "value", m.value);
                s.push(',');
                write_kv_labels(&mut s, "labels", &m.labels);
            }
            Self::Trace(t) => {
                s.push(',');
                write_kv_str(&mut s, "name", &t.name);
                s.push(',');
                write_kv_str(&mut s, "stage", &t.stage);
                s.push(',');
                write_kv_num(&mut s, "duration_ms", t.duration_ms);
                s.push(',');
                write_kv_labels(&mut s, "attrs", &t.attrs);
            }
        }
        s.push('}');
        s
    }
}

/// A sink failure — secret-free (it never renders a path's contents, only a stable reason). The
/// concrete binary-side sinks map their I/O errors onto this so the emit seam stays vendor-free.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The sink's transport failed (file append / stdout write / exporter). Carries a secret-free
    /// reason string only.
    #[error("telemetry sink emit failed: {0}")]
    Emit(String),
}

/// The emit seam every sink implements: take one [`TelemetryRecord`] and deliver it to the
/// configured transport. Object-safe (`dyn TelemetrySink`) so the binary can select the active sink
/// at run time from config. Emission is **best-effort at the call site** — a telemetry failure must
/// never fail or mask the operation it observes (the audit / telemetry never breaks the operation,
/// §6) — so callers log-and-continue on `Err` rather than propagating.
pub trait TelemetrySink: Send + Sync {
    /// Deliver one record to the sink's transport.
    ///
    /// # Errors
    /// Returns [`TelemetryError`] if the underlying transport failed; the caller treats it as
    /// best-effort (log + continue).
    fn emit(&self, record: &TelemetryRecord) -> Result<(), TelemetryError>;

    /// Which sink this is (for diagnostics / `/sys` reporting).
    fn kind(&self) -> SinkKind;
}

// --- JSONL field writers (hand-rolled; qfs-store carries no serde_json) ----------------------------

/// Write a `"key":"value"` pair with the string value JSON-escaped.
fn write_kv_str(out: &mut String, key: &str, val: &str) {
    write_json_string(out, key);
    out.push(':');
    write_json_string(out, val);
}

/// Write a `"key":true|false` pair.
fn write_kv_bool(out: &mut String, key: &str, val: bool) {
    write_json_string(out, key);
    out.push(':');
    out.push_str(if val { "true" } else { "false" });
}

/// Write a `"key":<number>` pair. A non-finite value (NaN / ±∞ — not representable in JSON) is
/// written as `0` so the line is always valid JSON rather than emitting a bare `NaN` token.
fn write_kv_num(out: &mut String, key: &str, val: f64) {
    write_json_string(out, key);
    out.push(':');
    if val.is_finite() {
        // `{}` on an f64 yields a minimal decimal (`1` for `1.0`), valid JSON.
        let _ = write!(out, "{val}");
    } else {
        out.push('0');
    }
}

/// Write a `"key":{ ... }` nested object of secret-free string labels (deterministic order: the
/// order the labels were attached).
fn write_kv_labels(out: &mut String, key: &str, labels: &[(String, String)]) {
    write_json_string(out, key);
    out.push_str(":{");
    for (i, (k, v)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(out, k);
        out.push(':');
        write_json_string(out, v);
    }
    out.push('}');
}

/// Append `s` as a quoted, RFC-8259-escaped JSON string. Escapes the two structural characters
/// (`"`, `\`) and the C0 control range (incl. `\n`, `\r`, `\t`) so a path or label containing a
/// quote or a newline can never break the one-line-per-record JSONL framing.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn audit_event() -> AuditEvent {
        AuditEvent {
            actor: "cli".to_string(),
            connection: "default".to_string(),
            verb: "INSERT".to_string(),
            path: "/local/notes.txt".to_string(),
            committed: true,
            ts: "2026-06-28T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn sink_kind_selection_by_config() {
        assert_eq!(SinkKind::parse("file"), Some(SinkKind::File));
        assert_eq!(SinkKind::parse("STDOUT"), Some(SinkKind::Stdout));
        assert_eq!(SinkKind::parse(" OTel "), Some(SinkKind::Otel));
        assert_eq!(SinkKind::parse("otlp"), Some(SinkKind::Otel));
        assert_eq!(SinkKind::parse("nope"), None);
        // Absent / unknown config falls back to the `file` default (never drops the stream).
        assert_eq!(SinkKind::from_config(None), SinkKind::File);
        assert_eq!(SinkKind::from_config(Some("garbage")), SinkKind::File);
        assert_eq!(SinkKind::from_config(Some("stdout")), SinkKind::Stdout);
        // Round-trips through its canonical token.
        for k in [SinkKind::File, SinkKind::Stdout, SinkKind::Otel] {
            assert_eq!(SinkKind::parse(k.as_str()), Some(k));
        }
    }

    #[test]
    fn audit_record_renders_a_metadata_only_jsonl_line() {
        let line = TelemetryRecord::Audit(audit_event()).to_jsonl();
        assert!(line.starts_with('{') && line.ends_with('}'));
        assert!(!line.contains('\n'), "a JSONL record is exactly one line");
        assert!(line.contains("\"signal\":\"audit\""));
        assert!(line.contains("\"verb\":\"INSERT\""));
        assert!(line.contains("\"path\":\"/local/notes.txt\""));
        assert!(line.contains("\"committed\":true"));
    }

    #[test]
    fn metric_and_trace_records_format_a_line() {
        let m = TelemetryRecord::Metric(
            MetricSample::counter("qfs_commit_total", 3.0).with_label("driver", "github"),
        );
        let ml = m.to_jsonl();
        assert!(ml.contains("\"signal\":\"metric\""));
        assert!(ml.contains("\"name\":\"qfs_commit_total\""));
        assert!(ml.contains("\"kind\":\"counter\""));
        assert!(ml.contains("\"value\":3"));
        assert!(ml.contains("\"labels\":{\"driver\":\"github\"}"));

        let t = TelemetryRecord::Trace(
            TraceSpan::new("qfs.commit", "commit", 12.5).with_attr("effects", "2"),
        );
        let tl = t.to_jsonl();
        assert!(tl.contains("\"signal\":\"trace\""));
        assert!(tl.contains("\"stage\":\"commit\""));
        assert!(tl.contains("\"duration_ms\":12.5"));
        assert!(tl.contains("\"attrs\":{\"effects\":\"2\"}"));
    }

    #[test]
    fn no_secret_leaks_into_a_telemetry_record() {
        // The records are built ONLY from labelled metadata — there is no field for a secret or a
        // row payload. A would-be secret token never appears in any signal's line because there is
        // nowhere to put it (the same metadata-only boundary as t76's audit content).
        let secret = "super-secret-token";
        for rec in [
            TelemetryRecord::Audit(audit_event()),
            TelemetryRecord::Metric(MetricSample::counter("qfs_commit_total", 1.0)),
            TelemetryRecord::Trace(TraceSpan::new("qfs.commit", "commit", 1.0)),
        ] {
            assert!(
                !rec.to_jsonl().contains(secret),
                "no telemetry signal may carry secret material"
            );
        }
    }

    #[test]
    fn json_strings_are_escaped_so_framing_cannot_break() {
        // A path carrying a quote + a newline must NOT break the one-line JSONL framing.
        let mut e = audit_event();
        e.path = "/local/\"odd\"\nname".to_string();
        let line = TelemetryRecord::Audit(e).to_jsonl();
        assert!(!line.contains('\n'), "the embedded newline must be escaped");
        assert!(line.contains("\\\"odd\\\""));
        assert!(line.contains("\\n"));
    }
}
