//! `qfs-driver-objstore` — the **object-storage driver(s)** (RFD-0001 §5, E4 t22): the
//! blob/namespace archetype over S3-compatible object storage, two `Driver` mounts sharing one
//! S3-compatible HTTP core (a self-contained SigV4 v4 signer + a thin client, NO vendor SDK):
//!
//! - **S3** (`/s3/<bucket>/<key>`) — AWS S3 over signed HTTPS.
//! - **R2** (`/r2/<bucket>/<key>`) — Cloudflare R2 over signed HTTPS (native build), or the native
//!   `worker::Bucket` binding under `wasm32` (the parked [`backend::R2BindingBackend`], gated so the
//!   native build never links it; `Backend::Http` is reused for native R2).
//!
//! Native verbs `ls cp mv rm` + universal `UPSERT`/`REMOVE`/`get` (streaming download). Both
//! drivers are the same [`ObjDriver`] under the hood, differing only at the mount edge ([`Scheme`])
//! — so the SigV4 core, DTOs, multipart, and pushdown are written once.
//!
//! ## Surface
//! - [`S3Driver`] / [`R2Driver`] — the introspective `Driver`: `mount()` = `/s3` / `/r2`, archetype
//!   [`Archetype::BlobNamespace`], the object-listing schema, per-node capabilities (a key node →
//!   `{ls,select,upsert,remove,cp,mv,rm}`; a bucket root → `{ls,upsert,cp,mv}`), a `Partial`
//!   pushdown (prefix listing + byte-range GET) and `@versionId` version support on versioned
//!   buckets.
//! - [`ObjApplier`] — the synchronous apply leg `applier()` returns and the
//!   [`qfs_runtime::SharedApplier`] the bridge drives under `COMMIT` (single PUT vs multipart with
//!   abort-on-error; the copy→verify→delete leg primitives for the runtime's cross-source cp/mv).
//! - [`s3_apply_driver`] / [`r2_apply_driver`] — wrap the applier in a
//!   [`qfs_runtime::PlanApplierBridge`] ready to `register` into a `DriverRegistry`.
//!
//! ## Truthful pushdown with residual (the t20 lesson, RFD §6)
//! [`ObjDriver::plan_ls`] pushes the key **prefix** (and delimiter) of a `WHERE` predicate down as
//! a native S3 `prefix=`/`delimiter=` list, and a GET pushes a byte **range** down as a `Range:`
//! header — but when a predicate is only **partially** expressible as a prefix/range, the **exact**
//! predicate is kept as a residual the engine re-filters. A predicate is NEVER silently dropped
//! (never wrong rows).
//!
//! ## No vendor leak (RFD §9)
//! The SigV4 signer and any `http::`-ish type stay inside the private `sigv4` module; only owned
//! DTOs ([`ObjectMeta`]/[`ListPage`]/[`PutResult`]) and a [`ByteStream`] cross the public API.
//! `reqwest` stays in `qfs-driver-http`; this crate rides a LOCAL [`HttpExchange`] seam over
//! `qfs-http-core` (the cf precedent), so it is an **independent runtime leaf**.
//!
//! ## Secret discipline (RFD §10)
//! Credentials are a [`qfs_secrets::Secret`] exposed only inside the signer; the `Authorization`
//! header is redacted by the shared `qfs-http-core` authority — never logged, never in an
//! [`ObjError`].
//!
//! ## Named parks (deferred per the ticket / t38)
//! - **wasm `R2BindingBackend`** — the native `worker::Bucket` backend is parked behind the
//!   [`ObjectBackend`] seam (no live wasm Workers CI lane yet); the DTOs are wasm-clean so it drops
//!   in later producing identical DTOs.
//! - **Live S3/R2 E2E, presigned URLs, SSE-C** — parked to **t38** (tests run against a mocked S3
//!   API + offline SigV4 vectors; no live network, no live credentials).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod backend;
pub mod bytestream;
pub mod dto;
mod effect;
mod error;
pub mod multipart;
pub mod path;
pub mod registry;
mod schema;
mod sigv4;
mod xml;

use std::sync::Arc;

use qfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;
use qfs_types::{CmpOp, Literal, Predicate};

