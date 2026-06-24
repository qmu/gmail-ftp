//! Planner-owned **E2E / external-interface** black-box validation of the t37 HTTP eval-error
//! hygiene close (the t32 carry-over).
//!
//! This is NOT a unit test and NOT a code review. It drives the PUBLIC `qfs-http` error surface
//! the way a caller observes it — building an [`qfs_http::HttpError`] from an executor error and
//! reading the rendered wire response body — and actively tries to make a sensitive upstream
//! string leak into the caller-facing body. The invariant under test:
//!
//! - For the **non-allowlisted** eval classes (`Auth` / `Internal`), where a careless driver is
//!   most likely to embed an upstream/credential string in its `message`, the rendered body must
//!   carry only the stable structured `code` + a generic per-class detail — NEVER the raw message.
//! - For the **safe** classes (`Parse` / `Usage` / `Capability` / `CommitRequired` /
//!   `CommitFailed`), the structured (secret-free, agent-facing) message is retained.
//!
//! We plant an unmistakable canary (`Bearer sk-LIVE-...-PLANTED`) inside each error's message and
//! assert the canary never reaches the body for the non-allowlisted classes, while the safe-class
//! message survives. Every assertion is on the observable rendered response (status + JSON body
//! bytes), never a private internal.

// E2E test: setup may unwrap/expect/panic freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_exec::{ErrorKind, ExecError};
use qfs_http::HttpError;

/// The planted canary: an upstream-style secret a careless driver might splice into an error
/// message. If it ever appears in a caller-facing body the hygiene control has failed.
const CANARY: &str = "Bearer sk-LIVE-7f9c-PLANTED-do-not-leak";

/// Render an `HttpError::Eval` over a planted-message `ExecError` of `kind` and return the
/// observable `(status, body_text)` the caller would see on the wire.
fn rendered_eval(kind: ErrorKind) -> (u16, String) {
    let err = ExecError::new(
        kind,
        "planted_code",
        format!("upstream said: {CANARY} is invalid"),
    );
    let http = HttpError::Eval(err);
    let status = http.status();
    let resp = http.into_response();
    (status, resp.body_text())
}

#[test]
fn auth_class_eval_error_drops_the_raw_upstream_message() {
    let (status, body) = rendered_eval(ErrorKind::Auth);
    // Auth maps to 403 at the wire.
    assert_eq!(status, 403, "Auth eval error should render 403: {body}");
    // The canary (and any fragment of it) must NOT survive into the caller-facing body.
    assert!(
        !body.contains(CANARY),
        "Auth-class body leaked the planted upstream secret: {body}"
    );
    assert!(
        !body.contains("sk-LIVE") && !body.contains("PLANTED"),
        "even a fragment of the planted secret leaked: {body}"
    );
    // It DOES carry the stable structured code + a generic per-class detail.
    assert!(
        body.contains("planted_code"),
        "the stable code should still surface for agent branching: {body}"
    );
    assert!(
        body.contains("credential/authorization error"),
        "a generic per-class detail should replace the raw message: {body}"
    );
    assert!(
        body.contains("\"error\":\"eval\""),
        "coarse class is eval: {body}"
    );
}

#[test]
fn internal_class_eval_error_drops_the_raw_upstream_message() {
    let (status, body) = rendered_eval(ErrorKind::Internal);
    assert_eq!(status, 500, "Internal eval error should render 500: {body}");
    assert!(
        !body.contains(CANARY) && !body.contains("sk-LIVE") && !body.contains("PLANTED"),
        "Internal-class body leaked the planted upstream secret: {body}"
    );
    assert!(
        body.contains("planted_code"),
        "the stable code should still surface: {body}"
    );
    assert!(
        body.contains("an internal error occurred"),
        "a generic detail should replace the raw message: {body}"
    );
}

#[test]
fn safe_classes_retain_their_structured_message() {
    // The executor's own well-typed, secret-free diagnostics keep their message: an agent
    // branches on these and they never carry a raw upstream/driver string.
    for kind in [
        ErrorKind::Parse,
        ErrorKind::Usage,
        ErrorKind::Capability,
        ErrorKind::CommitRequired,
        ErrorKind::CommitFailed,
    ] {
        // Use a benign, agent-facing message (NOT the secret canary) — these classes are
        // contractually secret-free, so retaining the message is safe and useful.
        let msg = "unsupported verb FOO; supported: INSERT, UPSERT, REMOVE";
        let err = ExecError::new(kind, "structured_code", msg);
        let body = HttpError::Eval(err).into_response().body_text();
        assert!(
            body.contains(msg),
            "safe class {kind:?} should retain its structured message: {body}"
        );
    }
}

#[test]
fn a_safe_class_carrying_a_secret_is_a_driver_bug_not_this_layers_concern_but_canary_in_unsafe_is_dropped(
) {
    // Cross-check: the SAME canary planted into a non-allowlisted (Auth) message is dropped,
    // confirming the allowlist gate — not the message content — is what decides retention.
    let (_, auth_body) = rendered_eval(ErrorKind::Auth);
    assert!(!auth_body.contains(CANARY), "{auth_body}");
}
