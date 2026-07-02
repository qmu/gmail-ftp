---
created_at: 2026-07-02T12:01:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, DB]
effort:
commit_hash: be17f8e
category: Added
depends_on: [20260702120030-qfs-init-one-operator.md]
---

# `qfs host` skeleton — gh-style hosts records, no protocol yet

Part of EPIC `20260702120000` (ADR 0008 §1/§6). Reserve the client-of-hosts surface: a hosts
record store and the `qfs host` verb, **without** the remote protocol (explicitly deferred by the
ADR — "the first remote-host ticket must own what host login speaks"). Owner-decided scope: the
gh-CLI-style hosts file skeleton — `login` records the host, performs no network I/O.

## Steps

1. **Hosts store** (System DB — a new SYSTEM_MIGRATIONS version in
   `packages/qfs/crates/store/src/lib.rs`): a `hosts` table — `(name, url, kind
   'local'|'remote', session_ref NULLABLE, created_at)`. `local` is seeded implicitly (present
   without login, cannot be removed). `session_ref` is a placeholder selector for the future t46
   session token — no token is stored by this ticket.
2. **`qfs host list|login|logout`** (new verb + launcher):
   - `list` — always shows `local (implicit)`, plus recorded remotes.
   - `login <url>` — validates + normalizes the URL, records the row, prints that remote sessions
     are not yet implemented (honest, actionable: the record exists so mounts can reference it
     when the protocol lands). NO network I/O.
   - `logout <name>` — removes the record (refuses `local`).
3. **Coordinate hookup**: `qfs connect --host <name>` (from `20260702120010`) validates the name
   against the hosts store — an unknown host is an actionable error naming `qfs host login`.
   Binding a mount to a remote host is allowed to *record* but the bind path fails closed with
   "remote hosts are not yet executable" (fail-closed, never silent).

## Key files

- `packages/qfs/crates/store/src/lib.rs` (SYSTEM_MIGRATIONS + table-existence tests)
- `packages/qfs/crates/cmd/src/lib.rs` + `crates/qfs/src/main.rs` (verb + launcher)
- a new `packages/qfs/crates/qfs/src/hosts.rs` (store I/O, following `path_binding.rs`'s shape)
- `packages/qfs/crates/qfs/src/connection.rs::run_connect` (host validation)

## Considerations

- **Naming collision**: `crates/qfs/src/host.rs` (TokioHost — the `qfs serve` runtime host) is
  unrelated. Name the new module `hosts.rs` (plural) and keep rustdoc explicit about the
  distinction.
- Keep the record shape minimal — every column needs a use case now (data-minimization policy);
  the protocol ticket adds what it needs when it exists.
- The ADR's "the host seam must stay thin" risk lands here: this ticket must NOT sketch a
  protocol (no HTTP client, no auth flow) — records only.

## Quality Gate

Global gate (EPIC) plus:

- Hermetic: hosts round-trip (login records/list shows/logout removes); `local` is always listed
  and refuses logout; `login` performs no network I/O (no reqwest/transport symbol in the module —
  assert by code review + the module having no transport dep); duplicate login upserts rather than
  duplicating.
- `qfs connect --host unknown` fails with the actionable error; `--host local` behaves identically
  to omitting the flag; a remote-host mount records but fails closed at bind with the documented
  message.
- Dispatch tests for the `host` verb.
