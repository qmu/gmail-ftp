---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: M
commit_hash: fb27e16
category: Added
depends_on: [20260626101200-t54-cloud-connections-consent-flows.md, 20260626101400-t56-upstream-oidc-federation.md]
---

# t66 — qfs Cloud OAuth brokering + team connections

## Overview

Delivers the **managed-tier team connections** for M9 (roadmap §3.2/§3.3): on a qfs Cloud Team, the
team's `connections` to Drive, Gmail, GitHub, and Slack are already wired at the **project** level —
no per-user GCP OAuth client, no tokens to mint — and `POLICY` decides what each member may touch.
The connection is brokered by qfs Cloud and shared across members. This builds directly on existing
pieces: t54 already wired the per-user consent flow into `connection add` using
`crates/google-auth` (`authorize()`/`StoredTokenSource`/`GoogleApiClient`) plus the GitHub analogue;
t56 added upstream OIDC federation so one identity reaches the cloud. What is new here is making a
connection a **team/project-shared** row (envelope-encrypted refresh token owned by the project, not
a person) and a brokering path so members act *as the team* without personal credential setup. No new
keywords; this is connection-resolution + identity wiring.

## Exact seams

- `crates/secrets/src/store.rs` `pub trait Secrets { get/put/remove/list }` and the t44-renamed
  `ConnectionId`/`ConnectionRecord` (formerly `crates/secrets/src/key.rs` `AccountId`/`AccountRecord`)
  — a team connection is a `ConnectionRecord` owned by a project rather than a user; the broker
  resolves it via `resolve()` (`crates/secrets/src/resolve.rs`, `AccountSource`/`Resolution` ladder)
  extended with a project/team source.
- t43 envelope-encrypted SQLite store — the team's refresh tokens are stored envelope-encrypted in
  the Project DB (per-connection data-key), reusing the t43 backend; `Secret` redaction/zeroization
  (`crates/secrets/src/secret.rs`) is unchanged.
- t54 consent wiring + `crates/google-auth/src/lib.rs` — `OAuthClient`/`authorize()`/
  `StoredTokenSource`/`GoogleApiClient` (Bearer inject, refresh-on-401). The broker reuses these to
  mint and refresh the *team* token once; members never run the consent themselves.
- t56 upstream OIDC federation — a member signs in (or federates upstream per decision D); the broker
  maps the federated identity → team membership to decide which project connections resolve.
- `crates/server/src/policy/enforce.rs` `evaluate(policy, plan) -> PolicyDecision` (pure,
  default-deny) and `crates/server/src/policy/model.rs` (`Rule`/`Policy`) — reach is bounded by
  `POLICY`, not by who holds the token; a shared team connection does NOT widen authorization.
- `/sys/connections` (the t53 `qfs-driver-sys` path) — team connections surface here as names +
  scopes + metadata only, never secrets (roadmap §3.2 `FROM /sys/connections |> SELECT service, name,
  scopes`).
- `crates/qfs/src/commit.rs` `networked_credential(driver)` / `live_registry()` — live cloud apply is
  gated on the brokered team credential resolving for the acting member.

## Implementation steps

1. Data model: extend the t44 `ConnectionRecord` / t43 Project-DB schema so a connection can be owned
   by a `project`/`team` rather than a single user (an `owner_scope` column: user | project). Add the
   System/Project-DB migration via t42's runner. Tree green (behavior unchanged for user-owned
   connections). 
2. Resolution: extend `crates/secrets/src/resolve.rs` `resolve()` with a project/team
   `AccountSource` so an acting member resolves the project's shared connection when policy grants it;
   pure resolution tests (no live creds) covering precedence (explicit user override > team default).
3. Brokering: in the binary leaf, when a cloud driver needs a credential for a team connection,
   resolve the project-owned token and refresh it via t54's `crates/google-auth`
   `StoredTokenSource`/`GoogleApiClient` (or the GitHub analogue) — the refresh writes back the
   envelope-encrypted token to the Project DB, not to a per-user slot.
4. Membership gate: tie connection resolution to t56 federated identity → team membership; a
   non-member (or a member without the `POLICY` grant) gets default-deny, not the token. Plan-level
   tests assert resolution returns "denied" without ever touching a secret.
5. `/sys/connections` view: surface team connections (service, name, scopes, owner_scope) through the
   t53 `qfs-driver-sys` read path — names + metadata only, never secrets (a redaction test asserts no
   token material is ever projected).
6. Docs + version: document team connections only after a live two-member smoke (member A's consent →
   member B acts as the team within policy) passes; patch-bump `crates/qfs/Cargo.toml`;
   `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/secrets/src/key.rs` (`ConnectionRecord` + `owner_scope`), `crates/secrets/src/resolve.rs`
  (project/team source in the `resolve()` ladder).
- t43 Project-DB schema + t42 migration for `owner_scope`.
- `crates/qfs/src/commit.rs` (broker resolution in `live_registry()`/`networked_credential`),
  binary brokering wiring reusing `crates/google-auth`.
- t53 `qfs-driver-sys` `/sys/connections` projection (metadata-only).
- `crates/qfs/Cargo.toml` (patch bump).

## Considerations

- **Secrets never leak — the floor for this whole ticket.** Team refresh tokens are
  envelope-encrypted at rest (t43), resolved by handle, refreshed in the leaf, and NEVER projected
  through `/sys/connections` or logs (`crates/secrets/src/secret.rs` redaction/zeroization is the
  authority; assert with a redaction test). `connections()`/`/sys/connections` show names + scopes
  only (roadmap §2.2/§3.2).
- **Authorization is `POLICY`, not token possession.** The product point is that members act *as the
  team* but bounded by `POLICY` (default-deny via `crates/server/src/policy/enforce.rs` `evaluate`).
  Sharing a connection must not become an implicit grant — membership + policy decide reach
  independently of who can resolve the token.
- **Identity ≠ authorization (roadmap §4.1).** t56 federation answers *who*; the team-connection
  resolution answers *what may connect*; keep them separate seams.
- **Reuse, don't re-implement.** t54 already built the consent + refresh clients; this ticket changes
  *ownership and resolution*, not the OAuth client code. Do not fork `crates/google-auth`.
- **Open product decision to FLAG:** the qfs Cloud brokering topology (does qfs Cloud hold the team
  refresh token centrally, or does each tenant's Project DB?) and the per-user-override precedence
  policy are managed-tier shaped — name them in the PR rather than guessing.
- **Versioning:** own PR + patch bump in `crates/qfs/Cargo.toml` (currently 0.0.7) + `v0.0.x` tag on
  ship.
