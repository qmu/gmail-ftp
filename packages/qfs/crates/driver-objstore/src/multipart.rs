//! Multipart-upload policy + sequencing state (RFD-0001 §6 idempotency/recovery).
//!
//! A `put` below the [`MultipartPolicy::threshold`] is one single `put_object`; at or above it the
//! upload is **multipart**: `create_multipart` → N × `upload_part` (each a bounded
//! [`ByteStream`](crate::ByteStream) chunk) → `complete_multipart`. Any failure mid-sequence
//! triggers `abort_multipart` so no orphan parts are billed — the abort-on-error invariant. The
//! `upload_id` is recoverable state the effect node can carry.

/// One uploaded part's `(part_number, etag)` — the receipt `complete_multipart` replays in order.
/// Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PartEtag {
    /// The 1-based part number (S3 requires parts numbered 1..=10000, in ascending order).
    pub part_number: u32,
    /// The part's ETag returned by `upload_part`.
    pub etag: String,
}

impl PartEtag {
    /// Construct a part receipt.
    #[must_use]
    pub fn new(part_number: u32, etag: impl Into<String>) -> Self {
        Self {
            part_number,
            etag: etag.into(),
        }
    }
}

/// The part-size policy: the byte threshold above which a `put` switches from a single PUT to a
/// multipart upload, and the size each part is cut to. Defaults to 8 MiB for both (the ticket's
/// default), which is also S3's 5 MiB minimum-part floor with headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartPolicy {
    /// The size at/above which an upload becomes multipart (bytes).
    pub threshold: usize,
    /// The size each non-final part is cut to (bytes).
    pub part_size: usize,
}

/// The default multipart threshold / part size: 8 MiB.
pub const DEFAULT_PART_SIZE: usize = 8 * 1024 * 1024;

impl Default for MultipartPolicy {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_PART_SIZE,
            part_size: DEFAULT_PART_SIZE,
        }
    }
}

impl MultipartPolicy {
    /// A policy with an explicit threshold + part size (tests use a tiny size to exercise the
    /// multipart path without an 8 MiB fixture).
    #[must_use]
    pub fn new(threshold: usize, part_size: usize) -> Self {
        Self {
            threshold,
            part_size: part_size.max(1),
        }
    }

    /// Whether a body of `len` bytes must use multipart (at/above the threshold).
    #[must_use]
    pub fn is_multipart(&self, len: usize) -> bool {
        len >= self.threshold
    }

    /// The number of parts a `len`-byte body splits into under this policy (at least 1).
    #[must_use]
    pub fn part_count(&self, len: usize) -> u32 {
        if len == 0 {
            return 1;
        }
        u32::try_from(len.div_ceil(self.part_size)).unwrap_or(u32::MAX)
    }
}

/// In-flight multipart sequencing state — the `upload_id` the backend assigned plus the ordered
/// part receipts accumulated so far. The `upload_id` is the recoverable handle an `abort` (on
/// failure) or `complete` (on success) replays. Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Multipart {
    /// The S3/R2-assigned upload id (recoverable state).
    pub upload_id: String,
    /// The ordered part receipts gathered so far.
    pub parts: Vec<PartEtag>,
}

impl Multipart {
    /// Begin tracking a multipart upload under `upload_id`.
    #[must_use]
    pub fn new(upload_id: impl Into<String>) -> Self {
        Self {
            upload_id: upload_id.into(),
            parts: Vec::new(),
        }
    }

    /// Record a completed part (preserving ascending part order).
    pub fn record_part(&mut self, part: PartEtag) {
        self.parts.push(part);
    }

    /// The part receipts in ascending part-number order (what `complete_multipart` sends).
    #[must_use]
    pub fn ordered_parts(&self) -> Vec<PartEtag> {
        let mut parts = self.parts.clone();
        parts.sort_by_key(|p| p.part_number);
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_decides_single_vs_multipart() {
        let policy = MultipartPolicy::default();
        assert!(
            !policy.is_multipart(DEFAULT_PART_SIZE - 1),
            "below = single PUT"
        );
        assert!(policy.is_multipart(DEFAULT_PART_SIZE), "at = multipart");
        assert!(
            policy.is_multipart(DEFAULT_PART_SIZE * 3),
            "above = multipart"
        );
    }

    #[test]
    fn part_count_ceils() {
        let policy = MultipartPolicy::new(10, 4);
        assert_eq!(policy.part_count(0), 1);
        assert_eq!(policy.part_count(4), 1);
        assert_eq!(policy.part_count(5), 2);
        assert_eq!(policy.part_count(12), 3);
    }

    #[test]
    fn multipart_keeps_parts_in_order() {
        let mut mp = Multipart::new("upload-1");
        mp.record_part(PartEtag::new(2, "e2"));
        mp.record_part(PartEtag::new(1, "e1"));
        let ordered = mp.ordered_parts();
        assert_eq!(ordered[0].part_number, 1);
        assert_eq!(ordered[1].part_number, 2);
    }
}
