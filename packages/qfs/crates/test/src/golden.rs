//! Canonical-JSON golden serialization + the cargo-native bless workflow + the
//! credential-shape scrub (t38, RFD §3/§5/§9/§10).
//!
//! The harness's golden strategy is **in-house** (ADR-0006): an owned DTO is serialized to
//! **canonical JSON** (deterministic map-key ordering + redacted non-deterministic fields)
//! and compared against a checked-in `tests/fixtures/*.json`. We deliberately do **not**
//! pull `insta` — it (and its `pest`/`ron`/`similar`/`console` support tree) is absent from
//! the offline cargo cache, so a dependency-light equivalent keeps the build affordable on
//! the tight disk and consistent with ADR-0001/0002/0003/0004/0005 (in-house over an
//! uncached vendor crate). The whole trip hand-rolled its goldens this way; t38 lifts the
//! pattern into one authority.
//!
//! ## Determinism (the hard part, RFD §6)
//! A plan is a DAG and batching is unordered. Before comparing, [`canonical_json`]:
//! 1. serializes the owned DTO via `serde_json` to a `serde_json::Value`,
//! 2. **recursively sorts every object's keys** (so a `BTreeMap`/struct field reorder cannot
//!    flap a golden),
//! 3. **redacts non-deterministic fields** (`timestamp`/`ts`/`request_id`/`id`/`run_id`/
//!    `updated_at`/`created_at`) to a stable sentinel,
//! 4. pretty-prints with a trailing newline (so a checked-in fixture is a clean text file).
//!
//! Node ordering inside a `Plan` is canonicalized separately by [`crate::plan_assert`] (a
//! stable topological normalization) before the DTO reaches here.
//!
//! ## Bless workflow (cargo-native, NOT `cargo insta review`)
//! When the env var `QFS_BLESS=1` is set, [`assert_golden`] **writes** the fixture instead of
//! comparing — the single, obvious update path (`QFS_BLESS=1 cargo test -p qfs-test`). With
//! the var unset (the CI default) a drift is a hard failure with a readable diff hint.

use serde::Serialize;

/// The sentinel a redacted non-deterministic field is replaced with before comparison, so a
/// timestamp / request-id never flaps a golden (RFD §6 determinism).
pub const REDACTED: &str = "<redacted>";

/// The object keys whose values are non-deterministic across runs/platforms and are redacted
/// to [`REDACTED`] before serialization. Lower-cased match (case-insensitive).
const NON_DETERMINISTIC_KEYS: &[&str] = &[
    "timestamp",
    "ts",
    "request_id",
    "requestid",
    "run_id",
    "runid",
    "updated_at",
    "created_at",
    "now",
    "nonce",
];

/// Serialize an owned DTO to **canonical JSON** (sorted keys + redacted non-deterministic
/// fields + trailing newline) — the single golden serializer every helper routes through.
///
/// # Panics
/// Panics (test-only) if the value cannot be serialized to JSON — a golden of an
/// unserializable DTO is a test-author bug, surfaced loudly rather than swallowed.
#[must_use]
pub fn canonical_json<T: Serialize>(value: &T) -> String {
    let raw = serde_json::to_value(value).unwrap_or_else(|e| {
        panic!("qfs-test golden: value is not serializable to JSON: {e}");
    });
    let canon = canonicalize(raw);
    let mut out = serde_json::to_string_pretty(&canon).unwrap_or_else(|e| {
        panic!("qfs-test golden: canonical value did not re-serialize: {e}");
    });
    out.push('\n');
    out
}

/// Recursively sort object keys and redact non-deterministic field values. Arrays keep their
/// order (the caller canonicalizes DAG-node order before serialization). A scalar passes
/// through unchanged unless its parent key marked it non-deterministic.
fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            // BTreeMap collection sorts keys lexicographically — deterministic by construction.
            let mut sorted = serde_json::Map::new();
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in entries {
                let v = if is_non_deterministic(&k) {
                    serde_json::Value::String(REDACTED.to_string())
                } else {
                    canonicalize(v)
                };
                sorted.insert(k, v);
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        other => other,
    }
}

/// Whether an object key names a non-deterministic field (case-insensitive).
fn is_non_deterministic(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    NON_DETERMINISTIC_KEYS.contains(&lower.as_str())
}

