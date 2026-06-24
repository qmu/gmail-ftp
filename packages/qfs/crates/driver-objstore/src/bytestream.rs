//! [`ByteStream`] — the bounded-memory, vendor-free body type that crosses the driver boundary in
//! both directions (RFD-0001 §9 no-vendor-leak; the streaming-not-buffering invariant).
//!
//! ## Why an owned chunk stream and not `Vec<u8>` / `hyper::Body`
//! A large object must move end-to-end without materializing the whole blob in memory, and the
//! engine must stay backend-agnostic: on EC2 the body is a `hyper` response stream, on `wasm32`
//! the R2 binding yields a `ReadableStream` — neither may leak past the crate. [`ByteStream`]
//! abstracts both as an iterator of **bounded chunks** (each `<= max_chunk`), so a GET pipes into
//! a codec/file chunk-by-chunk and a PUT/multipart consumes the source one part at a time. The
//! whole object is never required to be resident.
//!
//! At E0 the concrete transports are mocked, so the in-memory chunk vector IS the stream; the
//! shape is the same one a real `hyper`/R2 adapter fills, so swapping the transport in later does
//! not change the engine-facing API (the conformance goal).

/// The default upper bound on a single chunk's length (1 MiB) — the streaming granularity. A
/// producer never emits a chunk larger than this, so a consumer's working set stays bounded
/// regardless of the object's total size.
pub const DEFAULT_MAX_CHUNK: usize = 1024 * 1024;

/// A bounded, owned byte stream — an iterator of length-capped chunks. Owned, vendor-free; the
/// only body type that crosses the driver's public API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteStream {
    chunks: Vec<Vec<u8>>,
    /// The per-chunk cap a producer respects (metadata; not a hard guard on already-built chunks).
    max_chunk: usize,
}

impl ByteStream {
    /// An empty stream (a zero-byte object).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            chunks: Vec::new(),
            max_chunk: DEFAULT_MAX_CHUNK,
        }
    }

    /// Build a stream from a whole buffer, **splitting** it into bounded chunks of at most
    /// [`DEFAULT_MAX_CHUNK`] bytes — so even a caller that has the whole blob hands the engine a
    /// bounded-granularity stream (the engine never sees one giant chunk).
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::from_bytes_with_chunk(bytes, DEFAULT_MAX_CHUNK)
    }

    /// Build a chunked stream from a whole buffer with an explicit chunk cap (the multipart
    /// part-size policy uses this to align chunk boundaries to part boundaries).
    #[must_use]
    pub fn from_bytes_with_chunk(bytes: impl Into<Vec<u8>>, max_chunk: usize) -> Self {
        let bytes = bytes.into();
        let cap = max_chunk.max(1);
        let chunks = if bytes.is_empty() {
            Vec::new()
        } else {
            bytes.chunks(cap).map(<[u8]>::to_vec).collect()
        };
        Self {
            chunks,
            max_chunk: cap,
        }
    }

    /// Build a stream from pre-formed chunks (e.g. a transport adapter that already yields frames).
    #[must_use]
    pub fn from_chunks(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks,
            max_chunk: DEFAULT_MAX_CHUNK,
        }
    }

    /// The per-chunk cap a producer respects.
    #[must_use]
    pub const fn max_chunk(&self) -> usize {
        self.max_chunk
    }

    /// The total byte length across all chunks (metadata — does not materialize a buffer).
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    /// Whether the stream carries no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.iter().all(Vec::is_empty)
    }

    /// The number of chunks (frames) the stream is split into — the streaming granularity a test
    /// asserts (proving a large body is NOT one buffer).
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Borrow the chunks (a consumer iterates them one bounded frame at a time).
    #[must_use]
    pub fn chunks(&self) -> &[Vec<u8>] {
        &self.chunks
    }

    /// Collect the whole stream into one contiguous buffer. This is the explicit, opt-in
    /// materialization point — a caller that genuinely needs the whole object (e.g. a small JSON
    /// blob handed to a codec) calls this knowingly; the streaming path never does.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        for chunk in self.chunks {
            out.extend_from_slice(&chunk);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bytes_splits_into_bounded_chunks() {
        // 2.5 chunks worth of data → 3 chunks, each <= the cap.
        let data = vec![7u8; DEFAULT_MAX_CHUNK * 2 + DEFAULT_MAX_CHUNK / 2];
        let stream = ByteStream::from_bytes(data.clone());
        assert_eq!(stream.len(), data.len());
        assert_eq!(stream.chunk_count(), 3, "bounded-memory chunking");
        for chunk in stream.chunks() {
            assert!(chunk.len() <= DEFAULT_MAX_CHUNK, "every chunk is bounded");
        }
        // Round-trips losslessly.
        assert_eq!(stream.into_bytes(), data);
    }

    #[test]
    fn empty_and_small_streams() {
        assert!(ByteStream::empty().is_empty());
        let small = ByteStream::from_bytes(b"hi".to_vec());
        assert_eq!(small.chunk_count(), 1);
        assert_eq!(small.len(), 2);
        assert!(!small.is_empty());
    }

    #[test]
    fn explicit_chunk_size_aligns_to_part_boundaries() {
        let s = ByteStream::from_bytes_with_chunk(vec![0u8; 10], 4);
        // 10 bytes / 4 = chunks of 4,4,2.
        assert_eq!(s.chunk_count(), 3);
        assert_eq!(s.chunks()[2].len(), 2);
    }
}
