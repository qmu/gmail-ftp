//! [`GDriveClient`] — the thin, **mockable** Drive API seam (RFD-0001 §9 no-heavy-SDK,
//! boundary B3), plus [`GoogleApiDriveClient`] (the real client over the t19
//! [`GoogleApiClient`]) and [`MockDriveClient`] (an in-memory fake for tests — no live Drive,
//! no network).
//!
//! The trait trades **only** in owned, vendor-free DTOs ([`FileMeta`] etc.); Drive JSON never
//! crosses it. The real impl builds an [`HttpRequest`] (no `Authorization` header — the
//! [`GoogleApiClient`] injects the bearer and refreshes on a 401), sends it, and translates the
//! response JSON into the owned DTOs. The token discipline is wholly inherited from t19: the
//! bearer lives behind a [`cfs_secrets::Secret`], is written only into a header the redacting
//! `HttpRequest` `Debug` hides, and is **never** logged or surfaced in a [`DriveError`].

use std::sync::{Arc, Mutex};

use cfs_google_auth::GoogleApiClient;
use cfs_http_core::{HttpMethod, HttpRequest, HttpResponse};

use crate::error::DriveError;
use crate::schema::{FileMeta, SharedDrive};

/// The Drive v3 API base URL. Every op is a path under this.
const API_BASE: &str = "https://www.googleapis.com/drive/v3";
/// The resumable/simple upload base URL.
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
/// The `fields` projection requested for every file metadata fetch — the columns the owned
/// [`FileMeta`] needs, and nothing more (least over-fetch).
const FILE_FIELDS: &str =
    "id,name,mimeType,parents,size,modifiedTime,md5Checksum,headRevisionId,driveId,trashed";

/// One page of a `files.list` result (the owned files + the next-page token).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilePage {
    /// The files on this page.
    pub files: Vec<FileMeta>,
    /// The next-page token, if there are further pages.
    pub next_page_token: Option<String>,
}

/// The thin Drive API seam. A driver issues every Drive call through this; the real impl rides
/// the t19 [`GoogleApiClient`] (bearer + refresh-on-401), the test impl answers from in-memory
/// fixtures. `Send + Sync` so an `Arc<dyn GDriveClient>` can be shared across the runtime's
/// blocking apply threads.
pub trait GDriveClient: Send + Sync {
    /// List files matching `query` (the Drive `q`), within an optional Shared Drive
    /// (`drive_id` ⇒ `supportsAllDrives` + `corpora=drive` + `driveId`), capped at `page_size`.
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn list_files(
        &self,
        query: &str,
        drive_id: Option<&str>,
        page_size: Option<u32>,
    ) -> Result<FilePage, DriveError>;

    /// Fetch one file's metadata → the owned [`FileMeta`] DTO.
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn get_file(&self, id: &str) -> Result<FileMeta, DriveError>;

    /// List the Shared Drives the account can see (the `/drive/shared` listing).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn list_drives(&self) -> Result<Vec<SharedDrive>, DriveError>;

    /// Download a binary file's raw bytes (`files.get?alt=media`).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn download(&self, id: &str, revision: Option<&str>) -> Result<Vec<u8>, DriveError>;

    /// Export a Google-native doc to `export_mime` bytes (`files.export`).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn export(&self, id: &str, export_mime: &str) -> Result<Vec<u8>, DriveError>;

    /// Create a new file under `parent` with `name`/`mime`/`bytes`; returns the new file id.
    /// Uploads are resumable in production; the seam exposes the create-or-update intent only.
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn upload(
        &self,
        parent: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String, DriveError>;

    /// Update (replace) an existing file's bytes by id (the retry-safe `UPSERT` path).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn update_content(&self, id: &str, mime: &str, bytes: &[u8]) -> Result<(), DriveError>;

    /// Rename and/or re-parent a file (the `UPDATE` / `mv` apply): set `new_name` and move from
    /// `remove_parents` to `add_parents`. Empty vectors / `None` leave the field unchanged.
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn modify_file(
        &self,
        id: &str,
        new_name: Option<&str>,
        add_parents: &[String],
        remove_parents: &[String],
    ) -> Result<(), DriveError>;

    /// Server-side copy a file to `parent` with `name` (the same-drive `cp` fast path); returns
    /// the new file id.
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn copy_file(&self, id: &str, parent: &str, name: &str) -> Result<String, DriveError>;

    /// Trash a file by id (the default `REMOVE` — **not** permanent delete).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn trash(&self, id: &str) -> Result<(), DriveError>;

    /// Permanently delete a file by id (the irreversible hard-delete, flagged explicitly).
    ///
    /// # Errors
    /// [`DriveError`] on a non-2xx status or an auth/transport failure.
    fn delete(&self, id: &str) -> Result<(), DriveError>;
}

/// The real Drive client: builds owned [`HttpRequest`]s and sends them through the t19
/// [`GoogleApiClient`], which injects the per-account bearer and refreshes on a 401. The account
/// selection is wholly upstream (the `GoogleApiClient` is constructed per account from a
/// [`cfs_google_auth::TokenSource`]); this client is account-agnostic.
pub struct GoogleApiDriveClient {
    api: Arc<GoogleApiClient>,
}

impl GoogleApiDriveClient {
    /// Build a Drive client over an authenticated [`GoogleApiClient`] (one per account).
    #[must_use]
    pub fn new(api: Arc<GoogleApiClient>) -> Self {
        Self { api }
    }

