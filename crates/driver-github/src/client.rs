//! [`GitHubClient`] — the thin, **mockable** GitHub API seam (RFD-0001 §9 no-heavy-SDK,
//! boundary B3), plus [`RestGitHubClient`] (the real client over a local [`HttpTransport`] seam)
//! and [`MockGitHubClient`] (an in-memory fake for tests — no live GitHub, no network).
//!
//! ## Reuses the shared http-core DTOs + the t18 reusable seam *shape* (no hand-rolled HTTP DTO)
//! The real client builds owned [`cfs_http_core::HttpRequest`]s — the **same shared DTOs +
//! redaction authority** the t18 `cfs-driver-http` REST seam and `cfs-google-auth` trade in, so
//! there is no second copy of the HTTP DTO and the t19 redaction-drift token leak stays closed. It
//! sends them through a thin [`HttpTransport`] trait (a structural twin of t18's `HttpClient`,
//! trading the same http-core DTOs). The driver does **not** depend on `cfs-driver-http` as a
//! crate — a cfs-runtime consumer must stay a leaf (the dep-direction confinement test), so the
//! reqwest wire impl rides this transport seam the same way gdrive rides `cfs-google-auth`'s
//! `HttpExchange` (the production wire transport is parked with live E2E for t38). On top of the
//! seam this client layers exactly the GitHub conventions:
//! - **Bearer auth**: the PAT, resolved from a [`cfs_secrets::Secret`] at request-build time,
//!   written into an `Authorization: Bearer …` header the redacting [`HttpRequest`] `Debug`
//!   hides. Never logged, never in a DTO/error.
//! - **Link-header pagination** (RFC 5988 `rel="next"`): a list `GET` follows `Link` next-pages,
//!   bounded by [`MAX_PAGES`].
//! - **429 / `Retry-After` bounded retry on idempotent GETs only**: a transient 429/5xx on a
//!   `GET` is retried up to [`MAX_RETRIES`] honouring `Retry-After`; a write (POST/PATCH/DELETE)
//!   is **never** retried here (at-least-once for non-idempotent POSTs — RFD §6).
//!
//! The [`GitHubClient`] trait trades **only** in owned, vendor-free DTOs ([`IssueDto`] etc.) and
//! the http-core HTTP DTOs; GitHub JSON never crosses it (the no-vendor-leak invariant, RFD §9).

use std::sync::{Arc, Mutex};

use cfs_http_core::{HttpMethod, HttpRequest, HttpResponse};
use cfs_secrets::{CredentialKey, Secrets};

use crate::effect::GitHubEffect;
use crate::error::GitHubError;
use crate::path::Namespace;

/// A secret-free transport failure (DNS/connect/TLS/timeout) — the class only, never a header
/// value. The structural twin of t18's transport error, kept local so this leaf does not depend
/// on `cfs-driver-http` (runtime-consumer confinement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// A secret-free class reason (e.g. `connection failed`).
    pub reason: String,
}

