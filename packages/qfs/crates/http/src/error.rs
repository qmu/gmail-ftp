//! HTTP error mapping (t32): every structured engine error → an HTTP status + a
//! machine-readable JSON **problem body** `{ "error", "detail", "param"? }`.
//!
//! ## Sanitisation (RFD §10) — UNCONDITIONAL hygiene (t37)
//! The body NEVER surfaces credentials or a raw upstream error. As of t37 this no longer relies
//! on every driver emitting a secret-free `CfsError`: for the non-allowlisted eval classes
//! (`Auth`/`Internal`), where a careless driver is most likely to embed upstream/credential text,
//! the body renders only the stable structured `code` plus a generic per-class detail (the raw
//! `message` is dropped — see [`eval_detail`]). The allowlisted/safe classes (the executor's own
//! well-typed diagnostics) keep their message. No token is ever placed in a body or logged.

use qfs_exec::{ErrorKind, ExecError};
use serde::Serialize;

use crate::params::BindError;
use crate::policy::PolicyError;
use crate::route::CompileError;

/// The HTTP-layer error: one variant per failure stage, each mapping to a status + problem
/// body. The handler builds one of these and renders it via [`HttpError::into_response`].
#[derive(Debug, Clone, PartialEq)]
pub enum HttpError {
    /// A param bind failure (missing / extra / type-mismatch). → 400, names the param.
    Bind(BindError),
    /// A read-only-policy denial (a write effect with no granting policy). → 403.
    Policy(PolicyError),
    /// No endpoint matched the request method+path. → 404.
    NotFound,
    /// A query evaluation failure (resolve / plan / scan). → 422.
    Eval(ExecError),
    /// The bounded result-size guard tripped (too many rows). → 413.
    Oversize {
        /// The configured maximum row count.
        max: usize,
    },
    /// An unexpected internal error (a sanitised, secret-free message). → 500.
    Internal(String),
}

/// The machine-readable JSON problem body (`{ "error", "detail", "param"? }`). Owned and
/// secret-free; the only response shape an error path produces.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProblemBody {
    /// The stable, coarse error class (e.g. `bind`, `policy`, `not_found`, `eval`, `oversize`,
    /// `internal`) — what an agent branches on.
    pub error: String,
    /// A sanitised, human/agent-facing detail message. Never a credential or raw upstream text.
    pub detail: String,
    /// The offending parameter name, for a bind error (so the caller can fix the request).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl HttpError {
    /// The HTTP status code for this error.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            HttpError::Bind(_) => 400,
            HttpError::Policy(_) => 403,
            HttpError::NotFound => 404,
            HttpError::Eval(e) => eval_status(e),
            HttpError::Oversize { .. } => 413,
            HttpError::Internal(_) => 500,
        }
    }

    /// The stable coarse `error` class string for the problem body.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            HttpError::Bind(_) => "bind",
            HttpError::Policy(_) => "policy",
            HttpError::NotFound => "not_found",
            HttpError::Eval(_) => "eval",
            HttpError::Oversize { .. } => "oversize",
            HttpError::Internal(_) => "internal",
        }
    }

    /// Build the owned, secret-free [`ProblemBody`] for this error.
    #[must_use]
    pub fn problem(&self) -> ProblemBody {
        match self {
            HttpError::Bind(b) => ProblemBody {
                error: "bind".to_string(),
                detail: b.detail(),
                param: Some(b.param().to_string()),
            },
            HttpError::Policy(p) => ProblemBody {
                error: "policy".to_string(),
                detail: p.to_string(),
                param: None,
            },
            HttpError::NotFound => ProblemBody {
                error: "not_found".to_string(),
                detail: "no endpoint matches this method and path".to_string(),
                param: None,
            },
            // t37 carry-over (t32 HTTP eval-error hygiene): caller-facing hygiene is now
            // UNCONDITIONAL, not driver-dependent. Previously the body copied `ExecError.message`
            // verbatim, so a secret-free body relied on every driver emitting a secret-free
            // `CfsError` — a single careless driver could leak an upstream token into the 4xx/5xx
            // body. We instead render the STABLE structured `code` plus a GENERIC per-class detail
            // for the non-allowlisted classes (Auth / Internal), where the message is most likely
            // to carry driver/upstream text. The structured-but-safe classes (Parse / Usage /
            // Capability / CommitRequired / CommitFailed) keep their message: these are the
            // executor's own well-typed, secret-free diagnostics that an agent branches on.
            HttpError::Eval(e) => ProblemBody {
                error: "eval".to_string(),
                detail: eval_detail(e),
                param: None,
            },
            HttpError::Oversize { max } => ProblemBody {
                error: "oversize".to_string(),
                detail: format!("result exceeds the {max}-row response limit"),
                param: None,
            },
            HttpError::Internal(msg) => ProblemBody {
                error: "internal".to_string(),
                detail: msg.clone(),
                param: None,
            },
        }
    }

    /// Render this error as a complete HTTP response (`application/json` problem body).
    #[must_use]
    pub fn into_response(self) -> crate::HttpResponse {
        let status = self.status();
        let body = problem_body(&self.problem());
        crate::HttpResponse::new(status, "application/json", body)
    }
}