    /// Send a request through the auth client, mapping its `AuthError` to a secret-free
    /// [`DriveError`] and classifying a non-2xx status under `op`.
    fn send(&self, op: &'static str, req: &HttpRequest) -> Result<HttpResponse, DriveError> {
        let resp = self.api.send(req).map_err(DriveError::from)?;
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(DriveError::Api {
                op,
                status: resp.status,
            })
        }
    }

    fn parse_json(op: &'static str, resp: &HttpResponse) -> Result<serde_json::Value, DriveError> {
        serde_json::from_slice(&resp.body).map_err(|_| DriveError::Decode {
            op,
            reason: "response body was not valid JSON".to_string(),
        })
    }

    /// A JSON-body request (POST/PATCH) to a Drive API path.
    fn json_req(
        method: HttpMethod,
        op: &'static str,
        url: String,
        body: &serde_json::Value,
    ) -> Result<HttpRequest, DriveError> {
        let bytes = serde_json::to_vec(body).map_err(|_| DriveError::Decode {
            op,
            reason: "could not encode the request body".to_string(),
        })?;
        Ok(HttpRequest::new(method, url)
            .header("Content-Type", "application/json")
            .with_body(bytes))
    }
}

impl GDriveClient for GoogleApiDriveClient {
    fn list_files(
        &self,
        query: &str,
        drive_id: Option<&str>,
        page_size: Option<u32>,
    ) -> Result<FilePage, DriveError> {
        let op = "files.list";
        let mut params: Vec<String> = vec![
            "supportsAllDrives=true".to_string(),
            "includeItemsFromAllDrives=true".to_string(),
            format!(
                "fields={}",
                encode(&format!("files({FILE_FIELDS}),nextPageToken"))
            ),
        ];
        if !query.is_empty() {
            params.push(format!("q={}", encode(query)));
        }
        if let Some(d) = drive_id {
            params.push("corpora=drive".to_string());
            params.push(format!("driveId={}", encode(d)));
        }
        if let Some(n) = page_size {
            params.push(format!("pageSize={n}"));
        }
        let url = format!("{API_BASE}/files?{}", params.join("&"));
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        let json = Self::parse_json(op, &resp)?;
        let files = json
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(decode_file).collect())
            .unwrap_or_default();
        let next_page_token = json
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(FilePage {
            files,
            next_page_token,
        })
    }

    fn get_file(&self, id: &str) -> Result<FileMeta, DriveError> {
        let op = "files.get";
        let url = format!(
            "{API_BASE}/files/{id}?supportsAllDrives=true&fields={}",
            encode(FILE_FIELDS)
        );
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        let json = Self::parse_json(op, &resp)?;
        decode_file(&json).ok_or(DriveError::Decode {
            op,
            reason: "file JSON missing required fields".to_string(),
        })
    }

    fn list_drives(&self) -> Result<Vec<SharedDrive>, DriveError> {
        let op = "drives.list";
        let url = format!("{API_BASE}/drives?fields={}", encode("drives(id,name)"));
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        let json = Self::parse_json(op, &resp)?;
        Ok(json
            .get("drives")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        let id = d.get("id").and_then(|v| v.as_str())?.to_string();
                        let name = d.get("name").and_then(|v| v.as_str())?.to_string();
                        Some(SharedDrive { id, name })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn download(&self, id: &str, _revision: Option<&str>) -> Result<Vec<u8>, DriveError> {
        let op = "files.get.media";
        let url = format!("{API_BASE}/files/{id}?alt=media&supportsAllDrives=true");
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        Ok(resp.body)
    }

    fn export(&self, id: &str, export_mime: &str) -> Result<Vec<u8>, DriveError> {
        let op = "files.export";
        let url = format!(
            "{API_BASE}/files/{id}/export?mimeType={}",
            encode(export_mime)
        );
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        Ok(resp.body)
    }

    fn upload(
        &self,
        parent: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String, DriveError> {
        let op = "files.create";
        // A multipart upload: metadata part + media part. Build the related body deterministically.
        let metadata = serde_json::json!({ "name": name, "parents": [parent], "mimeType": mime });
        let body = multipart_related(&metadata, mime, bytes, op)?;
        let url = format!("{UPLOAD_BASE}/files?uploadType=multipart&supportsAllDrives=true");
        let req = HttpRequest::new(HttpMethod::Post, url)
            .header(
                "Content-Type",
                format!("multipart/related; boundary={MULTIPART_BOUNDARY}"),
            )
            .with_body(body);
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        json.get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or(DriveError::Decode {
                op,
                reason: "files.create response missing file id".to_string(),
            })
    }

    fn update_content(&self, id: &str, mime: &str, bytes: &[u8]) -> Result<(), DriveError> {
        let op = "files.update";
        let url = format!("{UPLOAD_BASE}/files/{id}?uploadType=media&supportsAllDrives=true");
        // PATCH-by-media is the idempotent (retry-safe) content replace by id.
        let req = HttpRequest::new(HttpMethod::Put, url)
            .header("Content-Type", mime)
            .with_body(bytes.to_vec());
        self.send(op, &req)?;
        Ok(())
    }

    fn modify_file(
        &self,
        id: &str,
        new_name: Option<&str>,
        add_parents: &[String],
        remove_parents: &[String],
    ) -> Result<(), DriveError> {
        let op = "files.update.meta";
        let mut params: Vec<String> = vec!["supportsAllDrives=true".to_string()];
        if !add_parents.is_empty() {
            params.push(format!("addParents={}", encode(&add_parents.join(","))));
        }
        if !remove_parents.is_empty() {
            params.push(format!(
                "removeParents={}",
                encode(&remove_parents.join(","))
            ));
        }
        let mut meta = serde_json::Map::new();
        if let Some(n) = new_name {
            meta.insert("name".to_string(), serde_json::Value::String(n.to_string()));
        }
        let url = format!("{API_BASE}/files/{id}?{}", params.join("&"));
        // A metadata rename/re-parent is a partial field update — Drive `files.update` is PATCH
        // semantics (shared `HttpMethod::Patch`, reconciled with the GitHub t24 driver).
        let req = Self::json_req(HttpMethod::Patch, op, url, &serde_json::Value::Object(meta))?;
        self.send(op, &req)?;
        Ok(())
    }

    fn copy_file(&self, id: &str, parent: &str, name: &str) -> Result<String, DriveError> {
        let op = "files.copy";
        let body = serde_json::json!({ "name": name, "parents": [parent] });
        let url = format!("{API_BASE}/files/{id}/copy?supportsAllDrives=true");
        let req = Self::json_req(HttpMethod::Post, op, url, &body)?;
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        json.get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or(DriveError::Decode {
                op,
                reason: "files.copy response missing file id".to_string(),
            })
    }

    fn trash(&self, id: &str) -> Result<(), DriveError> {
        let op = "files.trash";
        let body = serde_json::json!({ "trashed": true });
        let url = format!("{API_BASE}/files/{id}?supportsAllDrives=true");
        // Setting `trashed=true` is a partial field update — PATCH (shared `HttpMethod::Patch`).
        let req = Self::json_req(HttpMethod::Patch, op, url, &body)?;
        self.send(op, &req)?;
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<(), DriveError> {
        let op = "files.delete";
        let url = format!("{API_BASE}/files/{id}?supportsAllDrives=true");
        self.send(op, &HttpRequest::new(HttpMethod::Delete, url))?;
        Ok(())
    }
}

/// The deterministic multipart boundary for an upload (no randomness — byte-stable requests).
const MULTIPART_BOUNDARY: &str = "cfs-gdrive-boundary";

/// Build a `multipart/related` body (metadata JSON part + media part) for a file create.
fn multipart_related(
    metadata: &serde_json::Value,
    mime: &str,
    bytes: &[u8],
    op: &'static str,
) -> Result<Vec<u8>, DriveError> {
    let meta_bytes = serde_json::to_vec(metadata).map_err(|_| DriveError::Decode {
        op,
        reason: "could not encode the upload metadata".to_string(),
    })?;
    let mut out = Vec::new();
    let dashes = format!("--{MULTIPART_BOUNDARY}\r\n");
    out.extend_from_slice(dashes.as_bytes());
    out.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    out.extend_from_slice(&meta_bytes);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(dashes.as_bytes());
    out.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(format!("\r\n--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    Ok(out)
}

/// Translate one Drive `files` JSON object into the owned [`FileMeta`]. Returns `None` if the
/// required `id`/`name`/`mimeType` are absent.
fn decode_file(json: &serde_json::Value) -> Option<FileMeta> {
    let id = json.get("id")?.as_str()?.to_string();
    let name = json.get("name")?.as_str()?.to_string();
    let mime_type = json.get("mimeType")?.as_str()?.to_string();
    let parents = json
        .get("parents")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // Drive returns `size` as a stringified int; parse leniently.
    let size = json
        .get("size")
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| v.as_i64())
        })
        .unwrap_or(0);
    let modified_time = json
        .get("modifiedTime")
        .and_then(|v| v.as_str())
        .map(parse_rfc3339_to_ms)
        .unwrap_or(0);
    let md5 = json
        .get("md5Checksum")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let rev = json
        .get("headRevisionId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let drive_id = json
        .get("driveId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let trashed = json
        .get("trashed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(FileMeta {
        id,
        name,
        mime_type,
        parents,
        size,
        modified_time,
        md5,
        rev,
        drive_id,
        trashed,
    })
}

/// Parse an RFC-3339 UTC timestamp (`YYYY-MM-DDThh:mm:ss...Z`) into epoch milliseconds. Tolerant:
/// returns 0 on any shape it does not recognize (a metadata convenience, never load-bearing).
fn parse_rfc3339_to_ms(s: &str) -> i64 {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return 0;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b).and_then(|p| p.parse().ok()) };
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (
        num(0, 4),
        num(5, 7),
        num(8, 10),
        num(11, 13),
        num(14, 16),
        num(17, 19),
    ) else {
        return 0;
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    (days * 86_400 + h * 3600 + mi * 60 + se) * 1000
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm). Pure integer math.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Minimal percent-encoding for a query parameter value. Dependency-free; encodes everything
/// outside the unreserved set so a Drive `q` (with spaces, `'`, `=`) rides safely in the URL.
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

/// An in-memory mock Drive client (tests / CI / wasm): answers from pre-seeded fixtures and
/// **records** every call so a test asserts the exact API surface the driver exercised — with
/// **no socket and no credentials**. The recorded calls also prove `PREVIEW` performs zero I/O
/// (the mock asserts it was never called) and that a write goes to the expected op.
#[derive(Default)]
pub struct MockDriveClient {
    files: Vec<FileMeta>,
    drives: Vec<SharedDrive>,
    list_pages: Mutex<Vec<FilePage>>,
    downloads: Mutex<Vec<(String, Vec<u8>)>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

/// One recorded Drive API call (the op + its salient owned arguments) — what a test asserts the
/// driver issued. Secret-free by construction (no token ever enters this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// `files.list` with the pushed `q` query, the optional Shared Drive id, and the cap.
    ListFiles {
        /// The Drive `q` search the driver pushed down.
        query: String,
        /// The Shared Drive id, if scoped.
        drive_id: Option<String>,
        /// The `pageSize` cap, if any.
        page_size: Option<u32>,
    },
    /// `files.get` for one id.
    GetFile {
        /// The file id fetched.
        id: String,
    },
    /// `drives.list`.
    ListDrives,
    /// `files.get?alt=media` (a raw download).
    Download {
        /// The downloaded file id.
        id: String,
        /// The pinned revision, if any.
        revision: Option<String>,
    },
    /// `files.export` (a Google-native doc export).
    Export {
        /// The exported file id.
        id: String,
        /// The export MIME type.
        export_mime: String,
    },
    /// `files.create` (a new-file upload).
    Upload {
        /// The destination parent folder id.
        parent: String,
        /// The new file name.
        name: String,
        /// The MIME type.
        mime: String,
        /// The byte length uploaded (never the bytes themselves in the assertion surface).
        len: usize,
    },
    /// `files.update` (a content replace by id — the retry-safe upsert path).
    UpdateContent {
        /// The file id replaced.
        id: String,
        /// The MIME type.
        mime: String,
        /// The byte length.
        len: usize,
    },
    /// `files.update` metadata (rename / re-parent).
    ModifyFile {
        /// The file id modified.
        id: String,
        /// The new name, if renamed.
        new_name: Option<String>,
        /// Parent ids added.
        add_parents: Vec<String>,
        /// Parent ids removed.
        remove_parents: Vec<String>,
    },
    /// `files.copy` (a server-side copy).
    CopyFile {
        /// The source file id.
        id: String,
        /// The destination parent.
        parent: String,
        /// The new name.
        name: String,
    },
    /// `files.trash` (the default REMOVE — not a permanent delete).
    Trash {
        /// The trashed file id.
        id: String,
    },
    /// `files.delete` (the irreversible hard delete).
    Delete {
        /// The deleted file id.
        id: String,
    },
}

impl MockDriveClient {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a file returned by `get_file` (matched by id).
    #[must_use]
    pub fn with_file(mut self, file: FileMeta) -> Self {
        self.files.push(file);
        self
    }

    /// Seed a Shared Drive returned by `list_drives`.
    #[must_use]
    pub fn with_drive(mut self, drive: SharedDrive) -> Self {
        self.drives.push(drive);
        self
    }

    /// Queue one file page returned (FIFO) by `list_files`.
    #[must_use]
    pub fn with_list_page(self, page: FilePage) -> Self {
        if let Ok(mut q) = self.list_pages.lock() {
            q.push(page);
        }
        self
    }

    /// Seed the bytes a `download` of `id` returns.
    #[must_use]
    pub fn with_download(self, id: &str, bytes: Vec<u8>) -> Self {
        if let Ok(mut d) = self.downloads.lock() {
            d.push((id.to_string(), bytes));
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

impl GDriveClient for MockDriveClient {
    fn list_files(
        &self,
        query: &str,
        drive_id: Option<&str>,
        page_size: Option<u32>,
    ) -> Result<FilePage, DriveError> {
        self.record(RecordedCall::ListFiles {
            query: query.to_string(),
            drive_id: drive_id.map(str::to_string),
            page_size,
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
            .unwrap_or_default();
        Ok(page)
    }

    fn get_file(&self, id: &str) -> Result<FileMeta, DriveError> {
        self.record(RecordedCall::GetFile { id: id.to_string() });
        self.files
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .ok_or(DriveError::Api {
                op: "files.get",
                status: 404,
            })
    }

    fn list_drives(&self) -> Result<Vec<SharedDrive>, DriveError> {
        self.record(RecordedCall::ListDrives);
        Ok(self.drives.clone())
    }

    fn download(&self, id: &str, revision: Option<&str>) -> Result<Vec<u8>, DriveError> {
        self.record(RecordedCall::Download {
            id: id.to_string(),
            revision: revision.map(str::to_string),
        });
        self.downloads
            .lock()
            .ok()
            .and_then(|d| d.iter().find(|(fid, _)| fid == id).map(|(_, b)| b.clone()))
            .ok_or(DriveError::Api {
                op: "files.get.media",
                status: 404,
            })
    }

    fn export(&self, id: &str, export_mime: &str) -> Result<Vec<u8>, DriveError> {
        self.record(RecordedCall::Export {
            id: id.to_string(),
            export_mime: export_mime.to_string(),
        });
        // The mock returns the seeded download bytes for the id, standing in for the export body.
        self.downloads
            .lock()
            .ok()
            .and_then(|d| d.iter().find(|(fid, _)| fid == id).map(|(_, b)| b.clone()))
            .ok_or(DriveError::Api {
                op: "files.export",
                status: 404,
            })
    }

    fn upload(
        &self,
        parent: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<String, DriveError> {
        self.record(RecordedCall::Upload {
            parent: parent.to_string(),
            name: name.to_string(),
            mime: mime.to_string(),
            len: bytes.len(),
        });
        Ok("file-new".to_string())
    }

    fn update_content(&self, id: &str, mime: &str, bytes: &[u8]) -> Result<(), DriveError> {
        self.record(RecordedCall::UpdateContent {
            id: id.to_string(),
            mime: mime.to_string(),
            len: bytes.len(),
        });
        Ok(())
    }

    fn modify_file(
        &self,
        id: &str,
        new_name: Option<&str>,
        add_parents: &[String],
        remove_parents: &[String],
    ) -> Result<(), DriveError> {
        self.record(RecordedCall::ModifyFile {
            id: id.to_string(),
            new_name: new_name.map(str::to_string),
            add_parents: add_parents.to_vec(),
            remove_parents: remove_parents.to_vec(),
        });
        Ok(())
    }

    fn copy_file(&self, id: &str, parent: &str, name: &str) -> Result<String, DriveError> {
        self.record(RecordedCall::CopyFile {
            id: id.to_string(),
            parent: parent.to_string(),
            name: name.to_string(),
        });
        Ok("file-copy".to_string())
    }

    fn trash(&self, id: &str) -> Result<(), DriveError> {
        self.record(RecordedCall::Trash { id: id.to_string() });
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<(), DriveError> {
        self.record(RecordedCall::Delete { id: id.to_string() });
        Ok(())
    }
}
