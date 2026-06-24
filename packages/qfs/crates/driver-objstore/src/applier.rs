//! [`ObjApplier`] — the object-storage driver's synchronous apply leg (RFD-0001 §6). It is the
//! lone impure seam the introspective [`crate::S3Driver`]/[`crate::R2Driver`] hand back via
//! `applier()`, and the [`qfs_runtime::SharedApplier`] the runtime's
//! [`qfs_runtime::PlanApplierBridge`] drives under `COMMIT`.
//!
//! Stateless across the call: it holds the [`ObjRegistry`] (backends behind `Arc`s) and performs
//! fresh S3/R2 API I/O on every call — so it implements `SharedApplier` (`&self` apply), the
//! statelessness contract the bridge requires.
//!
//! ## Streaming PUT vs multipart (RFD §6)
//! A `Put` whose body is below the [`MultipartPolicy`] threshold is one `put_object`; at/above it
//! the upload is multipart, and **any** mid-sequence failure triggers `abort_multipart` so no
//! orphan parts are billed — the abort-on-error invariant.
//!
//! ## Copy→verify→delete legs (RFD §6/§7)
//! The cross-source `cp`/`mv` the runtime composes is NOT orchestrated here; this crate exposes the
//! **leg primitives** ([`ObjApplier::copy_leg`], [`ObjApplier::verify_leg`],
//! [`ObjApplier::delete_leg`]) the planner sequences (a same-backend `cp` = copy→verify; an `mv` =
//! copy→verify→delete).
//!
//! ## Secret safety
//! No credential or object body is ever logged or placed in an [`ObjError`]; the secret is wholly
//! behind the SigV4 backend (used only to sign a redacted header), never here.

use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_runtime::{EffectError, EffectOutput, SharedApplier};

use crate::backend::ObjectBackend;
use crate::bytestream::ByteStream;
use crate::dto::PutResult;
use crate::effect::ObjEffect;
use crate::error::ObjError;
use crate::multipart::{Multipart, MultipartPolicy, PartEtag};
use crate::registry::ObjRegistry;

/// The synchronous object-storage apply leg. Holds the [`ObjRegistry`] (backends behind `Arc`s) so
/// the leg is cheap to clone for the runtime bridge and safe to share across blocking apply
/// threads, plus the [`MultipartPolicy`] that decides single-PUT vs multipart.
#[derive(Clone)]
pub struct ObjApplier {
    registry: ObjRegistry,
    policy: MultipartPolicy,
}

impl ObjApplier {
    /// Build an applier over a bucket registry with the default 8 MiB multipart policy.
    #[must_use]
    pub fn new(registry: ObjRegistry) -> Self {
        Self {
            registry,
            policy: MultipartPolicy::default(),
        }
    }

    /// Build an applier with an explicit multipart policy (tests use a tiny threshold to exercise
    /// the multipart path without an 8 MiB fixture).
    #[must_use]
    pub fn with_policy(registry: ObjRegistry, policy: MultipartPolicy) -> Self {
        Self { registry, policy }
    }

    /// Borrow the registry (e.g. for the read path: ls/get go through the driver, not the applier).
    #[must_use]
    pub fn registry(&self) -> &ObjRegistry {
        &self.registry
    }

    /// The multipart policy this applier routes uploads with.
    #[must_use]
    pub const fn policy(&self) -> MultipartPolicy {
        self.policy
    }

    /// Apply one effect node: decode it to an [`ObjEffect`], then dispatch to the addressed
    /// backend. Returns the affected count (1 for a put/delete).
    fn apply_node(&self, node: &EffectNode) -> Result<u64, ObjError> {
        let effect = ObjEffect::from_node(node)?;
        self.apply_effect(&effect)
    }

    /// Apply one decoded [`ObjEffect`] against the addressed backend. The single place S3/R2 API
    /// I/O happens.
    fn apply_effect(&self, effect: &ObjEffect) -> Result<u64, ObjError> {
        match effect {
            ObjEffect::Put { bucket, key, body } => {
                let backend = self.registry.bucket(bucket)?.backend().clone();
                self.put(&*backend, bucket, key, body)?;
                Ok(1)
            }
            ObjEffect::Delete {
                bucket,
                key,
                version_id,
            } => {
                let backend = self.registry.bucket(bucket)?.backend().clone();
                backend.delete_object(bucket, key, version_id.as_deref())?;
                Ok(1)
            }
        }
    }

