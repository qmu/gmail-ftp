//! **Table-valued / effectful-shaped** built-ins (RFD-0001 §3/§6/§10, ticket t08):
//! `READ(path)` and `http.get(url, …)`. These are the purity invariant's load-bearing
//! case: they *look* effectful but, in the stdlib, construct a **deferred** [`PlanNode`]
//! (a read source node) and perform **no** network/file I/O — execution is E2's runtime.
//! This keeps every plan dry-runnable.
//!
//! Both are **capability-gated** (RFD §10): with [`EvalCtx::capabilities_enabled`] off
//! (the pure-eval default), constructing the node is denied so an unattended context never
//! plans a read it is not authorised for. The deferred nodes are **read** sources (safe to
//! retry — idempotency/recovery, RFD §6).

use qfs_types::Value;

use super::{value_type_label, BuiltinFn, EvalCtx, FnError, FnSig};
use qfs_types::ColumnType;

/// The kind of deferred source a table-valued built-in constructs (a *description*, never
/// an executed read). E2's runtime materialises it; the stdlib only shapes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNodeKind {
    /// `READ(path)` — a deferred blob read of `path` (bytes/text, decoded via codecs
    /// downstream). Carries the resolved path string.
    Read {
        /// The path to read (a virtual path; no I/O here).
        path: String,
    },
    /// `http.get(url, …)` — a deferred one-row REST source `{status, headers, body}`
    /// (body decoded via codecs downstream). Carries the requested URL.
    HttpGet {
        /// The URL to GET (a description; no request is made here).
        url: String,
    },
}

/// A deferred plan/source node a table-valued built-in evaluates to (RFD §3 purity). It is
/// **data**: a description of a read to be performed later under `COMMIT`, never the read
/// itself. Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNode {
    /// What deferred source this node describes.
    pub kind: PlanNodeKind,
}

impl PlanNode {
    /// A deferred `READ(path)` source node.
    #[must_use]
    pub fn read(path: impl Into<String>) -> Self {
        Self {
            kind: PlanNodeKind::Read { path: path.into() },
        }
    }

    /// A deferred `http.get(url)` source node.
    #[must_use]
    pub fn http_get(url: impl Into<String>) -> Self {
        Self {
            kind: PlanNodeKind::HttpGet { url: url.into() },
        }
    }
}

/// The set of table-valued built-ins, in stable (name) order.
pub(super) fn table_valued_builtins() -> Vec<BuiltinFn> {
    vec![
        BuiltinFn::table_valued("READ", FnSig::fixed(1, ColumnType::Bytes), read),
        BuiltinFn::table_valued("http.get", FnSig::range(1, 2, ColumnType::Json), http_get),
    ]
}

/// `READ(path)` — construct a deferred blob-read source node. Capability-gated; performs
/// **no** file I/O. The result is a [`PlanNode`] the runtime (E2) materialises.
fn read(args: &[Value], ctx: &EvalCtx) -> Result<PlanNode, FnError> {
    let path = match args.first() {
        Some(Value::Text(s)) => s.clone(),
        Some(other) => {
            return Err(FnError::Type {
                name: "READ".to_string(),
                expected: "Text",
                found: value_type_label(other),
            })
        }
        None => {
            return Err(FnError::Arity {
                name: "READ".to_string(),
                expected: 1,
                found: 0,
            })
        }
    };
    if !ctx.capabilities_enabled {
        return Err(FnError::CapabilityDenied {
            builtin: "READ",
            requested: path,
        });
    }
    Ok(PlanNode::read(path))
}

/// `http.get(url, headers=>…)` — construct a deferred one-row REST source node.
/// Capability-gated; performs **no** network I/O. The `headers` keyword is shape-only here
/// (the request is E2's; the node carries the URL).
fn http_get(args: &[Value], ctx: &EvalCtx) -> Result<PlanNode, FnError> {
    let url = match args.first() {
        Some(Value::Text(s)) => s.clone(),
        Some(other) => {
            return Err(FnError::Type {
                name: "http.get".to_string(),
                expected: "Text",
                found: value_type_label(other),
            })
        }
        None => {
            return Err(FnError::Arity {
                name: "http.get".to_string(),
                expected: 1,
                found: 0,
            })
        }
    };
    if !ctx.capabilities_enabled {
        return Err(FnError::CapabilityDenied {
            builtin: "http.get",
            requested: url,
        });
    }
    Ok(PlanNode::http_get(url))
}
