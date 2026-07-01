---
created_at: 2026-06-30T00:41:30+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Config]
effort: 4h
commit_hash: c892417
category: Added
depends_on: [20260630004110-design-connection-declaration-grammar.md]
---

# Secret reference resolution (`env:` / `vault:`)

Part of EPIC `20260630004100`. Turn a `SecretRef` into a value at use time, secret-free on failure.

## Implementation steps

1. A `resolve_secret(SecretRef) -> Result<Secret, _>`:
   - `env:<VAR>` → read the environment variable; missing → a structured, secret-free error.
   - `vault:<path>` → read from the existing envelope-encrypted credential store (the same store
     `qfs connection add` writes, `crate::secret_store`/`qfs_secrets`); needs `QFS_PASSPHRASE`; a
     locked/absent store fails closed (never returns a partial/empty secret).
2. The resolved value is a `qfs_secrets::Secret` (redacting Debug/Display, zeroized on drop) — it
   never crosses a DTO, a log, or a `describe` (reuse the t27 discipline).
3. Resolution is **lazy**: declaring/describing a connection resolves nothing; only an actual
   read/commit pulls the secret.
4. Tests: env hit/miss; vault hit (seeded store) / locked / missing; assert the error carries no
   secret and a stable code; a planted-canary test that the value never appears in Debug output.

## Key files

- New `crates/qfs/src/secret_ref.rs` (or in `crate::connection`), `crate::secret_store`,
  `qfs_secrets`.

## Considerations

- The `vault:` path keys into the credential store by `(driver, connection)` or a free path — align
  with the design ADR's vault addressing. This is the bridge that keeps `qfs connection add` relevant
  as the secret *store* behind a `SECRET vault:…` reference.