/// Serialize a [`ProblemBody`] to JSON bytes. Falls back to a minimal hand-built body if
/// serialization ever fails (it cannot, for this owned shape) so the path stays panic-free.
#[must_use]
pub fn problem_body(problem: &ProblemBody) -> Vec<u8> {
    serde_json::to_vec(problem).unwrap_or_else(|_| {
        br#"{"error":"internal","detail":"failed to encode error body"}"#.to_vec()
    })
}

/// t37: render the caller-facing detail for an eval error with UNCONDITIONAL hygiene.
///
/// For the **allowlisted/safe** classes — the executor's own well-typed diagnostics
/// (`Parse`/`Usage`/`Capability`/`CommitRequired`/`CommitFailed`) — we keep the structured
/// `message`: it is the executor's secret-free, agent-facing text (and these never carry a raw
/// upstream/driver string). For the **non-allowlisted** classes (`Auth`/`Internal`) — where the
/// message is most likely to embed an upstream/driver error that a careless driver could fill
/// with sensitive text — we DROP the message entirely and emit only the stable structured `code`
/// plus a fixed generic per-class detail. Caller-facing hygiene therefore no longer depends on
/// every driver emitting a secret-free `CfsError`.
#[must_use]
fn eval_detail(e: &ExecError) -> String {
    match e.kind {
        // Safe classes: the structured executor message is kept (secret-free by construction).
        ErrorKind::Parse
        | ErrorKind::Usage
        | ErrorKind::Capability
        | ErrorKind::CommitRequired
        | ErrorKind::CommitFailed => e.message.clone(),
        // Non-allowlisted: reduce to `code` + a generic detail; NEVER the raw message.
        ErrorKind::Auth => {
            format!(
                "{}: a credential/authorization error occurred upstream",
                e.code
            )
        }
        ErrorKind::Internal => format!("{}: an internal error occurred", e.code),
    }
}

/// Map an evaluation [`ExecError`] to an HTTP status. A capability denial (an unreadable
/// source / undeclared verb) is the caller asking for something the federation cannot serve
/// → 422 (unprocessable), NOT 500. A genuine internal error → 500.
fn eval_status(e: &ExecError) -> u16 {
    match e.kind {
        // Parse/usage/capability eval failures are "the request's query cannot be processed".
        ErrorKind::Parse | ErrorKind::Usage | ErrorKind::Capability => 422,
        ErrorKind::CommitRequired | ErrorKind::CommitFailed => 422,
        ErrorKind::Auth => 403,
        ErrorKind::Internal => 500,
    }
}

impl From<BindError> for HttpError {
    fn from(e: BindError) -> Self {
        HttpError::Bind(e)
    }
}

impl From<PolicyError> for HttpError {
    fn from(e: PolicyError) -> Self {
        HttpError::Policy(e)
    }
}

impl From<CompileError> for HttpError {
    fn from(e: CompileError) -> Self {
        match e {
            CompileError::Policy(p) => HttpError::Policy(p),
            other => HttpError::Internal(other.to_string()),
        }
    }
}
