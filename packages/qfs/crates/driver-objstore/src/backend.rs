//! [`ObjectBackend`] — the thin, **mockable** S3-compatible transport seam (RFD-0001 §9
//! no-heavy-SDK, boundary B3), plus the real [`HttpBackend`] (SigV4 over a local
//! [`HttpExchange`] seam on `qfs-http-core` — the `qfs-google-auth` / `qfs-driver-cf` precedent,
//! so this crate does NOT depend on `qfs-driver-http` and stays an independent runtime leaf), the
//! parked wasm [`R2BindingBackend`], and [`MockObjectBackend`] (in-memory fixtures for tests — no
//! live S3/R2, no network).
//!
//! The trait trades **only** in owned, vendor-free DTOs ([`ObjectMeta`]/[`ListPage`]/[`PutResult`]
//! and [`ByteStream`]); S3 XML, `http::*`, SigV4 internals, and `worker::*` env bindings never
//! cross it. The SigV4 signer ([`crate::sigv4`]) and any `http::`-ish type stay inside
//! [`HttpBackend`]; only the owned DTOs surface.
//!
//! ## Token discipline (RFD §10)
//! Credentials are [`SigV4Credentials`] resolved at construction; the secret key is exposed only
//! transiently inside the signer to compute the signature + the `Authorization` header value,
//! which the shared `qfs-http-core` redaction authority hides in every `Debug`/log. The secret is
//! never stored in a DTO, never in an [`ObjError`].

use std::sync::{Arc, Mutex};

use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};

use crate::bytestream::ByteStream;
use crate::dto::{ListPage, PutResult};
use crate::error::ObjError;
use crate::multipart::PartEtag;
use crate::sigv4::{self, SigV4Credentials, SigningContext, UNSIGNED_PAYLOAD};

/// A transport failure before an HTTP status was received — secret-free (built from the request
/// shape only). The error half of the local [`HttpExchange`] seam (mirrors the cf base's
/// `TransportError`); kept local so this crate does NOT depend on `qfs-driver-http`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport error for {method} {url}: {reason}")]
pub struct TransportError {
    /// The HTTP method (uppercase token).
    pub method: String,
    /// The request URL (secret-free).
    pub url: String,
    /// A secret-free reason (the transport's class, never a header value).
    pub reason: String,
}

/// The thin **synchronous** transport seam the [`HttpBackend`] sends owned signed [`HttpRequest`]s
/// over (the `qfs-google-auth` / `qfs-driver-cf` `HttpExchange` precedent). A non-2xx status is
/// **not** an error — it rides in the [`HttpResponse`] so the backend classifies it. The
/// production binary adapts an `Arc<dyn qfs_driver_http::HttpClient>` to this with a trivial DTO
/// copy; `reqwest` stays confined in `qfs-driver-http` and never crosses this boundary (RFD §9).
///
/// `Send + Sync` so an `Arc<dyn HttpExchange>` can be shared across the runtime bridge's blocking
/// apply threads.
pub trait HttpExchange: Send + Sync {
    /// Execute one request synchronously.
    ///
    /// # Errors
    /// [`TransportError`] if the wire exchange fails before a status is received.
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl HttpExchange for Arc<dyn HttpExchange> {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).exchange(req)
    }
}

