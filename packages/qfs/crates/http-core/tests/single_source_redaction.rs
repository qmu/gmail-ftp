//! Regression guard (t19 refinement): the header-redaction set is **single-sourced** in
//! `qfs-http-core`. Before this crate, both `qfs-driver-http` and `qfs-google-auth` hand-copied
//! `SENSITIVE_HEADERS` + `is_sensitive_header` + the redacting `Debug`, and the copies had already
//! drifted — risking a token leak by drift (one side adds a sensitive header, the other's copy
//! lags and copies that header *value* across the seam with redaction silently missing it).
//!
//! This test mechanically asserts there is exactly **one** definition of the redaction authority
//! in the workspace — here. The sibling crates may only *re-export* it (`pub use qfs_http_core::…`)
//! or *call* it; if anyone reintroduces a second `pub const SENSITIVE_HEADERS` or a second
//! `pub fn is_sensitive_header`, this fails. It is the structural complement to the
//! `dep_direction.rs` graph assertions (which prove both HTTP crates *depend on* this leaf).

// Test code: setup/assertions may panic/unwrap/expect freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Walk up from this crate's manifest dir to the workspace root (the dir holding `Cargo.toml`
/// with a `[workspace]` table). We start at `crates/http-core` and the root is two levels up.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/http-core -> crates -> <root>
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above crates/http-core")
        .to_path_buf()
}

/// Recursively collect every `.rs` file under `dir`, skipping `target/` and any `tests/`,
/// `benches/`, or `examples/` directory. A `pub const`/`pub fn` *definition* of the redaction
/// authority lives in a crate's `src/`; test/bench sources that merely *mention* the signature
/// strings (like this very file) must not be counted as a second definition.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path.file_name().is_some_and(|n| {
                n == "target" || n == "tests" || n == "benches" || n == "examples"
            });
            if skip {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn redaction_authority_is_defined_exactly_once_in_http_core() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut files = Vec::new();
    rust_sources(&crates_dir, &mut files);
    assert!(
        !files.is_empty(),
        "expected to find Rust sources under {}",
        crates_dir.display()
    );

    // Definition signatures (a *re-export* `pub use qfs_http_core::…` does not match these).
    let const_def = "pub const SENSITIVE_HEADERS";
    let fn_def = "pub fn is_sensitive_header";

    let mut const_sites = Vec::new();
    let mut fn_sites = Vec::new();
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        if src.contains(const_def) {
            const_sites.push(file.clone());
        }
        if src.contains(fn_def) {
            fn_sites.push(file.clone());
        }
    }

    let authority = crates_dir.join("http-core").join("src").join("lib.rs");

    assert_eq!(
        const_sites,
        vec![authority.clone()],
        "single-source violation: `SENSITIVE_HEADERS` must be defined ONLY in \
         qfs-http-core/src/lib.rs (the lone redaction authority). A second definition reintroduces \
         the drift hazard — a header added to one copy and not the other leaks a value. Found \
         definitions in: {const_sites:?}"
    );
    assert_eq!(
        fn_sites,
        vec![authority],
        "single-source violation: `is_sensitive_header` must be defined ONLY in \
         qfs-http-core/src/lib.rs. Sibling crates must re-export (`pub use qfs_http_core::…`) or \
         call it, never redefine it. Found definitions in: {fn_sites:?}"
    );
}
