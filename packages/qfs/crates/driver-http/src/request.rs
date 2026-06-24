//! The **reusable REST request/response seam** (RFD-0001 §5/§9): owned, vendor-free DTOs that
//! describe one HTTP exchange — [`HttpMethod`], [`HttpRequest`], [`HttpResponse`] — plus the
//! header-redaction authority ([`SENSITIVE_HEADERS`] / [`is_sensitive_header`]).
//!
//! As of the t19 refinement these DTOs and the redacting `Debug` live in the shared **leaf**
//! [`qfs_http_core`] — the single source of truth depended on by BOTH this crate and
//! `qfs-google-auth`, so the redaction set cannot drift between the two HTTP seams (a header
//! added to [`qfs_http_core::SENSITIVE_HEADERS`] is inherited here for free). This module
//! **re-exports** them so the existing `qfs_driver_http::request::*` and
//! `qfs_driver_http::{HttpMethod, HttpRequest, HttpResponse, SENSITIVE_HEADERS}` paths are
//! unchanged; the concrete [`crate::client::HttpClient`] (the reqwest impl) stays local and trades
//! only in these DTOs.
//!
//! ## Secret discipline (RFD §10)
//! [`HttpRequest`] carries already-resolved header *values* (a token may sit in an
//! `Authorization` header by the time it is on the wire), so its `Debug` is **manual** and
//! **redacts** the value of every sensitive header (the one redaction authority lives in
//! [`qfs_http_core`]). A request is never logged with `{:?}` carrying a live token.

pub use qfs_http_core::{
    is_sensitive_header, HttpMethod, HttpRequest, HttpResponse, SENSITIVE_HEADERS,
};
