//! The output renderers (ticket t29): [`Renderer`] with [`JsonRenderer`] (the stable
//! machine schema) and [`TableRenderer`] (human, TTY-aware). Both render **owned DTOs only**
//! ([`RowSet`], [`PlanPreview`], [`ExecError`]) — no vendor types reach the renderer (RFD §9).
//!
//! ## The table formatter dependency decision (ADR-0002/0003 precedent)
//! The ticket flags `comfy-table` as a candidate but warns against bloating the offline build.
//! We ship an **own, dependency-light** column-aligned formatter (~60 lines, no new crate) for
//! the same reasons ADR-0002 chose an in-house combine engine over DuckDB: the disk is tight
//! (~97%), the team has a consistent anti-heavy-dep precedent, and a one-shot CLI table needs
//! only fixed-width column alignment — not comfy-table's styling/wrapping/Unicode-border
//! machinery. The renderer is behind the [`Renderer`] trait, so a richer formatter could land
//! later without touching callers.
//!
//! ## Stable JSON schema
//! - rows:  `{"rows":[ {col: value, …} ]}`
//! - plan:  `{"preview":{…},"committed":bool}`
//! - error: `{"error":{"code","kind","message","path"?,"detail"?}}` (the t01-superset envelope)

use std::io::Write;

use crate::dto::{PlanPreview, RowSet};
use crate::error::ExecError;

/// The render seam: turn an owned DTO into bytes on a writer. Errors are the writer's `io`
/// errors only — the DTOs are already validated/owned.
pub trait Renderer {
    /// Render a read result.
    ///
    /// # Errors
    /// The underlying writer's `io::Error`.
    fn rows(&self, rows: &RowSet, w: &mut dyn Write) -> std::io::Result<()>;

    /// Render an effect plan preview / committed-apply summary.
    ///
    /// # Errors
    /// The underlying writer's `io::Error`.
    fn plan(&self, plan: &PlanPreview, w: &mut dyn Write) -> std::io::Result<()>;

    /// Render a `DESCRIBE <path>` report — the agent's first loop step (ticket t39). The JSON
    /// form is the stable machine contract; the table form is the human projection.
    ///
    /// # Errors
    /// The underlying writer's `io::Error`.
    fn describe(&self, report: &qfs_core::DescribeReport, w: &mut dyn Write)
        -> std::io::Result<()>;

    /// Render a structured error (always to stderr by the caller).
    ///
    /// # Errors
    /// The underlying writer's `io::Error`.
    fn error(&self, err: &ExecError, w: &mut dyn Write) -> std::io::Result<()>;
}

/// The output format (ticket t29). `--json` is an alias for `Json`; the default is resolved by
/// `IsTerminal` (table on a TTY, json when piped) unless an explicit flag overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Machine-readable JSON (stable schema).
    Json,
    /// Human column-aligned table.
    Table,
}

impl OutputFormat {
    /// Build the matching boxed renderer.
    #[must_use]
    pub fn renderer(self) -> Box<dyn Renderer> {
        match self {
            OutputFormat::Json => Box::new(JsonRenderer),
            OutputFormat::Table => Box::new(TableRenderer),
        }
    }
}

/// The machine renderer: the stable JSON schema, pretty-printed for human/CI readability while
/// staying a single parseable document (an agent parses it; a human reads it).
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonRenderer;

impl Renderer for JsonRenderer {
    fn rows(&self, rows: &RowSet, w: &mut dyn Write) -> std::io::Result<()> {
        let json = serde_json::to_string(rows).unwrap_or_else(|_| "{\"rows\":[]}".to_string());
        writeln!(w, "{json}")
    }

    fn plan(&self, plan: &PlanPreview, w: &mut dyn Write) -> std::io::Result<()> {
        let json = serde_json::to_string(plan).unwrap_or_else(|_| "{}".to_string());
        writeln!(w, "{json}")
    }

    fn describe(
        &self,
        report: &qfs_core::DescribeReport,
        w: &mut dyn Write,
    ) -> std::io::Result<()> {
        // The DescribeReport is the stable agent-facing contract; serialize it as-is. A
        // serialization failure is impossible for these owned DTOs, but never panic.
        let json = serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string());
        writeln!(w, "{json}")
    }

    fn error(&self, err: &ExecError, w: &mut dyn Write) -> std::io::Result<()> {
        writeln!(w, "{}", error_envelope(err))
    }
}