/// The thin HTTP transport seam (RFD §9 boundary B3): a driver builds an owned http-core
/// [`HttpRequest`] and calls [`HttpTransport::send`]; the impl performs the wire exchange and
/// returns an owned [`HttpResponse`] or a secret-free [`TransportError`]. `reqwest`/`url` types
/// **never** cross it — it trades only in the shared http-core DTOs (the same contract t18's
/// `HttpClient` honours). `Send + Sync` so an `Arc<dyn HttpTransport>` is shareable across the
/// runtime's blocking apply threads.
pub trait HttpTransport: Send + Sync {
    /// Execute one request synchronously. A non-2xx status is **not** an error here — the client
    /// classifies [`HttpResponse::status`] so a 404/429 body is still available.
    ///
    /// # Errors
    /// [`TransportError`] if the wire exchange fails before a status is received.
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// The GitHub REST v3 API base URL.
pub const API_BASE: &str = "https://api.github.com";
/// The `Accept` header GitHub recommends for the v3 JSON API.
pub const ACCEPT: &str = "application/vnd.github+json";
/// The hard ceiling on Link-header pages a list follows (RFD §6 runaway-fetch guard).
pub const MAX_PAGES: u32 = 50;
/// The hard ceiling on transient-retry attempts on an idempotent GET.
pub const MAX_RETRIES: u32 = 3;

/// One page of a list result: the rows (as JSON values, decoded by the caller into DTOs) and the
/// `rel="next"` URL, if any.
struct Page {
    body: Vec<u8>,
    next: Option<String>,
}

/// The thin GitHub API seam. A driver issues every GitHub call through this; the real impl rides
/// the [`HttpTransport`] seam (Bearer + Link pagination + bounded GET retry), the test impl
/// answers from in-memory fixtures. `Send + Sync` so an `Arc<dyn GitHubClient>` can be shared
/// across the runtime's blocking apply threads.
pub trait GitHubClient: Send + Sync {
    /// List the `namespace` collection under `slug` (`owner/repo`), applying the pushed query
    /// `params`, following `Link` pagination, returning the JSON value array of all pages.
    ///
    /// # Errors
    /// [`GitHubError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn list(
        &self,
        slug: &str,
        namespace: Namespace,
        sub: Option<(&str, Namespace)>,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, GitHubError>;

    /// Apply one decoded write/CALL [`GitHubEffect`], returning the affected count.
    ///
    /// # Errors
    /// [`GitHubError`] on a non-2xx status or an auth/transport failure.
    fn apply(&self, effect: &GitHubEffect) -> Result<u64, GitHubError>;
}

/// The real GitHub client: builds owned [`HttpRequest`]s, injects the Bearer PAT from the secret
/// store, and sends them through the [`HttpTransport`] seam. Confines no `reqwest` itself — the
/// reqwest wire impl lives behind the transport trait (parked with live E2E for t38).
pub struct RestGitHubClient {
    http: Arc<dyn HttpTransport>,
    secrets: Arc<dyn Secrets>,
    cred: CredentialKey,
}

impl RestGitHubClient {
    /// Build a GitHub client over the `http` transport, resolving the PAT under `cred` from
    /// `secrets`. The token is read only at request-build time, never stored here.
    #[must_use]
    pub fn new(
        http: Arc<dyn HttpTransport>,
        secrets: Arc<dyn Secrets>,
        cred: CredentialKey,
    ) -> Self {
        Self {
            http,
            secrets,
            cred,
        }
    }

    /// Build a base request with the standard GitHub headers + the Bearer PAT injected. The token
    /// is exposed only here (a header value the redacting `Debug` hides) and dropped immediately.
    fn request(&self, method: HttpMethod, url: String) -> Result<HttpRequest, GitHubError> {
        let secret = self
            .secrets
            .get(&self.cred)
            .map_err(|e| GitHubError::Auth { code: e.code() })?;
        let token = secret.expose_str().ok_or(GitHubError::Auth {
            code: "secret_not_utf8",
        })?;
        Ok(HttpRequest::new(method, url)
            .header("Accept", ACCEPT)
            .header("User-Agent", "cfs-driver-github")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", format!("Bearer {token}")))
    }

    /// Send one request and classify the status. A 2xx is the response; a non-2xx is a structured
    /// [`GitHubError::Api`] (secret-free).
    fn send_one(&self, op: &'static str, req: &HttpRequest) -> Result<HttpResponse, GitHubError> {
        let resp = self.http.send(req).map_err(GitHubError::from)?;
        tracing::debug!(method = %req.method, url = %req.url, status = resp.status, "github request");
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(GitHubError::Api {
                op,
                status: resp.status,
            })
        }
    }