/// An in-memory mock transport (tests / CI / wasm): records every request and answers from a FIFO
/// queue of scripted responses — so a test asserts the exact signed request shape the backend
/// built **without any socket**. Mirrors the cf `MockExchange`.
#[derive(Default)]
pub struct MockExchange {
    responses: Mutex<std::collections::VecDeque<Result<HttpResponse, TransportError>>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl MockExchange {
    /// An empty mock (every `exchange` after the queue drains returns a terminal transport error).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one scripted success response (consumed FIFO).
    #[must_use]
    pub fn with_response(self, resp: HttpResponse) -> Self {
        if let Ok(mut q) = self.responses.lock() {
            q.push_back(Ok(resp));
        }
        self
    }

    /// The requests this mock received, in order — what a test asserts against.
    #[must_use]
    pub fn recorded(&self) -> Vec<HttpRequest> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl HttpExchange for MockExchange {
    fn exchange(&self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(req.clone());
        }
        let next = self.responses.lock().ok().and_then(|mut q| q.pop_front());
        next.unwrap_or_else(|| {
            Err(TransportError {
                method: req.method.as_str().to_string(),
                url: req.url.clone(),
                reason: "mock exhausted: no scripted response".to_string(),
            })
        })
    }
}

/// The S3-compatible transport seam every backend implements (RFD §9). The driver's `ls`/`get`/
/// `put`/multipart/`delete`/`copy` are written once against this trait; the SigV4 [`HttpBackend`]
/// and the parked wasm [`R2BindingBackend`] are interchangeable impls producing **identical**
/// owned DTOs. No `http`/`worker`/SigV4 type crosses this boundary.
///
/// `Send + Sync` so a backend can be shared across the runtime bridge's blocking apply threads.
pub trait ObjectBackend: Send + Sync {
    /// List objects under `bucket`, optionally filtered by `prefix` and rolled up at `delimiter`,
    /// continuing from `continuation_token`. Returns one owned [`ListPage`].
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status, a decode failure, or a transport failure.
    fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
    ) -> Result<ListPage, ObjError>;

    /// Download an object as a bounded [`ByteStream`]. `range` is an optional inclusive
    /// `(start, end)` byte range (pushed down as a `Range:` header); `version_id` selects a
    /// specific version.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<(u64, u64)>,
        version_id: Option<&str>,
    ) -> Result<ByteStream, ObjError>;

    /// Upload an object in one PUT (the single-PUT path, below the multipart threshold).
    /// Returns the new ETag + (versioned bucket) version id.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn put_object(&self, bucket: &str, key: &str, body: &ByteStream)
        -> Result<PutResult, ObjError>;

    /// Begin a multipart upload; returns the assigned `upload_id`.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn create_multipart(&self, bucket: &str, key: &str) -> Result<String, ObjError>;

    /// Upload one part of a multipart upload; returns the part's ETag.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: &[u8],
    ) -> Result<String, ObjError>;

    /// Complete a multipart upload, replaying the ordered part receipts. Returns the final ETag +
    /// version id.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[PartEtag],
    ) -> Result<PutResult, ObjError>;

    /// Abort a multipart upload (frees orphan parts — the abort-on-error invariant). Idempotent.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<(), ObjError>;

    /// Delete an object (idempotent). `version_id` deletes a specific version (a permanent,
    /// irreversible delete); without it, a versioned bucket inserts a recoverable delete-marker.
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn delete_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ObjError>;

    /// Server-side copy `src_key` → `dst_key` within the same backend (the `cp`/`mv` first leg).
    /// Returns the new object's metadata (ETag for the verify step).
    ///
    /// # Errors
    /// [`ObjError`] on a non-2xx status or a transport failure.
    fn copy_object(
        &self,
        bucket: &str,
        src_key: &str,
        dst_key: &str,
    ) -> Result<PutResult, ObjError>;
}

/// The endpoint + region config the SigV4 [`HttpBackend`] routes with. R2 uses a
/// per-account endpoint and the `auto` region; S3 uses `https://<bucket>.s3.<region>.amazonaws.com`
/// or a configured endpoint. The caller injects the resolved base; the op builds the URL on top.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The base URL, e.g. `https://s3.us-east-1.amazonaws.com` or an R2 account endpoint. No
    /// trailing slash.
    pub base_url: String,
    /// The AWS region for the SigV4 credential scope (`us-east-1`, or `auto` for R2).
    pub region: String,
}

impl Endpoint {
    /// Construct an endpoint config.
    #[must_use]
    pub fn new(base_url: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            region: region.into(),
        }
    }
}

