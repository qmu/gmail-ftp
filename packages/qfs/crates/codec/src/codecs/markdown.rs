//! `MarkdownFrontmatterCodec` — a markdown file with optional YAML frontmatter ↔ a
//! single row (RFD §4: "a markdown file with YAML frontmatter becomes a queryable table,
//! frontmatter keys = columns, body = content"). This is what makes
//! `.workaholic/**/*.md` itself a relation.
//!
//! Decode: if the document starts with a `---` line, everything up to the next `---`
//! line is parsed as YAML and its keys become columns; the remaining text becomes the
//! `body` column. A document with no frontmatter decodes to a single `body` column.
//! Encode: the **first** row's `body` column (if present) is the body; all other columns
//! are serialized back as a key-stable YAML frontmatter block (deterministic, RFD §6).
//!
//! Documented non-preservation (RFD §4): comments and exact whitespace in the
//! frontmatter are not preserved; only the data and the body text round-trip.

use qfs_types::{Column, ColumnType, Row, Schema, Value};

use crate::convert::{json_to_value, value_to_json};
use crate::{CfsError, Codec, RowBatch};

/// The fence that delimits a YAML frontmatter block.
const FENCE: &str = "---";
/// The column name the document body is mapped to.
const BODY: &str = "body";

/// The `md+frontmatter` codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct MarkdownFrontmatterCodec;

impl Codec for MarkdownFrontmatterCodec {
    fn fmt(&self) -> &str {
        "md+frontmatter"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let text = std::str::from_utf8(bytes).map_err(|e| CfsError::Decode {
            fmt: "md+frontmatter",
            detail: format!("invalid utf-8: {e}"),
        })?;

        let (frontmatter, body) = split_frontmatter(text);

        let mut columns = Vec::new();
        let mut values = Vec::new();

        if let Some(fm) = frontmatter {
            let parsed: serde_json::Value =
                serde_yaml::from_str(fm).map_err(|e| CfsError::Decode {
                    fmt: "md+frontmatter",
                    detail: format!("frontmatter yaml: {e}"),
                })?;
            if let serde_json::Value::Object(map) = parsed {
                for (key, child) in &map {
                    let value = json_to_value(child);
                    columns.push(Column::new(
                        key.clone(),
                        value.type_of(),
                        matches!(value, Value::Null),
                    ));
                    values.push(value);
                }
            }
        }

        columns.push(Column::new(BODY, ColumnType::Text, false));
        values.push(Value::Text(body.to_string()));

        let schema = Schema::new(columns);
        Ok(RowBatch::new(schema, vec![Row::new(values)]))
    }

    fn encode(&self, batch: &RowBatch) -> Result<Vec<u8>, CfsError> {
        let Some(row) = batch.rows.first() else {
            return Ok(Vec::new());
        };

        let mut frontmatter = serde_json::Map::new();
        let mut body = String::new();
        for (col, value) in batch.schema.columns.iter().zip(&row.values) {
            if col.name == BODY {
                if let Value::Text(t) = value {
                    body = t.clone();
                }
            } else {
                frontmatter.insert(col.name.clone(), value_to_json(value));
            }
        }

        let mut out = String::new();
        if !frontmatter.is_empty() {
            let yaml =
                serde_yaml::to_string(&serde_json::Value::Object(frontmatter)).map_err(|e| {
                    CfsError::Encode {
                        fmt: "md+frontmatter",
                        detail: e.to_string(),
                    }
                })?;
            out.push_str(FENCE);
            out.push('\n');
            out.push_str(&yaml);
            out.push_str(FENCE);
            out.push('\n');
        }
        out.push_str(&body);
        Ok(out.into_bytes())
    }
}

/// Split a markdown document into `(frontmatter, body)`. The frontmatter is `Some` only
/// when the document begins with a `---` fence and a closing `---` fence is found; the
/// body is everything after the closing fence (with one leading newline trimmed).
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    // The opening fence must be the very first line.
    let rest = match text.strip_prefix(FENCE) {
        Some(after) if after.starts_with('\n') || after.is_empty() => {
            after.trim_start_matches('\n')
        }
        // `---` followed by other chars on the same line is not a fence.
        _ => return (None, text),
    };

    // Find the closing fence at the start of a line.
    let mut search_from = 0;
    while let Some(rel) = rest[search_from..].find(FENCE) {
        let pos = search_from + rel;
        let at_line_start = pos == 0 || rest[..pos].ends_with('\n');
        let after = &rest[pos + FENCE.len()..];
        let ends_line = after.is_empty() || after.starts_with('\n');
        if at_line_start && ends_line {
            let frontmatter = rest[..pos].trim_end_matches('\n');
            let body = after.strip_prefix('\n').unwrap_or(after);
            return (Some(frontmatter), body);
        }
        search_from = pos + FENCE.len();
    }
    // Unterminated frontmatter: treat the whole thing as body (lenient, RFD §4).
    (None, text)
}