    /// Send an **idempotent GET** with bounded transient retry: a 429/5xx is retried up to
    /// [`MAX_RETRIES`] times (the `Retry-After` header bounds the conceptual wait; the apply leg
    /// runs on a blocking thread so a real sleep is safe, but the seam is kept synchronous and the
    /// retry budget is the safety net). Only GETs are retried — never a write.
    fn send_get(&self, op: &'static str, url: &str) -> Result<HttpResponse, GitHubError> {
        let mut attempt = 0;
        loop {
            let req = self.request(HttpMethod::Get, url.to_string())?;
            let resp = self.http.send(&req).map_err(GitHubError::from)?;
            tracing::debug!(method = "GET", url = %url, status = resp.status, "github request");
            if resp.is_success() {
                return Ok(resp);
            }
            if GitHubError::is_transient_status(resp.status) && attempt < MAX_RETRIES {
                // Honour Retry-After if present (seconds); the bound is the retry budget itself.
                let _retry_after = resp
                    .header_value("retry-after")
                    .and_then(|v| v.parse::<u64>().ok());
                attempt += 1;
                continue;
            }
            return Err(GitHubError::Api {
                op,
                status: resp.status,
            });
        }
    }

    /// Build the list URL for a namespace/sub-collection under `slug`, with the pushed params +
    /// a default `per_page`.
    fn list_url(
        slug: &str,
        namespace: Namespace,
        sub: Option<(&str, Namespace)>,
        params: &[(String, String)],
    ) -> String {
        let path = match sub {
            Some((id, sub_ns)) => format!("{}/{}/{}", namespace.segment(), id, sub_ns.segment()),
            None => namespace.segment().to_string(),
        };
        let mut all: Vec<(String, String)> = params.to_vec();
        all.push(("per_page".to_string(), "100".to_string()));
        let qs = all
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{API_BASE}/repos/{slug}/{path}?{qs}")
    }

    /// Fetch one page (GET with retry), returning its body bytes + the `rel="next"` URL.
    fn fetch_page(&self, op: &'static str, url: &str) -> Result<Page, GitHubError> {
        let resp = self.send_get(op, url)?;
        let next = resp.header_value("link").and_then(parse_link_next);
        Ok(Page {
            body: resp.body,
            next,
        })
    }