/// The real SigV4-signed S3-compatible backend: builds owned [`HttpRequest`]s, **signs** them with
/// the SigV4 signer (the [`SigV4Credentials`] secret exposed only inside the signer), and sends
/// them over the local [`HttpExchange`] seam. `reqwest` stays inside `qfs-driver-http`; no vendor
/// type crosses. The `amz_date`/`date_stamp` are injected (a runtime-free, testable clock seam).
pub struct HttpBackend {
    exchange: Arc<dyn HttpExchange>,
    endpoint: Endpoint,
    creds: SigV4Credentials,
    /// The fixed signing timestamp seam. In production the binary refreshes this per request from
    /// the wall clock; kept injectable so signing stays pure + the offline vectors reproduce.
    amz_date: String,
    date_stamp: String,
}

impl HttpBackend {
    /// Build a SigV4 backend over `exchange`, routing to `endpoint`, bearing `creds`. The
    /// `amz_date` (`YYYYMMDDTHHMMSSZ`) / `date_stamp` (`YYYYMMDD`) seam is injected.
    #[must_use]
    pub fn new(
        exchange: Arc<dyn HttpExchange>,
        endpoint: Endpoint,
        creds: SigV4Credentials,
        amz_date: impl Into<String>,
        date_stamp: impl Into<String>,
    ) -> Self {
        Self {
            exchange,
            endpoint,
            creds,
            amz_date: amz_date.into(),
            date_stamp: date_stamp.into(),
        }
    }

    /// Build + SigV4-sign a request. The body hash is [`UNSIGNED_PAYLOAD`] (the streaming mode);
    /// the secret is exposed only inside the signer, and the resulting `Authorization` header is
    /// redacted in every `Debug`/log.
    fn signed(&self, method: HttpMethod, url: String, body: Option<Vec<u8>>) -> HttpRequest {
        let mut req = HttpRequest::new(method, url);
        if let Some(b) = body {
            req = req.with_body(b);
        }
        let ctx = SigningContext {
            region: &self.endpoint.region,
            service: "s3",
            amz_date: &self.amz_date,
            date_stamp: &self.date_stamp,
        };
        sigv4::sign(req, &self.creds, &ctx, UNSIGNED_PAYLOAD)
    }

    /// The object URL `<base>/<bucket>/<key>` (the path-style addressing the mock + R2 use).
    fn object_url(&self, bucket: &str, key: &str) -> String {
        format!("{}/{bucket}/{key}", self.endpoint.base_url)
    }

    /// Send a signed request, mapping a transport failure to a secret-free [`ObjError::Transport`]
    /// and a non-2xx status to [`ObjError::Api`] under `op`.
    fn send(&self, op: &'static str, req: &HttpRequest) -> Result<HttpResponse, ObjError> {
        let resp = self
            .exchange
            .exchange(req)
            .map_err(|_| ObjError::Transport {
                reason: "object-storage request could not be completed".to_string(),
            })?;
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(ObjError::Api {
                op,
                status: resp.status,
            })
        }
    }

    /// Read the `ETag` + `x-amz-version-id` response headers into a [`PutResult`].
    fn put_result_from(resp: &HttpResponse) -> PutResult {
        let etag = resp.header_value("ETag").unwrap_or_default().to_string();
        let mut result = PutResult::new(etag);
        if let Some(vid) = resp.header_value("x-amz-version-id") {
            result.version_id = Some(vid.to_string());
        }
        result
    }
}

