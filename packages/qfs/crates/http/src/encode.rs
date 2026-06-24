//! Response encoding (t32): content negotiation + codec-registry encoding + the bounded
//! result-size guard.
//!
//! Rows are encoded via the **codec registry** (t15) — `ENCODE json` (default) or `ENCODE
//! csv` (on `Accept: text/csv` / `?format=csv`). No bespoke serializer: the owned
//! [`qfs_exec::RowSet`] is turned back into an owned [`qfs_core::RowBatch`] and handed to the
//! registry codec, which returns bytes. No vendor type crosses this seam (RFD §9).

use qfs_core::{CodecRegistry, RowBatch};
use qfs_exec::RowSet;

use crate::error::HttpError;
use crate::HttpRequest;

/// The default bounded in-memory result-row cap. A result exceeding this returns `413` rather
/// than buffering unboundedly (the ticket's bounded-buffer guard; pagination is a follow-up).
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// The negotiated content type: which codec format + its HTTP `Content-Type` header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// `application/json` via the `json` codec (the default).
    Json,
    /// `text/csv` via the `csv` codec.
    Csv,
}

impl ContentType {
    /// The codec-registry format name (`"json"` / `"csv"`).
    #[must_use]
    pub fn codec_fmt(self) -> &'static str {
        match self {
            ContentType::Json => "json",
            ContentType::Csv => "csv",
        }
    }

    /// The HTTP `Content-Type` header value.
    #[must_use]
    pub fn header(self) -> &'static str {
        match self {
            ContentType::Json => "application/json",
            ContentType::Csv => "text/csv",
        }
    }
}

/// Negotiate the response codec from the request: `?format=csv` or `Accept: text/csv` selects
/// CSV; everything else defaults to JSON. The explicit `?format=` query param wins over the
/// `Accept` header (a deliberate caller override).
#[must_use]
pub fn negotiate(req: &HttpRequest) -> ContentType {
    if let Some(fmt) = req.query.get("format") {
        return match fmt.to_ascii_lowercase().as_str() {
            "csv" => ContentType::Csv,
            _ => ContentType::Json,
        };
    }
    match req.headers.get("accept") {
        Some(accept) if accept.to_ascii_lowercase().contains("text/csv") => ContentType::Csv,
        _ => ContentType::Json,
    }
}

/// Encode a [`RowSet`] to response bytes via the codec registry, enforcing the bounded
/// result-size guard. The owned rows are rebuilt into a [`RowBatch`] and encoded by the
/// registry codec for `content` — no bespoke serializer.
///
/// # Errors
///   * [`HttpError::Oversize`] (413) if the result exceeds `max_rows`.
///   * [`HttpError::Internal`] (500) if the codec is missing or encoding fails — sanitised,
///     never an upstream raw error.
pub fn encode_rows(
    rows: RowSet,
    content: ContentType,
    codecs: &CodecRegistry,
    max_rows: usize,
) -> Result<Vec<u8>, HttpError> {
    if rows.len() > max_rows {
        return Err(HttpError::Oversize { max: max_rows });
    }
    let codec = codecs.resolve(content.codec_fmt()).map_err(|_| {
        HttpError::Internal(format!("no `{}` codec registered", content.codec_fmt()))
    })?;
    let batch = RowBatch::new(rows.schema, rows.rows);
    codec
        .encode(&batch)
        .map_err(|_| HttpError::Internal("failed to encode response rows".to_string()))
}
