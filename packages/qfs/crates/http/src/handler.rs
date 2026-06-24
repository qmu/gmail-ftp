//! The request handler (t32): the `dispatch` pipeline `bind → rewrite → eval → encode`.
//!
//! Given a matched [`CompiledRoute`] + the request, the handler:
//!   1. binds path / query-string / body params as TYPED values ([`QueryArgs::bind`]);
//!   2. substitutes them into a CLONE of the pre-parsed query AST ([`crate::rewrite`]) — the
//!      injection-safe step (no re-parse, no string splice);
//!   3. re-asserts the read-only policy gate on the lowered plan (defence in depth);
//!   4. evaluates the bound query through the [`qfs_exec`] read executor (t29);
//!   5. encodes the rows via the codec registry (t15) with the negotiated content type and the
//!      bounded result guard.
//!
//! A `tracing` span records the route, the param NAMES only (never values — RFD §10), the
//! status, and the row count. No credential or token is ever logged.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use qfs_core::Engine;
use qfs_exec::{execute_read, ReadRegistry};
use qfs_server::PolicyDef;

use crate::encode::{encode_rows, negotiate};
use crate::error::HttpError;
use crate::params::QueryArgs;
use crate::policy::assert_read_only;
use crate::rewrite::bind_params;
use crate::route::CompiledRoute;
use crate::{HttpRequest, HttpResponse};

/// The shared, request-independent context the handler evaluates against: the engine
/// (mounts + codecs), the read-driver registry, the resolved policies, and the bounded
/// result-row cap. Cloned cheaply (everything is `Arc`/owned-small) into each request future
/// so no lock is held across an `.await` (the t30 rule).
#[derive(Clone)]
pub struct EndpointCtx {
    /// The engine: mounts (resolution) + codecs (encoding).
    pub engine: Arc<Engine>,
    /// The read-driver registry the executor scans through.
    pub reads: Arc<ReadRegistry>,
    /// The LIVE resolved policies (shared with the binding; refreshed on every reconcile so a
    /// hot policy change is visible to the next request without rebuilding the context).
    pub policies: Arc<RwLock<Arc<BTreeMap<String, PolicyDef>>>>,
    /// The bounded in-memory result-row cap (413 beyond it).
    pub max_rows: usize,
}

impl EndpointCtx {
    /// Construct a context over the live shared policy handle.
    #[must_use]
    pub fn new(
        engine: Arc<Engine>,
        reads: Arc<ReadRegistry>,
        policies: Arc<RwLock<Arc<BTreeMap<String, PolicyDef>>>>,
        max_rows: usize,
    ) -> Self {
        Self {
            engine,
            reads,
            policies,
            max_rows,
        }
    }

    /// Snapshot the live policies (clones the inner `Arc`; the guard is dropped immediately so
    /// no lock is held across an `.await`).
    #[must_use]
    fn policies_snapshot(&self) -> Arc<BTreeMap<String, PolicyDef>> {
        self.policies
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(BTreeMap::new()))
    }
}

/// Dispatch a request against a matched route + its extracted path params. Returns the encoded
/// [`HttpResponse`] on success, or an error response (mapped via [`HttpError::into_response`])
/// on any failure stage.
///
/// `path_params` are the named segments the router already extracted for this route.
pub async fn dispatch(
    route: &CompiledRoute,
    path_params: BTreeMap<String, String>,
    req: &HttpRequest,
    ctx: &EndpointCtx,
) -> HttpResponse {
    match dispatch_inner(route, path_params, req, ctx).await {
        Ok(resp) => resp,
        Err(err) => {
            // Trace the failure with the route + status + class only (no values, no secrets).
            tracing::warn!(
                target: "qfs::http",
                route = %route.name,
                status = err.status(),
                class = err.class(),
                "endpoint request failed"
            );
            err.into_response()
        }
    }
}

/// The fallible body of [`dispatch`], returning a structured [`HttpError`] for the caller to
/// render. Kept separate so the happy path is linear and every `?` maps cleanly to a status.
async fn dispatch_inner(
    route: &CompiledRoute,
    path_params: BTreeMap<String, String>,
    req: &HttpRequest,
    ctx: &EndpointCtx,
) -> Result<HttpResponse, HttpError> {
    // 1. Bind path / query-string / body params as TYPED values, validated against the route's
    //    declared params. A missing/extra/type-mismatch → 400 naming the param. RESERVED
    //    negotiation knobs (`format`) are stripped first — they select the codec, not a query
    //    param, so they are not subject to the closed-param contract.
    let body_params = decode_body_params(req);
    let query_params: BTreeMap<String, String> = req
        .query
        .iter()
        .filter(|(k, _)| !is_reserved_query_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let args = QueryArgs::bind(&route.params, &path_params, &query_params, &body_params)?;

    // 2. Substitute into a CLONE of the pre-parsed query (the injection-safe rewrite). The
    //    request value never re-enters the parser; it becomes a single typed literal node.
    let mut bound = route.query.clone();
    bind_params(&mut bound, &args);

    // 3. Defence-in-depth read-only gate on the bound plan (registration already gated it).
    //    Snapshot the live policies BEFORE the gate; no lock is held across the later `.await`.
    let plan = qfs_exec::build_plan(&bound, &ctx.engine).map_err(HttpError::Eval)?;
    let policies = ctx.policies_snapshot();
    let policy = policies.get(&route.name);
    assert_read_only(&plan, policy).map_err(HttpError::Policy)?;

    // 4. Evaluate the bound query through the qfs-exec read executor (t29).
    let rows = execute_read(&bound, &ctx.engine.mounts, &ctx.reads)
        .await
        .map_err(HttpError::Eval)?;

    // 5. Negotiate + encode via the codec registry, with the bounded result guard.
    let content = negotiate(req);
    let row_count = rows.len();
    let body = encode_rows(rows, content, &ctx.engine.codecs, ctx.max_rows)?;

    tracing::info!(
        target: "qfs::http",
        route = %route.name,
        params = ?route.params,
        status = 200u16,
        rows = row_count,
        "endpoint request served"
    );

    Ok(HttpResponse::new(200, content.header(), body))
}

/// Whether a query-string key is a RESERVED negotiation knob (not an endpoint param). `format`
/// selects the response codec ([`crate::encode::negotiate`]); it is excluded from the closed
/// param contract so a `?format=csv` request is not rejected as an extra param.
fn is_reserved_query_key(key: &str) -> bool {
    matches!(key, "format")
}

/// Decode body params for a read endpoint. The t32 read contract binds simple
/// `application/x-www-form-urlencoded` body params (the common AI-agent POST-read shape);
/// anything else yields an empty map (the bind then relies on path/query params). A malformed
/// body is not an error — it simply contributes no params.
fn decode_body_params(req: &HttpRequest) -> BTreeMap<String, String> {
    let is_form = req.headers.get("content-type").is_some_and(|ct| {
        ct.to_ascii_lowercase()
            .contains("application/x-www-form-urlencoded")
    });
    if !is_form || req.body.is_empty() {
        return BTreeMap::new();
    }
    let text = String::from_utf8_lossy(&req.body);
    parse_urlencoded(&text)
}

/// Parse an `a=1&b=2` urlencoded string into a map (last-wins on duplicates). A minimal
/// decoder: `+` → space and `%XX` percent-decoding for the common cases. No external crate.
fn parse_urlencoded(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in text.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

/// Minimal percent-decode (`+` → space, `%XX` → byte). Falls back to the raw text on a
/// malformed escape (never panics).
fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let mut out = Vec::with_capacity(bytes.len());
    let raw = bytes.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(byte) = u8::from_str_radix(&bytes[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