impl ObjectBackend for HttpBackend {
    fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
    ) -> Result<ListPage, ObjError> {
        let op = "list_objects_v2";
        // The prefix + delimiter are pushed down as native S3 query params (the truthful-pushdown
        // first leg; a residual the engine re-filters is the driver's concern, not the backend's).
        let mut query: Vec<String> = vec!["list-type=2".to_string()];
        if let Some(p) = prefix {
            query.push(format!("prefix={p}"));
        }
        if let Some(d) = delimiter {
            query.push(format!("delimiter={d}"));
        }
        if let Some(t) = continuation_token {
            query.push(format!("continuation-token={t}"));
        }
        let url = format!("{}/{bucket}?{}", self.endpoint.base_url, query.join("&"));
        let req = self.signed(HttpMethod::Get, url, None);
        let resp = self.send(op, &req)?;
        crate::xml::parse_list_objects(&resp.body).map_err(|reason| ObjError::Decode { op, reason })
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<(u64, u64)>,
        version_id: Option<&str>,
    ) -> Result<ByteStream, ObjError> {
        let op = "get_object";
        let mut url = self.object_url(bucket, key);
        if let Some(vid) = version_id {
            url.push_str(&format!("?versionId={vid}"));
        }
        let mut req = self.signed(HttpMethod::Get, url, None);
        // A byte-range pushdown rides in the Range: header (the GET pushdown).
        if let Some((start, end)) = range {
            req = req.header("Range", format!("bytes={start}-{end}"));
        }
        let resp = self.send(op, &req)?;
        // The body bytes are framed into a bounded ByteStream (streaming, not one buffer).
        Ok(ByteStream::from_bytes(resp.body))
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: &ByteStream,
    ) -> Result<PutResult, ObjError> {
        let op = "put_object";
        let req = self.signed(
            HttpMethod::Put,
            self.object_url(bucket, key),
            Some(body.clone().into_bytes()),
        );
        let resp = self.send(op, &req)?;
        Ok(Self::put_result_from(&resp))
    }

    fn create_multipart(&self, bucket: &str, key: &str) -> Result<String, ObjError> {
        let op = "create_multipart";
        let url = format!("{}?uploads", self.object_url(bucket, key));
        let req = self.signed(HttpMethod::Post, url, None);
        let resp = self.send(op, &req)?;
        crate::xml::parse_upload_id(&resp.body).map_err(|reason| ObjError::Decode { op, reason })
    }

    fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: &[u8],
    ) -> Result<String, ObjError> {
        let op = "upload_part";
        let url = format!(
            "{}?partNumber={part_number}&uploadId={upload_id}",
            self.object_url(bucket, key)
        );
        let req = self.signed(HttpMethod::Put, url, Some(body.to_vec()));
        let resp = self.send(op, &req)?;
        Ok(resp.header_value("ETag").unwrap_or_default().to_string())
    }

    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[PartEtag],
    ) -> Result<PutResult, ObjError> {
        let op = "complete_multipart";
        let url = format!("{}?uploadId={upload_id}", self.object_url(bucket, key));
        let body = crate::xml::render_complete_multipart(parts);
        let req = self.signed(HttpMethod::Post, url, Some(body.into_bytes()));
        let resp = self.send(op, &req)?;
        Ok(Self::put_result_from(&resp))
    }

    fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<(), ObjError> {
        let op = "abort_multipart";
        let url = format!("{}?uploadId={upload_id}", self.object_url(bucket, key));
        let req = self.signed(HttpMethod::Delete, url, None);
        let resp = self
            .exchange
            .exchange(&req)
            .map_err(|_| ObjError::Transport {
                reason: "object-storage abort could not be completed".to_string(),
            })?;
        // A 204/200 or a 404 (already gone) is success (idempotent).
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(ObjError::Api {
                op,
                status: resp.status,
            })
        }
    }

    fn delete_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ObjError> {
        let op = "delete_object";
        let mut url = self.object_url(bucket, key);
        if let Some(vid) = version_id {
            url.push_str(&format!("?versionId={vid}"));
        }
        let req = self.signed(HttpMethod::Delete, url, None);
        let resp = self
            .exchange
            .exchange(&req)
            .map_err(|_| ObjError::Transport {
                reason: "object-storage delete could not be completed".to_string(),
            })?;
        // A delete of an absent key is success (idempotent).
        if resp.is_success() || resp.status == 404 {
            Ok(())
        } else {
            Err(ObjError::Api {
                op,
                status: resp.status,
            })
        }
    }

    fn copy_object(
        &self,
        bucket: &str,
        src_key: &str,
        dst_key: &str,
    ) -> Result<PutResult, ObjError> {
        let op = "copy_object";
        let req = self
            .signed(HttpMethod::Put, self.object_url(bucket, dst_key), None)
            .header("x-amz-copy-source", format!("/{bucket}/{src_key}"));
        let resp = self.send(op, &req)?;
        Ok(Self::put_result_from(&resp))
    }
}

