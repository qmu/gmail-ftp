---
created_at: 2026-06-30T01:01:30+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort: 2h
commit_hash:
category: Changed
depends_on: []
---

# Plain-language onboarding: de-jargon the `QFS_PASSPHRASE` text

Roadmap "Onboarding & polish": first-run and credential text is too jargon-heavy. The worst offender
is `QFS_PASSPHRASE`, described as *"the master passphrase that unlocks your local credential vault
(argon2id KDF; NOT a service credential)."* Rewrite as plain English — *a password you choose that
encrypts the service logins you save on this machine* — and move the cryptography detail into a "how
it's stored" section for those who want it.

## Where the text lives (all prose — no clap help string compiled into the binary)

The jargon appears in **five** places that must stay consistent:

- `packages/qfs/install.sh:32-41` (`next_steps()`, line 34: "the master passphrase … argon2id KDF;
  NOT a service credential").
- `docs/guide/getting-started.md:222-237` ("Connecting a real service", line 224 same phrasing).
- `docs/guide/connections.md:45-68` ("Unlocking the store with `QFS_PASSPHRASE`", line 47 "master
  passphrase").
- `docs/guide/concepts.md:271`.
- `README.md:50-52`.

## Plan

1. Rewrite all five occurrences in plain English.
2. Move the crypto detail (argon2id KDF, data-key envelope, per-store salt) into a dedicated
   **"How it's stored"** subsection in `docs/guide/connections.md` (the README envelope-encryption
   detail at lines 47-55 is the deep-dive anchor).

## Considerations

- These are hand-authored files (safe to edit). `gen-docs --check` does **not** cover guide pages, so
  no regeneration needed and no Rust change — but if any binary text/version changes, bump the patch
  in `crates/qfs/Cargo.toml` per CLAUDE.md.
- Related: the post-install "next steps" rewrite (`20260630010110`) edits the same `install.sh
  next_steps()` and `README` — that ticket depends on this one to avoid clobbering.
