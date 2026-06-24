//! t37 CI gate: **the type system, not discipline, keeps secrets out of logs/plans.**
//!
//! A credential or plan type must never hold live key material in a plaintext `String` field — it
//! must be wrapped in [`qfs_secrets::Secret`] (redacting `Debug`/`Display`, no `Serialize`,
//! zeroized on drop). This mechanical gate scans the source of the credential-store crate and the
//! pure plan/effect crate for struct/enum fields whose NAME looks like a secret
//! (`token`/`secret`/`password`/`api_key`/`credential`/`bearer`/`access_key`/`refresh_token`/…)
//! but whose TYPE is a bare `String`/`&str`. Such a field is the exact shape a leak rides on, so
//! we fail the build if one appears.
//!
//! ## What it scans (documented)
//! - `crates/secrets/src/*.rs` — the credential store + DTOs (the `Secret` wrapper itself is the
//!   sanctioned home of key material and is exempt: it is bytes-by-construction, not a token
//!   `String` field).
//! - `crates/plan/src/*.rs` — the pure `Plan`/`Effect` types that travel to PREVIEW/audit and must
//!   never embed a secret (RFD §3 purity invariant: a plan carries only an account *selector*).
//!
//! It mirrors the `dep_direction`/deny-test style: a fail-closed, reviewable mechanical assertion
//! rather than relying on a reviewer to spot a plaintext token. A genuinely-needed token field
//! must use `Secret`; if a scanned NAME is a false positive (a non-secret field that merely
//! contains the word "token", e.g. a `csrf_token` that is public), it is added to the documented
//! allowlist below with a rationale.

// Test code: setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// The secret-shaped field-name fragments. A field whose name contains one of these AND whose type
/// is a bare `String`/`&str` is a candidate plaintext-token leak.
const SECRET_NAME_FRAGMENTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passphrase",
    "api_key",
    "apikey",
    "credential",
    "bearer",
    "access_key",
    "refresh",
    "client_secret",
    "private_key",
];

/// Documented false positives: a `(file_substr, line_substr)` pair whose match is a NON-secret use
/// of a secret-shaped word. Each entry is a reviewed exemption; keep them minimal and explained.
const ALLOWLIST: &[(&str, &str)] = &[
    // The `Secret` wrapper's own docs/identifiers mention "secret"/"token" pervasively; the type
    // holds `Zeroizing<Vec<u8>>`, never a plaintext `String` token field, so its file is exempt.
    ("secrets/src/secret.rs", ""),
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is .../crates/secrets; go up two to the workspace root.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().unwrap().parent().unwrap().to_path_buf()
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(rust_sources(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}

/// Whether a source line declares a struct/enum FIELD (`name: Type`) whose name is secret-shaped
/// and whose type is a bare `String`/`&str` (the leak shape). Returns the offending field name.
fn plaintext_secret_field(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // Skip comments and attributes.
    if trimmed.starts_with("//") || trimmed.starts_with("#[") || trimmed.starts_with("*") {
        return None;
    }
    // A field declaration looks like `[pub] name: Type,`. Split on the first `:`.
    let colon = trimmed.find(':')?;
    let name_part = trimmed[..colon].trim_start_matches("pub ").trim();
    // The field name must be a bare identifier (no `(`, `<`, `::`, spaces) — excludes method sigs.
    if name_part.is_empty()
        || !name_part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let name_lower = name_part.to_ascii_lowercase();
    if !SECRET_NAME_FRAGMENTS
        .iter()
        .any(|frag| name_lower.contains(frag))
    {
        return None;
    }
    // The type half: is it a bare String / &str (not Secret, not Option<Secret>, not a non-string)?
    let type_part = trimmed[colon + 1..].trim().trim_end_matches(',').trim();
    let is_plaintext_string = type_part == "String"
        || type_part == "&str"
        || type_part == "&'static str"
        || type_part.starts_with("Option<String")
        || type_part.starts_with("Option<&str");
    if is_plaintext_string && !type_part.contains("Secret") {
        return Some(name_part.to_string());
    }
    None
}

#[test]
fn no_credential_or_plan_type_holds_a_plaintext_token_string() {
    let root = workspace_root();
    let mut scanned_dirs = Vec::new();
    let mut files = Vec::new();
    for sub in ["crates/secrets/src", "crates/plan/src"] {
        let dir = root.join(sub);
        assert!(dir.is_dir(), "expected scan dir {} to exist", dir.display());
        scanned_dirs.push(sub);
        files.extend(rust_sources(&dir));
    }
    assert!(
        !files.is_empty(),
        "the gate scanned zero files (wrong root?)"
    );

    let mut violations = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            // Skip allowlisted (file, line) combinations.
            let allowed = ALLOWLIST
                .iter()
                .any(|(fsub, lsub)| rel.contains(fsub) && (lsub.is_empty() || line.contains(lsub)));
            if allowed {
                continue;
            }
            if let Some(field) = plaintext_secret_field(line) {
                violations.push(format!(
                    "{rel}:{}: field `{field}` is a plaintext secret-shaped `String` — wrap it in \
                     qfs_secrets::Secret (the type system keeps secrets out of logs/plans, t37): \
                     `{}`",
                    lineno + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "t37 plaintext-token gate found {} violation(s) across {scanned_dirs:?}:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sanity: the scanner actually FIRES on a planted plaintext token field (so a green result means
/// "no violations", not "the scanner is broken"). A self-test of the gate's detector.
#[test]
fn scanner_detects_a_planted_plaintext_token_field() {
    assert_eq!(
        plaintext_secret_field("    pub access_token: String,"),
        Some("access_token".to_string())
    );
    assert_eq!(
        plaintext_secret_field("    refresh_token: Option<String>,"),
        Some("refresh_token".to_string())
    );
    // A `Secret`-wrapped token is NOT a violation.
    assert_eq!(plaintext_secret_field("    pub token: Secret,"), None);
    // A non-secret-shaped field is NOT a violation.
    assert_eq!(plaintext_secret_field("    pub name: String,"), None);
    // A method signature / non-field line is NOT a violation.
    assert_eq!(
        plaintext_secret_field("fn token(&self) -> &str {"),
        None,
        "a method signature is not a field declaration"
    );
}