// --------------------------------------------------------------------------------------------
// Parked wasm R2 binding backend (RFD §8 deployment mapping: /r2 → native binding, not HTTP)
// --------------------------------------------------------------------------------------------

/// The native Cloudflare R2 Workers-binding backend (RFD §8): under `wasm32`, an `/r2` mount can
/// be served by the platform `worker::Bucket` binding instead of signed HTTP — get/put/list/delete
/// /multipart map onto the binding's methods, producing the **identical** owned DTOs.
///
/// **Named park** (per the ticket): there is no live wasm Workers CI lane yet, and the `worker`
/// crate is not vendored, so the binding handle is held as an opaque marker and its methods are
/// `unimplemented`-free stubs that return a structured "binding unavailable" error. This is gated
/// `#[cfg(target_arch = "wasm32")]` so the **native build never links it** (the ticket's hard
/// requirement); the DTOs + the [`ObjectBackend`] seam are wasm-clean, so the real binding impl
/// drops in here later behind the same trait, native `Backend::Http` is reused for R2 until then.
#[cfg(target_arch = "wasm32")]
pub struct R2BindingBackend {
    /// The bound R2 namespace name (the `worker::Bucket` binding key the Worker env supplies). The
    /// real `worker::Bucket` handle replaces this marker when the wasm Workers lane lands.
    binding_name: String,
}

#[cfg(target_arch = "wasm32")]
impl R2BindingBackend {
    /// Construct the parked binding backend over the named R2 binding.
    #[must_use]
    pub fn new(binding_name: impl Into<String>) -> Self {
        Self {
            binding_name: binding_name.into(),
        }
    }

    /// The structured "binding not yet wired" error every parked method returns — secret-free, and
    /// it names the binding so a Worker operator sees which binding is unconfigured.
    fn parked(&self, op: &'static str) -> ObjError {
        let _ = &self.binding_name;
        ObjError::Api { op, status: 501 }
    }
}

#[cfg(target_arch = "wasm32")]
impl ObjectBackend for R2BindingBackend {
    fn list_objects_v2(
        &self,
        _bucket: &str,
        _prefix: Option<&str>,
        _delimiter: Option<&str>,
        _continuation_token: Option<&str>,
    ) -> Result<ListPage, ObjError> {
        // Real impl: self.bucket.list().prefix(..).delimiter(..).cursor(..).execute() → ListPage.
        Err(self.parked("list_objects_v2"))
    }

    fn get_object(
        &self,
        _bucket: &str,
        _key: &str,
        _range: Option<(u64, u64)>,
        _version_id: Option<&str>,
    ) -> Result<ByteStream, ObjError> {
        // Real impl: self.bucket.get(key).range(..).execute() → stream the body into a ByteStream.
        Err(self.parked("get_object"))
    }

    fn put_object(
        &self,
        _bucket: &str,
        _key: &str,
        _body: &ByteStream,
    ) -> Result<PutResult, ObjError> {
        // Real impl: self.bucket.put(key, body).execute() → PutResult { etag, version_id }.
        Err(self.parked("put_object"))
    }

    fn create_multipart(&self, _bucket: &str, _key: &str) -> Result<String, ObjError> {
        Err(self.parked("create_multipart"))
    }

    fn upload_part(
        &self,
        _bucket: &str,
        _key: &str,
        _upload_id: &str,
        _part_number: u32,
        _body: &[u8],
    ) -> Result<String, ObjError> {
        Err(self.parked("upload_part"))
    }

    fn complete_multipart(
        &self,
        _bucket: &str,
        _key: &str,
        _upload_id: &str,
        _parts: &[PartEtag],
    ) -> Result<PutResult, ObjError> {
        Err(self.parked("complete_multipart"))
    }

    fn abort_multipart(&self, _bucket: &str, _key: &str, _upload_id: &str) -> Result<(), ObjError> {
        Err(self.parked("abort_multipart"))
    }

