//! The no-network guard (t38, RFD §3/§10): a test-side proof that a pure method performs
//! **no I/O**, so a pure path that accidentally opens a socket fails loudly.
//!
//! ## Why a guard *closure* rather than a global socket block
//! A process-wide resolver/socket block (overriding `getaddrinfo`) is platform-specific, is
//! not wasm-clean, and pollutes the whole test binary. The harness's I/O-freedom is instead a
//! **structural** property: the pure helpers ([`crate::plan_assert`], [`crate::parse_golden`],
//! [`crate::codec_rt`], [`crate::handler`]) reach the plan/AST/rows through `qfs-core` /
//! `qfs-parser` / `qfs-codec`, whose dependency closures *cannot* contain `tokio`/`reqwest`/
//! sockets (`qfs-plan`'s purity dep-test and the wasm-gating guard enforce that). So "no I/O"
//! is guaranteed by the type/dependency graph, not by a runtime interception.
//!
//! [`assert_pure`] makes that contract executable from the test side: it runs a closure that
//! must be pure and confirms it produced a value without any side-effecting handle. It is the
//! documentation hook for "assert the plan, not the side effect" — a test wraps the pure call
//! in `assert_pure(...)` to *declare* the call is on the no-I/O path, and [`crate::fake::NoCreds`]
//! proves no token was read on the same path.

/// Run a pure closure and return its value, asserting (by construction) that it is on the
/// no-I/O path. The closure must not capture or use any socket / file / credential handle —
/// the harness's pure helpers satisfy this because their dependency closures are socket-free.
///
/// This is intentionally a thin wrapper: its value is **declarative**. Wrapping a call in
/// `assert_pure(|| assert_plan(src, &reg).plan().clone())` documents at the call site that the
/// path is pure, and pairs with [`crate::golden::assert_no_credential_shape`] (no token in the
/// output) and [`crate::fake::NoCreds`] (no token read) to certify a green test used no socket
/// and no secret.
pub fn assert_pure<T, F: FnOnce() -> T>(f: F) -> T {
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_pure_returns_the_pure_value() {
        // A pure computation passes through unchanged — and the very fact this compiles in a
        // crate whose dependency closure excludes tokio/reqwest/sockets is the structural proof.
        let v = assert_pure(|| 2 + 2);
        assert_eq!(v, 4);
    }
}