pub use applier::ObjApplier;
pub use backend::{
    Endpoint, HttpBackend, HttpExchange, MockExchange, MockObjectBackend, ObjectBackend,
    RecordedCall, TransportError,
};
pub use bytestream::{ByteStream, DEFAULT_MAX_CHUNK};
pub use dto::{ListPage, ObjectMeta, PutResult};
pub use effect::{ObjEffect, BODY_COL, KEY_COL};
pub use error::ObjError;
pub use multipart::{Multipart, MultipartPolicy, PartEtag, DEFAULT_PART_SIZE};
pub use path::{ObjNode, Scheme, R2_MOUNT, S3_MOUNT};
pub use registry::{Bucket, ObjRegistry};
pub use schema::object_listing_schema;
pub use sigv4::{SigV4Credentials, SigningContext};

/// The AWS/R2 least-privilege scope labels (RFD §10). Documented labels only — never a credential.
/// The server `POLICY` reasons over these.
pub const S3_READ_SCOPE: &str = "s3:GetObject s3:ListBucket";
/// The S3 write least-privilege scope label.
pub const S3_WRITE_SCOPE: &str = "s3:PutObject s3:DeleteObject s3:AbortMultipartUpload";

/// A pushed-down `ls` listing plan: the native `prefix`/`delimiter` an S3 `list_objects_v2` runs,
/// plus the **truthful residual** predicate the engine re-filters after the native list. A `None`
/// residual means the predicate was pushed down **completely** (no local re-filter needed).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ListPushdown {
    /// The native key prefix pushed to S3 (`None` lists the whole bucket).
    pub prefix: Option<String>,
    /// The native delimiter pushed to S3 (the "directory" rollup; `None` lists recursively).
    pub delimiter: Option<String>,
    /// The exact predicate the engine still re-filters (truthful residual; `None` = fully pushed).
    pub residual: Option<Predicate>,
}

/// The shared object-storage driver (RFD §5) — one implementation behind both the [`S3Driver`] and
/// [`R2Driver`] newtypes, parameterized only by its [`Scheme`] (mount + driver id). Owns the
/// synchronous [`ObjApplier`] (over an [`ObjRegistry`]) and the declared pushdown profile.
pub struct ObjDriver {
    scheme: Scheme,
    applier: ObjApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl ObjDriver {
    /// Build an object-storage driver for `scheme` over `registry`.
    #[must_use]
    pub fn new(scheme: Scheme, registry: ObjRegistry) -> Self {
        Self {
            scheme,
            applier: ObjApplier::new(registry),
            // A blob namespace pushes projection (the listing column subset) and a prefix/range
            // WHERE down to its native list/GET; everything else stays a truthful residual the
            // engine re-filters (see `plan_ls`). `project=true, where_=true`; the residual is the
            // planner's concern, not a profile flag.
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: true,
                limit: true,
                order: false,
                join: false,
                aggregate: false,
                distinct: false,
                group_by: false,
            },
            procs: Vec::new(),
        }
    }

    /// Build a driver with an explicit multipart policy (tests use a tiny threshold).
    #[must_use]
    pub fn with_policy(scheme: Scheme, registry: ObjRegistry, policy: MultipartPolicy) -> Self {
        let mut d = Self::new(scheme, registry);
        d.applier = ObjApplier::with_policy(d.applier.registry().clone(), policy);
        d
    }

    /// Borrow the synchronous applier (e.g. to drive a `qfs_plan::commit` directly, or to build the
    /// runtime bridge / call the cp/mv leg primitives).
    #[must_use]
    pub fn obj_applier(&self) -> &ObjApplier {
        &self.applier
    }

    /// Borrow the bucket registry (the read path resolves a handle, then reads).
    #[must_use]
    pub fn registry(&self) -> &ObjRegistry {
        self.applier.registry()
    }