/// The crate-relative fixtures directory: `crates/test/tests/fixtures/`. Resolved from
/// `CARGO_MANIFEST_DIR` so the path is correct regardless of the cwd a test runs under.
#[must_use]
pub fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Assert an owned DTO's [`canonical_json`] equals the checked-in fixture `<name>.json`, or,
/// under `QFS_BLESS=1`, (re)write the fixture — the cargo-native bless workflow.
///
/// On a mismatch the panic message shows both renderings and the one-line bless command, so a
/// reviewer can eyeball the diff and update intentionally.
///
/// # Panics
/// - Under `QFS_BLESS=1`: panics only if the fixture file cannot be written (an environment
///   problem, surfaced rather than silently passing).
/// - Otherwise: panics on a content mismatch, or if the fixture is missing (run the bless
///   command to seed it).
pub fn assert_golden<T: Serialize>(name: &str, value: &T) {
    let actual = canonical_json(value);
    let dir = fixtures_dir();
    let path = dir.join(format!("{name}.json"));

    if std::env::var_os("QFS_BLESS").is_some() {
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
            panic!("qfs-test bless: cannot create fixtures dir {dir:?}: {e}");
        });
        std::fs::write(&path, actual.as_bytes()).unwrap_or_else(|e| {
            panic!("qfs-test bless: cannot write fixture {path:?}: {e}");
        });
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "qfs-test golden `{name}`: fixture {path:?} is missing.\n\
             Seed it with:  QFS_BLESS=1 cargo test -p qfs-test"
        );
    });

    assert!(
        actual == expected,
        "qfs-test golden `{name}` drift (fixture {path:?}).\n\
         --- expected (checked-in) ---\n{expected}\n\
         --- actual (this run) ---\n{actual}\n\
         If the change is intended, re-bless:  QFS_BLESS=1 cargo test -p qfs-test"
    );
}

// ---------------------------------------------------------------------------
// Credential-shape scrub (RFD §10): a golden must contain no token-shaped string.
// ---------------------------------------------------------------------------

/// Assert a rendered golden contains **no credential-shaped string** (RFD §10): a "passing"
/// golden must provably carry no token. The scrub flags the common secret prefixes/shapes a
/// leak would take — OAuth bearer tokens, AWS keys, Slack tokens, Google refresh tokens,
/// PEM private-key headers, and long bearer blobs.
///
/// This is the test-side enforcement of the purity/least-privilege invariant: a `Plan` /
/// preview / AST snapshot rides only owned DTOs that never embed a secret, so a token shape
/// appearing in one is a failed review — caught mechanically here.
///
/// # Panics
/// Panics with the offending shape if any credential-shaped substring is present.
pub fn assert_no_credential_shape(rendered: &str) {
    /// Static credential-shape markers a leaked token would carry. Kept conservative (clear
    /// secret prefixes), not a heuristic entropy scan — a false positive here is a test break.
    const SECRET_SHAPES: &[&str] = &[
        "Bearer ",    // an OAuth bearer header value
        "ya29.",      // Google OAuth access token
        "AKIA",       // AWS access key id
        "xoxb-",      // Slack bot token
        "xoxp-",      // Slack user token
        "xapp-",      // Slack app-level token
        "sk-",        // generic secret-key prefix
        "ghp_",       // GitHub personal access token
        "gho_",       // GitHub OAuth token
        "-----BEGIN", // a PEM private key header
        "1//",        // Google refresh-token shape
    ];
    for shape in SECRET_SHAPES {
        assert!(
            !rendered.contains(shape),
            "credential-shape leak: golden contains `{shape}` — a token shape must never appear \
             in an owned-DTO snapshot (RFD §10). Rendered:\n{rendered}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample {
        zeta: u32,
        alpha: u32,
        request_id: String,
        nested: Inner,
    }

    #[derive(Serialize)]
    struct Inner {
        ts: u64,
        body: String,
    }

    #[test]
    fn canonical_json_sorts_keys_and_redacts_nondeterministic_fields() {
        let a = Sample {
            zeta: 1,
            alpha: 2,
            request_id: "req-abc-123".to_string(),
            nested: Inner {
                ts: 1_700_000_000,
                body: "hello".to_string(),
            },
        };
        let rendered = canonical_json(&a);
        // alpha precedes zeta (keys sorted), and the non-deterministic fields are redacted.
        let alpha_at = rendered.find("alpha").unwrap();
        let zeta_at = rendered.find("zeta").unwrap();
        assert!(alpha_at < zeta_at, "keys must be sorted:\n{rendered}");
        assert!(rendered.contains(REDACTED));
        assert!(!rendered.contains("req-abc-123"), "request_id not redacted");
        assert!(!rendered.contains("1700000000"), "ts not redacted");
        assert!(rendered.contains("hello"), "deterministic field kept");
        assert!(rendered.ends_with('\n'), "trailing newline");
    }

    #[test]
    fn canonical_json_is_stable_across_field_order() {
        // Two values differing only in declaration order canonicalize identically.
        let one = serde_json::json!({"b": 1, "a": {"d": 4, "c": 3}});
        let two = serde_json::json!({"a": {"c": 3, "d": 4}, "b": 1});
        assert_eq!(canonical_json(&one), canonical_json(&two));
    }

    #[test]
    fn scrub_passes_clean_text_and_flags_token_shapes() {
        // A clean owned-DTO render passes.
        assert_no_credential_shape(r#"{"name":"work","route":"/hooks/x"}"#);
    }

    #[test]
    #[should_panic(expected = "credential-shape leak")]
    fn scrub_flags_a_bearer_token() {
        assert_no_credential_shape(r#"{"auth":"Bearer ya29.A0ARrdaM-leaked"}"#);
    }
}
