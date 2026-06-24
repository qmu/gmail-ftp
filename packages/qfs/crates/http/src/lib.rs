//! The **HTTP serving binding** (t32, RFD-0001 §8): the leaf crate that turns the
//! `/server/endpoints` registry into live HTTP routes — PostgREST/Hasura-style, but over the
//! federated qfs query model.
//!
//! ## What it does
//! An HTTP request is *a cause that makes a plan run* (RFD §6). This crate:
//!   1. reconciles a route table from [`qfs_server::ServerState::endpoints`] through the
//!      generic [`qfs_server::Binding`] seam (t30), hot-swapping it atomically on every
//!      committed `/server/endpoints` mutation;
//!   2. binds path / query-string / (read) body params as **typed [`qfs_core::Value`]s** into
//!      the pre-parsed endpoint query (t31 [`qfs_core::StatementSpec`], rehydrated via
//!      `from_canonical` with NO re-parse), as a typed AST rewrite — never string-spliced, so
//!      a request value carries **zero parse-time injection surface**;
//!   3. evaluates the bound query through the [`qfs_exec`] read executor (t29) to owned rows;
//!   4. encodes the rows via the codec registry (t15): `json` default, `csv` on negotiation;
//!   5. gates writes read-only-by-default (the t32 registration-time policy gate, t34 is the
//!      full engine).
//!
//! ## Topology (the t32 headline decision)
//! qfs-http is a **leaf**: only the terminal `qfs` binary consumes it (the serve composition
//! root, the HTTP sibling of the t28 shell adapter). That keeps three guards green:
//!   * `qfs-server` stays runtime-free and `Binding::reconcile` synchronous + owned-snapshot
//!     (CO-t30-1) — the async listener lives HERE, not in the server;
//!   * `qfs-exec`'s consumer set stays coherent (CO-t29-4): qfs-http is a leaf integration
//!     consumer of the read executor, the same role qfs-cmd plays;
//!   * tokio dead-ends in the terminal binary (the t28 runtime-leaf precondition) — qfs-http
//!     uses tokio for the HTTP I/O domain but never `qfs-runtime`.
//!
//! ## Worker portability (E7/t35)
//! Every native HTTP type is isolated in this crate behind the owned [`HttpRequest`] /
//! [`HttpResponse`] DTOs and the generic [`qfs_server::Binding`] trait. The
//! `EndpointDef → query → codec` pipeline ([`Router::dispatch`]) is vendor-free, so the same
//! pipeline maps to a Cloudflare Worker `fetch` handler later: only the thin wire
//! parse/serialize shim ([`serve`]) is native-specific. No axum (see `docs/adr/0004`).

mod binding;
mod encode;
mod error;
mod handler;
mod params;
mod policy;
mod rewrite;
mod route;
mod serve;

#[cfg(test)]
mod tests;

pub use binding::HttpBinding;
pub use encode::{negotiate, ContentType, DEFAULT_MAX_ROWS};
pub use error::{problem_body, HttpError, ProblemBody};
pub use handler::EndpointCtx;
pub use params::{BindError, QueryArgs};
pub use policy::{assert_read_only, PolicyError};
pub use route::{compile_endpoint, CompileError, RoutePattern, Router};
pub use serve::{
    serve, serve_config, serve_config_full, serve_config_with, serve_on, serve_on_with, Fallback,
    DEFAULT_BIND_ADDR,
};

use std::collections::BTreeMap;

/// An HTTP method, owned and vendor-free. A closed set mirroring the frozen DDL
/// [`qfs_core::HttpMethod`]; `Other` keeps an unrecognised-but-valid token verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// Any other method token, uppercased and kept verbatim.
    Other(String),
}

impl Method {
    /// Parse a method token (case-insensitive), uppercasing an unrecognised token.
    #[must_use]
    pub fn parse(token: &str) -> Self {
        match token.to_ascii_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "PATCH" => Method::Patch,
            "DELETE" => Method::Delete,
            other => Method::Other(other.to_string()),
        }
    }

    /// The canonical uppercase token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Other(s) => s.as_str(),
        }
    }
}

/// An owned, vendor-free HTTP request — the only request shape the binding pipeline ever sees
/// (RFD §9). The native wire parser ([`serve`]) and the test harness both build this; a CF
/// Worker `fetch` would build the identical DTO (E7/t35).
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    /// The request method.
    pub method: Method,
    /// The request path (no query string), e.g. `/items/42`.
    pub path: String,
    /// The parsed query-string params (`?a=1&b=2` → `{a:1, b:2}`), last-wins on duplicates.
    pub query: BTreeMap<String, String>,
    /// The request headers, lowercased keys (a small map; this is not a streaming server).
    pub headers: BTreeMap<String, String>,
    /// The raw request body bytes (read endpoints may bind body params; empty for GET).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A minimal request (the common test constructor): method + path, no query/headers/body.
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    /// Set a query-string param (builder form).
    #[must_use]
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.insert(key.into(), value.into());
        self
    }

    /// Set a header (builder form); the key is lowercased.
    #[must_use]
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(key.into().to_ascii_lowercase(), value.into());
        self
    }
}

/// An owned, vendor-free HTTP response — the only response shape the binding pipeline produces
/// (RFD §9). The native wire serializer ([`serve`]) turns it into HTTP/1.1 bytes; a CF Worker
/// would turn it into a `Response` (E7/t35).
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The `Content-Type` header value (e.g. `application/json`).
    pub content_type: String,
    /// The response body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Construct a response.
    #[must_use]
    pub fn new(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
        }
    }

    /// The body as UTF-8 text (lossy) — a test/debug convenience.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}