    /// The scheme (S3 / R2) this driver answers for.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// List objects under a `/s3/<bucket>` (or `/r2/<bucket>`) node, applying the pushed-down
    /// prefix/delimiter and returning the page **plus** the truthful residual the engine still
    /// re-filters (the t20 lesson). The only place `ls` I/O happens.
    ///
    /// # Errors
    /// [`ObjError`] on an unregistered bucket or a backend failure.
    pub fn ls(
        &self,
        path: &Path,
        pushdown: &ListPushdown,
        continuation_token: Option<&str>,
    ) -> Result<(ListPage, Option<Predicate>), ObjError> {
        let node = ObjNode::parse(path)?;
        let bucket = node.bucket().ok_or_else(|| ObjError::InvalidPath {
            path: path.as_str().to_string(),
            reason: "ls needs a /<scheme>/<bucket> address",
        })?;
        let handle = self.registry().bucket(bucket)?;
        let page = handle.backend().clone().list_objects_v2(
            bucket,
            pushdown.prefix.as_deref(),
            pushdown.delimiter.as_deref(),
            continuation_token,
        )?;
        Ok((page, pushdown.residual.clone()))
    }

    /// Stream-download an object addressed by `path` (honouring an optional `@versionId` and a
    /// pushed-down byte `range`) into a bounded [`ByteStream`]. The only place `get` I/O happens.
    ///
    /// # Errors
    /// [`ObjError`] on an unregistered bucket, a non-object path, or a backend failure.
    pub fn get(&self, path: &Path, range: Option<(u64, u64)>) -> Result<ByteStream, ObjError> {
        let ObjNode::Object {
            bucket,
            key,
            version_id,
            ..
        } = ObjNode::parse(path)?
        else {
            return Err(ObjError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "get needs a concrete /<scheme>/<bucket>/<key> object address",
            });
        };
        let handle = self.registry().bucket(&bucket)?;
        handle
            .backend()
            .clone()
            .get_object(&bucket, &key, range, version_id.as_deref())
    }

    /// Plan a `WHERE` predicate over a key listing into a [`ListPushdown`]: extract the longest
    /// **literal key prefix** that is *exactly* equivalent to (or a safe superset of) the
    /// predicate, push it down as `prefix=`, and keep the **exact** predicate as a residual unless
    /// it is fully captured by the prefix. Pure — constructs the plan, performs no I/O.
    ///
    /// Truthful-residual contract (the t20 lesson): the returned `prefix` is always a **superset**
    /// filter (it never excludes a row the predicate would keep), and the `residual` is dropped
    /// **only** when the prefix is provably exactly the predicate (a `key LIKE 'p%'` whose only
    /// constraint is that prefix). Anything richer keeps the residual so the engine re-filters.
    #[must_use]
    pub fn plan_ls(predicate: Option<&Predicate>, delimiter: Option<&str>) -> ListPushdown {
        let Some(pred) = predicate else {
            return ListPushdown {
                prefix: None,
                delimiter: delimiter.map(str::to_string),
                residual: None,
            };
        };
        match key_prefix_of(pred) {
            // The predicate is *exactly* a key-prefix constraint → push the prefix, drop residual.
            Some((prefix, true)) => ListPushdown {
                prefix: Some(prefix),
                delimiter: delimiter.map(str::to_string),
                residual: None,
            },
            // A prefix is a safe SUPERSET of the predicate (e.g. one conjunct of an AND, or a `=`
            // also constrains other columns) → push the prefix AND keep the exact predicate as a
            // residual the engine re-filters. NEVER drop it.
            Some((prefix, false)) => ListPushdown {
                prefix: Some(prefix),
                delimiter: delimiter.map(str::to_string),
                residual: Some(pred.clone()),
            },
            // No safe prefix at all → push nothing, keep the whole predicate as the residual.
            None => ListPushdown {
                prefix: None,
                delimiter: delimiter.map(str::to_string),
                residual: Some(pred.clone()),
            },
        }
    }

    /// The per-node capability set (RFD §5):
    /// - a **key node** (`/<scheme>/<bucket>/<key>`) → `{ls,select,upsert,remove,cp,mv,rm}`.
    /// - a **bucket root** (`/<scheme>/<bucket>`) → `{ls,select,upsert,cp,mv}` (list + key-in-row
    ///   write; no `rm`/`remove` without a key).
    /// - anything else (root / unregistered bucket) → the empty set.
    fn caps_for(&self, path: &Path) -> Capabilities {
        match ObjNode::parse(path) {
            Ok(ObjNode::Object { bucket, .. }) if self.registry().has_bucket(&bucket) => {
                Capabilities::from_verbs(&[
                    Verb::Ls,
                    Verb::Select,
                    Verb::Upsert,
                    Verb::Remove,
                    Verb::Cp,
                    Verb::Mv,
                    Verb::Rm,
                ])
            }
            Ok(ObjNode::Bucket { bucket, .. }) if self.registry().has_bucket(&bucket) => {
                Capabilities::from_verbs(&[
                    Verb::Ls,
                    Verb::Select,
                    Verb::Upsert,
                    Verb::Cp,
                    Verb::Mv,
                ])
            }
            _ => Capabilities::none(),
        }
    }
}