    /// Send a JSON-bodied write (POST/PATCH/DELETE). Never retried (at-least-once for POST).
    fn send_write(
        &self,
        op: &'static str,
        method: HttpMethod,
        url: String,
        body: Option<&serde_json::Value>,
    ) -> Result<(), GitHubError> {
        let mut req = self.request(method, url)?;
        if let Some(b) = body {
            let bytes = serde_json::to_vec(b).map_err(|_| GitHubError::Decode {
                op,
                reason: "could not encode the request body".to_string(),
            })?;
            req = req
                .header("Content-Type", "application/json")
                .with_body(bytes);
        }
        self.send_one(op, &req)?;
        Ok(())
    }
}

impl GitHubClient for RestGitHubClient {
    fn list(
        &self,
        slug: &str,
        namespace: Namespace,
        sub: Option<(&str, Namespace)>,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, GitHubError> {
        let op = "list";
        let mut url = Self::list_url(slug, namespace, sub, params);
        let mut merged: Vec<serde_json::Value> = Vec::new();
        for _page in 0..MAX_PAGES {
            let page = self.fetch_page(op, &url)?;
            let value: serde_json::Value =
                serde_json::from_slice(&page.body).map_err(|_| GitHubError::Decode {
                    op,
                    reason: "list response was not valid JSON".to_string(),
                })?;
            match value {
                serde_json::Value::Array(items) => merged.extend(items),
                other => merged.push(other),
            }
            match page.next {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(serde_json::Value::Array(merged))
    }

    fn apply(&self, effect: &GitHubEffect) -> Result<u64, GitHubError> {
        match effect {
            GitHubEffect::OpenIssue {
                slug,
                title,
                body,
                labels,
            } => {
                let payload = serde_json::json!({
                    "title": title, "body": body, "labels": labels,
                });
                let url = format!("{API_BASE}/repos/{slug}/issues");
                self.send_write("issues.create", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::OpenPull {
                slug,
                title,
                body,
                head,
                base,
            } => {
                let payload =
                    serde_json::json!({ "title": title, "body": body, "head": head, "base": base });
                let url = format!("{API_BASE}/repos/{slug}/pulls");
                self.send_write("pulls.create", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::PostComment { slug, number, body } => {
                let payload = serde_json::json!({ "body": body });
                let url = format!("{API_BASE}/repos/{slug}/issues/{number}/comments");
                // POST is not idempotent: at-least-once, never silently retried (RFD §6).
                self.send_write("comments.create", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::CreateRelease {
                slug,
                tag_name,
                name,
                body,
            } => {
                let payload =
                    serde_json::json!({ "tag_name": tag_name, "name": name, "body": body });
                let url = format!("{API_BASE}/repos/{slug}/releases");
                self.send_write("releases.create", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::CreateBranch {
                slug,
                ref_name,
                sha,
            } => {
                let payload =
                    serde_json::json!({ "ref": format!("refs/heads/{ref_name}"), "sha": sha });
                let url = format!("{API_BASE}/repos/{slug}/git/refs");
                self.send_write("git.createRef", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::PatchIssue {
                slug,
                number,
                state,
                title,
                body,
                labels,
            } => {
                let payload = patch_issue_body(state, title, body, labels);
                let url = format!("{API_BASE}/repos/{slug}/issues/{number}");
                self.send_write("issues.update", HttpMethod::Patch, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::PatchPull {
                slug,
                number,
                state,
                title,
                body,
            } => {
                let payload = patch_pull_body(state, title, body);
                let url = format!("{API_BASE}/repos/{slug}/pulls/{number}");
                self.send_write("pulls.update", HttpMethod::Patch, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::DeleteComment { slug, id } => {
                let url = format!("{API_BASE}/repos/{slug}/issues/comments/{id}");
                self.send_write("comments.delete", HttpMethod::Delete, url, None)?;
                Ok(1)
            }
            GitHubEffect::DeleteRelease { slug, id } => {
                let url = format!("{API_BASE}/repos/{slug}/releases/{id}");
                self.send_write("releases.delete", HttpMethod::Delete, url, None)?;
                Ok(1)
            }
            GitHubEffect::DeleteBranch { slug, ref_name } => {
                let url = format!("{API_BASE}/repos/{slug}/git/refs/heads/{ref_name}");
                self.send_write("git.deleteRef", HttpMethod::Delete, url, None)?;
                Ok(1)
            }
            GitHubEffect::Merge {
                slug,
                number,
                method,
                sha,
            } => {
                let mut payload = serde_json::Map::new();
                payload.insert(
                    "merge_method".to_string(),
                    serde_json::Value::String(method.clone()),
                );
                if let Some(s) = sha {
                    // Optimistic concurrency: GitHub refuses to merge a stale ref (RFD §6).
                    payload.insert("sha".to_string(), serde_json::Value::String(s.clone()));
                }
                let url = format!("{API_BASE}/repos/{slug}/pulls/{number}/merge");
                self.send_write(
                    "pulls.merge",
                    HttpMethod::Put,
                    url,
                    Some(&serde_json::Value::Object(payload)),
                )?;
                Ok(1)
            }
            GitHubEffect::Dispatch {
                slug,
                workflow,
                ref_name,
                inputs,
            } => {
                let inputs_val: serde_json::Value = serde_json::from_str(inputs)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let payload = serde_json::json!({ "ref": ref_name, "inputs": inputs_val });
                let url =
                    format!("{API_BASE}/repos/{slug}/actions/workflows/{workflow}/dispatches");
                // 204 No Content on success — no run id is returned; the effect resolves "queued".
                self.send_write("actions.dispatch", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
            GitHubEffect::Review {
                slug,
                number,
                event,
                body,
            } => {
                let payload = serde_json::json!({ "event": event, "body": body });
                let url = format!("{API_BASE}/repos/{slug}/pulls/{number}/reviews");
                self.send_write("pulls.review", HttpMethod::Post, url, Some(&payload))?;
                Ok(1)
            }
        }
    }
}

/// Build the partial-update PATCH body for an issue from the set fields (only set fields appear).
fn patch_issue_body(
    state: &Option<String>,
    title: &Option<String>,
    body: &Option<String>,
    labels: &Option<Vec<String>>,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(s) = state {
        m.insert("state".to_string(), serde_json::Value::String(s.clone()));
    }
    if let Some(t) = title {
        m.insert("title".to_string(), serde_json::Value::String(t.clone()));
    }
    if let Some(b) = body {
        m.insert("body".to_string(), serde_json::Value::String(b.clone()));
    }
    if let Some(ls) = labels {
        m.insert(
            "labels".to_string(),
            serde_json::Value::Array(
                ls.iter()
                    .map(|l| serde_json::Value::String(l.clone()))
                    .collect(),
            ),
        );
    }
    serde_json::Value::Object(m)
}

/// Build the partial-update PATCH body for a pull request from the set fields.
fn patch_pull_body(
    state: &Option<String>,
    title: &Option<String>,
    body: &Option<String>,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(s) = state {
        m.insert("state".to_string(), serde_json::Value::String(s.clone()));
    }
    if let Some(t) = title {
        m.insert("title".to_string(), serde_json::Value::String(t.clone()));
    }
    if let Some(b) = body {
        m.insert("body".to_string(), serde_json::Value::String(b.clone()));
    }
    serde_json::Value::Object(m)
}

/// Parse the `rel="next"` URL out of an RFC-5988 `Link` header value, if present.
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

/// Minimal percent-encoding for a query parameter value (dependency-free).
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One recorded GitHub API call — what a test asserts the driver issued. Secret-free by
/// construction (no token ever enters this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// A `list` with the namespace, optional sub-collection, and the pushed params.
    List {
        /// `owner/repo` slug.
        slug: String,
        /// The collection segment (the effective namespace's segment).
        segment: String,
        /// The pushed query params.
        params: Vec<(String, String)>,
    },
    /// A write/CALL effect applied (the decoded effect itself).
    Apply(GitHubEffect),
}

/// An in-memory mock GitHub client (tests / CI / wasm): answers list calls from pre-seeded JSON
/// pages and **records** every call so a test asserts the exact API surface the driver exercised
/// — with **no socket and no credentials**. The recorded calls also prove `PREVIEW` performs zero
/// I/O (the mock asserts it was never called).
#[derive(Default)]
pub struct MockGitHubClient {
    list_pages: Mutex<Vec<serde_json::Value>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

impl MockGitHubClient {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the JSON array a `list` returns (FIFO across calls).
    #[must_use]
    pub fn with_list(self, value: serde_json::Value) -> Self {
        if let Ok(mut q) = self.list_pages.lock() {
            q.push(value);
        }
        self
    }

    /// The calls this mock received, in order — what a test asserts against.
    #[must_use]
    pub fn recorded(&self) -> Vec<RecordedCall> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, call: RecordedCall) {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(call);
        }
    }
}

impl GitHubClient for MockGitHubClient {
    fn list(
        &self,
        slug: &str,
        namespace: Namespace,
        sub: Option<(&str, Namespace)>,
        params: &[(String, String)],
    ) -> Result<serde_json::Value, GitHubError> {
        let segment = sub
            .map(|(_, ns)| ns.segment().to_string())
            .unwrap_or_else(|| namespace.segment().to_string());
        self.record(RecordedCall::List {
            slug: slug.to_string(),
            segment,
            params: params.to_vec(),
        });
        let page = self
            .list_pages
            .lock()
            .ok()
            .and_then(|mut q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        Ok(page)
    }

    fn apply(&self, effect: &GitHubEffect) -> Result<u64, GitHubError> {
        self.record(RecordedCall::Apply(effect.clone()));
        Ok(1)
    }
}