/// The human renderer: a fixed-width, column-aligned table (own implementation, no vendor dep).
#[derive(Debug, Default, Clone, Copy)]
pub struct TableRenderer;

impl Renderer for TableRenderer {
    fn rows(&self, rows: &RowSet, w: &mut dyn Write) -> std::io::Result<()> {
        let headers: Vec<String> = rows.columns().iter().map(|c| (*c).to_string()).collect();
        if headers.is_empty() {
            return writeln!(w, "(0 columns, {} row(s))", rows.len());
        }
        // Render each cell to a display string (the human projection of a Value).
        let cells: Vec<Vec<String>> = rows
            .rows
            .iter()
            .map(|r| r.values.iter().map(display_value).collect())
            .collect();
        render_table(&headers, &cells, w)?;
        writeln!(w, "({} row(s))", rows.len())
    }

    fn plan(&self, plan: &PlanPreview, w: &mut dyn Write) -> std::io::Result<()> {
        if plan.committed {
            writeln!(w, "COMMITTED:")?;
        }
        writeln!(w, "{}", plan.preview)
    }

    fn describe(
        &self,
        report: &qfs_core::DescribeReport,
        w: &mut dyn Write,
    ) -> std::io::Result<()> {
        render_describe_table(report, w)
    }

    fn error(&self, err: &ExecError, w: &mut dyn Write) -> std::io::Result<()> {
        write!(w, "error[{}]: {}", err.code, err.message)?;
        if let Some(path) = &err.path {
            write!(w, " (at {path})")?;
        }
        if let Some(detail) = &err.detail {
            write!(w, " — {detail}")?;
        }
        writeln!(w)
    }
}

/// Build the stable `{"error":{…}}` JSON envelope (t01-superset: `code` + `kind` + optional
/// `path`/`detail`). Hand-built so the field order is stable for golden tests.
fn error_envelope(err: &ExecError) -> String {
    let mut fields = format!(
        "\"code\":\"{}\",\"kind\":\"{}\",\"message\":\"{}\"",
        escape(err.code),
        escape(err.kind.as_str()),
        escape(&err.message),
    );
    if let Some(path) = &err.path {
        fields.push_str(&format!(",\"path\":\"{}\"", escape(path)));
    }
    if let Some(detail) = &err.detail {
        fields.push_str(&format!(",\"detail\":\"{}\"", escape(detail)));
    }
    format!("{{\"error\":{{{fields}}}}}")
}

/// Minimal JSON string escaping for the hand-built error envelope.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// The human display projection of one [`Value`] (a table cell). Secret-free; bytes are shown
/// by length, JSON/struct/array compactly.
fn display_value(v: &qfs_core::Value) -> String {
    use qfs_core::Value;
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Timestamp(t) => t.to_string(),
        Value::Struct(_) | Value::Array(_) | Value::Json(_) => {
            serde_json::to_string(v).unwrap_or_else(|_| "?".to_string())
        }
        // Value is #[non_exhaustive]: an unmodeled future variant renders via serde.
        _ => serde_json::to_string(v).unwrap_or_else(|_| "?".to_string()),
    }
}