impl Driver for ObjDriver {
    fn mount(&self) -> &str {
        self.scheme.mount()
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        let node = ObjNode::parse(path).map_err(|_| qfs_driver::CfsError::InvalidPath {
            path: path.as_str().to_string(),
            reason: "not a valid object-storage address",
        })?;
        match node {
            ObjNode::Bucket { .. } | ObjNode::Object { .. } => Ok(NodeDesc::new(
                Archetype::BlobNamespace,
                schema::object_listing_schema(),
            )),
            ObjNode::Root { .. } => Err(qfs_driver::CfsError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "an object-storage mount root is not a describable node",
            }),
        }
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        self.caps_for(path)
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn version_support(&self, path: &Path) -> VersionSupport {
        // A versioned bucket exposes full @versionId history (RFD §4); a non-versioned bucket only
        // a snapshot ETag for optimistic concurrency.
        match ObjNode::parse(path) {
            Ok(node) => match node.bucket().and_then(|b| self.registry().bucket(b).ok()) {
                Some(handle) if handle.is_versioned() => VersionSupport::Versioned,
                Some(_) => VersionSupport::Snapshot,
                None => VersionSupport::None,
            },
            Err(_) => VersionSupport::None,
        }
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// The AWS S3 driver (`/s3`). A thin newtype over [`ObjDriver`] fixing the [`Scheme::S3`] mount +
/// driver id `s3`.
pub struct S3Driver(ObjDriver);

impl S3Driver {
    /// Build an S3 driver over `registry`.
    #[must_use]
    pub fn new(registry: ObjRegistry) -> Self {
        Self(ObjDriver::new(Scheme::S3, registry))
    }

    /// Borrow the shared inner driver (for `ls`/`get`/`plan_ls`/the applier).
    #[must_use]
    pub fn inner(&self) -> &ObjDriver {
        &self.0
    }
}

impl Driver for S3Driver {
    fn mount(&self) -> &str {
        self.0.mount()
    }
    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        self.0.describe(path)
    }
    fn capabilities(&self, path: &Path) -> Capabilities {
        self.0.capabilities(path)
    }
    fn procedures(&self) -> &[ProcSig] {
        self.0.procedures()
    }
    fn pushdown(&self) -> &PushdownProfile {
        self.0.pushdown()
    }
    fn version_support(&self, path: &Path) -> VersionSupport {
        self.0.version_support(path)
    }
    fn applier(&self) -> &dyn PlanApplier {
        self.0.applier()
    }
}

/// The Cloudflare R2 driver (`/r2`). A thin newtype over [`ObjDriver`] fixing the [`Scheme::R2`]
/// mount + driver id `r2`. On the native build it reuses the same SigV4 [`HttpBackend`] as S3; the
/// native `worker::Bucket` binding ([`backend::R2BindingBackend`]) is parked behind the
/// [`ObjectBackend`] seam under `wasm32` only.
pub struct R2Driver(ObjDriver);

impl R2Driver {
    /// Build an R2 driver over `registry`.
    #[must_use]
    pub fn new(registry: ObjRegistry) -> Self {
        Self(ObjDriver::new(Scheme::R2, registry))
    }

