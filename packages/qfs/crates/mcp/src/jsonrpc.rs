//! JSON-RPC 2.0 framing (t47) — the wire envelope every MCP method rides.
//!
//! This is a deliberately small subset of JSON-RPC 2.0, sufficient for the MCP request/response
//! transport (a single request object per `POST /mcp`, a single response object back). It is
//! **pure** (owned DTOs + serde, no I/O), so the framing is unit-testable against golden wire
//! shapes with no listener and no engine.
//!
//! ## Scope (honestly bounded)
//! - One request object per call — **batch** requests (a JSON array of requests) are NOT handled
//!   this milestone; the 2025-06-18 MCP spec removed JSON-RPC batching, so a single object is the
//!   forward-looking shape. A batch body is reported as an invalid-request error.
//! - A request with **no `id`** (or a `notifications/*` method) is a NOTIFICATION: it is handled
//!   for its effect but produces **no** response object (JSON-RPC forbids responding to one).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON-RPC version literal every request/response carries.
pub const JSONRPC_VERSION: &str = "2.0";

// --- standard JSON-RPC error codes (the closed set this surface emits) -----------------------

/// Invalid JSON was received (the body did not parse).
pub const CODE_PARSE_ERROR: i64 = -32700;
/// The JSON was not a valid Request object.
pub const CODE_INVALID_REQUEST: i64 = -32600;
/// The method does not exist / is not supported.
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Invalid method parameters.
pub const CODE_INVALID_PARAMS: i64 = -32602;
/// An internal JSON-RPC error.
pub const CODE_INTERNAL_ERROR: i64 = -32603;

/// An incoming JSON-RPC request. `id` is absent for a notification; `params` is method-specific
/// (an object or array, or absent). `jsonrpc` is accepted leniently (we do not hard-fail a
/// missing/odd version string — robustness over pedantry for a localhost dev surface).
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// The JSON-RPC version (`"2.0"`); accepted leniently.
    #[serde(default)]
    pub jsonrpc: String,
    /// The request id. `None` (absent or JSON `null`) marks a notification — no response is sent.
    #[serde(default)]
    pub id: Option<Value>,
    /// The method name (e.g. `initialize`, `tools/list`, `tools/call`).
    pub method: String,
    /// The method parameters (object/array), if any.
    #[serde(default)]
    pub params: Option<Value>,
}

impl Request {
    /// Whether this request is a NOTIFICATION (no id) — it is handled for effect but never gets a
    /// response object (JSON-RPC §4.1). A `notifications/*` method is a notification by convention
    /// even were an id mistakenly present.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none() || self.method.starts_with("notifications/")
    }
}

/// A JSON-RPC error object (`{code, message, data?}`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorObject {
    /// The numeric error code (one of the `CODE_*` constants).
    pub code: i64,
    /// A short, secret-free description of the error.
    pub message: String,
    /// Optional structured data; omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Build an error object with no `data`.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// A `-32601 method not found` error naming the offending method.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(CODE_METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    /// A `-32602 invalid params` error.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(CODE_INVALID_PARAMS, message)
    }
}

/// A JSON-RPC response. Exactly one of `result` / `error` is present (JSON-RPC §5). `id` echoes
/// the request id (JSON `null` when the id could not be determined, e.g. a parse error).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The echoed request id (`null` when unknown).
    pub id: Value,
    /// The success result (present iff `error` is absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error (present iff `result` is absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// A success response echoing `id` and carrying `result`.
    #[must_use]
    pub fn result(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.unwrap_or(Value::Null),
            result: Some(result),
            error: None,
        }
    }

    /// An error response echoing `id` and carrying `error`.
    #[must_use]
    pub fn error(id: Option<Value>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.unwrap_or(Value::Null),
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_request_with_params() {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "describe", "arguments": { "path": "/mail/drafts" } }
        }))
        .unwrap();
        assert_eq!(req.method, "tools/call");
        assert!(!req.is_notification());
        assert_eq!(req.id, Some(json!(7)));
    }

    #[test]
    fn a_request_without_id_is_a_notification() {
        let req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .unwrap();
        assert!(req.is_notification());
    }

    #[test]
    fn success_response_omits_error_and_echoes_id() {
        let resp = Response::result(Some(json!("abc")), json!({"ok": true}));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["result"], json!({"ok": true}));
        assert!(v.get("error").is_none(), "no error field on success");
    }

    #[test]
    fn error_response_omits_result_and_defaults_id_to_null() {
        let resp = Response::error(None, ErrorObject::method_not_found("frobnicate"));
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["id"], Value::Null);
        assert_eq!(v["error"]["code"], CODE_METHOD_NOT_FOUND);
        assert!(v.get("result").is_none(), "no result field on error");
    }
}