/// Render a `DESCRIBE <path>` report as a readable human block (ticket t39): the archetype +
/// native-verb hint, a columns table, the supported universal verbs, the `CALL` procedures, the
/// prelude aliases, and the pushdown summary — everything an agent (or operator) reads to write
/// the next statement. Secret-free by construction (the report carries schema + capabilities
/// only).
fn render_describe_table(
    report: &qfs_core::DescribeReport,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(w, "path:      {}", report.path)?;
    writeln!(
        w,
        "archetype: {} ({})",
        archetype_label(report.archetype),
        report.native_verbs
    )?;

    // Columns table (name + type + nullability).
    writeln!(w, "columns:")?;
    if report.columns.is_empty() {
        writeln!(w, "  (none — schema filled by the engine at query time)")?;
    } else {
        let headers = vec!["name".to_string(), "type".to_string(), "null".to_string()];
        let cells: Vec<Vec<String>> = report
            .columns
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    column_type_label(&c.ty),
                    if c.nullable { "yes" } else { "no" }.to_string(),
                ]
            })
            .collect();
        let mut indented = Vec::new();
        render_table(&headers, &cells, &mut indented)?;
        for line in String::from_utf8_lossy(&indented).lines() {
            writeln!(w, "  {line}")?;
        }
    }

    // Supported universal verbs (the parse-time gate set).
    let verbs = report.verbs.supported_labels();
    let verbs = if verbs.is_empty() {
        "(none)".to_string()
    } else {
        verbs.join(" ")
    };
    writeln!(w, "verbs:     {verbs}")?;

    // CALL procedures.
    if report.procedures.is_empty() {
        writeln!(w, "procedures: (none)")?;
    } else {
        writeln!(w, "procedures:")?;
        for p in &report.procedures {
            let params: Vec<String> = p
                .params
                .iter()
                .map(|param| format!("{}:{}", param.name, column_type_label(&param.ty)))
                .collect();
            let irr = if p.irreversible {
                "  [irreversible]"
            } else {
                ""
            };
            writeln!(w, "  CALL {}({}){irr}", p.name, params.join(", "))?;
        }
    }

    // Prelude aliases.
    if report.aliases.is_empty() {
        writeln!(w, "aliases:   (none)")?;
    } else {
        let aliases: Vec<String> = report
            .aliases
            .iter()
            .map(|a| format!("{} -> {}", a.name, a.desugars_to))
            .collect();
        writeln!(w, "aliases:   {}", aliases.join(", "))?;
    }

    // Pushdown summary.
    let pd = &report.pushdown;
    if pd.is_local_only() {
        writeln!(w, "pushdown:  (local-only — filter/project run in qfs)")?;
    } else {
        let mut flags = Vec::new();
        for (on, label) in [
            (pd.where_, "where"),
            (pd.project, "project"),
            (pd.limit, "limit"),
            (pd.order, "order"),
            (pd.join, "join"),
            (pd.aggregate, "aggregate"),
            (pd.distinct, "distinct"),
            (pd.group_by, "group_by"),
        ] {
            if on {
                flags.push(label);
            }
        }
        writeln!(w, "pushdown:  {}", flags.join(" "))?;
    }
    Ok(())
}

/// The human label for an [`Archetype`] (a stable, readable name).
fn archetype_label(archetype: qfs_core::Archetype) -> &'static str {
    use qfs_core::Archetype;
    match archetype {
        Archetype::BlobNamespace => "blob",
        Archetype::RelationalTable => "relational",
        Archetype::AppendLog => "append",
        Archetype::ObjectGraphWorkflow => "object-graph",
        _ => "node",
    }
}

/// The human label for a [`ColumnType`]: the serde tag for scalars (`Text`/`Int`/…), and a
/// compact shape for nested types (`Array<…>` / `Struct{…}` / `Json`) so a deeply-nested column
/// stays one readable token instead of a full JSON blob in the table cell.
fn column_type_label(ty: &qfs_core::ColumnType) -> String {
    match serde_json::to_value(ty).ok() {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Object(map)) => {
            // A single-key tagged object: `{"Array": …}` / `{"Struct": …}`.
            if let Some((tag, _)) = map.into_iter().next() {
                match tag.as_str() {
                    "Array" => "Array<…>".to_string(),
                    "Struct" => "Struct{…}".to_string(),
                    other => other.to_string(),
                }
            } else {
                "?".to_string()
            }
        }
        _ => "?".to_string(),
    }
}

/// Render a fixed-width column-aligned table: header row, a `-` rule, then each data row, with
/// every column padded to its widest cell. Own implementation (no comfy-table).
fn render_table(
    headers: &[String],
    rows: &[Vec<String>],
    w: &mut dyn Write,
) -> std::io::Result<()> {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            let len = cell.chars().count();
            if len > widths[i] {
                widths[i] = len;
            }
        }
    }

    write_row(headers, &widths, w)?;
    // The rule under the header.
    let rule: Vec<String> = widths.iter().map(|n| "-".repeat(*n)).collect();
    write_row(&rule, &widths, w)?;
    for row in rows {
        write_row(row, &widths, w)?;
    }
    Ok(())
}

/// Write one space-padded, ` | `-separated table row.
fn write_row(cells: &[String], widths: &[usize], w: &mut dyn Write) -> std::io::Result<()> {
    let mut line = String::new();
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str(" | ");
        }
        let empty = String::new();
        let cell = cells.get(i).unwrap_or(&empty);
        let pad = width.saturating_sub(cell.chars().count());
        line.push_str(cell);
        for _ in 0..pad {
            line.push(' ');
        }
    }
    writeln!(w, "{}", line.trim_end())
}