    /// Borrow the shared inner driver.
    #[must_use]
    pub fn inner(&self) -> &ObjDriver {
        &self.0
    }
}

impl Driver for R2Driver {
    fn mount(&self) -> &str {
        self.0.mount()
    }
    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        self.0.describe(path)
    }
    fn capabilities(&self, path: &Path) -> Capabilities {
        self.0.capabilities(path)
    }
    fn procedures(&self) -> &[ProcSig] {
        self.0.procedures()
    }
    fn pushdown(&self) -> &PushdownProfile {
        self.0.pushdown()
    }
    fn version_support(&self, path: &Path) -> VersionSupport {
        self.0.version_support(path)
    }
    fn applier(&self) -> &dyn PlanApplier {
        self.0.applier()
    }
}

/// Wrap an [`S3Driver`]'s synchronous applier in the runtime [`PlanApplierBridge`], yielding the
/// async `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id `s3`.
#[must_use]
pub fn s3_apply_driver(driver: &S3Driver) -> PlanApplierBridge<ObjApplier> {
    PlanApplierBridge::new(Arc::new(driver.inner().obj_applier().clone()))
}

/// Wrap an [`R2Driver`]'s synchronous applier in the runtime [`PlanApplierBridge`] under the driver
/// id `r2`.
#[must_use]
pub fn r2_apply_driver(driver: &R2Driver) -> PlanApplierBridge<ObjApplier> {
    PlanApplierBridge::new(Arc::new(driver.inner().obj_applier().clone()))
}

/// Extract the longest literal **key prefix** a predicate implies, plus whether that prefix is
/// **exactly** the predicate (so the residual can be dropped) or only a **superset** (so the
/// residual must be kept). Returns `None` when no safe prefix can be derived (push nothing).
///
/// - `key = 'p'` → (`p`, exact=false): the prefix `p` is a superset of `=`, but `=` also rejects
///   `pX`, so the residual stays. (Equality is conservatively a superset, never dropped.)
/// - `key LIKE 'p%'` → (`p`, exact=true): the listing prefix IS the constraint; residual dropped.
/// - `key >= 'a'` / `key BETWEEN 'a' AND 'b'` → the common leading prefix as a superset; residual
///   kept (a range is not a single prefix).
/// - one conjunct of an `AND` that yields a prefix → superset; residual kept.
fn key_prefix_of(pred: &Predicate) -> Option<(String, bool)> {
    const KEY: &str = "key";
    let is_key = |c: &qfs_types::ColRef| c.path.len() == 1 && c.path[0].as_str() == KEY;
    match pred {
        // `key LIKE 'prefix%'` with the wildcard ONLY at the end is exactly a prefix list.
        Predicate::Like(col, pat) if is_key(col) => {
            let p = &pat.0;
            if let Some(stripped) = p.strip_suffix('%') {
                if !stripped.is_empty() && !stripped.contains(['%', '_']) {
                    return Some((stripped.to_string(), true));
                }
            }
            None
        }
        // `key = 'x'` → push `x` as a superset prefix, keep the exact `=` as a residual.
        Predicate::Cmp(col, CmpOp::Eq, Literal::Text(v)) if is_key(col) && !v.is_empty() => {
            Some((v.clone(), false))
        }
        // `key >= 'a'` / `key > 'a'` → the literal is a lower bound; a prefix superset is unsafe in
        // general (it would exclude later keys), so push nothing — keep the whole residual. We
        // deliberately do NOT push an ordering bound as a prefix (correctness over cleverness).
        // `key BETWEEN 'a' AND 'b'` → the common leading prefix of a..b is a safe superset.
        Predicate::Between(col, Literal::Text(lo), Literal::Text(hi)) if is_key(col) => {
            let common = common_prefix(lo, hi);
            if common.is_empty() {
                None
            } else {
                Some((common, false))
            }
        }
        // An AND: if either side yields a prefix, push it as a superset and keep the WHOLE
        // predicate as a residual (the other conjunct still constrains).
        Predicate::And(lhs, rhs) => key_prefix_of(lhs)
            .or_else(|| key_prefix_of(rhs))
            .map(|(p, _exact)| (p, false)),
        _ => None,
    }
}

/// The longest common leading byte prefix of two strings (UTF-8 boundary-safe).
fn common_prefix(a: &str, b: &str) -> String {
    let mut out = String::new();
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca == cb {
            out.push(ca);
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests;