    /// Upload `body` to `bucket/key`, choosing single-PUT (below threshold) vs multipart (at/above)
    /// and aborting on any mid-multipart failure. The body is framed into bounded chunks so a large
    /// object streams part-by-part (no full-object buffer beyond the source `Vec` the effect
    /// carries at E0).
    fn put(
        &self,
        backend: &dyn ObjectBackend,
        bucket: &str,
        key: &str,
        body: &[u8],
    ) -> Result<PutResult, ObjError> {
        if !self.policy.is_multipart(body.len()) {
            // Single PUT: stream the body as one bounded ByteStream.
            return backend.put_object(bucket, key, &ByteStream::from_bytes(body.to_vec()));
        }
        self.multipart_put(backend, bucket, key, body)
    }

    /// The multipart upload sequence with the abort-on-error invariant: create → N×upload_part →
    /// complete, and on **any** part failure, abort the upload (freeing orphan parts) and surface a
    /// structured [`ObjError::MultipartAborted`].
    fn multipart_put(
        &self,
        backend: &dyn ObjectBackend,
        bucket: &str,
        key: &str,
        body: &[u8],
    ) -> Result<PutResult, ObjError> {
        let upload_id = backend.create_multipart(bucket, key)?;
        let mut mp = Multipart::new(upload_id.clone());

        // Cut the body into part-sized, bounded chunks (the streaming granularity = part size).
        let chunks = ByteStream::from_bytes_with_chunk(body.to_vec(), self.policy.part_size);
        for (i, chunk) in chunks.chunks().iter().enumerate() {
            let part_number = u32::try_from(i + 1).unwrap_or(u32::MAX);
            match backend.upload_part(bucket, key, &upload_id, part_number, chunk) {
                Ok(etag) => mp.record_part(PartEtag::new(part_number, etag)),
                Err(err) => {
                    // Abort-on-error: free orphan parts. The abort's own failure is swallowed (the
                    // original failure is what the caller must see); we surface the original cause.
                    let _ = backend.abort_multipart(bucket, key, &upload_id);
                    return Err(ObjError::MultipartAborted {
                        part: part_number,
                        reason: err.to_string(),
                    });
                }
            }
        }
        backend.complete_multipart(bucket, key, &upload_id, &mp.ordered_parts())
    }

    // ----------------------------------------------------------------------------------------
    // Cross-source cp/mv leg primitives (RFD §6/§7) — the runtime composes these; not here.
    // ----------------------------------------------------------------------------------------

    /// The **copy** leg: a same-backend server-side copy `src_key` → `dst_key`. Returns the new
    /// object's [`PutResult`] (the ETag the verify leg checks).
    ///
    /// # Errors
    /// [`ObjError`] on an unregistered bucket or a backend failure.
    pub fn copy_leg(
        &self,
        bucket: &str,
        src_key: &str,
        dst_key: &str,
    ) -> Result<PutResult, ObjError> {
        let backend = self.registry.bucket(bucket)?.backend().clone();
        backend.copy_object(bucket, src_key, dst_key)
    }

    /// The **verify** leg: confirm the destination object's ETag matches `expected_etag` (the
    /// copy→verify→delete safety check — never delete the source before the copy is proven). Reads
    /// the destination's head metadata via a 0-length ranged GET is overkill at E0; instead the
    /// verify compares the [`PutResult`] ETag the copy leg returned against `expected_etag`.
    ///
    /// # Errors
    /// [`ObjError::Conflict`] if the ETags differ (the copy did not land identically).
    pub fn verify_leg(copy_result: &PutResult, expected_etag: &str) -> Result<(), ObjError> {
        if copy_result.etag == expected_etag {
            Ok(())
        } else {
            Err(ObjError::Conflict {
                version: copy_result.etag.clone(),
            })
        }
    }

    /// The **delete** leg: remove the source object (the final `mv` step, run ONLY after the verify
    /// leg proves the copy landed). Idempotent.
    ///
    /// # Errors
    /// [`ObjError`] on an unregistered bucket or a backend failure.
    pub fn delete_leg(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<(), ObjError> {
        let backend = self.registry.bucket(bucket)?.backend().clone();
        backend.delete_object(bucket, key, version_id)
    }
}

impl SharedApplier for ObjApplier {
    fn apply_shared(&self, node: &EffectNode) -> Result<EffectOutput, EffectError> {
        let affected = self.apply_node(node)?;
        Ok(EffectOutput::new(node.id, affected))
    }
}

impl PlanApplier for ObjApplier {
    /// The introspective `qfs_driver::Driver::applier()` seam (t09): a synchronous, `&mut self`
    /// apply leg. The applier is stateless, so this delegates to the same `&self` core as
    /// [`SharedApplier::apply_shared`]. The structured [`ObjError`] is reduced to the plan crate's
    /// owned `(id, reason)` shape — secret-free by construction.
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        let affected = self
            .apply_node(node)
            .map_err(|e| ApplyError::new(node.id, e.to_string()))?;
        Ok(AppliedEffect::new(node.id, affected))
    }
}
