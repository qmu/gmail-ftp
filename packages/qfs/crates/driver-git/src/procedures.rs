//! The `CALL git.*` procedure declarations (RFD §3): `merge`, `rebase`, `checkout`, `tag` — the
//! irreducible state transitions that do not reduce to a single CRUD verb. `CALL` resolves ONLY
//! these declared procedures (the capability gate); `git.merge` ≠ `github.merge` by namespace
//! (the mount qualifies them). The plan *construction* for each lives in [`crate::planner`]; this
//! module is the declaration surface `procedures()` returns.
//!
//! None is declared `irreversible`: every effect a git procedure produces is content-addressed or
//! reflog-recoverable (the deliberate git safety win, RFD §6). `requires_scopes` carries the local
//! write-scope label only (never a credential — the local object model needs no network token).

use qfs_driver::{Param, ProcSig};
use qfs_types::ColumnType;

/// The declared `CALL git.*` procedures. `procedures()` returns this slice; an undeclared
/// `CALL git.foo` is rejected structurally by [`qfs_driver::resolve_proc`].
#[must_use]
pub fn git_procedures() -> Vec<ProcSig> {
    let scopes = vec![crate::GIT_WRITE_SCOPE.to_string()];
    vec![
        // git.merge(base, ours, theirs) → a pure plan DAG; a conflict is a typed plan-build error
        // in PREVIEW with zero effects (NOT irreversible — content-addressed + reflog-recoverable).
        ProcSig::new("merge")
            .with_params(vec![
                Param::new("branch", ColumnType::Text),
                Param::new("base", ColumnType::Text),
                Param::new("ours", ColumnType::Text),
                Param::new("theirs", ColumnType::Text),
            ])
            .irreversible(false)
            .requires_scopes(scopes.clone()),
        // git.rebase(...) — same conflict surface as merge (replay onto ours).
        ProcSig::new("rebase")
            .with_params(vec![
                Param::new("branch", ColumnType::Text),
                Param::new("base", ColumnType::Text),
                Param::new("ours", ColumnType::Text),
                Param::new("theirs", ColumnType::Text),
            ])
            .irreversible(false)
            .requires_scopes(scopes.clone()),
        // git.checkout(ref) — move HEAD; reflog-recorded, reversible.
        ProcSig::new("checkout")
            .with_params(vec![Param::new("ref", ColumnType::Text)])
            .irreversible(false)
            .requires_scopes(scopes.clone()),
        // git.tag(name, target) — create a lightweight tag ref (CAS creation).
        ProcSig::new("tag")
            .with_params(vec![
                Param::new("name", ColumnType::Text),
                Param::new("target", ColumnType::Text),
            ])
            .irreversible(false)
            .requires_scopes(scopes),
    ]
}
