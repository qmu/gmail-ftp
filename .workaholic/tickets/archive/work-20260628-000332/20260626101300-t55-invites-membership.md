---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: M
commit_hash: ecb5561
category: Added
depends_on: [20260626100300-t45-identity-users-accounts-local-signup.md]
---

# t55 — Invites (email / one-time URL) + membership

## Overview

Delivers the front door of milestone **M5** ("Self-hosted multi-user", roadmap Part 6) and the
roadmap §3.3 "Invites & membership" behavior: a host operator invites a person by email (when the
server is configured for outbound mail) or hands out a **one-time signup URL**, and the invited
person joins the host's own identity store, becoming a member of the host and of a project. This
implements the joining half of decision B (every deployment holds its own `users`/`accounts`) and
the §4.1 separation of identity from authorization — membership says *you belong here*, `POLICY`
(t57) still decides *what you may touch*. The identity tables `users`/`accounts` and local
sign-up already exist as a library after t45 (new `qfs-identity` crate over the System DB). What
is genuinely **new**: an `invites` table, one-time/expiring invite tokens, a `memberships`
concept (host + per-project), and the accept-invite path that turns a token into a `users` row.

## Exact seams

- `qfs-identity` (new in t45) over the System DB — extend with `invites` and `memberships`
  tables and the accept-invite operation that calls t45's local sign-up to create the `users`
  row; reuse t45's argon2id password hashing path for the password set during accept.
- `crates/secrets/src/secret.rs` `Secret` (redacted/zeroized) and `crates/crypto-core/src/lib.rs`
  (`sha256`, `sha256_hex`, `constant_time_eq`, `hex_lower`, ZERO deps) — invite tokens are
  high-entropy random handles stored as a `sha256` digest (never plaintext), compared with
  `constant_time_eq`; mirrors how session/bearer handles are stored elsewhere.
- t42 System DB + embedded migrations runner — `invites`/`memberships` are new System-DB tables
  added via a new versioned migration applied idempotently on relaunch.
- `crates/http/src/serve.rs` / `src/route.rs` `Router`/`compile_endpoint` / `src/handler.rs`
  `dispatch` / `src/params.rs` (typed param binding — the untrusted-input seam) — the
  accept-invite URL is served over the existing in-house listener; the one-time token arrives as
  an untrusted param and must be bound through `params.rs`.
- t46 session handling — accepting an invite establishes a session (opaque token, secure-cookie
  semantics via the `http-core` redaction-aware `HttpResponse`); reuse rather than re-implement.
- Email send path: when configured, an invite email is an ordinary qfs effect — a
  `CALL mail.send(...)` plan against `driver-gmail` through the standard commit path
  (`crates/exec` `apply_commit`), NOT a bespoke mailer. When mail is not configured, the
  one-time URL is the fallback (no silent failure).
- Binary wiring: `crates/qfs/src/serve.rs` (`run_serve`) wires the accept-invite route; the
  invite-create surface is an admin action (a write to a `/sys/*` path once t53 lands).

## Implementation steps

1. **Schema + migration (green, no behavior).** Add an `invites` table (id, email-or-null,
   token digest, project ref, role/initial-membership, expires_at, consumed_at, created_by) and a
   `memberships` table (user ref, scope = host|project, project ref, role) via a new t42
   migration. Pure rusqlite, tokio-free; unit-test the migration applies idempotently.
2. **Invite mint (pure core + store).** In `qfs-identity`, add `create_invite(...)` that
   generates a random token, stores only its `sha256` digest, and returns the plaintext token
   once to the caller. Unit-test digest storage and single-return semantics with no I/O.
3. **Accept path.** Add `accept_invite(token, signup_details)` that looks up by digest with
   `constant_time_eq`, rejects expired/consumed tokens, creates the `users` row via t45 local
   sign-up, inserts the `memberships` row(s), marks the invite consumed atomically, and returns
   a t46 session. Test expiry, replay (consumed), and wrong-token rejection.
4. **HTTP accept route + email-or-URL delivery.** Serve `GET/POST` accept over `crates/http`
   binding the token through `params.rs`; on success set the session cookie. Wire invite email as
   a `CALL mail.send` plan when configured, else surface the one-time URL. Native wiring in
   `crates/qfs/src/serve.rs`.
5. **Honest docs + version.** Document invite/accept in `docs/guide/*` only once it works; bump
   the patch in `crates/qfs/Cargo.toml`; run `cargo build/test/clippy/fmt` +
   `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/identity/src/*` (the t45 `qfs-identity` crate) — `invites`/`memberships` model,
  `create_invite`, `accept_invite`.
- New migration file under the t42 store crate's embedded migrations.
- `crates/qfs/src/serve.rs` — accept-invite route + invite-email plan wiring.
- `crates/http/src/route.rs` / `handler.rs` / `params.rs` — the accept route (untrusted token).
- `docs/guide/*` — invite/membership usage (honest, post-ship).

## Considerations

- **Safety floor.** Minting an invite and accepting one are explicit, committed actions — never
  implicit on a preview. The invite email is a real `CALL mail.send` effect and so is
  irreversible: it must go through the normal commit boundary (and, once t59 lands, the safety
  mode), not a side channel.
- **Token hygiene.** One-time tokens are high-entropy, stored only as `sha256` digests
  (`crypto-core`), compared in constant time, single-use (consumed atomically), and expiring.
  Never log the plaintext token; redact it in any audit record. Accept is the untrusted-input
  boundary — bind the token via `params.rs`, never string-concat it into a query.
- **Identity ≠ authorization (§4.1).** Membership grants belonging, not capability; what a member
  may do is still default-deny until `POLICY` (t57) grants it. Do not let "is a member" leak into
  the authorization decision.
- **Dep-direction & purity.** `qfs-identity` stays a pure-ish leaf over rusqlite (tokio-free);
  the HTTP/mail wiring lands on the binary leaf `crates/qfs`. Keep `qfs-cmd` free of these deps.
- **Open product decision (flag, don't guess).** Self-service signup vs. invite-only, whether the
  default initial membership is host-wide or project-scoped, and the role taxonomy
  (super-admin/project-admin/member) overlap with the t53 admin split that the roadmap leaves
  explicitly open (§3.4 info box) — flag these rather than baking them in.
- **Versioning.** One PR + patch bump in `crates/qfs/Cargo.toml` + `v0.0.x` tag on ship.
