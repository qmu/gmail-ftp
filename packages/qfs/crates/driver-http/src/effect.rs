//! [`HttpEffect`] — the owned effect DTO the REST driver realises a plan leaf as (RFD-0001
//! §6), and the decode from a runtime [`EffectNode`] onto it. The driver's apply leg
//! ([`crate::applier`]) builds an [`crate::request::HttpRequest`] from this + the config +
//! the resolved secret, sends it, and decodes the response to rows.
//!
//! ## Why an explicit effect enum
//! The closed core [`EffectKind`] (`Read`/`List`/`Insert`/`Upsert`/`Update`/`Remove`/`Call`)
//! is universal. The REST driver maps the verbs onto HTTP **methods internally** — there are
//! no HTTP-verb keywords in the DSL (RFD §3 "the path is the type"). [`HttpEffect::from_node`]
//! decodes the `(kind, target.path, args)` triple into the concrete REST operation, carrying
//! the optional ad-hoc [`HttpEffect::override_url`] / [`HttpEffect::override_headers`] the
//! `http.get` TVF injects (a no-config one-off request).

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::request::HttpMethod;

/// The well-known column carrying an **absolute override URL** — set by the `http.get` TVF for
/// a no-config one-off request (the URL is the literal argument, not built from a config base).
pub const URL_COL: &str = "__http_url";
/// The well-known column prefix for an **override header**: a column named `__http_h:Accept`
/// carries the value for the `Accept` header. Used by `http.get(url, headers=>{...})`.
pub const HEADER_COL_PREFIX: &str = "__http_h:";

/// One fully-decoded REST effect — what the apply leg executes against the World. Owned DTOs;
/// no `reqwest`/`url` type appears here. `Remove` is inherently irreversible (RFD §10).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct HttpEffect {
    /// The HTTP method the universal verb maps onto.
    pub method: HttpMethod,
    /// The VFS path the effect targets (`/rest/<api>/<segment>/...`). The applier resolves the
    /// resource + base URL from it — unless [`HttpEffect::override_url`] is set.
    pub vfs_path: String,
    /// An absolute override URL (the `http.get` no-config probe). When set, the config base
    /// URL and resource mapping are bypassed and this URL is used verbatim.
    pub override_url: Option<String>,
    /// Ad-hoc override headers (the `http.get` `headers=>{...}` arg). Added on top of the
    /// config `default_headers`.
    pub override_headers: Vec<(String, String)>,
    /// The request body bytes, if the verb carries a payload (`INSERT`/`UPSERT`). The applier
    /// encodes the row args through the codec to produce this; carried pre-encoded for a
    /// `http.get` probe (always `None` there).
    pub body: Option<Vec<u8>>,
    /// Whether this effect is irreversible (`REMOVE` → `DELETE`).
    pub irreversible: bool,
}

/// Why a node could not be decoded into an [`HttpEffect`] — a construction/contract bug
/// surfaced as a terminal effect failure (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// A machine-facing, secret-free reason.
    pub reason: String,
}

impl HttpEffect {
    /// Decode a runtime [`EffectNode`] into the concrete REST operation, mapping the universal
    /// verb onto an HTTP method (`SELECT/READ/LIST→GET`, `INSERT→POST`, `UPSERT→PUT`,
    /// `REMOVE→DELETE`).
    ///
    /// # Errors
    /// [`DecodeError`] if the kind is not one the REST driver services (`UPDATE`/`CALL`) — the
    /// out-of-scope PATCH/procedure verbs.
    pub fn from_node(node: &EffectNode) -> Result<Self, DecodeError> {
        let method = match &node.kind {
            EffectKind::Read | EffectKind::List => HttpMethod::Get,
            EffectKind::Insert => HttpMethod::Post,
            EffectKind::Upsert => HttpMethod::Put,
            EffectKind::Remove => HttpMethod::Delete,
            EffectKind::Update => {
                return Err(DecodeError {
                    reason: "UPDATE (PATCH) is out of scope for the generic REST driver".into(),
                })
            }
            EffectKind::Call(proc) => {
                return Err(DecodeError {
                    reason: format!("CALL {proc} is not serviced by the generic REST driver"),
                })
            }
            other => {
                return Err(DecodeError {
                    reason: format!(
                        "{} is not serviced by the generic REST driver",
                        other.label()
                    ),
                })
            }
        };

        let override_url = read_url_override(node);
        let override_headers = read_header_overrides(node);
        let body = read_body(node);

        Ok(Self {
            method,
            vfs_path: node.target.path.as_str().to_string(),
            override_url,
            override_headers,
            body,
            irreversible: node.irreversible,
        })
    }

    /// The stable verb label (for capability-denied errors / the audit ledger).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self.method {
            HttpMethod::Get => "SELECT",
            HttpMethod::Post => "INSERT",
            HttpMethod::Put => "UPSERT",
            HttpMethod::Delete => "REMOVE",
            // `qfs_http_core::HttpMethod` is a foreign `#[non_exhaustive]` enum: a wildcard is
            // required for a total match. All four mapped methods are handled above; a future
            // variant falls back to its uppercase wire token rather than panicking (lib policy).
            _ => self.method.as_str(),
        }
    }
}

/// Read the `http.get` override URL out of the first row's [`URL_COL`] value, if present.
fn read_url_override(node: &EffectNode) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == URL_COL)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(u)) => Some(u.clone()),
        _ => None,
    }
}

/// Read every `__http_h:<name>` override header out of the first row.
fn read_header_overrides(node: &EffectNode) -> Vec<(String, String)> {
    let Some(row) = node.args.rows.first() else {
        return Vec::new();
    };
    node.args
        .schema
        .columns
        .iter()
        .enumerate()
        .filter_map(|(idx, col)| {
            let name = col.name.strip_prefix(HEADER_COL_PREFIX)?;
            match row.values.get(idx) {
                Some(Value::Text(v)) => Some((name.to_string(), v.clone())),
                _ => None,
            }
        })
        .collect()
}

/// Read a pre-encoded body out of the first row's `__http_body` column (the TVF path). For
/// `INSERT`/`UPSERT` the body is normally encoded from the row args by the applier via the
/// codec, so this is only set on the `http.get` probe path (always `None` there in practice).
fn read_body(node: &EffectNode) -> Option<Vec<u8>> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == "__http_body")?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        Some(Value::Text(t)) => Some(t.clone().into_bytes()),
        _ => None,
    }
}
