//! [`RestApplier`] — the REST driver's synchronous apply leg (RFD-0001 §6). It is the lone
//! impure seam the introspective [`crate::RestDriver`] hands back via `applier()`, and the
//! [`qfs_runtime::SharedApplier`] the runtime's [`qfs_runtime::PlanApplierBridge`] drives
//! under `COMMIT`.
//!
//! This is also the **reusable REST request/response machinery** t24 (GitHub) and t25 (Slack)
//! layer on: build a request from `(verb, config, secret, rows)`, send it through the injected
//! [`crate::client::HttpClient`], classify the status into a structured error, decode the body
//! through the codec registry to rows, and follow pagination at the edge. None of it is
//! API-specific — a specific API supplies a [`crate::config::RestApiConfig`] and reuses all of
//! it.
//!
//! Stateless across the request: it holds the config, the codec, the client, and a shared
//! [`qfs_secrets::Secrets`] handle behind `Arc`s, performing fresh World I/O on every call —
//! so it implements `SharedApplier` (`&self` apply), the statelessness contract the bridge
//! requires.

use std::sync::Arc;

use qfs_codec::{Codec, RowBatch};
use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};
use qfs_secrets::Secrets;

use crate::config::{AuthStrategy, Pagination, RestApiConfig};
use crate::effect::HttpEffect;
use crate::error::HttpError;
use crate::request::{HttpMethod, HttpRequest, HttpResponse};

/// The synchronous REST apply leg. Holds the per-instance config, the resolved response codec,
/// the HTTP transport client, and the shared secrets surface — all behind `Arc` so the leg is
/// cheap to clone for the runtime bridge and safe to share across blocking apply threads.
#[derive(Clone)]
pub struct RestApplier {
    config: Arc<RestApiConfig>,
    codec: Arc<dyn Codec>,
    client: Arc<dyn crate::client::HttpClient>,
    secrets: Arc<dyn Secrets>,
}

impl RestApplier {
    /// Build an applier for `config`, decoding responses with `codec`, sending through
    /// `client`, and resolving auth through `secrets`.
    #[must_use]
    pub fn new(
        config: Arc<RestApiConfig>,
        codec: Arc<dyn Codec>,
        client: Arc<dyn crate::client::HttpClient>,
        secrets: Arc<dyn Secrets>,
    ) -> Self {
        Self {
            config,
            codec,
            client,
            secrets,
        }
    }

    /// Borrow the config (e.g. for the driver's introspective methods).
    #[must_use]
    pub fn config(&self) -> &RestApiConfig {
        &self.config
    }

    /// Apply one decoded [`HttpEffect`]: build the base request, send it (following pagination
    /// for a paginated `GET`), and return the affected row count. The single place World I/O
    /// happens. Returns the decoded rows alongside the count so the interpreter (E1/E4) can
    /// surface them; the runtime's [`EffectOutput`] carries only the count today.
    fn apply_effect(&self, effect: &HttpEffect) -> Result<(RowBatch, u64), HttpError> {
        let base = self.build_request(effect)?;
        // A bodyless GET may paginate; every other method is a single exchange.
        if matches!(effect.method, HttpMethod::Get) && effect.override_url.is_none() {
            self.send_paginated(base)
        } else {
            let resp = self.send_one(&base)?;
            let rows = self.decode(&resp)?;
            let n = rows.rows.len() as u64;
            Ok((rows, n))
        }
    }

    /// Build the base [`HttpRequest`] for an effect: resolve the URL (override or
    /// base+resource), layer config `default_headers` then any override headers, inject the
    /// resolved auth header, and attach the body.
    fn build_request(&self, effect: &HttpEffect) -> Result<HttpRequest, HttpError> {
        let url = self.resolve_url(effect)?;
        let mut req = HttpRequest::new(effect.method, url);
        for (name, value) in &self.config.default_headers {
            req = req.header(name.clone(), value.clone());
        }
        for (name, value) in &effect.override_headers {
            req = req.header(name.clone(), value.clone());
        }
        req = self.inject_auth(req)?;
        if let Some(body) = &effect.body {
            req = req.with_body(body.clone());
        }
        Ok(req)
    }