    fn delete_object(
        &self,
        _bucket: &str,
        _key: &str,
        _version_id: Option<&str>,
    ) -> Result<(), ObjError> {
        // Real impl: self.bucket.delete(key).execute().
        Err(self.parked("delete_object"))
    }

    fn copy_object(
        &self,
        _bucket: &str,
        _src_key: &str,
        _dst_key: &str,
    ) -> Result<PutResult, ObjError> {
        Err(self.parked("copy_object"))
    }
}

// --------------------------------------------------------------------------------------------
// In-memory mock backend (tests / CI)
// --------------------------------------------------------------------------------------------

/// One recorded object-storage backend call (the op + its salient owned arguments) — what a test
/// asserts the driver issued. Secret-free by construction (no credential ever enters this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// `list_objects_v2` (carries the pushed-down prefix/delimiter/continuation).
    List {
        /// The bucket.
        bucket: String,
        /// The pushed-down key prefix.
        prefix: Option<String>,
        /// The pushed-down delimiter.
        delimiter: Option<String>,
        /// The continuation token.
        continuation_token: Option<String>,
    },
    /// `get_object` (carries the pushed-down byte range + the addressed version).
    Get {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The pushed-down inclusive byte range.
        range: Option<(u64, u64)>,
        /// The addressed version id.
        version_id: Option<String>,
    },
    /// `put_object` (single-PUT path).
    Put {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The uploaded byte length (metadata; the bytes are not retained beyond the fixture).
        len: usize,
    },
    /// `create_multipart`.
    CreateMultipart {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
    },
    /// `upload_part`.
    UploadPart {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The upload id.
        upload_id: String,
        /// The 1-based part number.
        part_number: u32,
        /// The part byte length.
        len: usize,
    },
    /// `complete_multipart`.
    CompleteMultipart {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The upload id.
        upload_id: String,
        /// The ordered part receipts.
        parts: Vec<PartEtag>,
    },
    /// `abort_multipart` (the orphan-part cleanup the abort-on-error invariant triggers).
    AbortMultipart {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The upload id.
        upload_id: String,
    },
    /// `delete_object` (carries the addressed version).
    Delete {
        /// The bucket.
        bucket: String,
        /// The key.
        key: String,
        /// The addressed version id.
        version_id: Option<String>,
    },
    /// `copy_object`.
    Copy {
        /// The bucket.
        bucket: String,
        /// The source key.
        src_key: String,
        /// The destination key.
        dst_key: String,
    },
}

/// An in-memory mock S3-compatible backend (tests / CI): answers from pre-seeded fixtures and
/// **records** every call so a test asserts the exact API surface the driver exercised — with
/// **no socket and no credentials**. An optional `fail_part_at` injects a mid-multipart failure to
/// prove the abort-on-error path.
#[derive(Default)]
pub struct MockObjectBackend {
    list_pages: Mutex<Vec<ListPage>>,
    get_body: Mutex<Vec<u8>>,
    put_result: Mutex<PutResult>,
    upload_id: Mutex<String>,
    /// If `Some(n)`, `upload_part` fails when `part_number == n` (the abort-on-error trigger).
    fail_part_at: Mutex<Option<u32>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

impl MockObjectBackend {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        let m = Self::default();
        if let Ok(mut id) = m.upload_id.lock() {
            *id = "mock-upload-id".to_string();
        }
        if let Ok(mut pr) = m.put_result.lock() {
            *pr = PutResult::new("\"mock-etag\"");
        }
        m
    }

    /// Seed a listing page `list_objects_v2` returns (FIFO across pages).
    #[must_use]
    pub fn with_list_page(self, page: ListPage) -> Self {
        if let Ok(mut p) = self.list_pages.lock() {
            p.push(page);
        }
        self
    }

    /// Seed the body bytes `get_object` returns.
    #[must_use]
    pub fn with_get_body(self, body: Vec<u8>) -> Self {
        if let Ok(mut b) = self.get_body.lock() {
            *b = body;
        }
        self
    }

    /// Seed the [`PutResult`] a put/complete reports (e.g. with a version id).
    #[must_use]
    pub fn with_put_result(self, result: PutResult) -> Self {
        if let Ok(mut pr) = self.put_result.lock() {
            *pr = result;
        }
        self
    }

