//! Codec round-trip checks (t38, RFD §4/§9): `roundtrip_codec(fmt, bytes) -> RoundTrip`
//! proving `DECODE∘ENCODE == identity` for `json/yaml/toml/csv/markdown+frontmatter`, over an
//! in-house input corpus.
//!
//! ## Identity is over ROWS, not bytes (the robust invariant)
//! Exact byte formatting (whitespace, key order, comments) is documented as non-preserving
//! for several codecs (RFD §4 markdown note). The load-bearing invariant is **row stability
//! under a re-encode cycle**: `decode(encode(decode(bytes)))` equals `decode(bytes)`. If the
//! rows survive a full DECODE→ENCODE→DECODE loop unchanged, the codec is a faithful inverse on
//! the data it claims to carry — which is the property every read/write path depends on.
//!
//! ## In-house corpus, not `proptest` (ADR-0006)
//! `proptest` is absent from the offline cargo cache, so the corpus is a small **deterministic,
//! seeded** set of representative inputs per format (the driver tickets used example-based
//! corpora for the same reason). The set covers the shapes each codec must handle (objects,
//! arrays, nested values, frontmatter+body) — enough to catch a non-inverse without an uncached
//! property-test framework.

use qfs_core::{CodecRegistry, RowBatch};

/// The outcome of a codec round-trip check: the original rows and the rows after a full
/// DECODE→ENCODE→DECODE cycle. [`RoundTrip::is_identity`] is the assertion.
#[derive(Debug, Clone)]
pub struct RoundTrip {
    /// The format under test (e.g. `"json"`).
    pub fmt: String,
    /// The rows from the first `decode(bytes)`.
    pub decoded: RowBatch,
    /// The rows from `decode(encode(decoded))` — must equal `decoded`.
    pub recoded: RowBatch,
}

impl RoundTrip {
    /// Whether the round-trip is the identity on rows: `decode(encode(decode(bytes)))` equals
    /// `decode(bytes)`.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.decoded == self.recoded
    }

    /// Assert the round-trip is the identity, with a readable diff hint on failure.
    ///
    /// # Panics
    /// Panics if the rows differ after the re-encode cycle.
    pub fn assert_identity(&self) {
        assert!(
            self.is_identity(),
            "codec `{}` is not a faithful inverse on rows:\n  decoded: {:?}\n  recoded: {:?}",
            self.fmt,
            self.decoded,
            self.recoded
        );
    }
}

/// Decode `bytes` with the `fmt` codec, re-encode, decode again, and return the [`RoundTrip`]
/// for assertion. Uses the canonical builtin [`CodecRegistry`] (the t15 set) so the harness
/// tests the *production* codecs, not a stand-in.
///
/// # Panics
/// Panics (test-only) if the format is unknown or any leg fails — a corpus entry that does not
/// decode is a test-author error.
#[must_use]
pub fn roundtrip_codec(fmt: &str, bytes: &[u8]) -> RoundTrip {
    let reg = CodecRegistry::with_builtins();
    let codec = reg
        .resolve(fmt)
        .unwrap_or_else(|e| panic!("qfs-test roundtrip_codec: unknown format `{fmt}`: {e}"));
    let decoded = codec
        .decode(bytes)
        .unwrap_or_else(|e| panic!("qfs-test roundtrip_codec: `{fmt}` decode failed: {e}"));
    let encoded = codec
        .encode(&decoded)
        .unwrap_or_else(|e| panic!("qfs-test roundtrip_codec: `{fmt}` encode failed: {e}"));
    let recoded = codec
        .decode(&encoded)
        .unwrap_or_else(|e| panic!("qfs-test roundtrip_codec: `{fmt}` re-decode failed: {e}"));
    RoundTrip {
        fmt: fmt.to_string(),
        decoded,
        recoded,
    }
}

/// The deterministic input corpus: representative inputs per format the round-trip is proven
/// over. A `(fmt, input-bytes)` list; `corpus()` is the single source the demonstration test
/// iterates. Seeded/example-based (no `proptest`, ADR-0006) but covering each codec's shapes.
#[must_use]
pub fn corpus() -> Vec<(&'static str, &'static [u8])> {
    vec![
        // JSON: an array of objects, a single object, and a scalar.
        ("json", br#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#),
        ("json", br#"{"name":"work","n":3}"#),
        // JSONL: one object per line.
        ("jsonl", b"{\"a\":1}\n{\"a\":2}\n"),
        // YAML: a list of maps.
        ("yaml", b"- a: 1\n  b: x\n- a: 2\n  b: y\n"),
        // TOML: a table with scalar fields.
        ("toml", b"name = \"work\"\nn = 3\n"),
        // CSV: a header row plus data rows.
        ("csv", b"a,b\n1,x\n2,y\n"),
        // Markdown + frontmatter: keys become columns, body becomes content.
        (
            "md+frontmatter",
            b"---\ntitle: Hello\ntag: note\n---\nThe body text.\n",
        ),
        // Markdown with no frontmatter: a single body column.
        ("md+frontmatter", b"Just a body, no frontmatter.\n"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_corpus_entry_round_trips_to_identity() {
        for (fmt, bytes) in corpus() {
            roundtrip_codec(fmt, bytes).assert_identity();
        }
    }

    #[test]
    fn covers_each_builtin_format_at_least_once() {
        let covered: std::collections::BTreeSet<&str> =
            corpus().into_iter().map(|(f, _)| f).collect();
        for fmt in ["json", "jsonl", "yaml", "toml", "csv", "md+frontmatter"] {
            assert!(covered.contains(fmt), "corpus missing format `{fmt}`");
        }
    }
}
