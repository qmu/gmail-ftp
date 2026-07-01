---
title: qfs GitHub Release (release-on-tag)
environment: production
confirmation_method: other
url: https://github.com/qmu/qfs/releases
---

## Procedure

qfs is **deploy-on-merge / release-on-tag** — there is no separate server. The published
GitHub Release IS the deliverable; `install.sh` consumes its native tarballs.

1. **Pre-merge readiness (branch-level proof).** All gates green on the branch and the patch
   version bumped:
   - `cd packages/qfs`
   - `cargo build --workspace`
   - `cargo test --workspace`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo fmt --all --check`
   - `cargo run -p xtask -- gen-docs --check`
   - `cargo run -p xtask -- gen-skills --check`
   - `packages/qfs/crates/qfs/Cargo.toml` `version` is the new `X.Y.Z`, ahead of `main`.
2. **Merge** the PR to `main` (promotes the change).
3. **Tag and push** from the merge commit — this triggers the release:
   - `git tag -a vX.Y.Z -m "qfs vX.Y.Z"`
   - `git push origin vX.Y.Z`
4. `.github/workflows/release.yml` builds the four native tarballs (Linux musl + macOS, both
   arches) on per-OS runners and publishes them to a GitHub Release via
   `softprops/action-gh-release` (CI owns publishing — do not create the release by hand).

## Confirmation

Split by phase (deploy-on-merge):

- **Pre-merge (readiness proof):** the seven gate commands in step 1 all pass and
  `Cargo.toml`'s `version` equals the target `X.Y.Z` (ahead of `main`). This is the
  branch/staging-level proof recorded before merge.
- **Post-merge (production promotion check):** after the `vX.Y.Z` tag is pushed, confirm the
  Release actually published with its artifacts:
  - `gh release view vX.Y.Z --repo qmu/qfs --json tagName,assets,isDraft`
  - **Pass** when the Release exists, `isDraft` is `false`, and `assets` contains the four
    native tarballs (`*-linux-*`, `*-darwin-*`). `release.yml` typically takes several minutes
    to finish the per-OS builds, so poll until the assets appear (or inspect the run with
    `gh run list --workflow=release.yml`).
