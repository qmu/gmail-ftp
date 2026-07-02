# ADR 0008 — The multi-host account model: the CLI is a client of hosts, local is a host, and the account layers get their own verbs

- **Status**: Implemented (tickets 20260702120010–120070, shipped on branch work-20260702-012808)
- **Date**: 2026-07-02
- **Deciders**: project owner (direction + business framing); design review discussion 2026-07-02
- **Supersedes / superseded by**: revises the account-surface parts of the t45/t54 CLI behavior
  (unverified sign-up as a cloud gate; the `connection` namespace carrying app credentials, account
  tokens, and consent records). No prior ADR covers this ground.
- **References**: RFD-0001 §10 (least privilege; fail-closed; no secret on argv); t45 identity,
  t46 sessions + the OAuth AS (the only place a password is actually verified), t54 cloud
  bind/consent gate, t55 invites (membership ≠ authorization), t57 actor-policy, t66 broker
  (managed team connections), t80 per-recipient E2E DEK wrapping, t81 shared-connection gate;
  `CLOUD_DRIVERS` (qfs-secrets `consent.rs`); the `gh` CLI hosts model (github.com / GHES as peer
  hosts of one client) as prior art.

## Context

qfs is an open-source project intended to become a **commercial product**: the source stays open and
self-hostable, and a **managed service** must be able to argue its value on top of the same code.
The anticipated structure is (1) a multi-tenant managed offering with a notion of a qfs server
"host", (2) a CLI that supports both open-source sign-in and managed-service sign-in, and (3) both
usable **together** by one person.

A design review of the current account surface found five defects that any such future sharpens:

1. **The CLI "sign-in" is self-assertion.** `qfs identity signup` records an argon2id hash, but no
   CLI path ever verifies the password — the t54 cloud gate (`require_signed_in`) only checks that
   exactly one `users` row exists. Password verification exists solely on the HTTP face
   (`oauth.rs` → t46 sessions). Collecting a secret that is never checked is a liability, and
   calling the row-existence check "authentication" is dishonest documentation.
2. **The multi-identity cliff.** A second `signup` on a host flips the sole-user resolution to
   `SoleUser::Many`, which fails every cloud bind closed — and the CLI has no login/session to
   recover with. Safe, but a dead end reachable by one innocent command.
