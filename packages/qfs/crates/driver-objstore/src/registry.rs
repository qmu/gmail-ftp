//! `registry` — the per-mount bucket registry (RFD-0001 §5). The engine builds it from the
//! configured buckets; the driver looks a handle up by the path's `<bucket>` segment.
//!
//! Each [`Bucket`] pairs a shared [`ObjectBackend`] with its **versioning** flag — the flag that
//! decides whether a plain `REMOVE` is irreversible (non-versioned: the object is gone) or
//! recoverable (versioned: a delete-marker is inserted). A single shared backend commonly serves
//! many buckets (one account/endpoint); the registry maps a bucket name to its handle so a
//! least-privilege deployment can scope per-bucket.

use std::collections::HashMap;
use std::sync::Arc;

use crate::backend::ObjectBackend;
use crate::error::ObjError;

/// One live bucket handle: the shared [`ObjectBackend`] + whether the bucket has versioning
/// enabled. Cheaply cloneable (the backend is behind an `Arc`).
#[derive(Clone)]
pub struct Bucket {
    backend: Arc<dyn ObjectBackend>,
    versioned: bool,
}

impl Bucket {
    /// Build a non-versioned bucket handle over `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn ObjectBackend>) -> Self {
        Self {
            backend,
            versioned: false,
        }
    }

    /// Build a **versioned** bucket handle (a plain `REMOVE` inserts a recoverable delete-marker).
    #[must_use]
    pub fn versioned(backend: Arc<dyn ObjectBackend>) -> Self {
        Self {
            backend,
            versioned: true,
        }
    }

    /// The shared backend (the read/commit I/O path).
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn ObjectBackend> {
        &self.backend
    }

    /// Whether the bucket has versioning enabled.
    #[must_use]
    pub const fn is_versioned(&self) -> bool {
        self.versioned
    }
}

/// The bucket registry, keyed by bucket name. Built by the engine from the configured buckets; the
/// driver resolves a handle by the path's `<bucket>` segment. Shared between the two scheme drivers
/// (`/s3` and `/r2`) via separate registry instances at construction.
#[derive(Clone, Default)]
pub struct ObjRegistry {
    buckets: HashMap<String, Bucket>,
}

impl ObjRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a bucket under `name`.
    #[must_use]
    pub fn with_bucket(mut self, name: impl Into<String>, handle: Bucket) -> Self {
        self.buckets.insert(name.into(), handle);
        self
    }

    /// Look up a bucket handle by name.
    ///
    /// # Errors
    /// [`ObjError::InvalidPath`] if no bucket is registered under `name`.
    pub fn bucket(&self, name: &str) -> Result<&Bucket, ObjError> {
        self.buckets.get(name).ok_or(ObjError::InvalidPath {
            path: name.to_string(),
            reason: "no such registered bucket",
        })
    }

    /// Whether a bucket is registered (the introspective capability gate uses this without
    /// borrowing the handle).
    #[must_use]
    pub fn has_bucket(&self, name: &str) -> bool {
        self.buckets.contains_key(name)
    }
}