    /// Resolve the request URL: the `http.get` override verbatim, or the config base URL joined
    /// with the resource path (the segment(s) after `/rest/<api>`).
    fn resolve_url(&self, effect: &HttpEffect) -> Result<String, HttpError> {
        if let Some(u) = &effect.override_url {
            return Ok(u.clone());
        }
        let resource_path =
            resource_path_of(&effect.vfs_path).ok_or_else(|| HttpError::Invalid {
                reason: format!("path {:?} names no /rest/<api>/<resource>", effect.vfs_path),
            })?;
        let base = self.config.base_url.trim_end_matches('/');
        Ok(format!("{base}/{resource_path}"))
    }

    /// Inject the auth header from the configured [`AuthStrategy`], resolving the secret
    /// through the [`Secrets`] handle at commit time. The token is read via `Secret::expose`
    /// **only** here, written into a header value, and never logged (the request's `Debug`
    /// redacts it).
    fn inject_auth(&self, req: HttpRequest) -> Result<HttpRequest, HttpError> {
        let (header_name, value_prefix, secret_ref) = match &self.config.auth {
            AuthStrategy::None => return Ok(req),
            AuthStrategy::Bearer { secret_ref } => ("Authorization", "Bearer ", secret_ref),
            AuthStrategy::Header { name, secret_ref } => (name.as_str(), "", secret_ref),
        };
        let key = secret_ref
            .credential_key()
            .map_err(|code| HttpError::Auth { code })?;
        let secret = self
            .secrets
            .get(&key)
            .map_err(|e| HttpError::Auth { code: e.code() })?;
        // `expose_str` is the single, grep-able door to the token; it lands in a header value
        // and is dropped immediately. The header value is redacted in every log surface.
        let token = secret.expose_str().ok_or(HttpError::Auth {
            code: "secret_not_utf8",
        })?;
        let value = format!("{value_prefix}{token}");
        Ok(req.header(header_name.to_string(), value))
    }

    /// Send a single request and classify the status: a 2xx is the response; a >= 400 is a
    /// structured [`HttpError`] (server/transient vs client/terminal). Emits a structured,
    /// **redacted** request log (method + URL + status; never an auth header).
    fn send_one(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let resp = self.client.send(req)?;
        // Structured observability (RFD §6): URL + status, redacted headers. `req` Debug is
        // already redacting, but we log only the safe scalar fields explicitly.
        tracing::debug!(
            method = %req.method,
            url = %req.url,
            status = resp.status,
            "rest request"
        );
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(HttpError::from_status(
                resp.status,
                req.method.as_str(),
                &req.url,
            ))
        }
    }

    /// Follow pagination for a `GET`, concatenating page rows up to the strategy's
    /// `max_pages` cap (RFD §6 — bound runaway fetches). The follow loop lives here, at the
    /// edge, so the *plan* stays a single pure `HttpEffect` (the ticket's hard-part note).
    fn send_paginated(&self, first: HttpRequest) -> Result<(RowBatch, u64), HttpError> {
        let cap = self.config.pagination.max_pages().max(1);
        let mut req = first;
        let mut all: Option<RowBatch> = None;
        let mut total: u64 = 0;
        for _page in 0..cap {
            let resp = self.send_one(&req)?;
            let batch = self.decode(&resp)?;
            total += batch.rows.len() as u64;
            all = Some(match all {
                None => batch,
                Some(mut acc) => {
                    acc.rows.extend(batch.rows);
                    acc
                }
            });
            match self.next_request(&req, &resp)? {
                Some(next) => req = next,
                None => break,
            }
        }
        Ok((all.unwrap_or_default(), total))
    }

    /// Compute the next-page request from the current request + response per the configured
    /// [`Pagination`] strategy, or `None` when there is no further page.
    fn next_request(
        &self,
        current: &HttpRequest,
        resp: &HttpResponse,
    ) -> Result<Option<HttpRequest>, HttpError> {
        match &self.config.pagination {
            Pagination::None => Ok(None),
            Pagination::Cursor {
                next_field, param, ..
            } => {
                let cursor = cursor_from_body(&resp.body, next_field);
                Ok(cursor.map(|c| {
                    let url = set_query_param(&current.url, param, &c);
                    rebuild_with_url(current, url)
                }))
            }
            Pagination::LinkHeader { .. } => {
                let next = resp
                    .header_value("link")
                    .and_then(parse_link_next)
                    .map(|url| rebuild_with_url(current, url));
                Ok(next)
            }
        }
    }

    /// Decode a response body to rows through the configured codec.
    fn decode(&self, resp: &HttpResponse) -> Result<RowBatch, HttpError> {
        self.codec.decode(&resp.body).map_err(|e| match e {
            qfs_codec::CfsError::Decode { fmt, detail } => HttpError::Decode { fmt, detail },
            other => HttpError::Decode {
                fmt: "unknown",
                detail: other.to_string(),
            },
        })
    }
}