3. **The `connection` namespace is a grab-bag.** `connection add google-app` (OAuth app secret),
   `connection add google <email>` (an account's refresh token), and `connection add gmail default`
   (a consent record) are three different layers pushed through one verb and one key space.
4. **Two selection mechanisms.** Most drivers select the active connection by name
   (`connection use`); Google selects by account email (`QFS_GOOGLE_ACCOUNT` / the active `google`
   connection). Global mutable selection state also makes a query's target ambiguous to a reader.
5. **The vault is passphrase-shaped, the future is not.** The credential store unlocks under one
   `QFS_PASSPHRASE`-derived KEK. The managed service must instead generate and hold key material
   in real key storage; OS keychains and agent processes sit in between. Hard-wiring the
   passphrase as *the* mechanism blocks the commercial answer.

The tempting reading is that the OSS CLI and the managed service are two products with two account
models. That fork is what this ADR rejects.

## Decision

### 1. The CLI is a multi-host client; **local is an implicit host**

A **host** is anything that executes qfs statements and owns accounts, connections, and a vault:

- **`local`** — the embedded engine in the CLI process (today's behavior), backed by the System /
  Project DBs. Implicit; always present; no network.
- **A self-hosted server** — the same open-source binary run as `qfs serve` on the user's infra.
- **The managed service** — the company-operated, multi-tenant host (`qfs.cloud`).

All three speak the same protocol and the same pipe-SQL. The CLI signs into hosts
(`qfs host login <url>`), may hold several host sessions at once, and every account, connection,
and mount is scoped to exactly one host. OSS vs managed stops being a fork: they are **two hosts of
one client**. (Prior art: the `gh` CLI's hosts file, where github.com and an Enterprise server are
peers.)

Multi-tenancy, org modelling, and billing are **internal to the managed host** and deliberately not
designed here — they cannot leak into the protocol seam this ADR fixes.

### 2. Authentication is the host's job — local delegates to the OS, remote hosts verify for real

- **Local host:** no password. Whoever can run the binary under this OS user *is* the operator —
  the OS login is the authentication layer, and pretending otherwise (an unverified password) adds
  risk, not security. `qfs init` (the first-run wizard, subsuming `identity signup`) creates the
  vault and the operator identity with an email as an **accountability label only**.
  **One `$HOME` = one operator** becomes an invariant: a second signup is refused with a clear
  error instead of bricking cloud binds (defect 2). Teams never share a `$HOME`; they meet on a
  server host.
- **Remote hosts (self-hosted or managed):** a real login — the **existing** t46 session machinery
  and OAuth AS, promoted from "local web face" to "how any remote host authenticates its
  operators". A password set for a remote host is verified there on every session (defect 1
  resolved: the only passwords we keep are ones something actually checks).

### 3. The account layers get their own verbs (dissolve the `connection` grab-bag)

Four nouns, one per layer (defect 3):

| Verb | Layer | Examples |
| ---- | ----- | -------- |
| `qfs init` / `qfs host` | operator ↔ hosts | `qfs init`, `qfs host login qfs.cloud`, `qfs host list` |
| `qfs app` | OAuth **app** registrations | `qfs app add google < credentials.json` |
| `qfs account` | external **service accounts** (token + consent) | `qfs account add google` (browser consent → `google:you@gmail.com`), `qfs account list` |
| `qfs connect` | **mounts** (routes) | `qfs connect /mail gmail you@gmail.com` |

`connection add/use/list/remove` is retired. qfs is pre-release: this is a hard break with no
compatibility shim (the project's standing no-backward-compat rule).

### 4. Selection state is abolished — **the mount carries (host, driver, account)**

There is no "active connection" and no `QFS_GOOGLE_ACCOUNT` selection (defect 4; the env var may
remain as a CI override only). A mount binds the full coordinate:

```sh
qfs connect /mail       gmail you@gmail.com                      # local host's account
qfs connect /mail-team  gmail team@corp.com --host qfs.cloud     # managed host's account
```

Two Gmail accounts coexist as two paths; a statement's target is readable from the statement alone
(the same property that makes DESCRIBE/preview trustworthy, extended to identity). This composes
with the declarative `connections.qfs` (`CONNECT /mail DRIVER gmail ACCOUNT 'you@gmail.com'`), and
one person combining OSS and managed (requirement 3) is just two mounts. Cross-host federation in
one query becomes a natural later extension; the first cut may keep each statement single-host.

### 5. The vault key lives in a **guardian slot**, and managed KMS is one of the slots

The vault DEK is wrapped per **KeyGuardian**, LUKS-key-slot style — several wraps of the same DEK
side by side (t80's per-recipient wrapping is already this mechanism):

```text
              vault DEK
   ┌─────────────┼──────────────┬───────────────┐
[passphrase]  [OS keychain]  [qfs-agent]   [managed KMS]
 (today)      (Mac/libsecret) (cross-pane)  (the product)
```

`QFS_PASSPHRASE` becomes the first guardian, not the mechanism. `qfs vault enroll keychain` adds a
slot (solving the per-pane re-entry complaint with the passphrase kept as recovery); the managed
host holds its vault in real key storage so a signed-in operator never touches a passphrase — the
commercial answer (defect 5) lands as a slot, not a fork.

### 6. What is reserved now vs. deferred

**Now (cheap, prevents rework):** the `host` dimension on account/connection/mount rows (a column
defaulting to `'local'`); the `qfs host` verb namespace; the four-layer verb split; the
one-`$HOME`-one-operator invariant; remote auth = t46 sessions.

**Deferred (uncertain, low delay-cost):** multi-tenant internals, org/billing, cross-host query
execution, self-host edition/licensing splits, the guardian implementations beyond passphrase.

## Consequences

- **The managed pitch becomes structural.** Same CLI, same language, switch hosts: no OAuth-app
  console ritual (the managed host offers first-party apps), no passphrase (KMS guardian),
  always-on jobs/triggers/endpoints, teams (t66/t80/t81 were always managed-shaped), multi-device.
  Every item is a host capability, not a product fork.
- **Docs get honest.** "Sign-in mandatory" for local cloud binds is re-documented as what it is
  (an accountability identity; OS-delegated authentication), and the unverified-password ritual
  disappears from every cookbook happy path.
- **Breaking CLI change.** All cookbooks, the getting-started guide, `install.sh` next-steps, the
  account-model overview, and the generated skills must be rewritten when the verbs land —
  acceptable pre-release, and the cookbook parse ratchet + `gen-skills --check` make the sweep
  mechanical to verify.
- **The t54 gate is re-scoped, not weakened.** Cloud binds still fail closed; what changes is what
  satisfies the gate (a local operator label + recorded consent locally; a verified session on
  remote hosts). Consent records and least-privilege scopes (t54/t57) carry over unchanged.
- **Risk: the `host` seam must stay thin.** The reserved column and verb cost little, but protocol
  work (what exactly `host login` speaks; how mounts serialize the host) is a real design surface
  the first remote-host ticket must own explicitly.