    /// Inject a mid-multipart failure at part `n` (the abort-on-error trigger).
    #[must_use]
    pub fn failing_part_at(self, n: u32) -> Self {
        if let Ok(mut f) = self.fail_part_at.lock() {
            *f = Some(n);
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

impl ObjectBackend for MockObjectBackend {
    fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        continuation_token: Option<&str>,
    ) -> Result<ListPage, ObjError> {
        self.record(RecordedCall::List {
            bucket: bucket.to_string(),
            prefix: prefix.map(str::to_string),
            delimiter: delimiter.map(str::to_string),
            continuation_token: continuation_token.map(str::to_string),
        });
        let page = self
            .list_pages
            .lock()
            .ok()
            .and_then(|mut p| {
                if p.is_empty() {
                    None
                } else {
                    Some(p.remove(0))
                }
            })
            .unwrap_or_default();
        Ok(page)
    }

    fn get_object(
        &self,
        bucket: &str,
        key: &str,
        range: Option<(u64, u64)>,
        version_id: Option<&str>,
    ) -> Result<ByteStream, ObjError> {
        self.record(RecordedCall::Get {
            bucket: bucket.to_string(),
            key: key.to_string(),
            range,
            version_id: version_id.map(str::to_string),
        });
        let body = self.get_body.lock().map(|b| b.clone()).unwrap_or_default();
        // Honour the range pushdown over the seeded body (inclusive).
        let bytes = match range {
            Some((start, end)) => {
                let s = usize::try_from(start).unwrap_or(0).min(body.len());
                let e = usize::try_from(end)
                    .unwrap_or(0)
                    .saturating_add(1)
                    .min(body.len());
                body.get(s..e).map(<[u8]>::to_vec).unwrap_or_default()
            }
            None => body,
        };
        Ok(ByteStream::from_bytes(bytes))
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: &ByteStream,
    ) -> Result<PutResult, ObjError> {
        self.record(RecordedCall::Put {
            bucket: bucket.to_string(),
            key: key.to_string(),
            len: body.len(),
        });
        Ok(self
            .put_result
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default())
    }

    fn create_multipart(&self, bucket: &str, key: &str) -> Result<String, ObjError> {
        self.record(RecordedCall::CreateMultipart {
            bucket: bucket.to_string(),
            key: key.to_string(),
        });
        Ok(self.upload_id.lock().map(|i| i.clone()).unwrap_or_default())
    }

    fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: &[u8],
    ) -> Result<String, ObjError> {
        self.record(RecordedCall::UploadPart {
            bucket: bucket.to_string(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
            part_number,
            len: body.len(),
        });
        // Injected mid-multipart failure (the abort-on-error trigger).
        let should_fail = self.fail_part_at.lock().ok().and_then(|f| *f) == Some(part_number);
        if should_fail {
            return Err(ObjError::Api {
                op: "upload_part",
                status: 500,
            });
        }
        Ok(format!("\"part-etag-{part_number}\""))
    }

    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[PartEtag],
    ) -> Result<PutResult, ObjError> {
        self.record(RecordedCall::CompleteMultipart {
            bucket: bucket.to_string(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
            parts: parts.to_vec(),
        });
        Ok(self
            .put_result
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default())
    }

    fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<(), ObjError> {
        self.record(RecordedCall::AbortMultipart {
            bucket: bucket.to_string(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
        });
        Ok(())
    }

    fn delete_object(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ObjError> {
        self.record(RecordedCall::Delete {
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: version_id.map(str::to_string),
        });
        Ok(())
    }

    fn copy_object(
        &self,
        bucket: &str,
        src_key: &str,
        dst_key: &str,
    ) -> Result<PutResult, ObjError> {
        self.record(RecordedCall::Copy {
            bucket: bucket.to_string(),
            src_key: src_key.to_string(),
            dst_key: dst_key.to_string(),
        });
        Ok(self
            .put_result
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default())
    }
}