impl SharedApplier for RestApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let effect = HttpEffect::from_node(node).map_err(|e| EffectError::terminal(e.reason))?;
        let retry_safe = effect.method.is_retry_safe();
        let (_rows, affected) = self
            .apply_effect(&effect)
            .map_err(|e| e.into_effect_error(retry_safe))?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for RestApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous,
    /// `&mut self` apply leg. The REST applier is stateless, so this delegates to the same
    /// `&self` core as [`SharedApplier::apply_shared`]. The structured [`HttpError`] is
    /// reduced to the plan crate's owned `(id, reason)` shape so no driver type leaks into
    /// `qfs-plan` — and the reason is secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let effect = HttpEffect::from_node(node).map_err(|e| ApplyError::new(node.id, e.reason))?;
        let (_rows, affected) = self
            .apply_effect(&effect)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}

/// The resource path of a `/rest/<api>/<resource>/...` VFS path — everything after the api
/// segment (segments 2..). Returns `None` if the path has no resource segment.
fn resource_path_of(vfs: &str) -> Option<String> {
    // /rest/<api>/<resource>/<rest...>  → join from the 3rd non-empty segment onward.
    let segments: Vec<&str> = vfs.split('/').filter(|s| !s.is_empty()).collect();
    // [rest, <api>, <resource>, ...]
    if segments.len() < 3 || segments[0] != "rest" {
        return None;
    }
    Some(segments[2..].join("/"))
}

/// The resource segment (`segments[2]`) of a `/rest/<api>/<resource>/...` VFS path — the key
/// the config's `resource_for_segment` matches for capability gating.
#[must_use]
pub(crate) fn resource_segment_of(vfs: &str) -> Option<String> {
    let segments: Vec<&str> = vfs.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 || segments[0] != "rest" {
        return None;
    }
    Some(segments[2].to_string())
}

/// Read a string cursor out of a JSON response body at the top-level `field`. Returns `None`
/// if the body is not a JSON object, the field is absent, or the value is null/non-stringy.
fn cursor_from_body(body: &[u8], field: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match v.get(field)? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Set (replacing any existing) a query `param=value` on `url`. A minimal, dependency-free
/// query splice — sufficient for the thin "passthrough param" pushdown this ticket scopes
/// (full WHERE→query lowering is deferred to E3).
fn set_query_param(url: &str, param: &str, value: &str) -> String {
    let encoded_value = percent_encode(value);
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    match query {
        None => format!("{base}?{param}={encoded_value}"),
        Some(q) => {
            let kept: Vec<&str> = q
                .split('&')
                .filter(|kv| {
                    let key = kv.split('=').next().unwrap_or("");
                    key != param
                })
                .collect();
            let prefix = if kept.is_empty() {
                String::new()
            } else {
                format!("{}&", kept.join("&"))
            };
            format!("{base}?{prefix}{param}={encoded_value}")
        }
    }
}

/// Minimal percent-encoding for a query value (encode the characters that would break the
/// query string). Dependency-free; covers the cursor/token case.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse the `rel="next"` URL out of an RFC 5988 `Link` header value, if present.
fn parse_link_next(link: &str) -> Option<String> {
    for part in link.split(',') {
        let part = part.trim();
        if !part.contains("rel=\"next\"") && !part.contains("rel=next") {
            continue;
        }
        let start = part.find('<')?;
        let end = part[start + 1..].find('>')? + start + 1;
        return Some(part[start + 1..end].to_string());
    }
    None
}

/// Rebuild a request preserving method/headers/body but with a new URL (the next page).
fn rebuild_with_url(current: &HttpRequest, url: String) -> HttpRequest {
    let mut next = HttpRequest::new(current.method, url);
    next.headers = current.headers.clone();
    next.body = current.body.clone();
    next
}
